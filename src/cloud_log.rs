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

use anyhow::{anyhow, Result};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
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
    /// keys = disabled, no error).
    pub fn load(partition: EspDefaultNvsPartition) -> Result<Option<Self>> {
        let nvs = EspNvs::new(partition, NVS_GCP_NS, false)
            .map_err(|e| anyhow!("open NVS namespace {}: {:?}", NVS_GCP_NS, e))?;

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

/// Background task. Periodically drains the queue and (in this stub)
/// logs each entry to serial showing what it WOULD send. The real POST
/// to Cloud Logging is in a follow-up commit.
pub fn run(cfg: GcpConfig, queue: LogQueue) -> ! {
    // Use eprintln rather than tracing here to avoid feedback loops
    // (this thread emitting tracing events that the layer would push
    // back onto the queue).
    eprintln!(
        "cloud_log: stub sender starting (project={}, sa={}, key_id={}, key_pem_bytes={}, min_severity={:?})",
        cfg.project_id,
        cfg.sa_email,
        cfg.sa_key_id,
        cfg.sa_key_pem.len(),
        cfg.min_severity
    );
    loop {
        std::thread::sleep(FLUSH_INTERVAL);
        let batch = queue.drain(BATCH_MAX_ENTRIES);
        if batch.is_empty() {
            continue;
        }
        eprintln!(
            "cloud_log: would POST batch of {} entries to projects/{}/logs/esp32-firmware",
            batch.len(),
            cfg.project_id
        );
        for entry in &batch {
            // One-line summary per entry.
            let ts = entry
                .timestamp_unix_secs
                .map(|s| s.to_string())
                .unwrap_or_else(|| "<no-ntp>".into());
            eprintln!(
                "  [{}] {:?} {} {}{}{}",
                ts,
                entry.severity,
                entry.target,
                entry.message,
                if entry.fields.is_empty() {
                    String::new()
                } else {
                    format!(
                        " {}",
                        serde_json::to_string(&entry.fields).unwrap_or_default()
                    )
                },
                if entry.dropped_before > 0 {
                    format!(" (dropped {} before)", entry.dropped_before)
                } else {
                    String::new()
                },
            );
        }
    }
}
