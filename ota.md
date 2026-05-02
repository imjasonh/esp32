# OTA updates over GHCR with cosign verification

This document describes how OTA updates work end-to-end on this project:
how firmware is built and signed in CI, how it's distributed via an OCI
registry, how the device discovers and applies updates, and how the
device protects against bad updates and unauthorized signers.

## Architecture

```
                                                  GitHub
                                                Container
                                                Registry
                                                  (GHCR)
   developer push                                    │
   ──────────────▶  ┌──────────────────┐ push       ▼            poll
                    │  GHA workflow    │ ─────▶ ┌────────┐ ◀────────────
                    │  publish.yml     │ sign   │  OCI   │   pull bundle
                    │   • cargo build  │ ─────▶ │artifact│   verify sig
                    │   • espflash     │        │ + sig  │   stream blob
                    │   • OCI push     │        └────────┘   write OTA
                    │   • cosign sign  │                     reboot
                    │     (keyless,    │                       │
                    │      OIDC)       │                       ▼
                    └──────────────────┘                   ┌────────┐
                                                           │ ESP32  │
                                                           └────────┘
```

Manual `make publish` from a developer machine works the same way —
just cosigns with a different OIDC identity (a developer's email instead
of the workflow's URI).

## Components

| Component | Lives at | Role |
|---|---|---|
| **Publisher** | `tools/publisher/` (Rust, host build) | Wraps the firmware `.bin` as an OCI artifact and pushes both `:latest` and `:sha-<short>` tags to GHCR. Prints the manifest digest on stdout for the next step. |
| **Cosign** | system binary, version 3 | Signs the just-pushed digest using keyless OIDC. Stores the signature as a sibling OCI artifact (Sigstore Bundle v0.3 format). |
| **GHA workflow** | `.github/workflows/publish.yml` | Runs `make publish` on every push to main. Uses ambient OIDC token for cosign — no secrets needed for signing. |
| **CI workflow** | `.github/workflows/ci.yml` | Runs on PRs. Builds firmware + publisher to verify they compile. No publishing or signing. |
| **OTA loop** | `src/ota.rs` | Background pthread, polls GHCR every 60 s with backoff + jitter, fetches manifest, compares to last applied. |
| **Sig verifier** | `src/sig.rs` | Parses Sigstore Bundle v0.3, verifies cert chain + DSSE signature + in-toto subject digest. |
| **Trust config** | `src/trust.rs`, `trust/*.pem` | Compile-time allowlist of `(identity, issuer)` tuples + bundled Sigstore Fulcio root + intermediate CAs. Updates require source edit + USB reflash. |

## Partition layout (4 MB flash)

```
0x001000  bootloader            (~28 KB)
0x008000  partition table         4 KB
0x009000  nvs                    24 KB    last_digest, pending_digest, runtime config
0x00f000  otadata                 8 KB    which slot to boot
0x011000  phy_init                4 KB
0x020000  ota_0                 1.75 MB   app slot 0
0x1e0000  ota_1                 1.75 MB   app slot 1
0x3a0000  unused                384 KB
```

The two app slots ping-pong: a new image always writes to the *inactive*
slot, then `otadata` is updated to point at it on next boot.

`partitions.csv` is the source of truth. The Makefile substitutes the
absolute project path into `sdkconfig.defaults.in` because IDF resolves
`CONFIG_PARTITION_TABLE_CUSTOM_FILENAME` relative to embuild's synthetic
project under `target/`, not the repo root.

## Update mechanism

### Polling loop (firmware, `src/ota.rs::run`)

1. Sleep `poll_interval ± 10%` jitter (default 60 s, NVS-configurable
   via `poll_secs`).
2. Hit GHCR's anonymous token endpoint with scope
   `repository:<repo>:pull`, get a Bearer token.
3. `GET /v2/<repo>/manifests/<tag>` with that token. Compute SHA-256
   of the response body — that's the manifest digest cosign signed.
4. Parse manifest. Check that `layers[0].mediaType` is
   `application/vnd.esp32.firmware.bin`. Compare layer digest to
   `last_digest` from NVS.
   - Match → `NoChange`, return to (1).
   - Different → continue.
5. **Verify the signature** (see "Device verification" below). On
   failure → log, increment `consecutive_failures`, return to (1)
   with exponential backoff (`60 s × 2^failures`, capped at 1 h).
6. `GET /v2/<repo>/blobs/<layer-digest>`. Stream the body chunk by
   chunk into the inactive OTA partition via `EspOta::write()`,
   updating an SHA-256 hasher in parallel.
7. After last byte: confirm `(actual size, actual SHA)` matches the
   manifest descriptor. On any mismatch → `EspOta::abort()`, fail
   loudly. On match → `EspOta::complete()` (sets boot partition).
8. Persist `pending_digest = layer.digest` to NVS.
9. `esp_restart()`.

### Post-reboot validation (firmware, `src/main.rs`)

1. Early in `main()`, call `is_pending_verify()` →
   reads `esp_ota_get_state_partition()`.
2. If `ESP_OTA_IMG_PENDING_VERIFY`, run the bringup checks:
   Wi-Fi connect, then HTTPS GETs to ipify and wttr.
3. On bringup success, call
   `esp_ota_mark_app_valid_cancel_rollback()` and promote
   `pending_digest → last_digest` in NVS.
4. On failure, `esp_restart()` — the bootloader rolls back to the
   previous slot on the next boot because the new image was never
   marked valid.
5. Spawn the OTA polling thread with a 48 KB stack (HTTPS + JSON +
   SHA + cert parsing + ECDSA needs the headroom).

## Anti-bricking + USB recovery

Two layers of protection:

### Layer 1 — bootloader rollback (always armed in firmware)

`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y` in `sdkconfig.defaults.in`
turns on ESP-IDF's pending-verify state. After an OTA reboot, the new
app runs in `PENDING_VERIFY`. If `mark_app_valid` isn't called before
the next reboot, the bootloader reverts. We gate `mark_app_valid` on
networking actually working — so an OTA that breaks Wi-Fi or DNS or
TLS auto-rolls back.

### Layer 2 — USB recovery (when both slots are bad)

If both OTA slots end up unbootable, or the bootloader / partition
table itself is corrupted, recover over USB:

1. Put the chip in serial download mode. The Inland board has
   auto-reset; espflash usually drives DTR/RTS automatically. If
   not: hold **BOOT** (IO0), tap **EN**, release BOOT.
2. From the project root:
   ```
   make flash-all
   ```
   This wipes flash and writes bootloader + partition table + a
   fresh app the same way the very first flash did.

## CI signing (publisher → GHCR)

### Publisher tool

`tools/publisher/` is a separate cargo project (its own
`rust-toolchain.toml` pinning `stable`, its own `.cargo/config.toml`
clearing the parent's xtensa target). Built for the host.

CLI:

```
publisher push --bin <fw.bin> --repo ghcr.io/<owner>/<name> --git-sha <short>
```

Produces an OCI manifest with:

- **Config**: `application/vnd.esp32.firmware.v1+json` — small JSON
  with `target_chip`, `idf_version`, `git_sha`, `built_at`,
  `bin_size`, `bin_sha256`.
- **Layer**: `application/vnd.esp32.firmware.bin` — the raw firmware
  bytes (no compression).
- **Annotations**: `org.opencontainers.image.source` so GHCR auto-links
  the package to the source repo on first push;
  `org.opencontainers.image.revision`, `created`.

Pushes the same artifact under both `:latest` and `:sha-<short>`. Both
tags resolve to the same content-addressed digest, so one cosign sign
covers both.

The publisher prints `digest: sha256:...` on stdout. The Makefile
captures it and passes it to `cosign sign` as `<repo>@<digest>` rather
than a tag — eliminates the race window between publisher push and
cosign sign.

### Cosign signing

`make publish` runs `cosign sign --yes <repo>@<digest>` after the
publisher push. Cosign:

1. Authenticates to its OIDC issuer (Sigstore's broker for interactive
   browser flow; or the workflow's ambient ID token in GHA).
2. Fetches a short-lived (10 min) X.509 cert from Fulcio with the
   OIDC identity baked in as a SAN.
3. Hashes the manifest payload, signs it with the cert's private key.
4. Bundles cert + signature + (optional) Rekor entry into a Sigstore
   Bundle v0.3 (Protobuf-as-JSON).
5. Pushes the bundle as a sibling OCI artifact at tag
   `sha256-<hex>` (no `.sig` suffix) using the OCI 1.1 referrers
   layout (image index → inner manifest → bundle blob).

### Identities used

| Source | OIDC issuer | SAN form |
|---|---|---|
| Manual `make publish` | `https://accounts.google.com` | `rfc822Name` (developer's email) |
| GHA workflow on push to main | `https://token.actions.githubusercontent.com` | `URI` (`https://github.com/<owner>/<repo>/.github/workflows/publish.yml@refs/heads/main`) |

Both must be present in `TRUSTED_IDENTITIES` for the device to accept
their signatures. Updating the allowlist requires editing source +
USB reflash; OTA cannot grant new signing identities.

### GHA workflow specifics

- Trigger: `push` to `main`, plus `workflow_dispatch` for manual
  reruns.
- Concurrency: `cancel-in-progress: true` with a single group, so a
  rapid sequence of pushes only publishes the latest commit.
- Permissions: `packages: write` (push to GHCR), `id-token: write`
  (cosign keyless OIDC), `contents: read`.
- Uses `astral-sh/setup-uv@v7` to provide Python 3.12 (the Makefile's
  `ensure-python-shim` symlinks `python3` to it),
  `esp-rs/xtensa-toolchain@v1.7` for the Xtensa Rust toolchain,
  `sigstore/cosign-installer@v4.1.1` for cosign 3.
- Wi-Fi creds (`WIFI_SSID`, `WIFI_PASS`) come from repo secrets and
  get written to `wifi.env` at workflow time. They're embedded into
  the firmware at compile time via `env!()` in `src/main.rs`.

## Device verification (`src/sig.rs`)

For each candidate update, before downloading the firmware blob:

1. **Fetch the bundle.** Walk the OCI 1.1 referrers layout: image
   index at tag `sha256-<manifest-digest>` → inner manifest →
   `application/vnd.dev.sigstore.bundle.v0.3+json` layer blob.
2. **Parse the leaf cert.** Decode `verificationMaterial.certificate.
   rawBytes` from base64, parse as DER X.509 with the `x509-cert`
   crate.
3. **Identity check.** Read SAN extension. Accept either
   `Rfc822Name` (email) or `UniformResourceIdentifier` (workflow
   URI). Read OIDC issuer from extension OID
   `1.3.6.1.4.1.57264.1.1`. Reject if `(identity, issuer)` isn't in
   `TRUSTED_IDENTITIES`.
4. **Cert chain.** Verify leaf was signed by bundled Sigstore
   intermediate (P-384 ECDSA-SHA384). Verify intermediate was signed
   by bundled Sigstore root (also P-384 ECDSA-SHA384). Both intermediate
   and root PEMs are `include_str!`'d at compile time.
5. **DSSE signature.** Decode `dsseEnvelope.payload` (base64) and
   `signatures[0].sig` (base64). Compute the DSSE PAE
   (`"DSSEv1 <len> <payloadType> <len> <payload>"`). Verify the
   ECDSA-P256 signature using the leaf cert's public key.
6. **In-toto binding.** Parse the DSSE payload as an in-toto
   Statement. Confirm `subject[0].digest.sha256` exactly matches the
   manifest digest we're about to install. This is the cryptographic
   binding from "what the signer attested" to "what we're about to
   apply".

If any step fails, the verifier returns an error. The OTA loop logs
it (with anyhow chain), bumps `consecutive_failures` (driving
backoff), and never touches the OTA partition.

## Future work

- **Rekor SET verification.** The Sigstore Bundle includes
  `tlogEntries[]` with Rekor's Signed Entry Timestamp. Verifying it
  on-device would give us (a) detection of a Fulcio key compromise
  that doesn't appear in the public log and (b) a trusted timestamp
  to enforce the cert validity window (Fulcio certs are 10 min, so
  without trusted time we currently skip the validity check). Bundle
  Rekor's public key, parse the SET, verify the inclusion proof.
- **GitHub Actions OIDC trust scoping by `job_workflow_ref`.**
  Currently the GHA identity in `TRUSTED_IDENTITIES` pins to
  `publish.yml@refs/heads/main`. We could also enforce
  `1.3.6.1.4.1.57264.1.x` Fulcio extensions like `Run Invocation
  URI` for stricter trust.
- **Force-update GPIO button.** Wire a button to a GPIO; on short
  press, skip the poll wait and trigger an immediate poll.
- **NVS provisioning mechanism.** `OtaConfig::load_from_nvs` already
  reads `repo`/`tag`/`poll_secs` from NVS, but we have no way to
  write them from outside the firmware. Could add a tiny HTTP
  endpoint on the device, a serial-console command, or push config
  changes as a separate OCI artifact channel.
- **`If-None-Match` on manifest fetches.** Marginal bandwidth savings
  given the manifest is ~500 bytes and we already digest-compare
  locally. Worth it only if poll interval drops well below 60 s.
