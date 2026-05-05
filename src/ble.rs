//! BLE peripheral on top of NimBLE.
//!
//! Phase 1 — read-only Device Information Service (0x180A):
//! manufacturer / model / firmware revision (= `FW_VERSION`) /
//! hardware revision / serial number (= MAC). Connect from any BLE
//! explorer (nRF Connect, LightBlue, Bluefy on iOS) and read the
//! values. No pairing, no encryption — diagnostic-only.
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
    utilities::BleUuid, BLEAdvertisementData, BLEDevice, NimbleProperties,
};

use crate::gcp_auth::device_mac;
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

/// Thread entry point. Initializes NimBLE, registers DIS, starts
/// advertising, then sleeps. NimBLE host runs in its own internal
/// FreeRTOS task — this thread just owns the one-shot setup and
/// (later) the notify-update loop.
pub fn run(cfg: Config, fw_version: &'static str) -> ! {
    crate::metrics::publish_self(&crate::metrics::handles::BLE);
    tracing::info!(name = %cfg.device_name, "ble: starting NimBLE peripheral");

    if let Err(e) = setup(&cfg, fw_version) {
        tracing::error!(
            error = %format!("{:#}", e),
            "ble: setup failed; thread idling",
        );
    } else {
        tracing::info!(name = %cfg.device_name, "ble: advertising");
    }

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn setup(cfg: &Config, fw_version: &'static str) -> Result<()> {
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

    Ok(())
}
