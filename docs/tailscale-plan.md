# Tailscale on the ESP32 — plan

What it would take to put this device on a tailnet, what's actually
been done in the wild, and which path makes sense given the hardware
and the rest of this firmware. Companion to `eink-plan.md` —
forward-looking, not a description of shipped code.

## Why bother

Plausible wins for *this* project:

- **Direct access to a NAT'd device** without exposing it to the
  public internet. SSH-ish console, an HTTP status endpoint, or pulling
  the latest serial log without sitting at the desk with `make monitor`.
- **OTA over tailnet.** The current poll loop hits `ghcr.io`, which
  is fine but means new builds are gated on the GHCR push. A push-mode
  channel from a laptop on the same tailnet ("hey device, fetch this
  digest") becomes possible.
- **Metrics/log siphon without GCP.** Today `cloud_log` and `metrics`
  ship to Cloud Logging / Cloud Monitoring. With a tailnet you can
  scrape directly from the device (Prometheus over tailnet is the
  obvious shape) and skip the GCP pipeline for at-home dev.
- **Conference / desk-LAN portability.** If the e-ink work
  ([`eink-plan.md`](eink-plan.md)) ends up showing personal data
  (calendar, GitHub PRs), routing that traffic over tailnet rather
  than the local Wi-Fi keeps secrets off whatever network the device
  happens to be on.

What it *won't* buy us: Tailscale's exit-node / subnet-router
features. The device isn't routing for anything else.

## What "Tailscale on an MCU" actually means

Tailscale upstream is Go, ~25 MB compiled, ~72 MB RSS. None of that
fits on this board. There is no upstream MCU port and no public
roadmap for one (see
[tailscale/tailscale#18013](https://github.com/tailscale/tailscale/issues/18013) —
open, untriaged). `libtailscale` is a C-ABI wrapper around the Go
`tsnet` package; it's still Go inside, so it's a non-starter here.

That leaves three architectural shapes:

1. **Re-implement the Tailscale wire protocol in C/C++** down to a
   level the ESP32 can run: the `ts2021` Noise IK handshake to the
   control server, map-response parsing, DISCO (PING/PONG/CALL_ME_MAYBE),
   STUN, optionally DERP, and a WireGuard data path wired into lwIP.
   This is what the two real projects below do.
2. **Plain WireGuard, no Tailscale.** The ESP32 is just a WireGuard
   peer; you handle keys, endpoints, and routing manually. Drops the
   Tailscale identity / ACL / coordination story but keeps the encrypted
   tunnel.
3. **Bridge / sidecar.** Run Tailscale on a Linux box (Pi, VPS,
   home server) that the ESP32 reaches over a plain WireGuard tunnel;
   the Linux box advertises the ESP32's IP into the tailnet. The ESP32
   isn't really on the tailnet, but tailnet peers can reach it.

Shape #1 is the only one that makes the ESP32 a real tailnet node.
Shapes #2 and #3 are useful escape hatches when the C port turns out
too big or too brittle for our constraints.

## The implementations worth knowing about

### MicroLink — `CamM2325/microlink`

The most complete option. ESP-IDF native component, MIT-licensed,
written in C/C++ on top of `wireguard-lwip` and ESP-IDF's bundled
mbedTLS. Implements:

- `ts2021` Noise IK to the control server with key-expiry handling
- WireGuard data plane (ChaCha20-Poly1305)
- DISCO (PING / PONG / CALL_ME_MAYBE) for direct-path discovery
- DERP relay with dynamic region selection (up to 32 regions)
- STUN over IPv4 + IPv6 for public-endpoint discovery
- MagicDNS hostname resolution
- 1–64 active WireGuard tunnels, with up to 1024 peers cached in NVS
  via LRU. Tested with 300+ peer tailnets.
- Works with the official Tailscale coordination server *and* with
  Headscale / Ionscale.

**Footprint claim**: 85–116 KB static SRAM depending on enabled
features (DERP/DISCO/STUN are Kconfig-toggleable). Recommends an
ESP32-S3 with PSRAM in production; on plain WROOM-32 the
MapResponse parser would have to make do without a 1 MB scratch
buffer, and big tailnets won't fit.

**Integration shape**: not a transparent lwIP netif. Provides its own
`microlink_tcp_*` / `microlink_udp_*` send/recv API. Existing
`esp-idf-svc::http::client` code does *not* automatically tunnel
through Tailscale — code that wants tailnet has to opt in.

**Auth**: pre-auth keys only (no OAuth device-code flow). Stored in
`sdkconfig.credentials`. No documented Rust binding.

This is the one to actually build on if we go ahead.

### tailscale-iot — `alfs/tailscale-iot`

The repo we were pointed at. A self-described "Frankenstein
proof-of-concept", largely LLM-generated, ESPHome-based, ESP32-C3
focused. Useful as a worked example more than a dependency.

What it has that MicroLink doesn't:

- **It's a real lwIP netif.** It registers a point-to-point netif
  (MTU 1280, route metric 50 vs. 100 for Wi-Fi) and inserts itself
  into `netif_list`. A `bind(INADDR_ANY)` socket on the device is
  reachable on both 192.168.x and 100.x.x. This is the integration
  shape we'd want long-term — it means existing socket code "just
  works" on the tailnet.
- More verbose internal docs (`LWIP.md`, `MULTI_PEER_ARCHITECTURE.md`)
  walking through the packet path, lwIP gotchas, and key/index maps.

What it gives up vs. MicroLink:

- **Headscale-only.** Doesn't talk to the official Tailscale
  coordination server.
- **DERP disabled** to save memory — direct UDP only. UDP-blocked
  networks won't connect.
- **IPv6 endpoints ignored.**
- **Static 5-peer cap.** Roughly 50 KB of session state for those 5
  peers, ~98 KB total resident, on an ESP32-C3 with 320 KB SRAM.
- ESPHome-based build, vendored `noise-c` submodule.
- "PoC quality" by the author's own admission.

Worth reading; probably not worth depending on.

### esp32-tailbridge — `pierrejay/esp32-tailbridge`

The shape-#3 option. ESP32 runs plain WireGuard; a Linux proxy runs
Tailscale and either advertises the device's WireGuard subnet into
the tailnet, or runs a separate Tailscale instance per device in
network namespaces (the README admits the latter "is definitely not
production-ready"). Requires an always-on Linux box. Dodges all the
ts2021 / DISCO / DERP work entirely.

### `WireGuard-ESP32-Arduino` (ciniml) and `esp_wireguard` (trombik)

Plain WireGuard libraries for ESP32 — single-peer, lwIP-integrated,
BSD-licensed. No Tailscale layer. These are the building blocks
underneath both MicroLink and the bridge approach. `esp_wireguard`
specifically targets ESP-IDF and is the closer fit for our build
system; it's marked "alpha" and tested only with single peers.

### Things that don't apply here

- **`tailscale/libtailscale`** — Go-backed C ABI, won't fit.
- **"Small Tailscale" / OpenWrt builds** — UPX-compressed Go, still
  multi-MB, still assumes a Linux userland.
- **Cloudflare `boringtun`** — Rust userspace WireGuard, not ported
  to ESP32, no Tailscale layer.

## Hardware reality check for *this* board

Inland ESP-WROOM-32, 4 MB flash, ~520 KB SRAM, **no PSRAM**. After
ESP-IDF + Wi-Fi + lwIP we have ~150–200 KB free heap; the existing
firmware already squeezes mbedTLS hard
([`docs/observability.md`](observability.md)) so three concurrent
TLS sessions fit. Adding Tailscale on top means:

- **Another long-lived TLS session** to the control server (`ts2021`
  uses Noise inside an outer TLS connection to e.g.
  `controlplane.tailscale.com`). That's a fourth speaker on the
  `ShortHttpsLock`, or a separate gating mechanism if Tailscale's
  control session has to stay open.
- **A second WireGuard / lwIP path.** lwIP can host multiple netifs
  fine, but the tx/rx buffer pool becomes shared between Wi-Fi and
  the tunnel netif. MicroLink trims the tunnel TX pool to 4 entries
  (~6 KB) on ESP32-C3 specifically to fit; we'd inherit that.
- **No PSRAM means no 1 MB MapResponse scratch buffer.** Big
  tailnets (the kind MicroLink advertises support for) won't fit. A
  small home tailnet (a few peers) is realistic; a 300-peer fleet is
  not.
- **App slot pressure.** Both OTA slots are 1.94 MB. Current image
  is comfortably under that, but a 100–200 KB Tailscale stack plus
  whatever crypto it pulls in (mbedTLS already there, plus
  ChaCha20-Poly1305 if not already enabled) eats into headroom for
  the e-ink work and anything else.

Verdict: feasible, not generous. The `ESP32-S3 + PSRAM` story
MicroLink prefers is the comfortable one; on this board Tailscale
will sit alongside the existing OTA + cloud-log + metrics threads
with very little room to spare.

## Path to actually shipping

A reasonable phasing that doesn't try to do everything at once:

1. **Pin the dependency choice.** Stand up MicroLink in a throwaway
   ESP-IDF-only project (no Rust, no OTA, no cloud-log) on a spare
   ESP32-WROOM-32 with a fresh tailnet. Confirm it connects, gets a
   100.x address, holds it through a Wi-Fi reconnect. If it can't
   make it without PSRAM, fall back to plain WireGuard
   (`esp_wireguard`) plus shape-#3 bridging — accept that the device
   isn't a real tailnet node.
2. **Vendor it as an ESP-IDF managed component.** embuild's CMake
   project takes IDF components from a few well-known dirs; either
   submodule MicroLink under `components/` or pull it via the IDF
   component registry. Generated Rust bindings via `bindgen` for the
   handful of functions we actually call.
3. **Expose the smallest useful surface.** Don't try to make every
   socket call go over tailnet. Start with one daemon thread that
   binds a TCP listener on the tailnet IP and serves a status JSON.
   That alone proves end-to-end and is enough to build on.
4. **Decide how to provision the auth key.** Same shape as
   `[wifi]` and `[gcp]` blocks in `provisioning.toml`: a `[tailscale]`
   block with a reusable preauth key (or device-level OAuth client
   creds, once MicroLink supports it — currently it doesn't). NVS
   key names cap at 15 chars (see `CLAUDE.md`); something like
   `ts_auth_key`, `ts_login_url`, `ts_hostname` works.
5. **Wire OTA / cloud-log over tailnet.** Optional, second-phase.
   Once a tailnet listener works, flip the OTA poll endpoint from
   GHCR to a private registry on the tailnet. cloud-log either gets a
   tailnet sink or stays on GCP — they're not mutually exclusive.
6. **Watch the heap.** The same `metrics` thread that already ships
   `free_heap` / `min_free_heap` to Cloud Monitoring is the obvious
   canary. Add a fixed regression dashboard before flipping
   Tailscale on; back it out at the first sign of fragmentation
   regressions.

## Design questions to nail down before coding

- **Trust model for the preauth key.** It's a long-lived secret that
  joins the device to the tailnet. Today our trust story (cosign
  Sigstore identities in NVS) is about the *firmware* the device
  will run. A leaked preauth key is a different blast radius —
  someone else's device pretending to be us on the tailnet. ACL tags
  on the Tailscale side scope what such a stolen key can do; we
  should provision the key with a tag and enforce in tailnet ACLs.
- **Does Tailscale replace or augment the GCP sender threads?** If
  augment, we hold one more TLS session and need to extend
  `ShortHttpsLock` to cover it. If replace, the GCP code can come
  out — but that loses durability when the tailnet is unreachable.
  Probably augment, with a way to disable each independently per
  device.
- **What identity does the device claim?** Hostname goes into
  MagicDNS. Stable across re-flashes (derive from chip MAC, like
  `device_mac()` in `gcp_auth.rs`) or human-assigned via
  `provisioning.toml`?
- **Headscale, Tailscale, or both?** MicroLink supports either.
  Picking the official Tailscale control plane gets us free DERP
  relays and zero ops; picking Headscale keeps everything on
  hardware we own. The firmware shouldn't care; the choice is per
  device via the login URL in NVS.

## Resources

- alfs PoC: <https://github.com/alfs/tailscale-iot>
  (`README.md`, `LWIP.md`, `MULTI_PEER_ARCHITECTURE.md`)
- MicroLink: <https://github.com/CamM2325/microlink>
- esp_wireguard (plain WireGuard, ESP-IDF): <https://github.com/trombik/esp_wireguard>
- WireGuard-ESP32-Arduino: <https://github.com/ciniml/WireGuard-ESP32-Arduino>
- esp32-tailbridge (sidecar shape): <https://github.com/pierrejay/esp32-tailbridge>
- Tailscale upstream feature request for thin-edge clients:
  <https://github.com/tailscale/tailscale/issues/18013>
- ts2021 / Noise registration background:
  <https://deepwiki.com/juanfont/headscale/2.3-authentication>
