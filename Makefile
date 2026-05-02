# ESP32 Rust — runnable documentation for build / flash / monitor.
# See notes.txt for one-time setup (espup, brew deps, Python 3.12 shim).
#
# Quick start:
#   make wifi.env        # one-time: copy wifi.env.example -> wifi.env
#   $EDITOR wifi.env     # fill in WIFI_SSID and WIFI_PASS
#   make run             # build, flash, open serial monitor

PORT ?= /dev/cu.usbserial-0001
BIN  := target/xtensa-esp32-espidf/release/esp32-blinky

# Host triple for building the publisher tool. Detected via rustc so this
# works on any developer machine without hardcoding aarch64-apple-darwin.
HOST_TRIPLE := $(shell rustc -vV | sed -n 's/^host: //p')

# App-only firmware binary (no bootloader / partition table) suitable for
# pushing as the OTA artifact layer.
FW_BIN := target/firmware.bin

# OCI artifact destination.
OCI_REPO ?= ghcr.io/imjasonh/esp32

# Toolchain paths installed by `espup install --targets esp32`. The
# wildcards absorb the date-stamped subdirectory names so espup upgrades
# don't require editing this file.
LIBCLANG_PATH  := $(firstword $(wildcard $(HOME)/.rustup/toolchains/esp/xtensa-esp32-elf-clang/*/esp-clang/lib))
XTENSA_GCC_BIN := $(firstword $(wildcard $(HOME)/.rustup/toolchains/esp/xtensa-esp-elf/*/xtensa-esp-elf/bin))

# A throwaway directory containing `python3` -> uv-managed Python 3.12.
# Put on PATH ahead of system python so embuild bootstraps the IDF venv
# against 3.12 instead of Apple's /usr/bin/python3 (3.9). Created on demand
# by the `ensure-python-shim` target — not checked in.
PYTHON_SHIM := $(CURDIR)/.embuild/python-shim

# Exported to every recipe shell. Replaces the old export-esp.sh script.
export LIBCLANG_PATH
export PATH := $(PYTHON_SHIM):$(XTENSA_GCC_BIN):$(PATH)

# Wi-Fi credentials (gitignored). Sourced inline because wifi.env uses
# bash `export VAR="value"` syntax, not Make assignment syntax.
WIFI_ENV := . ./wifi.env

.PHONY: help build flash flash-all monitor run clean ensure-python-shim save-image publish pull-verify check-gh-env

help:
	@echo "Targets:"
	@echo "  make build     Compile firmware (requires wifi.env)"
	@echo "  make flash     Build (if needed) and flash app to $(PORT)"
	@echo "  make flash-all Erase + write bootloader, partition table, app"
	@echo "  make monitor   Open serial monitor on $(PORT)  (Ctrl+C to exit)"
	@echo "  make run       Build + flash + monitor"
	@echo "  make clean     cargo clean"
	@echo "  make wifi.env  Create wifi.env from template"
	@echo "  make save-image   Convert ELF -> app-only .bin (target/firmware.bin)"
	@echo "  make publish      Build, save-image, push OCI artifact to OCI_REPO"
	@echo "  make pull-verify  Pull from OCI_REPO and check layer matches local .bin"
	@echo ""
	@echo "Override the port: make flash PORT=/dev/cu.usbserial-XXXX"

build: wifi.env ensure-python-shim sdkconfig.defaults
	$(WIFI_ENV) && cargo build --release

# sdkconfig.defaults is generated from the .in template so we can substitute
# the absolute project path into CONFIG_PARTITION_TABLE_CUSTOM_FILENAME
# (IDF resolves it relative to the embuild synthetic project, not our repo
# root, so a bare "partitions.csv" doesn't work). Gitignored.
sdkconfig.defaults: sdkconfig.defaults.in partitions.csv
	@sed 's|@PROJECT_DIR@|$(CURDIR)|g' sdkconfig.defaults.in > sdkconfig.defaults

# Idempotent: creates $(PYTHON_SHIM) and (re)points python3 at the current
# uv-managed 3.12 interpreter. Auto-installs Python 3.12 via uv if missing.
ensure-python-shim:
	@command -v uv >/dev/null || { echo "ERROR: uv not installed. See notes.txt step 4."; exit 1; }
	@uv python find 3.12 >/dev/null 2>&1 || uv python install 3.12
	@mkdir -p $(PYTHON_SHIM)
	@ln -sf "$$(uv python find 3.12)" $(PYTHON_SHIM)/python3
	@ln -sf "$$(uv python find 3.12)" $(PYTHON_SHIM)/python

flash: build
	espflash flash --port $(PORT) $(BIN)

# Full reflash: erase entire flash, then write bootloader + partition table
# + app. Use this when the partition table changes (phase 0 of OTA), or as
# the recovery path if both OTA slots are bad and anti-bricking failed.
# After erase, otadata is empty and the bootloader picks ota_0 by default.
BOOTLOADER := $(firstword $(wildcard target/xtensa-esp32-espidf/release/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin))

flash-all: build
	@test -n "$(BOOTLOADER)" || { echo "ERROR: bootloader.bin not found under target/. Run 'make build' first."; exit 1; }
	espflash erase-flash --port $(PORT)
	espflash flash --port $(PORT) \
	    --bootloader $(BOOTLOADER) \
	    --partition-table partitions.csv \
	    $(BIN)

monitor:
	espflash monitor --port $(PORT)

run: build
	espflash flash --port $(PORT) --monitor $(BIN)

clean:
	cargo clean

# Convert the firmware ELF into an app-only .bin (no bootloader / partition
# table). This is the format the OTA writer expects on the device, and what
# we wrap as an OCI artifact layer for OTA distribution.
save-image: $(FW_BIN)

$(FW_BIN): build
	espflash save-image --chip esp32 $(BIN) $(FW_BIN)
	@echo "Wrote $(FW_BIN) ($$(wc -c < $(FW_BIN)) bytes)"

# Build firmware -> save-image -> push OCI artifact to OCI_REPO. Tags
# pushed: :latest and :sha-<short>. Requires GH_TOKEN in gh.env.
publish: $(FW_BIN) check-gh-env
	. ./gh.env && cd tools/publisher && cargo run --release --target $(HOST_TRIPLE) -- push \
	    --bin $(CURDIR)/$(FW_BIN) \
	    --repo $(OCI_REPO) \
	    --git-sha $$(git rev-parse --short HEAD)

# Pull the latest artifact from OCI_REPO and verify its layer SHA matches
# our locally-built firmware. Confirms the round trip works and exercises
# the same oci-distribution API the device will use in phase 2.
pull-verify: $(FW_BIN) check-gh-env
	. ./gh.env && cd tools/publisher && cargo run --release --target $(HOST_TRIPLE) -- pull-verify \
	    --repo $(OCI_REPO) \
	    --tag latest \
	    --bin $(CURDIR)/$(FW_BIN) \
	    --auth

check-gh-env:
	@test -f gh.env || { \
	    echo "ERROR: gh.env not found."; \
	    echo "Create it with:"; \
	    echo "    echo 'export GH_TOKEN=ghp_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx' > gh.env"; \
	    echo "See README.md for how to mint a classic PAT with write:packages."; \
	    exit 1; \
	}

# If wifi.env exists, this target is up-to-date and the recipe doesn't run.
# If it doesn't exist, copy the template, tell the user to edit it, and
# fail so they can't accidentally flash with placeholder creds.
wifi.env:
	@cp wifi.env.example wifi.env
	@echo ""
	@echo "Created wifi.env from wifi.env.example."
	@echo "Edit it with your real Wi-Fi SSID and password, then re-run."
	@echo ""
	@exit 1
