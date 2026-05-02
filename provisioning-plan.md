# Provisioning — plan

Move secrets and per-device config out of the firmware image. The OTA
artifact becomes device-agnostic; per-device data lives in NVS and is
written via USB at provisioning time.

## What moves vs what stays

### Moves to NVS (provisioned via USB)

- **Wi-Fi credentials** — `WIFI_SSID`, `WIFI_PASS`. Currently embedded
  via `env!()`. Real secret; mustn't be in a public OCI image.
- **GCP service-account key + identifiers** — for `logs-plan.md`. Real
  secret.
- **`TRUSTED_IDENTITIES`** — currently a `const &[(&str, &str)]` in
  `src/trust.rs`. Not secret, but architecturally cleaner to store
  alongside the rest of the device config. See "Soft vs hard trust"
  below.
- **Sigstore root + intermediate CA PEMs** — currently
  `include_str!`. Same logic as TRUSTED_IDENTITIES.

### Stays in firmware

- Code paths that read the above from NVS
- Verification logic (`src/sig.rs`)
- OTA loop (`src/ota.rs`)
- Wi-Fi connect logic
- Cloud Logging sender (when added)
- Anything else that's pure logic

## Storage layout

NVS partition stays at `0x9000`, but we bump its size from 24 KB to 32
KB to absorb the new entries comfortably (Sigstore PEMs alone are ~3 KB
each; SA private key ~2 KB; etc.).

```
                    Old             New
  nvs:    0x9000   24 KB (0x6000)  32 KB (0x8000)
  otadata: shifts back by 8 KB; OTA app slots stay the same size
```

Three NVS namespaces:

```
ns        key            type    notes
─────────────────────────────────────────────────────────────────────
wifi      ssid           str     using ESP-IDF's native wifi NVS
wifi      pass           str     storage (esp_wifi_set_storage)

device    fw_repo        str     "ghcr.io/imjasonh/esp32"
device    fw_tag         str     "latest"
device    poll_secs      u32     default 60
device    last_digest    str     OTA state (already there)
device    pending_digest str     OTA state (already there)

trust     identities     blob    JSON array of {identity, issuer} pairs
trust     fulcio_root    blob    PEM bytes
trust     fulcio_inter   blob    PEM bytes

gcp       project_id     str
gcp       sa_email       str
gcp       sa_key_pem     blob    RSA private key PEM (~1.7 KB)
gcp       log_severity   u8      0=DEBUG..4=ERROR; default 1=INFO
```

Reusing the existing `wifi` and `device` namespaces; new `trust` and
`gcp` namespaces.

## Provisioning tool

A new `tools/provision/` cargo project (host-side, like
`tools/publisher/`) that:

1. Reads a `provisioning.toml` (gitignored) with the operator's values:
   ```toml
   [wifi]
   ssid = "..."
   pass = "..."

   [device]
   fw_repo = "ghcr.io/imjasonh/esp32"
   fw_tag = "latest"
   poll_secs = 60

   [trust]
   identities = [
     { identity = "imjasonh@gmail.com",
       issuer = "https://accounts.google.com" },
     { identity = "https://github.com/imjasonh/esp32/.github/workflows/publish.yml@refs/heads/main",
       issuer = "https://token.actions.githubusercontent.com" },
   ]
   fulcio_root_pem = "trust/fulcio_root.pem"
   fulcio_intermediate_pem = "trust/fulcio_intermediate.pem"

   [gcp]
   project_id = "..."
   sa_email = "..."
   sa_key_pem = "gcp-sa-key.pem"
   log_severity = "info"
   ```
2. Builds a binary NVS image using ESP-IDF's `nvs_partition_gen.py`
   (we already have it in `.embuild/`):
   ```
   python .embuild/.../nvs_partition_gen.py generate \
       provisioning.csv target/nvs.bin 0x8000
   ```
   (the tool generates the intermediate CSV from the TOML).
3. Flashes the NVS partition over USB:
   ```
   espflash write-bin --address 0x9000 target/nvs.bin
   ```

Wired into the Makefile as:
```
make provision     # writes NVS partition from provisioning.toml
```

Single command, single source of truth (the TOML), no on-device console
code needed — all the work happens host-side.

## First-boot behavior (unprovisioned device)

Right after `EspNvs::new("device", ...)`, the firmware checks for the
required keys (`wifi/ssid` and `trust/identities` are the bare
minimum). If absent:

1. Log loudly to serial: `"NOT PROVISIONED — run 'make provision' from
   the project root after editing provisioning.toml"`.
2. Sit in a slow loop blinking an external LED if one's wired (or just
   the serial log). **Do not** start Wi-Fi or OTA.
3. Reboot every 60s in case of a stuck state, and re-check.

A fresh device flashed with `make flash-all` but no `make provision`
sits in this state until provisioned. After provisioning, normal boot.

## Soft vs hard trust separation

Putting `TRUSTED_IDENTITIES` in NVS gives a **clean architectural
separation** but only a **soft guarantee**. The OTA-distributed
firmware code is what reads NVS; a malicious OTA image could ignore
NVS and hardcode "trust everyone". To prevent that cryptographically,
we'd need **Secure Boot v2** + Flash Encryption, which is a real
subproject (irreversible eFuse burns; high stakes). See "Future
work".

In practice this still gets us most of what we want:
- The OCI image is no longer a per-device artifact — same bytes
  everywhere.
- Trust changes happen via a deliberate USB flash (re-provision), not
  as a side effect of regular OTA.
- The convention is enforced by code review of the firmware that gets
  signed and shipped.

## Build / dev flow changes

- `gh.env` stays (still needed for `make publish`'s cosign registry
  auth). `wifi.env` and the `WIFI_SSID`/`WIFI_PASS` `env!()` calls in
  `src/main.rs` go away.
- `src/main.rs`, `src/trust.rs`, future cloud-log code all read
  exclusively from NVS. No compile-time constants for any of the
  things in the NVS schema.
- `Makefile`: `flash-all` no longer touches secrets; add `provision`
  target; add `bootstrap` convenience that runs `flash-all` +
  `provision` for the common new-device case.
- GHA `publish.yml`: stop writing `wifi.env`. The published image is
  device-agnostic and contains no secrets.
- `make flash-all` still works on a brand-new chip; the device just
  sits in "not provisioned" state until `make provision` runs.

### Dev-only shortcut (optional, off by default, never in CI)

Cargo feature `dev-embedded-creds`:

```toml
[features]
default = []
dev-embedded-creds = []
```

When the feature is on, `build.rs` reads `WIFI_SSID`/`WIFI_PASS` from
`wifi.env` and `src/main.rs` writes them into NVS on first boot if the
keys aren't already there (a "self-provision from baked-in defaults"
fallback). Local devs who want to skip provisioning can opt in:

```
$(WIFI_ENV) && cargo build --release --features dev-embedded-creds
```

CI's `publish.yml` and `ci.yml` **never** pass `--features
dev-embedded-creds`. The published image is the strict-NVS path. To
prevent accidents, the GHA workflow can `grep -c
'dev-embedded-creds' Cargo.toml` and fail if it's enabled in
`[features].default`.

## Migration from current setup

Easiest path: `make bootstrap` (flash-all + provision) on the existing
device. Wipes everything, lays down the new partition table + new
firmware + provisioned NVS in one step. Brief downtime; no risk of
brick.

## Concerns and questions

### Concerns

1. **Soft trust separation** (described above). For real
   tamper-resistance, Secure Boot v2 is the answer. Document as
   future work.
2. **NVS partition is unencrypted.** The SA private key sits in flash
   bytes. Anyone with physical access can dump it. Mitigations: Flash
   Encryption (paired with Secure Boot, also eFuse-rooted, also
   irreversible). For a hobby project, accept the risk.
3. **NVS schema migrations.** When we add/rename keys, existing devices
   need to be re-provisioned (or do a migration in-firmware). For now:
   re-provision.
4. **Lose `provisioning.toml` = lose secrets.** Single source of truth
   is great; losing it is bad. Suggest keeping a copy in a password
   manager.
5. **First-time flow is slightly more friction.** Current: edit
   `wifi.env`, `make run`. Future: edit `provisioning.toml`,
   `make flash-all && make provision && make monitor`. Not bad, but a
   second command.

### Settled

- **Strict-inert when not provisioned.** Unprovisioned device sits in
  a serial-only "NOT PROVISIONED" loop until `make provision` runs.
  No surprise boots, no fallback creds.
- **Dev-only embedded-creds shortcut.** Behind a non-default cargo
  feature; CI builds explicitly omit it and produce the strict
  device-agnostic image.
- **Migration**: `make bootstrap` (flash-all + provision). Downtime
  is fine.

### Open

- **Per-device vs shared `provisioning.toml`?** With one device, one
  TOML. If we ever scale, do we want a tool that takes per-device
  values from a separate source (env vars, secrets manager)?

## Future work

- **Secure Boot v2 + Flash Encryption** — hardware-rooted trust and
  confidentiality. Irreversible eFuse burns; significant effort and
  risk; massive security upgrade.
- **NVS encryption** without Secure Boot — limited value (any flasher
  can disable it), but trivial to enable. Probably not worth it
  alone.
- **Provisioning over BLE or Wi-Fi AP** — for at-scale fleet
  provisioning. Standard ESP-IDF wifi-provisioning component handles
  this. Adds firmware size; only worth it if we ever ship more than
  one device.
- **Remote re-provisioning of non-secret keys** (poll interval, log
  severity, etc.) via the OTA channel — push small "config artifacts"
  alongside firmware artifacts in OCI.
- **Rotating the SA key** — currently means re-provisioning every
  device. With remote re-provisioning above, becomes a push button.
