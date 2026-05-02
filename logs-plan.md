# Cloud Logging — plan (issue #2)

Ship structured logs from the ESP32 to Google Cloud Logging so we can
monitor + diagnose remotely without a serial cable. No prebuilt
GCP/auth libraries on-device — talk directly to the REST APIs.

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
RAM. A background pthread drains the buffer, batches entries, and
POSTs them to Cloud Logging using a cached OAuth2 access token.
Tokens are obtained via the standard service-account JWT bearer flow
and cached for their ~1-hour lifetime so we only do RSA signing once
per refresh.

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

Entries are batched per POST. Resource labels carry device identity
(MAC). `jsonPayload` carries every tracing event field as a top-level
JSON key — preserves the structured fields tracing already emits.

## On-device implementation

### Tracing capture (`src/cloud_log.rs`, new)

A `tracing_subscriber::Layer` that intercepts events. For each event:
- Capture severity (`INFO`/`WARN`/`ERROR`/`DEBUG`/`TRACE` mapped from tracing levels).
- Capture all structured fields into a `serde_json::Map` (or write
  directly to a String at capture time to avoid a serde dep at this
  layer).
- Take a wall-clock timestamp (requires NTP — see below).
- Push onto a bounded `crossbeam` channel or `heapless::spsc` queue.
  If full, drop oldest (or newest) and bump a dropped-counter; we
  log the drop count in the next batch so it shows up in Cloud
  Logging.

This layer composes with the existing log bridge (tracing →
EspLogger → serial) — events go to both serial and the cloud queue.

### Sender task

Background pthread, ~32 KB stack:

1. Wait for Wi-Fi up.
2. Loop:
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

### Crates and footprint

- `rsa` (or mbedtls FFI) — RSA-PKCS#1 v1.5 signing for JWT. ~50 KB.
- `base64` — already have.
- `sha2` — already have (used by RSA signing too).
- `serde_json` — already have.
- `time` — for RFC 3339 timestamp formatting. ~40 KB.
- Logging code itself: ~10 KB.

Total budget estimate: ~100 KB additional firmware. We have 463 KB
free per OTA slot, so plenty of headroom.

RAM: ring buffer (default ~4 KB), token cache (~2 KB), JWT scratch
(~1 KB) → ~10 KB total during steady state.

## Compile-time configuration

Following the existing `wifi.env` pattern, a new `gcp.env` (gitignored)
that the Makefile sources:

```
export GCP_PROJECT_ID="my-logs-project"
export GCP_SA_EMAIL="esp32-logger@my-logs-project.iam.gserviceaccount.com"
export GCP_SA_KEY_ID="abc..."
export GCP_SA_PRIVATE_KEY_PEM_PATH=./gcp-sa-key.pem
```

`build.rs` reads the PEM file and embeds the bytes into the firmware
via `env!()`/`include_bytes!()`. CI gets the same values from GHA
secrets and writes `gcp.env` at workflow time.

## Concerns and questions for review

### Concerns

1. **Firmware is public on GHCR — anyone can pull it and extract the
   SA private key.** This is the same threat model we already accept
   for `WIFI_PASS`. Mitigations:
   - **Scope the SA tightly.** Role: only `roles/logging.logWriter`.
     Project: a dedicated logs-only GCP project with no other
     resources. Worst case if leaked: someone fills our log buckets,
     bounded by quota and our willingness to ignore them.
   - **Alternative**: keep SA creds out of firmware; provision via
     NVS over USB on first boot (parallels what we already deferred
     for OTA repo/tag). More work, eliminates the leak.

2. **RSA-2048 signing on ESP32 is slow** — ~hundreds of ms per JWT.
   Cached for 1h, so amortized cost is negligible. But the first-time
   sign blocks the sender task for noticeable time. Acceptable.

3. **Log volume + cost.** If we ship every tracing event we could
   easily hit Cloud Logging quotas or bill. Default to filtering at
   `INFO` or higher for cloud, leave `DEBUG` for serial only. Make
   the threshold an NVS-configurable knob (similar to `poll_secs`).

4. **Wi-Fi outages / device offline.** Sender task buffers with a
   bounded queue, drops oldest when full, reports drop count in next
   batch. Long offline period = lossy. Acceptable for a monitoring
   channel; not a record of truth.

5. **NTP dependency.** We don't currently sync time, so adding SNTP
   is part of this work. SNTP fail → use server-side timestamps.

### Questions

- **GCP project**: do you have one already, or should this plan
  include creating a dedicated logs project?
- **Severity threshold default**: cloud-ship `INFO`+ only?
  `WARN`+? Configurable via NVS?
- **Provisioning model**: bake the SA key in firmware (matches
  current `wifi.env` pattern, accepts the public-firmware leak), or
  do NVS-via-USB (more work, no leak, but requires touching a dev
  workflow we haven't built)?
- **Pure-Rust `rsa` crate or mbedtls FFI?** Same trade-off we
  discussed for cosign verification: pure-Rust is +50 KB, mbedtls
  FFI saves the size at the cost of some unsafe code. Probably go
  pure-Rust for first cut given current 463 KB headroom.
- **Batch size + flush interval?** I proposed 5s / batch, no firm
  upper bound on entry count. Tuneable via NVS later.
- **Log name schema**: `projects/<id>/logs/esp32-firmware` per device,
  or per chip MAC, or per app component (`esp32-firmware/ota`,
  `esp32-firmware/wifi`)?

## Future work (not in v1)

- **Trace export** (full structured spans, not just events) — would
  let us see request flows and timing.
- **Metrics export** to Cloud Monitoring — counters for OTA verifies,
  poll cycles, Wi-Fi reconnects.
- **Alerting** based on log-based metrics (e.g., page on >5 OTA
  verify failures in 1h).
- **OIDC instead of SA key** — would eliminate the key-leak concern,
  but ESP32 doesn't have a way to get a usable OIDC token from
  anywhere right now.
