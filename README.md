# ESP32 Rust

Std Rust on an Inland ESP-WROOM-32 dev board. Currently a Wi-Fi + HTTPS
demo; e-ink display work coming next (see [`eink-plan.md`](eink-plan.md)).

## Hardware

- **Board**: Inland ESP-WROOM-32 (Micro Center SKU 027466). Connects as
  `/dev/cu.usbserial-0001` on macOS via the onboard CP210x USB-UART.
- **No user LED**: this specific board variant only has a power LED ("D1",
  always on). There is no software-controllable LED. For visible output,
  attach an external LED + ~330Ω resistor to a GPIO, or watch logs via
  `make monitor`.

## Prerequisites (one-time, macOS)

```bash
# Cargo tools:
#   espup     installs the Xtensa Rust toolchain (rustc fork + LLVM + GCC)
#   espflash  flashes firmware over USB serial
#   ldproxy   linker shim used by esp-idf-sys
cargo install espup espflash ldproxy

# C build deps for the ESP-IDF SDK
brew install cmake ninja dfu-util

# Xtensa Rust toolchain + GCC + esp-clang, installed under
# ~/.rustup/toolchains/esp/. The Makefile finds them via wildcards
# so future espup updates don't require any path edits.
espup install --targets esp32

# uv (the Makefile uses it to install Python 3.12 and shim it into PATH
# so the ESP-IDF Python venv is bootstrapped against 3.12 instead of
# Apple's 3.9). Skip if you already have uv.
curl -LsSf https://astral.sh/uv/install.sh | sh
```

## First build

```bash
make wifi.env             # creates wifi.env from template
$EDITOR wifi.env          # fill in WIFI_SSID and WIFI_PASS
make run                  # build, flash, open serial monitor
```

The first build clones ESP-IDF v5.2.2 into `.embuild/` and bootstraps a
Python venv inside it. Plan on 5–10 minutes. Subsequent builds are fast.

## Day-to-day

```
make build      Compile (requires wifi.env)
make flash      Build (if needed) and flash app to PORT
make flash-all  Erase + write bootloader, partition table, and app
make monitor    Open serial monitor; Ctrl+C to exit
make run        Build + flash + monitor
make clean      cargo clean

make flash PORT=/dev/cu.usbserial-XXXX   # override the port
```

Use `make flash-all` after partition-table changes, or as the recovery
path if both OTA slots end up bad and the bootloader can't auto-revert
(see "USB recovery" in [`ota-plan.md`](ota-plan.md)). Day-to-day use is
just `make flash` or `make run`.

## What the firmware does

`src/main.rs` connects to Wi-Fi using the credentials baked in at compile
time from `wifi.env`, then issues two HTTPS GETs and logs the responses
over serial:

- `https://api.ipify.org?format=json` — public IP, JSON
- `https://wttr.in/?format=3` — one-line weather for your location

## Project layout

```
src/main.rs            firmware
Cargo.toml             deps: esp-idf-svc, embedded-svc, log, anyhow
build.rs               embuild + cargo:rerun-if-env-changed for WIFI_*
.cargo/config.toml     target, linker, runner, build-std
rust-toolchain.toml    pins the `esp` rustc toolchain
sdkconfig.defaults     ESP-IDF kconfig overrides (stack, mbedtls bundle)
Makefile               canonical build / flash / monitor entrypoint
wifi.env.example       template for compile-time Wi-Fi creds
eink-plan.md           plan for the upcoming e-ink display work
notes.txt              internal design notes and issues hit during setup
```
