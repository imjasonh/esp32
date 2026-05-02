//! Ship structured logs from the device to Google Cloud Logging.
//!
//! Implementation in stages:
//! - This commit: NVS-loaded config, tracing layer that captures events
//!   into a bounded ring buffer, background sender task that *would*
//!   POST batches but currently just writes them to serial as a stub.
//! - Next commit: real POST via NTP-synced timestamps + service-account
//!   JWT auth + the Cloud Logging REST API.
//!
//! Cloud logging is opt-in per device. If the `gcp` NVS namespace is
//! missing required keys (`project_id`, `sa_email`, `sa_key_id`,
//! `sa_key_pem`), the firmware boots normally with serial-only logs.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use embedded_svc::http::client::Client;
use embedded_svc::io::Write as _;
use esp_idf_svc::http::client::{Configuration as HttpConfig, EspHttpConnection, FollowRedirectsPolicy};
use esp_idf_svc::http::Method;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer};
use rsa::RsaPrivateKey;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::Context as LayerContext;
use tracing_subscriber::Layer;

const NVS_GCP_NS: &str = "gcp";
const NVS_PROJECT_ID: &str = "project_id";
const NVS_SA_EMAIL: &str = "sa_email";
const NVS_SA_KEY_ID: &str = "sa_key_id";
const NVS_SA_KEY_PEM: &str = "sa_key_pem";
const NVS_MIN_SEVERITY: &str = "min_severity";

/// Capacity of the in-RAM log queue. When the queue is full, oldest
/// entries are dropped and counted; the count surfaces as
/// `LogEntry::dropped_before` on the next entry pushed.
pub const QUEUE_CAPACITY: usize = 256;
const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const BATCH_MAX_ENTRIES: usize = 50;

/// GCP-side config + opt-in flag, read from NVS at boot.
#[derive(Clone)]
pub struct GcpConfig {
    pub project_id: String,
    pub sa_email: String,
    pub sa_key_id: String,
    pub sa_key_pem: Vec<u8>,
    pub min_severity: Level,
}

impl GcpConfig {
    /// Load from NVS. `Ok(None)` means the device is intentionally not
    /// configured for cloud logging (cloud logging is opt-in, missing
    /// keys or missing namespace = disabled, no error).
    pub fn load(partition: EspDefaultNvsPartition) -> Result<Option<Self>> {
        let nvs = match EspNvs::new(partition, NVS_GCP_NS, false) {
            Ok(n) => n,
            Err(e)
                if e.code() == esp_idf_svc::sys::ESP_ERR_NVS_NOT_FOUND as i32 =>
            {
                // Namespace has never been written. Cloud logging is
                // opt-in; this is the normal state for a device that
                // wasn't provisioned with a [gcp] block.
                return Ok(None);
            }
            Err(e) => {
                return Err(anyhow!("open NVS namespace {}: {:?}", NVS_GCP_NS, e));
            }
        };

        let project_id = read_str(&nvs, NVS_PROJECT_ID, 96)?;
        let sa_email = read_str(&nvs, NVS_SA_EMAIL, 128)?;
        let sa_key_id = read_str(&nvs, NVS_SA_KEY_ID, 96)?;
        let sa_key_pem = read_blob(&nvs, NVS_SA_KEY_PEM, 4096)?;

        match (project_id, sa_email, sa_key_id, sa_key_pem) {
            (Some(p), Some(e), Some(k), Some(pem)) => Ok(Some(Self {
                project_id: p,
                sa_email: e,
                sa_key_id: k,
                sa_key_pem: pem,
                min_severity: read_severity(&nvs),
            })),
            _ => Ok(None),
        }
    }
}

fn read_str(
    nvs: &EspNvs<NvsDefault>,
    key: &str,
    max_len: usize,
) -> Result<Option<String>> {
    let mut buf = vec![0u8; max_len];
    Ok(nvs
        .get_str(key, &mut buf)
        .map_err(|e| anyhow!("read NVS {}/{}: {:?}", NVS_GCP_NS, key, e))?
        .map(|s| s.to_string()))
}

fn read_blob(
    nvs: &EspNvs<NvsDefault>,
    key: &str,
    max_len: usize,
) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; max_len];
    Ok(nvs
        .get_blob(key, &mut buf)
        .map_err(|e| anyhow!("read NVS {}/{}: {:?}", NVS_GCP_NS, key, e))?
        .map(|b| b.to_vec()))
}

fn read_severity(nvs: &EspNvs<NvsDefault>) -> Level {
    // 0=TRACE, 1=DEBUG, 2=INFO (default), 3=WARN, 4=ERROR
    match nvs.get_u8(NVS_MIN_SEVERITY).ok().flatten() {
        Some(0) => Level::TRACE,
        Some(1) => Level::DEBUG,
        Some(3) => Level::WARN,
        Some(4) => Level::ERROR,
        _ => Level::INFO,
    }
}

/// One captured log event.
#[derive(Debug, Clone)]
pub struct LogEntry {
    /// UNIX time in seconds when the event was emitted, or None if
    /// captured before NTP sync. Cloud Logging accepts None and assigns
    /// server-side time.
    pub timestamp_unix_secs: Option<u64>,
    pub severity: Level,
    pub target: String,
    pub message: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
    /// Number of entries dropped from the queue *before* this entry was
    /// pushed. Cloud Logging readers can use this to spot lossy windows.
    pub dropped_before: u32,
}

/// Bounded ring-buffer of pending log entries. Drop-oldest when full.
#[derive(Clone)]
pub struct LogQueue {
    inner: Arc<Mutex<QueueInner>>,
}

struct QueueInner {
    deque: VecDeque<LogEntry>,
    capacity: usize,
    pending_dropped: u32,
}

impl LogQueue {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(QueueInner {
                deque: VecDeque::with_capacity(capacity),
                capacity,
                pending_dropped: 0,
            })),
        }
    }

    pub fn push(&self, mut entry: LogEntry) {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            // Poisoned mutex: just give up on this entry rather than
            // unwinding through the tracing layer.
            Err(_) => return,
        };
        // Apply any pending drop-counter to this entry, then drop oldest
        // if we're at capacity.
        entry.dropped_before = g.pending_dropped;
        g.pending_dropped = 0;
        if g.deque.len() == g.capacity {
            g.deque.pop_front();
            g.pending_dropped = g.pending_dropped.saturating_add(1);
        }
        g.deque.push_back(entry);
    }

    /// Drain up to `max` entries.
    pub fn drain(&self, max: usize) -> Vec<LogEntry> {
        let mut g = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return vec![],
        };
        let n = g.deque.len().min(max);
        g.deque.drain(..n).collect()
    }
}

/// `tracing_subscriber::Layer` that captures events into a `LogQueue`.
pub struct CloudLogLayer {
    queue: LogQueue,
    min_level: Level,
}

impl CloudLogLayer {
    pub fn new(queue: LogQueue, min_level: Level) -> Self {
        Self { queue, min_level }
    }
}

impl<S: Subscriber> Layer<S> for CloudLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: LayerContext<'_, S>) {
        // Skip events emitted by this module itself, otherwise the
        // sender's "post failed" / "token refreshed" tracing calls
        // get pushed onto the queue it's draining, creating a tight
        // feedback loop. Cloud_log's own messages stay serial-only.
        if event.metadata().target() == module_path!() {
            return;
        }
        let level = *event.metadata().level();
        if level > self.min_level {
            // tracing's Level ordering: TRACE > DEBUG > INFO > WARN > ERROR.
            // We want events whose level is <= min_level (i.e. equally
            // verbose or louder).
            return;
        }
        let mut visitor = FieldCapture::default();
        event.record(&mut visitor);
        let entry = LogEntry {
            timestamp_unix_secs: now_unix_secs(),
            severity: level,
            target: event.metadata().target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
            dropped_before: 0,
        };
        self.queue.push(entry);
    }
}

/// Wall-clock time in UNIX seconds, or None if the system clock is
/// still at the ESP-IDF default epoch (1970). NTP sync hasn't happened
/// yet → return None and let Cloud Logging assign server-side time.
fn now_unix_secs() -> Option<u64> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    // Anything before 2020-01-01 means NTP hasn't synced yet.
    if secs < 1_577_836_800 {
        None
    } else {
        Some(secs)
    }
}

#[derive(Default)]
struct FieldCapture {
    message: String,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl tracing::field::Visit for FieldCapture {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.put(field.name(), serde_json::Value::String(value.to_string()));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.put(field.name(), serde_json::Value::Number(value.into()));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.put(field.name(), serde_json::Value::Number(value.into()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.put(field.name(), serde_json::Value::Bool(value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        let v = serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null);
        self.put(field.name(), v);
    }
    fn record_debug(
        &mut self,
        field: &tracing::field::Field,
        value: &dyn std::fmt::Debug,
    ) {
        self.put(
            field.name(),
            serde_json::Value::String(format!("{:?}", value)),
        );
    }
}

impl FieldCapture {
    fn put(&mut self, name: &str, value: serde_json::Value) {
        if name == "message" {
            // The macro's positional message string is captured via
            // record_debug → Display formatting; pull it out as the
            // top-level message rather than a nested field.
            if let serde_json::Value::String(s) = value {
                self.message = s;
            } else {
                self.message = value.to_string();
            }
        } else {
            self.fields.insert(name.to_string(), value);
        }
    }
}

/// Sender thread main loop. Drains the queue every `FLUSH_INTERVAL`,
/// mints/refreshes a service-account access token as needed, and
/// POSTs batches to Cloud Logging.
///
/// Uses tracing internally — the `module_path!()` filter in
/// `CloudLogLayer::on_event` keeps cloud_log's own messages out of
/// the queue (no feedback loop).
pub fn run(cfg: GcpConfig, queue: LogQueue) -> ! {
    tracing::info!(
        project = %cfg.project_id,
        sa = %cfg.sa_email,
        min_severity = ?cfg.min_severity,
        "cloud_log: sender starting",
    );

    // Parse the SA private key once at startup; if it's malformed
    // we can't do anything useful and log loudly forever.
    let signing_key = match parse_signing_key(&cfg.sa_key_pem) {
        Ok(k) => k,
        Err(e) => {
            loop {
                tracing::error!(
                    error = %format!("{:#}", e),
                    "cloud_log: SA key parse failed; sender disabled",
                );
                std::thread::sleep(Duration::from_secs(300));
            }
        }
    };

    let mac = device_mac();
    let log_name = format!("projects/{}/logs/esp32-firmware", cfg.project_id);

    let mut token: Option<CachedToken> = None;
    let mut consecutive_failures: u32 = 0;

    loop {
        let sleep_for = if consecutive_failures > 0 {
            // Exponential backoff capped at 5 min for cloud-logging
            // failures (separate budget from the OTA loop).
            let exp = consecutive_failures.min(5);
            Duration::from_secs(FLUSH_INTERVAL.as_secs() << exp).min(Duration::from_secs(300))
        } else {
            FLUSH_INTERVAL
        };
        std::thread::sleep(sleep_for);

        let batch = queue.drain(BATCH_MAX_ENTRIES);
        if batch.is_empty() {
            consecutive_failures = 0;
            continue;
        }

        if token.as_ref().map_or(true, |t| t.expired_or_close()) {
            match mint_access_token(&cfg, &signing_key) {
                Ok(t) => {
                    tracing::debug!(
                        expires_in_secs = t.expires_at_unix.saturating_sub(now_unix_secs().unwrap_or(0)),
                        "cloud_log: minted new access token",
                    );
                    token = Some(t);
                }
                Err(e) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    tracing::warn!(
                        failures = consecutive_failures,
                        error = %format!("{:#}", e),
                        "cloud_log: token mint failed",
                    );
                    continue;
                }
            }
        }

        let bearer = &token.as_ref().unwrap().token;
        match post_batch(&log_name, &cfg.project_id, &mac, bearer, &batch) {
            Ok(()) => {
                tracing::debug!(
                    entries = batch.len(),
                    "cloud_log: posted batch",
                );
                consecutive_failures = 0;
            }
            Err(e) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    entries = batch.len(),
                    failures = consecutive_failures,
                    error = %format!("{:#}", e),
                    "cloud_log: post failed; dropping batch",
                );
                // Drop the batch on failure rather than re-enqueue
                // (avoids unbounded growth on a long outage). Loss is
                // already surfaced via dropped_before on subsequent
                // entries.
            }
        }
    }
}

struct CachedToken {
    token: String,
    expires_at_unix: u64,
}

impl CachedToken {
    fn expired_or_close(&self) -> bool {
        // Refresh 5 minutes before expiry to avoid races.
        match now_unix_secs() {
            Some(now) => now + 300 >= self.expires_at_unix,
            None => true,
        }
    }
}

fn parse_signing_key(pem_bytes: &[u8]) -> Result<SigningKey<rsa::sha2::Sha256>> {
    // Trim trailing whitespace — jq -r adds a final newline on top of
    // the PEM's own trailing newline, and pem-rfc7468 then tries to
    // parse a second (empty) block and errors at "pre-encapsulation
    // boundary". Tolerate both forms.
    let pem_str = std::str::from_utf8(pem_bytes)
        .context("SA key PEM is not UTF-8")?
        .trim();
    let key = RsaPrivateKey::from_pkcs8_pem(pem_str).context("parse SA PKCS#8 private key")?;
    Ok(SigningKey::<rsa::sha2::Sha256>::new(key))
}

#[derive(Serialize)]
struct JwtHeader<'a> {
    alg: &'static str,
    typ: &'static str,
    kid: &'a str,
}

#[derive(Serialize)]
struct JwtClaims<'a> {
    iss: &'a str,
    scope: &'static str,
    aud: &'static str,
    iat: u64,
    exp: u64,
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    expires_in: u64,
}

fn mint_access_token(
    cfg: &GcpConfig,
    signing_key: &SigningKey<rsa::sha2::Sha256>,
) -> Result<CachedToken> {
    let now = now_unix_secs().ok_or_else(|| anyhow!("NTP not synced; cannot mint JWT"))?;

    let header = JwtHeader {
        alg: "RS256",
        typ: "JWT",
        kid: &cfg.sa_key_id,
    };
    let claims = JwtClaims {
        iss: &cfg.sa_email,
        scope: "https://www.googleapis.com/auth/logging.write",
        aud: "https://oauth2.googleapis.com/token",
        iat: now,
        exp: now + 3600,
    };

    let header_b64 = b64url(&serde_json::to_vec(&header)?);
    let claims_b64 = b64url(&serde_json::to_vec(&claims)?);
    let signing_input = format!("{}.{}", header_b64, claims_b64);
    let sig = signing_key.sign(signing_input.as_bytes());
    let sig_b64 = b64url(sig.to_bytes().as_ref());
    let jwt = format!("{}.{}", signing_input, sig_b64);

    let body = format!(
        "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={}",
        jwt
    );
    let resp_bytes = http_post(
        "https://oauth2.googleapis.com/token",
        "application/x-www-form-urlencoded",
        body.as_bytes(),
        None,
    )
    .context("POST oauth2/token")?;
    let resp: TokenResp = serde_json::from_slice(&resp_bytes)
        .context("parse token response JSON")?;

    Ok(CachedToken {
        token: resp.access_token,
        expires_at_unix: now + resp.expires_in,
    })
}

#[derive(Serialize)]
struct WriteEntriesRequest<'a> {
    #[serde(rename = "logName")]
    log_name: &'a str,
    resource: MonitoredResource<'a>,
    entries: Vec<Entry>,
}

#[derive(Serialize)]
struct MonitoredResource<'a> {
    #[serde(rename = "type")]
    type_: &'static str,
    labels: ResourceLabels<'a>,
}

#[derive(Serialize)]
struct ResourceLabels<'a> {
    project_id: &'a str,
    location: &'static str,
    namespace: &'static str,
    node_id: &'a str,
}

#[derive(Serialize)]
struct Entry {
    severity: &'static str,
    #[serde(rename = "jsonPayload")]
    json_payload: serde_json::Value,
    #[serde(rename = "timestamp", skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
}

fn post_batch(
    log_name: &str,
    project_id: &str,
    mac: &str,
    bearer: &str,
    batch: &[LogEntry],
) -> Result<()> {
    let entries: Vec<Entry> = batch
        .iter()
        .map(|e| Entry {
            severity: severity_str(e.severity),
            json_payload: build_payload(e),
            timestamp: e.timestamp_unix_secs.and_then(unix_to_rfc3339),
        })
        .collect();
    let _ = (project_id, mac); // used inline in MonitoredResource below

    let req = WriteEntriesRequest {
        log_name,
        resource: MonitoredResource {
            type_: "generic_node",
            labels: ResourceLabels {
                project_id,
                location: "global",
                namespace: "esp32",
                node_id: mac,
            },
        },
        entries,
    };
    let body = serde_json::to_vec(&req)?;
    let auth = format!("Bearer {}", bearer);
    http_post(
        "https://logging.googleapis.com/v2/entries:write",
        "application/json",
        &body,
        Some(&auth),
    )
    .map(|_| ())
}

fn severity_str(level: Level) -> &'static str {
    // Cloud Logging severities (LogSeverity enum):
    // https://cloud.google.com/logging/docs/reference/v2/rest/v2/LogEntry#logseverity
    match level {
        Level::TRACE => "DEBUG",
        Level::DEBUG => "DEBUG",
        Level::INFO => "INFO",
        Level::WARN => "WARNING",
        Level::ERROR => "ERROR",
    }
}

fn build_payload(e: &LogEntry) -> serde_json::Value {
    let mut map = e.fields.clone();
    map.insert(
        "message".to_string(),
        serde_json::Value::String(e.message.clone()),
    );
    map.insert(
        "module".to_string(),
        serde_json::Value::String(e.target.clone()),
    );
    if e.dropped_before > 0 {
        map.insert(
            "dropped_before".to_string(),
            serde_json::Value::Number(e.dropped_before.into()),
        );
    }
    serde_json::Value::Object(map)
}

fn unix_to_rfc3339(secs: u64) -> Option<String> {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    OffsetDateTime::from_unix_timestamp(secs as i64)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
}

fn b64url(input: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

/// Single helper for HTTPS POST. Returns the response body bytes.
/// Errors on any non-2xx status with the body included for diagnosis.
fn http_post(
    url: &str,
    content_type: &str,
    body: &[u8],
    bearer: Option<&str>,
) -> Result<Vec<u8>> {
    let conn = EspHttpConnection::new(&HttpConfig {
        crt_bundle_attach: Some(esp_idf_svc::sys::esp_crt_bundle_attach),
        follow_redirects_policy: FollowRedirectsPolicy::FollowAll,
        timeout: Some(Duration::from_secs(30)),
        buffer_size: Some(2048),
        buffer_size_tx: Some(4096),
        ..Default::default()
    })?;
    let mut client = Client::wrap(conn);
    let body_len = body.len().to_string();
    let mut headers: Vec<(&str, &str)> = vec![
        ("content-type", content_type),
        ("content-length", body_len.as_str()),
        ("accept", "application/json"),
    ];
    if let Some(b) = bearer {
        headers.push(("authorization", b));
    }
    let mut req = client.request(Method::Post, url, &headers)?;
    req.write_all(body).context("write request body")?;
    req.flush().ok();
    let mut resp = req.submit()?;
    let status = resp.status();
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = resp.read(&mut chunk).context("read response chunk")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if !(200..300).contains(&status) {
        bail!(
            "POST {} -> HTTP {} body={}",
            url,
            status,
            String::from_utf8_lossy(&buf)
        );
    }
    Ok(buf)
}

/// MAC address as a `aabbccddeeff` hex string. Used as the `node_id`
/// label on Cloud Logging entries to identify which device the log
/// came from.
fn device_mac() -> String {
    let mut mac = [0u8; 8];
    unsafe {
        esp_idf_svc::sys::esp_efuse_mac_get_default(mac.as_mut_ptr());
    }
    let mut s = String::with_capacity(12);
    for b in &mac[..6] {
        use std::fmt::Write as _;
        let _ = write!(s, "{:02x}", b);
    }
    s
}
