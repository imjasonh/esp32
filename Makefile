# ESP32 Rust — runnable documentation for build / flash / monitor.
# See notes.txt for one-time setup (espup, brew deps, Python 3.12 shim).
#
# Quick start:
#   make wifi.env        # one-time: copy wifi.env.example -> wifi.env
#   $EDITOR wifi.env     # fill in WIFI_SSID and WIFI_PASS
#   make run             # build, flash, open serial monitor

PORT ?= /dev/cu.usbserial-0001
BIN  := target/xtensa-esp32-espidf/release/esp32-blinky

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

.PHONY: help build flash monitor run clean ensure-python-shim

help:
	@echo "Targets:"
	@echo "  make build     Compile firmware (requires wifi.env)"
	@echo "  make flash     Build (if needed) and flash to $(PORT)"
	@echo "  make monitor   Open serial monitor on $(PORT)  (Ctrl+C to exit)"
	@echo "  make run       Build + flash + monitor"
	@echo "  make clean     cargo clean"
	@echo "  make wifi.env  Create wifi.env from template"
	@echo ""
	@echo "Override the port: make flash PORT=/dev/cu.usbserial-XXXX"

build: wifi.env ensure-python-shim
	$(WIFI_ENV) && cargo build --release

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

monitor:
	espflash monitor --port $(PORT)

run: build
	espflash flash --port $(PORT) --monitor $(BIN)

clean:
	cargo clean

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
