# BLE — plan

Goal: the device advertises itself as a connectable BLE peripheral with a
recognizable name, and a phone (or laptop) can connect to it and read a
handful of read-only "what is this thing" attributes — firmware version,
uptime, free heap, IP address, RSSI, OTA state. No phone-side build for
the first cut: we use a standard BLE explorer app to view the GATT.

## Scope of v1

- Power on → BLE advertising starts after Wi-Fi is up.
- Advertised local name: `esp32-<last4-of-mac>` (e.g. `esp32-A3F2`).
- One standard GATT service: **Device Information Service (0x180A)** —
  manufacturer, model, firmware revision (= `FW_VERSION` git SHA),
  hardware revision, serial number (= base MAC).
- One custom GATT service exposing live diagnostic state. Read + notify
  on dynamic chars (uptime, heap, RSSI), read-only on static ones.
- No pairing, no encryption — diagnostic data only, link is connectable
  by anyone in range. Pairing comes in v2 if/when we add write
  characteristics (reboot, force-OTA-poll, etc.).
- Connect from a phone using **nRF Connect** (Android/iOS) or
  **LightBlue Explorer** to verify everything is wired up. No app to build.

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

## Phone-side options for v1 (no app to build)

1. **nRF Connect** — Nordic, free, Android + iOS. Most powerful: full
   GATT browser, decoders for standard characteristics, log of every
   ATT request, can save device profile. **Recommended.**
2. **LightBlue Explorer** — Punch Through, free, Android + iOS.
   Friendlier UI, slightly less detail; good first impression.
3. **Web Bluetooth from Chrome on Android** — `navigator.bluetooth.requestDevice(...)`,
   no app install. Doesn't work on iOS Safari (Apple disables it).
   Useful if we want a one-pager dashboard later.
4. **`bluetoothctl` / `bluetoothd` on Linux** — fine for desktop testing.

Path of least resistance: install nRF Connect on the phone, scan,
connect, screenshot the DIS + custom service. That's "v1 done".

## Phone-side options for later (with build effort)

- **Web Bluetooth dashboard**: a single static HTML page (host on
  GitHub Pages or load from a USB stick) that connects, subscribes to
  the notify chars, and renders a live status panel. ~200 LOC of JS.
  Android/desktop only.
- **Flutter app** with `flutter_blue_plus`: works on iOS + Android,
  ~1 day of work for a polished status screen.
- **Native iOS (CoreBluetooth) / Android (BluetoothLeScanner)**: most
  control, most code.
- **Tasker / Shortcuts**: phone automation that pings the device and
  triggers something on disconnect/reconnect. Cute, niche.

## Security

v1: no pairing, no bonding, no encryption. Read-only diagnostic data
is exposed unauthenticated to anyone in BLE range. That's the same
threat surface as our serial console, which is fine for a desk
device.

If/when we add **write** characteristics — reboot, force-OTA-poll,
clear NVS, etc. — flip to:

- LE Secure Connections (ECDH key agreement, MITM-protected) with a
  static passkey provisioned via NVS (new namespace `ble`, key
  `passkey`, 6 digits).
- Encrypted-link required on writeable chars; reads stay open.
- Bonding optional (we don't really need to remember devices).

Don't reach for "just works" pairing — it's no real security, and the
NVS-provisioned passkey path is cheap.

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

NVS toggle: a `[ble] enabled = true|false` block in
`provisioning.toml` so devices that don't need it skip BLE init
entirely (saves ~45 KB heap). Default true.

Stack budget: 16 KB seems right (no crypto, no HTTPS — just NimBLE
callbacks and our snapshot logic). Match the metrics thread budget.

## Implementation phases

1. **Phase 1 — minimum viable peripheral.** NimBLE init, advertise,
   Device Information Service only. Verify with nRF Connect. ~80 LOC
   in a new `src/ble.rs`. Adjust `sdkconfig.defaults.in`. Confirm
   heap headroom on the metrics dashboard before merging.
2. **Phase 2 — diagnostic service.** Custom 128-bit service with the
   read+notify chars listed above. Hook into the existing 5 s
   `metrics` tick so we don't run two timers.
3. **Phase 3 (optional) — Web Bluetooth dashboard.** Static HTML in
   `tools/ble-dashboard/`, served from GitHub Pages on each push.

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
