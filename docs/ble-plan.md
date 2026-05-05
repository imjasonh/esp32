# BLE — plan

Goal: the device advertises itself as a connectable BLE peripheral with a
recognizable name, and a phone (or laptop) can connect to it and read a
handful of read-only "what is this thing" attributes — firmware version,
uptime, free heap, IP address, RSSI, OTA state. No phone-side build for
the first cut: we use a standard BLE explorer app to view the GATT.

## Goals

- The device advertises a recognizable name and one or more GATT
  services after boot.
- An **iOS** user can connect from a browser and see live status. This
  drives the dashboard choice (Web Bluetooth) — Safari refuses to
  ship it, so we target third-party iOS browsers
  (**Bluefy** / **WebBLE**) which expose `navigator.bluetooth` via
  WKWebView. Same HTML works in Chrome on Android and desktop.
- BLE is **opt-in via NVS**: presence of a `[ble]` section in
  `provisioning.toml` switches it on, exactly like the existing `[gcp]`
  block. Absent → BLE init is skipped entirely (no flash bloat
  benefit, but ~45 KB heap saved).
- Read characteristics first; write characteristics + pairing land
  later.

## Scope of v1 (smallest working thing)

- BLE init when `[ble]` is configured in NVS, advertised name from
  config (default `esp32-<last4-of-mac>`).
- One standard service: **Device Information Service (0x180A)** —
  manufacturer, model, firmware revision (= `FW_VERSION` git SHA),
  hardware revision, serial number (= base MAC).
- A handful of custom diagnostic chars (uptime, free heap, RSSI, IP).
  Read + notify; the metrics thread already snapshots these every 5 s.
- No pairing, no encryption. Read-only.
- Verify on iOS using **nRF Connect for iOS** (native explorer app,
  free) before touching the dashboard.
- Then a one-page Web Bluetooth dashboard checked into
  `tools/ble-dashboard/`, viewable on iOS in **Bluefy**, on Android in
  Chrome, on desktop in Chrome/Edge.

## Stack choice: NimBLE, not Bluedroid

ESP-IDF ships two host stacks:

| | Bluedroid (default) | NimBLE |
|--|--|--|
| Modes | BT Classic + BLE | BLE only |
| Flash | ~280 KB | ~110 KB |
| Heap (idle) | ~30–50 KB | ~10–20 KB |
| Apache mynewt port | — | yes |
| ESP-IDF support | first-class | first-class |

We want BLE only, and our heap is already tight (`mbedtls` plus the
three TLS sessions — see `docs/observability.md`). NimBLE wins on both
axes. Set `CONFIG_BT_ENABLED=y`, `CONFIG_BT_NIMBLE_ENABLED=y`,
`CONFIG_BT_CONTROLLER_ENABLED=y` (controller mode = host+controller in
the same chip), and disable Classic-only knobs.

Tradeoff to remember: if we ever want BT Classic (A2DP, SPP, HFP) we'd
have to switch back. Unlikely for this project.

## Rust binding choice

Two options on top of NimBLE:

- **`esp-idf-svc::bt`** (already a dep). Safe-ish wrappers, but
  low-level — you wire GAP/GATT callbacks yourself and translate
  between Rust types and `esp_idf_svc::sys::*` C structs. Few examples
  in the wild for full GATT servers.
- **`esp32-nimble`** (third-party, ~v0.10). Higher-level: builder API
  for advertising data, `Service::add_characteristic(...)` with
  closures for read/write callbacks, automatic notification plumbing.
  Pulls in `esp-idf-sys` directly; coexists fine with `esp-idf-svc`.

Recommend `esp32-nimble` for v1 — gets us to a working GATT in ~100
lines instead of ~600. We can always drop to raw `esp-idf-svc::bt` /
sys if we hit a wall.

Add to `Cargo.toml`:

```toml
esp32-nimble = "0.10"   # pin once we pick a version
```

## Memory + coexistence

The ESP32 has one 2.4 GHz radio shared between Wi-Fi and BT. ESP-IDF's
coexistence scheduler time-slices automatically; in practice Wi-Fi
keeps working with a small throughput hit while BLE advertising +
single connection is active. Knobs:

```
CONFIG_BT_ENABLED=y
CONFIG_BT_CONTROLLER_ENABLED=y
CONFIG_BTDM_CTRL_MODE_BLE_ONLY=y      # not BR/EDR
CONFIG_BT_NIMBLE_ENABLED=y
CONFIG_BT_NIMBLE_MAX_CONNECTIONS=1    # we only ever expect one phone
CONFIG_SW_COEXIST_ENABLE=y            # Wi-Fi/BT scheduler
```

Heap budget needs re-checking once enabled. Budget rough numbers:

- BLE controller: ~30 KB heap reserved at init
- NimBLE host: ~10 KB working set
- One active GATT connection: another ~5 KB

So expect ~45 KB extra steady-state heap consumed. We currently sit at
~70–90 KB free at idle (`min_free_heap` from metrics). Should fit, but
the OTA-download window will be tighter — first thing to watch on the
metrics dashboard once it's deployed.

Flash: ~110 KB extra in the app slot. We have headroom in the 1.94 MB
slots; no partition table change needed.

If coexistence ever turns out to be flaky during OTA, the escape hatch
is `OtaDownloadGuard` — add a "BLE adv paused during OTA" arm
analogous to how `cloud_log` and `metrics` skip while
`OTA_DOWNLOAD_IN_PROGRESS` is set.

## What gets advertised

**GAP advertising payload** (visible to any scanner without
connecting, 31-byte legacy budget):

- Flags (general discoverable + BR/EDR not supported)
- Complete local name: `esp32-<last4>` (8 bytes incl. type/length)
- 16-bit service UUIDs: `0x180A` (DIS) + our 128-bit custom service
  UUID (which goes in scan response to fit)
- TX power level

**Scan response** (another 31 bytes, sent on demand):

- 128-bit custom service UUID
- Manufacturer-specific data: 2-byte company id (use 0xFFFF "tests
  and not assigned" for now) + a packed `{uptime_s: u32, free_heap_kb:
  u16}` so a passive scanner can see the device is alive without
  connecting.

**GATT — Device Information Service (0x180A)**, all standard 16-bit
UUIDs, single read each:

| Char | UUID | Value |
|--|--|--|
| Manufacturer Name | 0x2A29 | `imjasonh` |
| Model Number | 0x2A24 | `esp32-blinky` |
| Firmware Revision | 0x2A26 | `FW_VERSION` (git SHA) |
| Hardware Revision | 0x2A27 | `Inland ESP-WROOM-32` |
| Serial Number | 0x2A25 | base MAC, e.g. `AA:BB:CC:DD:EE:FF` |

**GATT — custom service** (random 128-bit UUID, generated once and
checked in):

| Char | Type | Update |
|--|--|--|
| Uptime (seconds, u32) | read + notify | every 5 s |
| Free heap bytes (u32) | read + notify | every 5 s |
| Min free heap bytes (u32) | read | snapshot |
| Wi-Fi SSID (utf8) | read | static after connect |
| Wi-Fi RSSI (i8 dBm) | read + notify | every 5 s |
| IPv4 address (4 bytes) | read | static after connect |
| Boot partition label (utf8) | read | `ota_0` / `ota_1` |
| OTA state (u8 enum) | read | running / pending-verify / idle / downloading |
| Last OTA poll (unix s, u64) | read | last manifest fetch |

The notify cadence reuses the existing 5 s metrics tick — same
snapshot the `metrics` thread already builds, just an extra fan-out to
BLE subscribers.

## Phone / browser viewing options

The iOS Web Bluetooth situation drives this:

- **Apple Safari does not implement Web Bluetooth** and has signalled
  no intent to. Workarounds that *do* work today on iOS:
  - **Bluefy – Web BLE Browser** (App Store, free) — Chromium-based
    WKWebView shim that exposes `navigator.bluetooth.*`. Same HTML,
    same JS as Chrome desktop.
  - **WebBLE** (App Store, paid one-time) — same idea, older.
  - **nRF Connect for iOS** — native explorer, not a browser, but
    works without us writing any phone code.

Recommended path:

1. **Bring-up / debugging**: nRF Connect (iOS, Android, free). Full
   GATT browser, raw ATT log, save device profile.
2. **Day-to-day "look at my thing"**: a single static HTML in
   `tools/ble-dashboard/index.html`, opened in Bluefy on iPhone or
   Chrome on Android/desktop. Hosted on GitHub Pages from `main`.
3. **Power-user / scripting**: `bluetoothctl` on Linux desktops.

Later (with build effort) if the Web Bluetooth dashboard hits its
ceiling:

- **Flutter app** (`flutter_blue_plus`) — iOS + Android, gives us
  background scanning, push notifications on state change.
- **Native iOS CoreBluetooth / Android BluetoothLeScanner** — most
  control, most code.
- **Apple Shortcuts / Android Tasker** — phone automation that pings
  the device and triggers something on (re)connect.

## NVS config schema

New `[ble]` block in `provisioning.toml`. Presence opts in; absence
skips BLE init entirely (mirrors `[gcp]`):

```toml
[ble]
# Optional. Defaults to "esp32-<last4-of-mac>" if omitted.
name = "kitchen-esp"

# Phase 3 (writes): enable encrypted-link writes with this passkey.
# Six digits, 000000–999999. Omit to keep BLE read-only.
# passkey = 314159
```

NVS keys (15-char cap — see CLAUDE.md):

| Namespace | Key | Type | Notes |
|--|--|--|--|
| `ble` | `name` | str | optional; falls back to mac-suffix default |
| `ble` | `passkey` | u32 | optional; phase 3 only |

Loader pattern matches `cloud_log::GcpConfig::load` —
`ble::Config::load(nvs) -> Result<Option<Config>>`, `Some(_)` only
when the namespace exists.

## Security

v1: no pairing, no bonding, no encryption. Read-only diagnostic data
is exposed unauthenticated to anyone in BLE range. Same threat
surface as our serial console — fine for a desk device.

When we add **write** characteristics — reboot, force-OTA-poll,
display message, clear NVS, etc. — flip to:

- LE Secure Connections (ECDH key agreement, MITM-protected) with the
  static `passkey` from NVS.
- Encrypted-link required on writeable chars; read chars stay open so
  the dashboard works for casual viewers.
- Bonding optional. Without it, the user re-enters the passkey each
  reconnect, which is fine for an occasional control surface and
  saves us NVS storage of bond keys.

Don't reach for "just works" pairing — it's no real security, and the
NVS-provisioned passkey path is cheap.

Web Bluetooth + pairing on iOS Bluefy: pairing prompts surface
through the OS the same way they would for a native app, so this is
all routine — no special dance needed.

## Lifecycle

```
main()
  ├─ wifi up
  ├─ sntp (if gcp)
  ├─ ota::run        (existing thread)
  ├─ cloud_log::run  (existing, gated on gcp)
  ├─ metrics::run    (existing, gated on gcp)
  └─ ble::run        ← new thread
      ├─ esp32_nimble::BLEDevice::take()
      ├─ register Device Information Service
      ├─ register custom service + notifying chars
      ├─ start advertising
      └─ loop:
          ├─ sleep 5 s
          ├─ snapshot heap/uptime/rssi (reuse metrics::snapshot)
          ├─ update characteristic values
          └─ notify subscribers
```

Opt-in is by `[ble]` section presence in NVS — see "NVS config
schema" above. No boolean toggle.

Stack budget: 16 KB seems right (no crypto, no HTTPS — just NimBLE
callbacks and our snapshot logic). Match the metrics thread budget.

## Implementation phases

1. **Phase 1 — minimum viable peripheral.** ✅ Shipped. NimBLE init,
   advertise with name from NVS, expose Device Information Service.
   Verify on iOS with nRF Connect. ~140 LOC in `src/ble.rs`, plus
   `[ble]` loader, provision-tool support, and the
   `sdkconfig.defaults.in` knobs.
2. **Phase 2 — diagnostic service + dashboard.** ✅ Shipped. Custom
   128-bit service with eight chars (uptime / free heap / min free
   heap / wifi SSID / wifi RSSI / IPv4 / boot partition / OTA state).
   Read + notify on the dynamic ones, refreshed by `ble::run`'s own
   5 s tick (independent of `metrics_interval_secs`, which defaults to
   300 s and is too slow for a phone dashboard). Static one-pager at
   `tools/ble-dashboard/index.html` consumes them via Web Bluetooth —
   tested in Chrome desktop, Android Chrome, and iOS Bluefy.
   Filter is `namePrefix: "esp32-"` plus `optionalServices` for the
   custom + DIS UUIDs, so the 31-byte legacy adv budget stays small
   (no need to fit a 128-bit UUID alongside the name).
3. **Phase 3 — write support.** Add the passkey-protected control
   characteristics: force-OTA-poll, reboot, push e-ink message
   (depends on `docs/eink-plan.md`). Flip the link to LE Secure
   Connections + encryption for writes.

## Future ideas (cool : effort)

1. **Wi-Fi provisioning over BLE.** If NVS has no SSID, boot in
   BLE-only mode advertising a "provisioning" service. Phone writes
   SSID + password, device persists to NVS and reboots. Replaces (or
   complements) the USB-only `make provision` path. Industry-standard
   pattern; ESP-IDF even has a `wifi_provisioning` component that
   wraps NimBLE for exactly this.
2. **OTA control surface.** Writeable chars to force a poll, view
   manifest history, mark-bad and reboot. Useful in the field where a
   serial cable isn't available.
3. **Trust-config rotation over BLE.** Push a new Sigstore identity
   list when the publisher's GHCR repo moves. Same encrypted-link
   guarding as OTA control.
4. **iBeacon / Eddystone broadcast.** Skip GATT entirely, just
   advertise presence + a tiny payload. Plays nicely with a "device
   nearby" home-automation rule.
5. **BLE-driven e-ink updates.** Once `docs/eink-plan.md` ships, a
   writeable "display message" characteristic lets a phone push text
   to the screen. Doorbell badge / desk sign without writing any
   server-side code.
