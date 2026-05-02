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
  signature, the cert chain to the Sigstore root, and that the signer's
  identity matches the allowlist provisioned into the device's NVS.
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
[`ota.md`](ota.md) for the full design.

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

## Optional: GCP Cloud Logging + Monitoring

The firmware can ship structured `tracing` events (both app and OTA) to
Google Cloud Logging *and* periodic chip-health metrics (heap, stack,
wifi, cpu, uptime, …) to Cloud Monitoring. Both are **opt-in per
device** — without the `[gcp]` block in `provisioning.toml` the device
boots normally and just emits to serial. Full design in
[`logs-plan.md`](logs-plan.md) and [`monitoring-plan.md`](monitoring-plan.md).

One service account and one key cover both APIs (the JWT requests
both scopes; one cached access token is shared by the cloud_log and
metrics threads).

One-time GCP setup, using `gcloud`:

```bash
PROJECT_ID=<YOUR_PROJECT_ID>
SA_NAME=<YOUR_SA_NAME>
SA_EMAIL=$SA_NAME@$PROJECT_ID.iam.gserviceaccount.com

# Create the service account.
gcloud iam service-accounts create $SA_NAME \
    --display-name="ESP32 device logger" \
    --project=$PROJECT_ID

# Grant only logging.logWriter + monitoring.metricWriter — least
# privilege. The device can write log entries and metric points;
# it cannot read, delete, or do anything else.
gcloud projects add-iam-policy-binding $PROJECT_ID \
    --member="serviceAccount:$SA_EMAIL" \
    --role="roles/logging.logWriter"
gcloud projects add-iam-policy-binding $PROJECT_ID \
    --member="serviceAccount:$SA_EMAIL" \
    --role="roles/monitoring.metricWriter"

# Create + download a JSON key. Keep this file safe — anyone with it
# can write logs + metrics as this SA.
gcloud iam service-accounts keys create gcp-sa-key.json \
    --iam-account=$SA_EMAIL \
    --project=$PROJECT_ID

# Extract the RSA private key PEM and the key id into the forms
# `tools/provision/` wants.
jq -j .private_key    gcp-sa-key.json > gcp-sa-key.pem  # -j: no trailing newline
KEY_ID=$(jq -r .private_key_id gcp-sa-key.json)
echo "sa_key_id = $KEY_ID"
```

Then add a `[gcp]` block to `provisioning.toml` (template in
`provisioning.toml.example`) using `$PROJECT_ID`, `$SA_EMAIL`, the
printed `KEY_ID`, and the path `gcp-sa-key.pem`. Re-run `make provision`
and reboot the device.

Logs land in Cloud Logging under
`projects/kontaindotme/logs/esp32-firmware`. The device's MAC is in
`resource.labels.node_id` for fleet filtering; the originating tracing
target is in `jsonPayload.module` so you can split app vs OTA in the
Cloud Logging UI (e.g.
`jsonPayload.module = "esp32_blinky::ota"` or
`= "esp32_blinky::sig"`).

Metrics land under `custom.googleapis.com/esp32/<name>` (e.g.
`free_heap`, `wifi_rssi`, `stack_hwm` with a `task` label). Snapshot
cadence is `metrics_interval_secs` in the `[gcp]` block (default 300s,
0 disables — cloud_log keeps running). Inspect with:

```bash
gcloud monitoring time-series list \
    --filter='metric.type=starts_with("custom.googleapis.com/esp32/")' \
    --interval-end-time=$(date -u +%Y-%m-%dT%H:%M:%SZ) \
    --interval-start-time=$(date -u -v-30M +%Y-%m-%dT%H:%M:%SZ) \
    --project=$PROJECT_ID
```

**Threat model**: the SA private key sits in NVS unencrypted. Anyone
with physical access to the chip can extract it. Mitigation is
strict scoping (only `roles/logging.logWriter`, only on a
logs-tolerant project). Real hardening = Flash Encryption + Secure
Boot v2 (deferred; see [`ota.md`](ota.md) Future work).
