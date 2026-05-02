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

const SSID: &str = env!("WIFI_SSID");
const PASS: &str = env!("WIFI_PASS");

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log_running_partition();
    log::info!("booting");

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop,
    )?;

    connect_wifi(&mut wifi)?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    log::info!("connected: ip={} gw={} dns={:?}", ip_info.ip, ip_info.subnet.gateway, ip_info.dns);

    fetch("https://api.ipify.org?format=json")?;
    fetch("https://wttr.in/?format=3")?;

    log::info!("done; idling. ctrl-r in espflash to reset.");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }
}

fn log_running_partition() {
    unsafe {
        let part = esp_idf_svc::sys::esp_ota_get_running_partition();
        if part.is_null() {
            log::warn!("running partition: <null>");
            return;
        }
        let label = CStr::from_ptr((*part).label.as_ptr()).to_string_lossy();
        log::info!(
            "running partition: {} (offset=0x{:x}, size=0x{:x})",
            label,
            (*part).address,
            (*part).size,
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
    log::info!("wifi started; connecting to ssid={}", SSID);
    wifi.connect()?;
    wifi.wait_netif_up()?;
    Ok(())
}

fn fetch(url: &str) -> Result<()> {
    log::info!("GET {}", url);

    let conn = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        ..Default::default()
    })?;
    let mut client = Client::wrap(conn);

    let req = client.request(Method::Get, url, &[("accept", "*/*")])?;
    let mut resp = req.submit()?;
    let status = resp.status();
    log::info!("  status={}", status);

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
    log::info!("  body ({} bytes): {}", total, String::from_utf8_lossy(&body).trim());
    Ok(())
}
