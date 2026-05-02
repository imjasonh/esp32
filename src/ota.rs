//! OTA polling and apply loop. Pulls firmware from an OCI registry,
//! verifies its layer digest, writes to the inactive OTA slot, and
//! reboots into the new image. The bootloader's rollback support
//! reverts on next boot if the new image doesn't call
//! `mark_valid_after_pending_verify_passed` (see main.rs).

use anyhow::{anyhow, bail, Context, Result};
use embedded_svc::http::client::Client;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection, FollowRedirectsPolicy};
use esp_idf_svc::http::Method;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::ota::EspOta;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

const NVS_NAMESPACE: &str = "ota";
const NVS_LAST_DIGEST: &str = "last_digest";
const NVS_PENDING_DIGEST: &str = "pending_digest";
// Operator-tunable settings; if a key is absent we fall back to the
// compile-time defaults in OtaConfig::default(). Writing these requires
// a future provisioning mechanism (HTTP endpoint or serial console);
// for now they're read-only-from-the-firmware's-perspective.
const NVS_REPO: &str = "repo";
const NVS_TAG: &str = "tag";
const NVS_POLL_SECS: &str = "poll_secs";

const BACKOFF_CAP: Duration = Duration::from_secs(3600); // 1h max between polls

/// Where to fetch firmware from.
pub struct OtaConfig {
    pub repo: String,           // "ghcr.io/imjasonh/esp32"
    pub tag: String,            // "latest"
    pub poll_interval: Duration,
}

impl Default for OtaConfig {
    fn default() -> Self {
        Self {
            repo: "ghcr.io/imjasonh/esp32".into(),
            tag: "latest".into(),
            // 60s for development; ota-plan.md targets 600s in steady state.
            poll_interval: Duration::from_secs(60),
        }
    }
}

impl OtaConfig {
    fn load_from_nvs(nvs: &EspNvs<NvsDefault>) -> Self {
        let mut cfg = Self::default();
        if let Some(repo) = read_string(nvs, NVS_REPO) {
            cfg.repo = repo;
        }
        if let Some(tag) = read_string(nvs, NVS_TAG) {
            cfg.tag = tag;
        }
        if let Ok(Some(secs)) = nvs.get_u32(NVS_POLL_SECS) {
            if secs > 0 {
                cfg.poll_interval = Duration::from_secs(secs as u64);
            }
        }
        cfg
    }
}

/// OCI image manifest, the only fields we care about.
#[derive(Deserialize)]
struct Manifest {
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct Descriptor {
    digest: String,
    size: u64,
    #[serde(rename = "mediaType")]
    media_type: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

/// Background loop. Runs forever; never returns. Spawn in a thread.
/// Reads configuration from NVS on startup, with compile-time defaults.
pub fn run(nvs_partition: EspDefaultNvsPartition, fw_version: &str) -> ! {
    let mut nvs = match EspNvs::new(nvs_partition, NVS_NAMESPACE, true) {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("ota: failed to open NVS namespace: {:?}", e);
            // Sleep forever rather than crash the app.
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        }
    };

    let cfg = OtaConfig::load_from_nvs(&nvs);
    let last = read_string(&nvs, NVS_LAST_DIGEST).unwrap_or_else(|| "<none>".into());
    tracing::info!(
        fw = fw_version,
        repo = %cfg.repo,
        tag = %cfg.tag,
        poll_secs = cfg.poll_interval.as_secs(),
        last_digest = %last,
        "ota: boot summary",
    );

    let mut consecutive_failures: u32 = 0;
    loop {
        let sleep_for = if consecutive_failures > 0 {
            backoff_with_jitter(cfg.poll_interval, consecutive_failures)
        } else {
            jittered(cfg.poll_interval)
        };
        tracing::info!(
            sleep_secs = sleep_for.as_secs(),
            failures = consecutive_failures,
            "ota: sleeping",
        );
        std::thread::sleep(sleep_for);
        match poll_once(&mut nvs, &cfg) {
            Ok(PollOutcome::NoChange) => {
                consecutive_failures = 0;
                tracing::info!("ota: no change");
            }
            Ok(PollOutcome::Updated(d)) => {
                tracing::info!(digest = %d, "ota: applied, rebooting in 1s");
                std::thread::sleep(Duration::from_secs(1));
                unsafe { esp_idf_svc::sys::esp_restart() };
            }
            Err(e) => {
                consecutive_failures += 1;
                tracing::warn!(
                    failures = consecutive_failures,
                    error = %e,
                    "ota: poll failed",
                );
            }
        }
    }
}

/// Exponential backoff capped at BACKOFF_CAP, with ±10% jitter applied.
fn backoff_with_jitter(base: Duration, failures: u32) -> Duration {
    // 2^10 = 1024x is plenty; the cap will be hit far earlier in practice.
    let exp = failures.min(10);
    let multiplied = base.saturating_mul(1u32 << exp);
    let bounded = multiplied.min(BACKOFF_CAP);
    jittered(bounded)
}

/// Apply ±10% jitter using the ESP32's hardware RNG. Used on the success
/// path too so a fleet doesn't poll in lockstep.
fn jittered(base: Duration) -> Duration {
    let r: u32 = unsafe { esp_idf_svc::sys::esp_random() };
    let pct = (r % 21) as i32 - 10; // -10..=+10
    let secs = base.as_secs() as i64;
    let delta = (secs * pct as i64) / 100;
    let new_secs = (secs + delta).max(1) as u64;
    Duration::from_secs(new_secs)
}

enum PollOutcome {
    NoChange,
    Updated(String),
}

fn poll_once(nvs: &mut EspNvs<NvsDefault>, cfg: &OtaConfig) -> Result<PollOutcome> {
    let token = fetch_anon_token(&cfg.repo)?;

    // Manifest fetch. We re-fetch every poll (no If-None-Match yet — the
    // ESP HTTP client doesn't make that trivial); we do compare the layer
    // digest from the manifest against last_applied_digest before
    // downloading the (much larger) layer.
    let manifest = fetch_manifest(&cfg.repo, &cfg.tag, &token)?;
    let layer = manifest
        .layers
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("manifest has no layers"))?;
    if layer.media_type != "application/vnd.esp32.firmware.bin" {
        bail!("unexpected layer mediaType: {}", layer.media_type);
    }
    tracing::info!(
        digest = %layer.digest,
        size = layer.size,
        "ota: manifest layer",
    );

    let last = read_string(nvs, NVS_LAST_DIGEST).unwrap_or_default();
    if last == layer.digest {
        return Ok(PollOutcome::NoChange);
    }
    tracing::info!(
        previous = %if last.is_empty() { "<none>" } else { &last },
        new = %layer.digest,
        "ota: new digest, downloading",
    );

    download_and_apply(&cfg.repo, &layer, &token)?;

    // Persist as pending; main.rs will promote to last_digest after the
    // post-reboot pending-verify check passes.
    write_string(nvs, NVS_PENDING_DIGEST, &layer.digest)?;
    Ok(PollOutcome::Updated(layer.digest))
}

fn fetch_anon_token(repo: &str) -> Result<String> {
    // Strip the "ghcr.io/" prefix; the token endpoint wants "<owner>/<name>".
    let repo_path = repo
        .strip_prefix("ghcr.io/")
        .ok_or_else(|| anyhow!("only ghcr.io is supported for now (got {})", repo))?;
    let url = format!(
        "https://ghcr.io/token?service=ghcr.io&scope=repository:{}:pull",
        repo_path
    );

    let mut buf = Vec::with_capacity(1024);
    fetch_to_buf(&url, &[], &mut buf)?;
    let resp: TokenResponse =
        serde_json::from_slice(&buf).context("parse token response JSON")?;
    Ok(resp.token)
}

fn fetch_manifest(repo: &str, tag: &str, token: &str) -> Result<Manifest> {
    let repo_path = repo
        .strip_prefix("ghcr.io/")
        .ok_or_else(|| anyhow!("only ghcr.io is supported for now (got {})", repo))?;
    let url = format!("https://ghcr.io/v2/{}/manifests/{}", repo_path, tag);
    let auth = format!("Bearer {}", token);

    let mut buf = Vec::with_capacity(4096);
    fetch_to_buf(
        &url,
        &[
            ("authorization", auth.as_str()),
            (
                "accept",
                "application/vnd.oci.image.manifest.v1+json",
            ),
        ],
        &mut buf,
    )?;
    let m: Manifest = serde_json::from_slice(&buf)
        .with_context(|| format!("parse manifest JSON ({} bytes)", buf.len()))?;
    Ok(m)
}

fn fetch_to_buf(url: &str, headers: &[(&str, &str)], buf: &mut Vec<u8>) -> Result<()> {
    let conn = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        follow_redirects_policy: FollowRedirectsPolicy::FollowAll,
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    })?;
    let mut client = Client::wrap(conn);
    let req = client.request(Method::Get, url, headers)?;
    let mut resp = req.submit()?;
    let status = resp.status();
    if status != 200 {
        bail!("GET {} -> {}", url, status);
    }
    let mut chunk = [0u8; 1024];
    loop {
        let n = resp.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

fn download_and_apply(repo: &str, layer: &Descriptor, token: &str) -> Result<()> {
    let repo_path = repo
        .strip_prefix("ghcr.io/")
        .ok_or_else(|| anyhow!("only ghcr.io is supported"))?;
    let url = format!("https://ghcr.io/v2/{}/blobs/{}", repo_path, layer.digest);
    let auth = format!("Bearer {}", token);

    let conn = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        follow_redirects_policy: FollowRedirectsPolicy::FollowAll,
        timeout: Some(Duration::from_secs(120)),
        buffer_size: Some(4096),
        ..Default::default()
    })?;
    let mut client = Client::wrap(conn);
    let headers = [
        ("authorization", auth.as_str()),
        ("accept", "application/octet-stream"),
    ];
    let req = client.request(Method::Get, &url, &headers)?;
    let mut resp = req.submit()?;
    if resp.status() != 200 {
        bail!("blob GET {} -> {}", url, resp.status());
    }

    let mut ota = EspOta::new().context("EspOta::new")?;
    let mut update = ota.initiate_update().context("initiate OTA update")?;

    let expected_sha_hex = layer
        .digest
        .strip_prefix("sha256:")
        .ok_or_else(|| anyhow!("non-sha256 digest: {}", layer.digest))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 4096];
    let mut total: u64 = 0;
    let mut next_log = 256u64 * 1024;
    loop {
        let n = resp.read(&mut buf).context("read blob chunk")?;
        if n == 0 {
            break;
        }
        update.write(&buf[..n]).context("OTA write")?;
        hasher.update(&buf[..n]);
        total += n as u64;
        if total >= next_log {
            tracing::info!(written = total, total = layer.size, "ota: download progress");
            next_log += 256 * 1024;
        }
    }
    if total != layer.size {
        update.abort().ok();
        bail!("blob size mismatch: got {}, expected {}", total, layer.size);
    }
    let actual_sha_hex = format!("{:x}", hasher.finalize());
    if actual_sha_hex != expected_sha_hex {
        update.abort().ok();
        bail!(
            "blob SHA mismatch: got {}, manifest says {}",
            actual_sha_hex,
            expected_sha_hex
        );
    }
    tracing::info!("ota: download complete, SHA verified, finalizing");
    update.complete().context("OTA complete (set boot partition)")?;
    Ok(())
}

fn read_string(nvs: &EspNvs<NvsDefault>, key: &str) -> Option<String> {
    let mut buf = [0u8; 96];
    match nvs.get_str(key, &mut buf) {
        Ok(Some(s)) => Some(s.to_string()),
        _ => None,
    }
}

fn write_string(nvs: &mut EspNvs<NvsDefault>, key: &str, value: &str) -> Result<()> {
    nvs.set_str(key, value)
        .with_context(|| format!("write NVS key {}", key))
}

/// On boot, before we connect Wi-Fi, check whether we're in pending-verify
/// state. Returns true if so (caller must run bringup checks and then call
/// `mark_valid_after_pending_verify_passed` or reboot to roll back).
pub fn is_pending_verify() -> bool {
    use esp_idf_svc::sys::*;
    unsafe {
        let part = esp_ota_get_running_partition();
        if part.is_null() {
            return false;
        }
        let mut state: esp_ota_img_states_t = 0;
        let err = esp_ota_get_state_partition(part, &mut state);
        if err != ESP_OK {
            tracing::warn!(err, "ota: esp_ota_get_state_partition failed");
            return false;
        }
        tracing::info!(
            state,
            name = ota_state_name(state),
            "ota: running partition state",
        );
        state == esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY
    }
}

/// Call after the post-OTA bringup checks passed: marks the running app
/// as valid (cancels rollback) and promotes pending_digest -> last_digest.
pub fn mark_valid_after_pending_verify_passed(
    nvs_partition: EspDefaultNvsPartition,
) -> Result<()> {
    use esp_idf_svc::sys::*;
    let err = unsafe { esp_ota_mark_app_valid_cancel_rollback() };
    if err != ESP_OK {
        bail!("esp_ota_mark_app_valid_cancel_rollback err={}", err);
    }
    tracing::info!("ota: marked app valid, rollback cancelled");

    let mut nvs = EspNvs::new(nvs_partition, NVS_NAMESPACE, true)
        .context("open ota NVS namespace")?;
    if let Some(pending) = read_string(&nvs, NVS_PENDING_DIGEST) {
        write_string(&mut nvs, NVS_LAST_DIGEST, &pending)?;
        // Best-effort clear of pending; not fatal if it fails.
        let _ = nvs.remove(NVS_PENDING_DIGEST);
        tracing::info!(digest = %pending, "ota: promoted pending -> last_digest");
    }
    Ok(())
}

fn ota_state_name(s: esp_idf_svc::sys::esp_ota_img_states_t) -> &'static str {
    use esp_idf_svc::sys::*;
    match s {
        s if s == esp_ota_img_states_t_ESP_OTA_IMG_NEW => "NEW",
        s if s == esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY => "PENDING_VERIFY",
        s if s == esp_ota_img_states_t_ESP_OTA_IMG_VALID => "VALID",
        s if s == esp_ota_img_states_t_ESP_OTA_IMG_INVALID => "INVALID",
        s if s == esp_ota_img_states_t_ESP_OTA_IMG_ABORTED => "ABORTED",
        s if s == esp_ota_img_states_t_ESP_OTA_IMG_UNDEFINED => "UNDEFINED",
        _ => "?",
    }
}

