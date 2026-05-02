# Cloud Logging — plan (issue #2)

Ship structured logs from the ESP32 to Google Cloud Logging so we can
monitor + diagnose remotely without a serial cable. No prebuilt
GCP/auth libraries on-device — talk directly to the REST APIs.

GCP credentials follow the same model the rest of the project now uses:
**NVS-provisioned via USB**, not embedded into the OTA-distributed
firmware. The published OCI image stays device-agnostic.

## Architecture

```
┌─────────── ESP32 ───────────┐                ┌─────────── GCP ───────────┐
│                              │                │                            │
│  tracing event ──┐           │                │                            │
│  tracing event ──┼─▶ buffer  │  POST logs:    │   logging.googleapis.com   │
│  tracing event ──┘   (ring)  │ ──────────────▶│   /v2/entries:write        │
│                       │      │  Bearer <tok>  │                            │
│                       ▼      │                │                            │
│                  background  │  POST oauth2:  │   oauth2.googleapis.com    │
│                  sender task │  ──────signed─▶│   /token (grant_type=      │
│                       │      │  JWT (RS256)   │    jwt-bearer)             │
│                       ▼      │ ◀──────────────│                            │
│                  HTTPS POST  │  access_token  │                            │
│                              │                │                            │
└──────────────────────────────┘                └────────────────────────────┘
```

A new tracing **layer** captures events into a bounded ring buffer in
RAM — both `src/main.rs` (wifi connect, HTTPS GETs, app behavior) and
`src/ota.rs` / `src/sig.rs` (OTA polling, verification, apply). All
events flow through tracing already, so a single layer gets both
"app" and "OTA" logs for free.

A background pthread drains the buffer, batches entries, and POSTs
them to Cloud Logging using a cached OAuth2 access token. Tokens are
obtained via the standard service-account JWT bearer flow and cached
for their ~1-hour lifetime so we only do RSA signing once per refresh.

## Auth flow (no library)

Service-account JWT → access-token → API call. Standard Google flow.

1. **JWT.** Header `{"alg":"RS256","typ":"JWT","kid":"<key id>"}`,
   claims `{"iss":"<sa-email>","scope":"https://www.googleapis.com/auth/logging.write","aud":"https://oauth2.googleapis.com/token","iat":<now>,"exp":<now+3600>}`.
   Base64url-encode each, concatenate with `.`, sign with the SA's
   RSA private key (RS256 = RSA-PKCS#1 v1.5 over SHA-256), append
   `.<sig>`.
2. **Token exchange.** POST to `https://oauth2.googleapis.com/token`
   with form body
   `grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=<jwt>`.
   Response: `{"access_token":"...","expires_in":3599,"token_type":"Bearer"}`.
3. **Cache.** Hold the access token in RAM with its expiry. Re-mint
   only when expired (or close to). RSA signing is the expensive step
   we want to avoid doing per-log.

## Logging API call

`POST https://logging.googleapis.com/v2/entries:write` with
`Authorization: Bearer <access_token>` and JSON body:

```json
{
  "logName": "projects/<project-id>/logs/esp32-firmware",
  "resource": {
    "type": "generic_node",
    "labels": {
      "project_id": "<project-id>",
      "location": "global",
      "namespace": "esp32",
      "node_id": "<chip-mac>"
    }
  },
  "entries": [
    {
      "severity": "INFO",
      "timestamp": "2026-05-02T19:45:00Z",
      "jsonPayload": {
        "message": "ota: boot summary",
        "module": "esp32_blinky::ota",
        "fw": "979c862",
        "repo": "ghcr.io/imjasonh/esp32",
        "tag": "latest",
        "poll_secs": 60,
        "last_digest": "sha256:..."
      }
    },
    ...
  ]
}
```

Single `logName` for both app + OTA logs; the tracing target's
`module` (e.g. `esp32_blinky::ota`, `esp32_blinky::sig`,
`esp32_blinky` for main) is folded into `jsonPayload` so Cloud
Logging filters can split by component (`jsonPayload.module =
"esp32_blinky::ota"`).

Resource labels carry device identity (MAC). `jsonPayload` carries
every tracing structured field as a top-level JSON key.

## Provisioning (NVS)

Add to the `gcp` NVS namespace, written by `tools/provision/`:

```
ns      key             type   notes
─────────────────────────────────────────────────────────────────────
gcp     project_id      str    GCP project (the logs land in)
gcp     sa_email        str    service-account email
gcp     sa_key_id       str    private-key id (for JWT `kid` header)
gcp     sa_key_pem      blob   RSA private key, PKCS#8 PEM (~1.7 KB)
gcp     min_severity    u8     0=DEBUG..4=ERROR; default 1=INFO
```

Updates to `provisioning.toml`:

```toml
[gcp]
project_id = "..."
sa_email = "esp32-logger@my-project.iam.gserviceaccount.com"
sa_key_id = "abc123..."          # from the SA JSON key file
sa_key_pem = "gcp-sa-key.pem"    # path; tool reads + embeds the bytes
min_severity = "info"            # debug / info / warn / error
```

If the entire `[gcp]` section is missing from `provisioning.toml`,
cloud logging is **disabled** — the firmware boots normally and just
emits to serial. Cloud logging is opt-in per device, like everything
else in NVS.

## On-device implementation

### Tracing capture (`src/cloud_log.rs`, new)

A `tracing_subscriber::Layer` that intercepts events. For each event:
- Capture severity (`INFO`/`WARN`/`ERROR`/`DEBUG`/`TRACE` mapped from tracing levels).
- Capture all structured fields into a `serde_json::Map` (string repr
  for non-string types).
- Capture the tracing target (module path).
- Take a wall-clock timestamp (requires NTP — see below).
- Push onto a bounded queue. If full, drop oldest and bump a
  dropped-counter; we log the drop count in the next batch so it
  shows up in Cloud Logging.

This layer composes with the existing log bridge (tracing →
EspLogger → serial) — events go to both serial and the cloud queue.

### Sender task

Background pthread, ~32 KB stack:

1. Wait for Wi-Fi up.
2. Wait for NTP sync (or proceed without timestamps).
3. Loop:
   - Sleep `flush_interval` (default 5s).
   - Drain queue (up to N entries or M bytes per batch).
   - If empty, continue.
   - Ensure access token is valid; refresh via JWT exchange if needed.
   - POST batch to `logging.googleapis.com`. On 4xx → log to serial,
     drop the batch. On 5xx / network error → keep batch, retry with
     backoff (similar to the OTA loop).

### Time sync (NTP)

Cloud Logging entries need real timestamps; without NTP our system
clock starts at 1970. Add an SNTP sync at boot via
`esp-idf-svc::sntp::EspSntp` (synchronous) before the sender task
starts. If sync fails, sender task falls back to omitting `timestamp`
so Cloud Logging assigns server-side time (less accurate but valid).

NTP sync is done by `main.rs` after Wi-Fi connect; it's a precondition
for both cloud logging *and* (eventually) Rekor SET timestamp validity
(see ota.md Future Work).

### Crates and footprint

- `rsa` — RSA-PKCS#1 v1.5 signing for JWT. ~50 KB.
- `base64` — already have.
- `sha2` — already have (used by RSA signing too).
- `serde_json` — already have.
- `time` — for RFC 3339 timestamp formatting + UNIX time. ~40 KB.
- Logging code itself: ~10 KB.

Total budget estimate: ~100 KB additional firmware. We have
plenty of headroom in the 1.94 MB OTA slot.

RAM: ring buffer (default ~4 KB), token cache (~2 KB), JWT scratch
(~1 KB) → ~10 KB total during steady state.

## Concerns

1. **Log volume + cost.** If we ship every tracing event we could
   easily hit Cloud Logging quotas or bill. Default to filtering at
   `INFO` for cloud, leave `DEBUG` for serial only. The threshold
   is NVS-configurable via `gcp/min_severity`.

2. **Wi-Fi outages / device offline.** Sender task buffers with a
   bounded queue, drops oldest when full, reports drop count in next
   batch. Long offline period = lossy. Acceptable for a monitoring
   channel; not a record of truth.

3. **NTP dependency.** We don't currently sync time, so adding SNTP
   is part of this work. SNTP fail → use server-side timestamps.

4. **NVS partition is unencrypted.** The SA private key sits in flash
   bytes. Anyone with physical access to the chip can dump it. Same
   threat model as the wifi password and the trust roots. Mitigation
   (real fix) is Flash Encryption + Secure Boot v2; deferred. For
   now we accept the risk and scope the SA tightly: role
   `roles/logging.logWriter` only, on a dedicated logs-only project.

5. **RSA-2048 signing on ESP32 is slow** — ~hundreds of ms per JWT.
   Cached for 1h, so amortized cost is negligible. But the first-time
   sign blocks the sender task for noticeable time. Acceptable.

## Settled

- **Provisioning**: NVS via USB (matches wifi + trust). The SA key
  never enters the OTA-distributed firmware image.
- **Severity threshold default**: `INFO`+ for cloud, configurable via
  `gcp/min_severity` in NVS.
- **Single log name** with `module` field for component filtering,
  rather than per-component `logName`s.
- **Pure-Rust `rsa`** crate, not mbedtls FFI. Headroom is plenty
  for the ~50 KB.
- **Opt-in per device**: missing `[gcp]` block → cloud logging
  disabled, serial-only. No GCP setup required to provision a device.

## Future work (not in v1)

- **Trace export** (full structured spans, not just events) — would
  let us see request flows and timing.
- **Metrics export** to Cloud Monitoring — counters for OTA verifies,
  poll cycles, Wi-Fi reconnects.
- **Alerting** based on log-based metrics (e.g., page on >5 OTA
  verify failures in 1h).
- **OIDC instead of SA key** — would eliminate the key-leak concern
  (paired with Secure Boot v2), but ESP32 doesn't have a way to get
  a usable OIDC token from anywhere right now.
