# ESP32 Rust

[![CI](https://github.com/imjasonh/esp32/actions/workflows/ci.yml/badge.svg)](https://github.com/imjasonh/esp32/actions/workflows/ci.yml)
[![Publish OTA](https://github.com/imjasonh/esp32/actions/workflows/publish.yml/badge.svg)](https://github.com/imjasonh/esp32/actions/workflows/publish.yml)

Std Rust on an Inland ESP-WROOM-32 dev board with end-to-end OTA over
GHCR + cosign keyless signing. E-ink display work coming next (see
[`eink-plan.md`](eink-plan.md)).

## What it does today

- Connects to Wi-Fi, fetches public IP and weather over HTTPS.
- Polls `ghcr.io/imjasonh/esp32:latest` every ~60s for new firmware.
- For each new digest, fetches the cosign Sigstore Bundle, verifies the
  signature, the cert chain to the bundled Sigstore root, and that the
  signer's identity matches the compile-time allowlist in
  [`src/trust.rs`](src/trust.rs).
- On verify-pass, streams the layer to the inactive OTA partition,
  reboots, and only marks the new image valid after Wi-Fi + registry
  bringup checks pass. Any post-OTA failure auto-rolls back via the
  bootloader.

Push to `main` → CI builds → publish workflow pushes a signed image to
GHCR → device picks it up on its next poll. See
[`ota.md`](ota.md) for the full design.

## Hardware

- **Board**: Inland ESP-WROOM-32 (Micro Center SKU 027466). Connects as
  `/dev/cu.usbserial-0001` on macOS via the onboard CP210x USB-UART.
- **No user LED** on this variant — only a power LED. Watch logs via
  `make monitor`.

## Prerequisites (one-time, macOS)

```bash
cargo install espup espflash ldproxy
brew install cmake ninja dfu-util cosign
espup install --targets esp32
curl -LsSf https://astral.sh/uv/install.sh | sh   # if you don't have uv
```

## First flash (USB)

```bash
make provisioning.toml             # creates from template
$EDITOR provisioning.toml          # fill in wifi creds + trust identities
make bootstrap                     # build, flash everything, write NVS
make monitor                       # watch it boot and connect
```

The first build clones ESP-IDF v5.2.2 into `.embuild/` (5–10 min).
Subsequent builds are fast.

The OTA-distributed firmware **contains no secrets** — Wi-Fi creds and
trust roots live in NVS, written via USB by `make provision`. See
[`provisioning-plan.md`](provisioning-plan.md) for the full design.

## Day-to-day

```
make build      Compile firmware
make flash      Build + flash app (use flash-all after partitions change)
make flash-all  Erase + write bootloader, partition table, app
make provision  Write NVS partition from provisioning.toml over USB
make bootstrap  flash-all + provision (new device setup)
make monitor    Open serial monitor; Ctrl+C to exit
make run        Build + flash + monitor
make publish    Build, push OCI artifact to ghcr.io/imjasonh/esp32, cosign sign
make clean      cargo clean
```

`make publish` requires `gh.env` (see [`ota.md`](ota.md) for
PAT setup) and a real cosign OIDC flow the first time per ~10min window
— a browser pops to authenticate. CI does this automatically via the
GitHub Actions workflow's ambient OIDC token.

