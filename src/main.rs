use anyhow::{anyhow, Result};
use embedded_svc::http::client::Client;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use esp_idf_svc::wifi::{
    AuthMethod, BlockingWifi, ClientConfiguration, Configuration as WifiConfig, EspWifi,
};
use std::ffi::CStr;
use std::time::Duration;

mod cloud_log;
mod gcp_auth;
mod metrics;
mod ota;
mod sig;
mod trust;

// NVS schema (must match what tools/provision/ writes):
//   namespace=wifi   key=ssid (str), key=pass (str)
const NVS_WIFI_NS: &str = "wifi";
const NVS_WIFI_SSID: &str = "ssid";
const NVS_WIFI_PASS: &str = "pass";

// Set by the Makefile from `git rev-parse --short HEAD` and baked into
// the firmware so each build identifies itself in the boot log.
const FW_VERSION: &str = env!("GIT_SHA");

fn main() -> Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    metrics::publish_self(&metrics::handles::MAIN);

    // Take NVS as early as possible — needed to decide whether to
    // install the cloud-log tracing subscriber, *before* any tracing
    // events fire.
    let nvs = EspDefaultNvsPartition::take()?;
    let gcp = cloud_log::GcpConfig::load(nvs.clone())?;
    let log_queue = if let Some(ref cfg) = gcp {
        let queue = cloud_log::LogQueue::new(cloud_log::QUEUE_CAPACITY);
        let layer = cloud_log::CloudLogLayer::new(queue.clone(), cfg.min_severity);
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        let _ = tracing_subscriber::registry().with(layer).try_init();
        Some(queue)
    } else {
        None
    };

    log_running_partition();
    tracing::info!(version = FW_VERSION, "booting");
    if gcp.is_some() {
        tracing::info!("cloud_log: subscribed");
    } else {
        tracing::info!("cloud_log: not configured (no [gcp] in NVS), serial only");
    }

    let pending_verify = ota::is_pending_verify();
    if pending_verify {
        tracing::info!("ota: image is in PENDING_VERIFY -- bringup must succeed before mark-valid");
    }

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;

    // Read remaining NVS config; strict-inert if missing. See ota.md.
    let (ssid, pass) = match read_wifi_creds(nvs.clone())? {
        Some(creds) => creds,
        None => block_unprovisioned("wifi/ssid or wifi/pass missing in NVS"),
    };
    let trust = match trust::TrustConfig::load(nvs.clone())? {
        Some(t) => t,
        None => block_unprovisioned("trust/identities or trust/fulcio_{root,inter} missing in NVS"),
    };
    tracing::info!(
        identities = trust.identities.len(),
        "loaded trust config from NVS",
    );

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs.clone()))?,
        sysloop,
    )?;

    connect_wifi(&mut wifi, &ssid, &pass)?;

    let ip_info = wifi.wifi().sta_netif().get_ip_info()?;
    tracing::info!(
        ip = %ip_info.ip,
        gateway = %ip_info.subnet.gateway,
        dns = ?ip_info.dns,
        "wifi connected",
    );

    // SNTP is only needed for cloud logging — the service-account JWT
    // auth requires real wall-clock time for `iat`/`exp` (Google rejects
    // tokens minted from a 1970 clock). Devices without `[gcp]` skip
    // the wait and boot faster. Cloud Logging entry timestamps
    // themselves are assigned server-side by GCP if we omit them.
    let _sntp = if gcp.is_some() {
        let sntp = esp_idf_svc::sntp::EspSntp::new_default()?;
        let started = std::time::Instant::now();
        loop {
            use esp_idf_svc::sntp::SyncStatus;
            if sntp.get_sync_status() == SyncStatus::Completed {
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "ntp: synced",
                );
                break;
            }
            if started.elapsed() > Duration::from_secs(15) {
                tracing::warn!(
                    "ntp: not synced after 15s, cloud logging will fail until clock catches up",
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        Some(sntp)
    } else {
        None
    };

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

    // One shared lock that serialises the *short* HTTPS calls across
    // cloud_log + metrics + OTA's manifest/sig-bundle fetches. The OTA
    // download (multi-second blob) deliberately runs without it. Even
    // when the [gcp] block is absent we still construct it so OTA's
    // signature can be the same; cloud_log/metrics just won't be there
    // to contend for it.
    let short_https = gcp_auth::new_short_https_lock();

    let ota_nvs = nvs.clone();
    let ota_trust = trust.clone();
    let ota_lock = short_https.clone();
    std::thread::Builder::new()
        // HTTPS + JSON + SHA256 is ~32 KB; phase 4a adds X.509 parsing
        // and ECDSA P-256/P-384 verification on top, which want more.
        // 48 KB is observed-safe with headroom.
        .stack_size(48 * 1024)
        .spawn(move || ota::run(ota_nvs, FW_VERSION, ota_trust, Some(ota_lock)))
        .expect("spawn ota thread");

    if let (Some(cfg), Some(queue)) = (gcp, log_queue) {
        // Build the shared TokenProvider once — both cloud_log and
        // metrics get an Arc clone and share the cached access token
        // (multi-scope JWT covers both APIs). Parsing the SA key at
        // construction time means a malformed key fails fast here
        // rather than once per minute in the sender thread.
        let auth = match gcp_auth::TokenProvider::new(cfg.clone()) {
            Ok(a) => Some(std::sync::Arc::new(a)),
            Err(e) => {
                tracing::error!(
                    error = %format!("{:#}", e),
                    "gcp_auth: TokenProvider init failed; cloud_log + metrics disabled",
                );
                None
            }
        };

        if let Some(auth) = auth {
            let cl_cfg = cfg.clone();
            let cl_auth = auth.clone();
            let cl_queue = queue.clone();
            let cl_lock = short_https.clone();
            std::thread::Builder::new()
                // RSA signing + HTTPS POST. 32 KB matches the OTA loop budget.
                .stack_size(32 * 1024)
                .spawn(move || cloud_log::run(cl_cfg, FW_VERSION, cl_auth, cl_queue, cl_lock))
                .expect("spawn cloud_log thread");

            if cfg.metrics_interval_secs > 0 {
                let m_cfg = cfg;
                let m_auth = auth;
                let m_queue = queue;
                let m_lock = short_https;
                std::thread::Builder::new()
                    // No crypto on hot path (token cached via auth).
                    // Mainly HTTPS POST + serde_json. 16 KB sufficient.
                    .stack_size(16 * 1024)
                    .spawn(move || metrics::run(m_cfg, FW_VERSION, m_auth, m_queue, m_lock))
                    .expect("spawn metrics thread");
            }
        }
    }

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

fn connect_wifi(wifi: &mut BlockingWifi<EspWifi<'static>>, ssid: &str, pass: &str) -> Result<()> {
    let auth_method = if pass.is_empty() {
        AuthMethod::None
    } else {
        AuthMethod::WPA2Personal
    };

    wifi.set_configuration(&WifiConfig::Client(ClientConfiguration {
        ssid: ssid
            .try_into()
            .map_err(|_| anyhow!("SSID too long (max 32 bytes)"))?,
        bssid: None,
        auth_method,
        password: pass
            .try_into()
            .map_err(|_| anyhow!("password too long (max 64 bytes)"))?,
        channel: None,
        ..Default::default()
    }))?;

    wifi.start()?;
    tracing::info!(ssid = ssid, "wifi started; connecting");
    wifi.connect()?;
    wifi.wait_netif_up()?;
    Ok(())
}

/// Read Wi-Fi credentials from NVS. Returns None if either key is missing.
fn read_wifi_creds(partition: EspDefaultNvsPartition) -> Result<Option<(String, String)>> {
    let nvs = EspNvs::new(partition, NVS_WIFI_NS, false)
        .map_err(|e| anyhow!("open NVS namespace {}: {:?}", NVS_WIFI_NS, e))?;
    let mut ssid_buf = [0u8; 64];
    let mut pass_buf = [0u8; 96];
    let ssid = nvs
        .get_str(NVS_WIFI_SSID, &mut ssid_buf)
        .map_err(|e| anyhow!("read NVS {}/{}: {:?}", NVS_WIFI_NS, NVS_WIFI_SSID, e))?;
    let pass = nvs
        .get_str(NVS_WIFI_PASS, &mut pass_buf)
        .map_err(|e| anyhow!("read NVS {}/{}: {:?}", NVS_WIFI_NS, NVS_WIFI_PASS, e))?;
    match (ssid, pass) {
        (Some(s), Some(p)) => Ok(Some((s.to_string(), p.to_string()))),
        _ => Ok(None),
    }
}

/// Sit forever logging that the device is unprovisioned. Doesn't reboot
/// (the firmware itself is healthy, just needs config), so a power cycle
/// in PENDING_VERIFY state will roll back as a safety net.
fn block_unprovisioned(reason: &str) -> ! {
    loop {
        tracing::error!(
            reason = reason,
            "NOT PROVISIONED — run `make provision` to write NVS, then reboot",
        );
        std::thread::sleep(Duration::from_secs(30));
    }
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
    tracing::debug!(
        url = url,
        status = status,
        bytes = total,
        body = %String::from_utf8_lossy(&body).trim(),
        "GET",
    );
    Ok(())
}
