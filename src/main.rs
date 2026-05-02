use anyhow::{anyhow, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use embedded_svc::http::client::Client;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration as WifiConfig, EspWifi,
};
use std::ffi::CStr;
use std::time::Duration;

mod ota;
mod sig;
mod trust;

const SSID: &str = env!("WIFI_SSID");
const PASS: &str = env!("WIFI_PASS");
// Set by the Makefile from `git rev-parse --short HEAD` and baked into
// the firmware so each build identifies itself in the boot log.
const FW_VERSION: &str = env!("GIT_SHA");

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log_running_partition();
    tracing::info!(version = FW_VERSION, "booting");

    let pending_verify = ota::is_pending_verify();
    if pending_verify {
        tracing::info!("ota: image is in PENDING_VERIFY -- bringup must succeed before mark-valid");
    }

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs.clone()))?,
        sysloop,
    )?;

    connect_wifi(&mut wifi)?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    tracing::info!(
        ip = %ip_info.ip,
        gateway = %ip_info.subnet.gateway,
        dns = ?ip_info.dns,
        "wifi connected",
    );

    fetch("https://api.ipify.org?format=json")?;
    fetch("https://wttr.in/?format=3")?;

    if pending_verify {
        match ota::mark_valid_after_pending_verify_passed(nvs.clone()) {
            Ok(()) => tracing::info!("ota: pending-verify passed, image is good"),
            Err(e) => {
                tracing::error!(error = %e, "ota: mark-valid failed; rebooting to trigger rollback");
                std::thread::sleep(Duration::from_secs(2));
                unsafe { esp_idf_svc::sys::esp_restart() };
            }
        }
    }

    let ota_nvs = nvs.clone();
    std::thread::Builder::new()
        // HTTPS + JSON + SHA256 is ~32 KB; phase 4a adds X.509 parsing
        // and ECDSA P-256/P-384 verification on top, which want more.
        // 48 KB is observed-safe with headroom.
        .stack_size(48 * 1024)
        .spawn(move || ota::run(ota_nvs, FW_VERSION))
        .expect("spawn ota thread");

    tracing::info!("main: idling, OTA loop running in background");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn log_running_partition() {
    unsafe {
        let part = esp_idf_svc::sys::esp_ota_get_running_partition();
        if part.is_null() {
            tracing::warn!("running partition: <null>");
            return;
        }
        let label = CStr::from_ptr((*part).label.as_ptr()).to_string_lossy();
        tracing::info!(
            label = %label,
            offset = format_args!("0x{:x}", (*part).address),
            size = format_args!("0x{:x}", (*part).size),
            "running partition",
        );
    }
}

fn connect_wifi(wifi: &mut BlockingWifi<EspWifi<'static>>) -> Result<()> {
    let auth_method = if PASS.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    wifi.set_configuration(&WifiConfig::Client(ClientConfiguration {
        ssid: SSID.try_into().map_err(|_| anyhow!("SSID too long (max 32 bytes)"))?,
        bssid: None,
        auth_method,
        password: PASS.try_into().map_err(|_| anyhow!("password too long (max 64 bytes)"))?,
        channel: None,
        ..Default::default()
    }))?;

    wifi.start()?;
    tracing::info!(ssid = SSID, "wifi started; connecting");
    wifi.connect()?;
    wifi.wait_netif_up()?;
    Ok(())
}

fn fetch(url: &str) -> Result<()> {
    let conn = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    })?;
    let mut client = Client::wrap(conn);

    let req = client.request(Method::Get, url, &[("accept", "*/*")])?;
    let mut resp = req.submit()?;
    let status = resp.status();

    let mut buf = [0u8; 1024];
    let mut total = 0usize;
    let mut body = Vec::with_capacity(512);
    loop {
        let n = resp.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n;
        body.extend_from_slice(&buf[..n]);
    }
    tracing::info!(
        url = url,
        status = status,
        bytes = total,
        body = %String::from_utf8_lossy(&body).trim(),
        "GET",
    );
    Ok(())
}
