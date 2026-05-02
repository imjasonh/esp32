# Device monitoring — plan

Periodically collect what the chip already knows about itself (heap,
stack, wifi, cpu, uptime) and ship it as **Cloud Monitoring time series**
via `monitoring.googleapis.com`. Same SA + key as cloud_log, broader
scope on the access token, separate POST path. Real metrics — chartable,
alertable — not log-based metrics.

External sensors (power, current, temperature) are out of scope for v1.

## What we collect

All from ESP-IDF APIs already linked in. No sdkconfig changes for v1.
Numeric only; categorical things (`reset_reason`, `wifi_ps_type`) stay
in `cloud_log` events where they belong.

| Metric type (custom.googleapis.com/esp32/…) | Source                                                  | Unit | Kind  |
|---------------------------------------------|---------------------------------------------------------|------|-------|
| `free_heap`                                 | `esp_get_free_heap_size()`                              | By   | GAUGE |
| `free_heap_internal`                        | `heap_caps_get_free_size(MALLOC_CAP_INTERNAL)`          | By   | GAUGE |
| `min_free_heap`                             | `esp_get_minimum_free_heap_size()` (water-mark / boot)  | By   | GAUGE |
| `largest_free_block`                        | `heap_caps_get_largest_free_block(MALLOC_CAP_DEFAULT)`  | By   | GAUGE |
| `stack_hwm` *(label `task`)*                | `uxTaskGetStackHighWaterMark` per known task            | By   | GAUGE |
| `wifi_rssi`                                 | `esp_wifi_sta_get_ap_info().rssi`                       | dBm  | GAUGE |
| `cpu_freq_mhz`                              | `esp_clk_cpu_freq()` / 1e6                              | MHz  | GAUGE |
| `uptime_secs`                               | `esp_timer_get_time() / 1_000_000`                      | s    | GAUGE |
| `boot_count`                                | NVS-persisted u32, ++'d at boot                         | 1    | GAUGE |
| `nvs_free_entries`                          | `nvs_get_stats().free_entries`                          | 1    | GAUGE |
| `cloud_log_queue_depth`                     | `LogQueue::len()` (new accessor)                        | 1    | GAUGE |
| `cloud_log_dropped_total`                   | running drop counter on `LogQueue`                      | 1    | GAUGE |

`stack_hwm` is one metric type with a `task` label (e.g.
`task=ota`, `task=cloud_log`, `task=metrics`, `task=main`) — keeps the
metric catalogue small and lets Cloud Monitoring chart all tasks on one
graph.

OTA-loop counters (`consecutive_failures`, `total_polls`,
`updates_applied`) belong to `src/ota.rs` and should be emitted from
there into the same metrics path — not reached into from `metrics.rs`.
Same goes for any future module-owned counters. `metrics.rs` only owns
chip-wide stats.

## Architecture

```
┌─────────────── ESP32 ───────────────┐                ┌──── GCP ────┐
│                                     │                │              │
│  cloud_log (existing) ──HTTPS──────────────────────▶│  Logging API │
│                              ▲                       │              │
│                              │ shares                │              │
│                         CachedToken                  │              │
│                       (logging+monitoring scopes)    │              │
│                              │                       │              │
│  metrics (new) ──HTTPS───────────────────────────────│  Monitoring  │
│  - sample every 5 min        │                       │  API         │
│  - one CreateTimeSeries POST │                       │              │
└──────────────────────────────┴─────┘                 └──────────────┘
```

The new component is one thread that wakes every N seconds, snapshots
~12 metrics, and POSTs a single `CreateTimeSeries` request. It piggy-
backs on cloud_log's access-token cache via a shared
`Arc<Mutex<CachedToken>>` — no second JWT mint, no second RSA sign.

## Auth

Same SA, same key as cloud_log. The JWT mint requests **both scopes**:

```
scope = "https://www.googleapis.com/auth/logging.write \
         https://www.googleapis.com/auth/monitoring.write"
```

(space-separated; Google accepts multi-scope JWTs and the resulting
access token works against both APIs.)

New IAM binding for the SA — least-privilege, only metric writes:

```bash
gcloud projects add-iam-policy-binding $PROJECT_ID \
    --member="serviceAccount:$SA_EMAIL" \
    --role="roles/monitoring.metricWriter"
```

`metricWriter` grants `monitoring.timeSeries.create` and metric-
descriptor create — nothing else. README "Optional: GCP Cloud Logging"
section grows to "Cloud Logging + Monitoring" with this extra binding.

## API call

`POST https://monitoring.googleapis.com/v3/projects/<id>/timeSeries`
with `Authorization: Bearer <access_token>` and:

```json
{
  "timeSeries": [
    {
      "metric": {
        "type": "custom.googleapis.com/esp32/free_heap"
      },
      "resource": {
        "type": "generic_node",
        "labels": {
          "project_id": "<project-id>",
          "location":   "global",
          "namespace":  "esp32",
          "node_id":    "<chip-mac>"
        }
      },
      "metricKind": "GAUGE",
      "valueType":  "INT64",
      "points": [
        { "interval": { "endTime": "2026-05-02T22:30:00Z" },
          "value":    { "int64Value": "182456" } }
      ]
    },
    {
      "metric": {
        "type":   "custom.googleapis.com/esp32/stack_hwm",
        "labels": { "task": "ota" }
      },
      "resource": { "type": "generic_node", "labels": { ... } },
      "metricKind": "GAUGE",
      "valueType":  "INT64",
      "points":     [ ... ]
    }
    // ... one entry per metric per task-label combination
  ]
}
```

Cloud Monitoring auto-creates `MetricDescriptor`s on first write — no
separate provisioning step. Up to 200 series per request; we have ~15
per snapshot, so one POST per cycle.

## On-device implementation

### Shared `gcp_auth` (refactor)

Move JWT mint + `CachedToken` out of `src/cloud_log.rs` into a small
`src/gcp_auth.rs`. Owns the SA key, mints a multi-scope token,
exposes `Arc<Mutex<CachedToken>>` to both `cloud_log` and `metrics`.
First caller through the door does the RSA sign; the other gets the
cached token for free.

### `src/metrics.rs` (new)

```rust
pub fn run(
    cfg: GcpConfig,
    auth: gcp_auth::TokenHandle,
    queue_stats: cloud_log::QueueStatsHandle,
) -> ! {
    let mac = device_mac();
    let resource = generic_node_resource(&cfg.project_id, &mac);
    let nvs_handle = open_nvs_for_stats();
    let task_handles = TaskHandles::collect(); // ota, cloud_log, metrics, main
    loop {
        std::thread::sleep(cfg.metrics_interval);
        let snapshot = collect(&task_handles, &nvs_handle, &queue_stats);
        let body = build_create_time_series(&resource, &snapshot, now_rfc3339());
        let bearer = auth.get_or_refresh()?;
        post_create_time_series(&cfg.project_id, &bearer, &body);
    }
}
```

Stack budget ~16 KB (HTTPS + JSON, no RSA on the hot path since auth
is cached). Spawned from `main.rs` only if `gcp.is_some()`.

### Boot count

Persisted in the existing `ota` NVS namespace as `boot_count` (u32);
incremented once at startup before any thread spawns. Doubles as a
useful number for OTA debugging too ("how many times has this device
rebooted in this slot?").

### Task handles for stack hwm

Each spawned thread publishes its `TaskHandle_t` into a `OnceLock` in
its own module:

```rust
pub static TASK_HANDLE: OnceLock<*mut c_void> = OnceLock::new();
fn run(...) -> ! {
    let _ = TASK_HANDLE.set(unsafe { xTaskGetCurrentTaskHandle() } as _);
    ...
}
```

`metrics::collect()` reads each handle (skipping any unset), calls
`uxTaskGetStackHighWaterMark` per. Missing → field omitted, not zeroed.

## Provisioning (NVS)

One new optional key in the existing `gcp` namespace:

```
ns      key                       type   notes
─────────────────────────────────────────────────────────────────
gcp     metrics_interval_secs     u32    default 300; 0 disables
```

`provisioning.toml`:

```toml
[gcp]
# ... existing fields ...
metrics_interval_secs = 300   # optional; default 300, 0 = off
```

Metrics thread only spawns if `gcp.is_some()`. If
`metrics_interval_secs = 0` the module is loaded but the thread
isn't spawned.

## Concerns

1. **Cloud Monitoring quotas + cost.** Custom metrics are billed at
   $0.30/MiB/month after the free tier (150 MiB/month). 15 series ×
   1 sample / 5 min × 30 days ≈ 130 K samples/device/month, well
   under the per-project series cap. Tunable via
   `metrics_interval_secs`.

2. **Reset detection on `boot_count`.** With GAUGE we just snapshot
   the current value; Cloud Monitoring doesn't try to interpret it as
   a rate. If we ever want "boots per hour" we'd switch to CUMULATIVE
   and provide `startTime`. Same for `dropped_total`.

3. **Auto-created descriptors are sticky.** Once Cloud Monitoring
   sees a custom metric, the descriptor sticks (with whatever label
   keys we sent first). Renaming `task=ota` → `task=ota_loop` later
   would create a new descriptor. Keep label vocab stable.

4. **`metrics_interval_secs = 0` should mean off, not zero-sleep.**
   Defensive check at thread spawn — don't enter a spinloop.

5. **Time-series clock skew.** `endTime` must be ≥ chip's NTP-synced
   wall clock. We already wait for NTP before spawning cloud_log;
   metrics gates on the same precondition.

## Settled

- **Cloud Monitoring API**, not log-based metrics.
- **Same SA, multi-scope JWT** — single access token covers logging +
  monitoring; cached and shared between threads.
- **GAUGE for all v1 metrics**, including the running totals — keeps
  the descriptor catalogue simple. Promote to CUMULATIVE only if
  needed for rate-style alerting.
- **Custom metric prefix `custom.googleapis.com/esp32/<name>`** with
  the `task` label for per-task fanout.
- **Auto-create descriptors** — no separate provisioning step on the
  GCP side; first POST creates the schema.
- **`generic_node` resource** with the same labels as cloud_log
  (consistent fleet identity).
- **Module owns its counters.** `metrics.rs` collects chip-wide
  stats only. OTA / cloud_log emit their own module-specific metrics
  via the same path. No cross-module reach-arounds.

## Future work (not in v1)

- **Per-task CPU runtime stats** via `vTaskGetRunTimeStats`. Needs
  `CONFIG_FREERTOS_USE_TRACE_FACILITY=y` +
  `CONFIG_FREERTOS_GENERATE_RUN_TIME_STATS=y`. Adds CPU% per task as
  another `stack_hwm`-style multi-task metric.
- **Light-sleep stats.** Once `esp_pm_*` is enabled, expose
  time-in-light-sleep as a CUMULATIVE metric. Pairs with the
  modem-sleep / power-saving work.
- **External power telemetry.** INA219/INA226 over I2C → bus voltage,
  current, instantaneous power as proper metrics. Worth its own plan.
- **Pre-declared MetricDescriptors with units + descriptions** —
  improves the Cloud Monitoring UI but not necessary for charting.
- **Alerting policies** as Terraform / `gcloud alpha monitoring`
  resources, checked into the repo so they version with the metrics
  themselves.
- **Cumulative metrics with `startTime`** for per-second rate
  aggregation in Cloud Monitoring (drops/sec, polls/sec, …).
