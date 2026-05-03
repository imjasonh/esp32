//! Pure-Rust algorithms shared between the firmware and the host-side
//! Antithesis test crate (`tools/antithesis/`). Anything in here MUST
//! NOT depend on `esp_idf_svc` or any other ESP-IDF-only crate — the
//! test crate includes this file verbatim via `#[path]` and builds for
//! the host triple.

use std::time::Duration;

/// Maximum sleep between OTA polls when backing off after failures.
pub const BACKOFF_CAP: Duration = Duration::from_secs(3600);

/// DSSE Pre-Authentication Encoding (https://github.com/secure-systems-lab/dsse).
/// PAE("DSSEv1", payloadType, payload) = "DSSEv1 <len(t)> <t> <len(p)> <p>"
pub fn pae_dsse_v1(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + payload_type.len() + payload.len());
    out.extend_from_slice(b"DSSEv1 ");
    out.extend_from_slice(payload_type.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload_type.as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload.len().to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(payload);
    out
}

/// Apply ±10% jitter to `base` using a caller-supplied 32-bit random
/// value (the firmware passes `esp_random()`, tests pass deterministic
/// values from the Antithesis SDK). Result is never less than 1s.
pub fn apply_jitter(base: Duration, rand: u32) -> Duration {
    let pct = (rand % 21) as i32 - 10; // -10..=+10
    let secs = base.as_secs() as i64;
    let delta = (secs * pct as i64) / 100;
    let new_secs = (secs + delta).max(1) as u64;
    Duration::from_secs(new_secs)
}

/// Exponential backoff capped at `BACKOFF_CAP`, with ±10% jitter
/// applied. `failures` is the consecutive failure count
/// (`failures == 0` is the success path; callers typically don't take
/// that route through here).
pub fn backoff_with_jitter(base: Duration, failures: u32, rand: u32) -> Duration {
    // 2^10 = 1024x is plenty; the cap will be hit far earlier in practice.
    let exp = failures.min(10);
    let multiplied = base.saturating_mul(1u32 << exp);
    let bounded = multiplied.min(BACKOFF_CAP);
    apply_jitter(bounded, rand)
}
