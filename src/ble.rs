//! BLE peripheral on top of NimBLE.
//!
//! Phase 2 — read-only Device Information Service (0x180A) plus a
//! custom diagnostic service. DIS exposes static identity
//! (manufacturer / model / firmware revision = `FW_VERSION` /
//! hardware revision / serial = MAC). The custom service exposes
//! eight live-ish chars that the on-device tick refreshes every 5 s
//! and notifies subscribers on:
//!
//!   uptime_secs        u32 LE
//!   free_heap          u32 LE
//!   min_free_heap      u32 LE
//!   wifi_ssid          utf8 (variable)
//!   wifi_rssi_dbm      i8
//!   ipv4               4 bytes (network order: a.b.c.d)
//!   boot_partition     utf8 (variable, e.g. "ota_0")
//!   ota_state          u8 (0=idle, 1=downloading)
//!
//! Connect from any BLE explorer (nRF Connect, LightBlue, Bluefy on
//! iOS, Chrome via the dashboard at `tools/ble-dashboard/`). No
//! pairing, no encryption — diagnostic-only reads.
//!
//! Opt-in via NVS: presence of the `ble` namespace switches it on,
//! exactly like `[gcp]` / `cloud_log::GcpConfig::load`. Absent →
//! BLE init is skipped entirely (saves ~45 KB heap on devices that
//! don't need it).
//!
//! See `docs/ble-plan.md`.

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use esp32_nimble::{
    utilities::{mutex::Mutex, BleUuid},
    uuid128, BLEAdvertisementData, BLECharacteristic, BLEDevice, NimbleProperties,
};
use std::ffi::CStr;
use std::sync::Arc;
use std::time::Duration;

use crate::gcp_auth::{device_mac, ota_download_in_progress};
use crate::nvs_util::read_str;

/// `module_path!()` for this module — used by cloud_log to filter out
/// our own tracing events without hardcoding the crate name.
pub const TARGET: &str = module_path!();

const NVS_BLE_NS: &str = "ble";
const NVS_NAME: &str = "name";

// Standard 16-bit UUIDs from the SIG-assigned numbers document.
const DIS_UUID: u16 = 0x180A;
const MANUFACTURER_NAME_CHAR: u16 = 0x2A29;
const MODEL_NUMBER_CHAR: u16 = 0x2A24;
const SERIAL_NUMBER_CHAR: u16 = 0x2A25;
const FIRMWARE_REV_CHAR: u16 = 0x2A26;
const HARDWARE_REV_CHAR: u16 = 0x2A27;

const MANUFACTURER_NAME: &str = "imjasonh";
const MODEL_NUMBER: &str = "esp32-blinky";
const HARDWARE_REVISION: &str = "Inland ESP-WROOM-32";

/// How often the diagnostic-service notify loop wakes and republishes
/// its dynamic chars. Independent of the cloud-side metrics tick (the
/// `metrics` thread runs at the configured `metrics_interval_secs`,
/// default 300 s, far too slow for a phone dashboard).
const NOTIFY_TICK: Duration = Duration::from_secs(5);

/// Runtime config loaded from NVS at boot.
#[derive(Clone, Debug)]
pub struct Config {
    /// Local name advertised in GAP. Defaults to `esp32-<last4-of-mac>`
    /// if the `ble/name` NVS key is absent.
    pub device_name: String,
}

impl Config {
    /// Load from NVS. `Ok(None)` means BLE is intentionally not
    /// enabled (the `ble` namespace doesn't exist). `Ok(Some(_))`
    /// means a `[ble]` block was provisioned, possibly with default
    /// values.
    pub fn load(partition: EspDefaultNvsPartition) -> Result<Option<Self>> {
        let nvs = match EspNvs::new(partition, NVS_BLE_NS, false) {
            Ok(n) => n,
            Err(e) if e.code() == esp_idf_svc::sys::ESP_ERR_NVS_NOT_FOUND as i32 => {
                return Ok(None);
            }
            Err(e) => {
                return Err(anyhow!("open NVS namespace {}: {:?}", NVS_BLE_NS, e));
            }
        };
        let configured = read_str(&nvs, NVS_BLE_NS, NVS_NAME, 64)?;
        let device_name = configured.unwrap_or_else(default_device_name);
        Ok(Some(Self { device_name }))
    }
}

/// `esp32-XXXX` where `XXXX` is the upper-cased last 4 hex chars of
/// the base MAC. Stable per device, fits in the 31-byte legacy
/// advertising budget alongside the DIS service UUID.
fn default_device_name() -> String {
    let mac = device_mac();
    let suffix = mac
        .get(mac.len().saturating_sub(4)..)
        .unwrap_or(&mac)
        .to_ascii_uppercase();
    format!("esp32-{}", suffix)
}

/// Format the 12-char hex MAC as `AA:BB:CC:DD:EE:FF` for the DIS
/// serial-number characteristic. Cosmetic; explorers display the
/// colon form by convention.
fn format_mac(mac_hex: &str) -> String {
    let mut s = String::with_capacity(17);
    for (i, c) in mac_hex.chars().enumerate() {
        if i > 0 && i % 2 == 0 {
            s.push(':');
        }
        s.push(c.to_ascii_uppercase());
    }
    s
}

/// Handles to the dynamic characteristics whose values the notify
/// loop refreshes every `NOTIFY_TICK`.
struct DynamicChars {
    uptime_secs: Arc<Mutex<BLECharacteristic>>,
    free_heap: Arc<Mutex<BLECharacteristic>>,
    min_free_heap: Arc<Mutex<BLECharacteristic>>,
    wifi_ssid: Arc<Mutex<BLECharacteristic>>,
    wifi_rssi: Arc<Mutex<BLECharacteristic>>,
    ipv4: Arc<Mutex<BLECharacteristic>>,
    ota_state: Arc<Mutex<BLECharacteristic>>,
}

/// One on-device snapshot of every dynamic char's current value.
/// Held for the duration of one tick; written into the chars under
/// their NimBLE locks before the notify fires.
#[derive(Default)]
struct Snapshot {
    uptime_secs: u32,
    free_heap: u32,
    min_free_heap: u32,
    wifi_ssid: Vec<u8>,
    wifi_rssi: i8,
    ipv4: [u8; 4],
    ota_state: u8,
}

/// Thread entry point. Initializes NimBLE, registers DIS + the
/// diagnostic service, starts advertising, then runs the
/// notify-update loop forever. NimBLE host runs in its own internal
/// FreeRTOS task — this thread owns setup + the periodic refresh.
pub fn run(cfg: Config, fw_version: &'static str) -> ! {
    crate::metrics::publish_self(&crate::metrics::handles::BLE);
    tracing::info!(name = %cfg.device_name, "ble: starting NimBLE peripheral");

    let dynamic = match setup(&cfg, fw_version) {
        Ok(d) => {
            tracing::info!(name = %cfg.device_name, "ble: advertising");
            d
        }
        Err(e) => {
            tracing::error!(
                error = %format!("{:#}", e),
                "ble: setup failed; thread idling, peripheral inactive",
            );
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    };

    loop {
        std::thread::sleep(NOTIFY_TICK);
        let snap = collect_snapshot();
        publish(&dynamic, &snap);
    }
}

fn setup(cfg: &Config, fw_version: &'static str) -> Result<DynamicChars> {
    let mac = device_mac();

    let device = BLEDevice::take();
    // `set_device_name` is an associated function on BLEDevice (no
    // self) — it writes the global `ble_svc_gap_device_name`. Method-
    // call syntax doesn't work here.
    BLEDevice::set_device_name(&cfg.device_name)
        .map_err(|e| anyhow!("set_device_name: {:?}", e))?;

    let server = device.get_server();
    server.on_connect(|_server, desc| {
        tracing::info!(peer = ?desc, "ble: client connected");
    });
    server.on_disconnect(|desc, reason| {
        tracing::info!(peer = ?desc, reason = ?reason, "ble: client disconnected");
    });

    // === Device Information Service (read-only, all static) ===
    let dis = server.create_service(BleUuid::from_uuid16(DIS_UUID));
    {
        let mut svc = dis.lock();
        svc.create_characteristic(
            BleUuid::from_uuid16(MANUFACTURER_NAME_CHAR),
            NimbleProperties::READ,
        )
        .lock()
        .set_value(MANUFACTURER_NAME.as_bytes());

        svc.create_characteristic(
            BleUuid::from_uuid16(MODEL_NUMBER_CHAR),
            NimbleProperties::READ,
        )
        .lock()
        .set_value(MODEL_NUMBER.as_bytes());

        svc.create_characteristic(
            BleUuid::from_uuid16(SERIAL_NUMBER_CHAR),
            NimbleProperties::READ,
        )
        .lock()
        .set_value(format_mac(&mac).as_bytes());

        svc.create_characteristic(
            BleUuid::from_uuid16(FIRMWARE_REV_CHAR),
            NimbleProperties::READ,
        )
        .lock()
        .set_value(fw_version.as_bytes());

        svc.create_characteristic(
            BleUuid::from_uuid16(HARDWARE_REV_CHAR),
            NimbleProperties::READ,
        )
        .lock()
        .set_value(HARDWARE_REVISION.as_bytes());
    }

    // === Custom diagnostic service ===
    // Random 128-bit base UUID; the per-char UUIDs share the same
    // suffix and only differ in the third hex group's low byte.
    let diag = server.create_service(uuid128!("5f6c2a00-fa9d-4d8a-bf8f-01a3c8ab9d9e"));
    let mut svc = diag.lock();

    let read_notify = NimbleProperties::READ | NimbleProperties::NOTIFY;

    let uptime_secs = svc.create_characteristic(
        uuid128!("5f6c2a01-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        read_notify,
    );
    let free_heap = svc.create_characteristic(
        uuid128!("5f6c2a02-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        read_notify,
    );
    let min_free_heap = svc.create_characteristic(
        uuid128!("5f6c2a03-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        read_notify,
    );
    let wifi_ssid = svc.create_characteristic(
        uuid128!("5f6c2a04-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        read_notify,
    );
    let wifi_rssi = svc.create_characteristic(
        uuid128!("5f6c2a05-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        read_notify,
    );
    let ipv4 = svc.create_characteristic(
        uuid128!("5f6c2a06-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        read_notify,
    );
    // Boot partition label is genuinely fixed for the lifetime of the
    // running image — set once, no notify.
    let boot_partition = svc.create_characteristic(
        uuid128!("5f6c2a07-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        NimbleProperties::READ,
    );
    boot_partition
        .lock()
        .set_value(running_partition_label().as_bytes());
    let ota_state = svc.create_characteristic(
        uuid128!("5f6c2a08-fa9d-4d8a-bf8f-01a3c8ab9d9e"),
        read_notify,
    );

    drop(svc);

    // Seed initial values so a client that reads before the first
    // 5 s tick gets something sensible.
    let snap = collect_snapshot();
    let dynamic = DynamicChars {
        uptime_secs,
        free_heap,
        min_free_heap,
        wifi_ssid,
        wifi_rssi,
        ipv4,
        ota_state,
    };
    publish(&dynamic, &snap);

    let advertising = device.get_advertising();
    advertising
        .lock()
        .set_data(
            BLEAdvertisementData::new()
                .name(&cfg.device_name)
                .add_service_uuid(BleUuid::from_uuid16(DIS_UUID)),
        )
        .map_err(|e| anyhow!("set advertising data: {:?}", e))?;
    advertising
        .lock()
        .start()
        .map_err(|e| anyhow!("start advertising: {:?}", e))?;

    Ok(dynamic)
}

fn publish(d: &DynamicChars, snap: &Snapshot) {
    d.uptime_secs
        .lock()
        .set_value(&snap.uptime_secs.to_le_bytes())
        .notify();
    d.free_heap
        .lock()
        .set_value(&snap.free_heap.to_le_bytes())
        .notify();
    d.min_free_heap
        .lock()
        .set_value(&snap.min_free_heap.to_le_bytes())
        .notify();
    d.wifi_ssid.lock().set_value(&snap.wifi_ssid).notify();
    d.wifi_rssi
        .lock()
        .set_value(&[snap.wifi_rssi as u8])
        .notify();
    d.ipv4.lock().set_value(&snap.ipv4).notify();
    d.ota_state.lock().set_value(&[snap.ota_state]).notify();
}

fn collect_snapshot() -> Snapshot {
    let mut s = Snapshot::default();

    unsafe {
        let micros = esp_idf_svc::sys::esp_timer_get_time();
        s.uptime_secs = (micros / 1_000_000) as u32;
        s.free_heap = esp_idf_svc::sys::esp_get_free_heap_size();
        s.min_free_heap = esp_idf_svc::sys::esp_get_minimum_free_heap_size();
    }

    let mut ap_info: esp_idf_svc::sys::wifi_ap_record_t = unsafe { core::mem::zeroed() };
    let err = unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut ap_info) };
    if err == esp_idf_svc::sys::ESP_OK {
        s.wifi_rssi = ap_info.rssi;
        // wifi_ap_record_t.ssid is a 33-byte array, NUL-terminated.
        let nul = ap_info.ssid.iter().position(|&b| b == 0).unwrap_or(32);
        s.wifi_ssid = ap_info.ssid[..nul].to_vec();
    }

    // Look up the STA netif by its well-known key. Returns NULL until
    // wifi has finished bringing up the interface, so we silently
    // leave ipv4 = [0;4] in that case.
    unsafe {
        let netif = esp_idf_svc::sys::esp_netif_get_handle_from_ifkey(
            b"WIFI_STA_DEF\0".as_ptr() as *const _,
        );
        if !netif.is_null() {
            let mut ip_info: esp_idf_svc::sys::esp_netif_ip_info_t = core::mem::zeroed();
            let err = esp_idf_svc::sys::esp_netif_get_ip_info(netif, &mut ip_info);
            if err == esp_idf_svc::sys::ESP_OK {
                // ip_info.ip.addr is a u32 in host order on ESP-IDF
                // despite being declared as such in lwip; the byte
                // order on the wire is little-endian per ESP-IDF
                // convention. For BLE we send the dotted-quad in
                // human reading order (a.b.c.d).
                let raw = ip_info.ip.addr.to_le_bytes();
                s.ipv4 = raw;
            }
        }
    }

    s.ota_state = if ota_download_in_progress() { 1 } else { 0 };

    s
}

fn running_partition_label() -> String {
    unsafe {
        let part = esp_idf_svc::sys::esp_ota_get_running_partition();
        if part.is_null() {
            return String::from("?");
        }
        CStr::from_ptr((*part).label.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}
