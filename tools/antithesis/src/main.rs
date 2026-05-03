//! Antithesis property tests for the firmware's pure-Rust algorithms.
//!
//! This binary is the host-side companion to `src/algos.rs` — it
//! includes that file verbatim via `#[path]` (so refactors stay in
//! lockstep) and exercises each algorithm with structured randomness
//! from the Antithesis SDK. Properties are checked with both
//! `assert_always!` / `assert_sometimes!` (visible to the Antithesis
//! simulator) and a plain `panic!` so violations also fail `cargo
//! run` locally and in CI.
//!
//! Locally:
//!   cargo run --release --target <host-triple>
//!
//! On the Antithesis platform the same binary runs inside the
//! deterministic simulator and the SDK takes over from the local
//! fallbacks. See https://antithesis.com/docs/using_antithesis/sdk/rust/

use antithesis_sdk::{antithesis_init, assert_always, assert_sometimes, random};
use serde_json::json;
use std::time::Duration;

#[path = "../../../src/algos.rs"]
mod algos;

/// How many random iterations each property test runs locally. On
/// Antithesis the simulator drives the harness for far longer, so this
/// only governs the `cargo run` baseline.
const ITERS: usize = 2_000;

/// Wraps `assert_always!` so a violation also panics — keeps the
/// Antithesis-platform reporting AND makes local / CI runs exit
/// non-zero on bugs (the SDK's local fallback is silent by default).
macro_rules! check {
    ($cond:expr, $msg:literal, $details:expr $(,)?) => {{
        let cond: bool = $cond;
        let details = $details;
        assert_always!(cond, $msg, &details);
        if !cond {
            panic!(
                "[antithesis-tests] invariant violated: {} details={}",
                $msg, details,
            );
        }
    }};
}

fn main() {
    antithesis_init();
    eprintln!("antithesis-tests: starting {} iterations per property", ITERS);

    test_pae_dsse_v1();
    test_apply_jitter();
    test_backoff_with_jitter();

    eprintln!("antithesis-tests: all properties held");
}

// ---------------------------------------------------------------------------
// pae_dsse_v1
// ---------------------------------------------------------------------------

fn test_pae_dsse_v1() {
    eprintln!("antithesis-tests: pae_dsse_v1");

    // A handful of payload-type strings that mirror the real DSSE
    // envelope types the firmware sees in the wild plus some odd
    // edge cases (empty, ASCII-with-spaces, multibyte UTF-8).
    let payload_types = [
        "application/vnd.in-toto+json",
        "application/json",
        "",
        "x",
        "type with spaces",
        "tïpe-with-utf8-✓",
    ];

    for _ in 0..ITERS {
        let pt = *random::random_choice(&payload_types).unwrap_or(&"");
        let payload_len = (random::get_random() % 4096) as usize;
        let payload: Vec<u8> = (0..payload_len)
            .map(|i| (random::get_random() ^ (i as u64)) as u8)
            .collect();

        let pae = algos::pae_dsse_v1(pt, &payload);

        // Header is the literal "DSSEv1 " followed by the payload-type
        // length in ASCII decimal. Anything else means we broke the
        // PAE format and signatures will silently mis-verify.
        check!(
            pae.starts_with(b"DSSEv1 "),
            "pae_dsse_v1: starts with DSSEv1 prefix",
            json!({"payload_type_len": pt.len(), "payload_len": payload.len()}),
        );

        // The trailing payload bytes are appended verbatim (no escaping).
        check!(
            pae.ends_with(&payload),
            "pae_dsse_v1: ends with raw payload",
            json!({"payload_type_len": pt.len(), "payload_len": payload.len()}),
        );

        // PAE is "DSSEv1 <len(pt)> <pt> <len(payload)> <payload>". Total
        // length is the sum of fixed prefix + decimal-rendered lengths
        // + the two values + 3 separator spaces (the leading
        // "DSSEv1 " contributes its own trailing space, hence +3 not +4).
        let expected_len = "DSSEv1 ".len()
            + pt.len().to_string().len()
            + 1
            + pt.len()
            + 1
            + payload.len().to_string().len()
            + 1
            + payload.len();
        check!(
            pae.len() == expected_len,
            "pae_dsse_v1: total length matches spec",
            json!({"got": pae.len(), "expected": expected_len}),
        );

        // Idempotent: the function is pure and must produce identical
        // output on a second call with the same inputs.
        let pae2 = algos::pae_dsse_v1(pt, &payload);
        check!(
            pae == pae2,
            "pae_dsse_v1: deterministic",
            json!({"len": pae.len()}),
        );

        // We know the format, so we can cheaply re-parse the type/len
        // fields and confirm they round-trip. Catches off-by-one in
        // the separator placement.
        let parsed = parse_pae(&pae);
        check!(
            parsed == Some((pt.as_bytes(), payload.as_slice())),
            "pae_dsse_v1: round-trip parses back to inputs",
            json!({"payload_type": pt, "payload_len": payload.len()}),
        );
    }
}

/// Parse a PAE blob back into `(payload_type, payload)` if it follows
/// the format. Used only by the test harness, kept here on purpose so
/// the test isn't tautologically using the same code path as the
/// implementation under test.
fn parse_pae(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = buf.strip_prefix(b"DSSEv1 ")?;
    let sp1 = rest.iter().position(|&b| b == b' ')?;
    let (pt_len_bytes, rest) = rest.split_at(sp1);
    let pt_len: usize = std::str::from_utf8(pt_len_bytes).ok()?.parse().ok()?;
    let rest = &rest[1..]; // skip space
    if rest.len() < pt_len + 1 {
        return None;
    }
    let (pt, rest) = rest.split_at(pt_len);
    let rest = &rest[1..]; // skip space after pt
    let sp2 = rest.iter().position(|&b| b == b' ')?;
    let (pl_len_bytes, rest) = rest.split_at(sp2);
    let pl_len: usize = std::str::from_utf8(pl_len_bytes).ok()?.parse().ok()?;
    let rest = &rest[1..]; // skip space
    if rest.len() != pl_len {
        return None;
    }
    Some((pt, rest))
}

// ---------------------------------------------------------------------------
// apply_jitter
// ---------------------------------------------------------------------------

fn test_apply_jitter() {
    eprintln!("antithesis-tests: apply_jitter");

    // Bases ranging from "tiny" (where the `.max(1)` clamp matters
    // most) to "OTA cap" (3600s) and beyond.
    let bases = [1u64, 5, 10, 60, 600, 3600, 7200];

    for _ in 0..ITERS {
        let base_secs = *random::random_choice(&bases).unwrap_or(&60);
        let rand = random::get_random() as u32;
        let base = Duration::from_secs(base_secs);
        let jittered = algos::apply_jitter(base, rand);
        let result_secs = jittered.as_secs();

        // Result is always at least 1s — the jitter formula has an
        // explicit `.max(1)` to keep the OTA loop from busy-spinning
        // when jitter rounds the sleep to zero.
        check!(
            result_secs >= 1,
            "apply_jitter: result is at least 1s",
            json!({"base": base_secs, "rand": rand, "got": result_secs}),
        );

        // Upper bound: ±10% jitter on the base, with a +1 slack for
        // integer-division rounding.
        let upper = base_secs + (base_secs / 10) + 1;
        check!(
            result_secs <= upper,
            "apply_jitter: result <= base + 10%",
            json!({"base": base_secs, "rand": rand, "got": result_secs, "upper": upper}),
        );

        // Lower bound: base * 0.9, clamped to 1s. Use a small slack
        // to absorb integer-division rounding.
        let lower = ((base_secs as i64) - (base_secs as i64 / 10) - 1).max(1) as u64;
        check!(
            result_secs >= lower,
            "apply_jitter: result >= base - 10%",
            json!({"base": base_secs, "rand": rand, "got": result_secs, "lower": lower}),
        );
    }

    // Coverage properties — we want the simulator to find inputs that
    // exercise both edges of the jitter range and the no-op midpoint.
    // These don't have to hold on every call; they only need to be
    // reached at least once across the run.
    for _ in 0..ITERS {
        let rand = random::get_random() as u32;
        let base = Duration::from_secs(100);
        let result = algos::apply_jitter(base, rand).as_secs();

        let zero = result == base.as_secs();
        let neg = result < base.as_secs();
        let pos = result > base.as_secs();
        let d_zero = json!({"rand": rand, "got": result});
        let d_neg = json!({"rand": rand, "got": result});
        let d_pos = json!({"rand": rand, "got": result});
        assert_sometimes!(zero, "apply_jitter: zero-jitter midpoint reached", &d_zero);
        assert_sometimes!(neg, "apply_jitter: negative jitter reached", &d_neg);
        assert_sometimes!(pos, "apply_jitter: positive jitter reached", &d_pos);
    }
}

// ---------------------------------------------------------------------------
// backoff_with_jitter
// ---------------------------------------------------------------------------

fn test_backoff_with_jitter() {
    eprintln!("antithesis-tests: backoff_with_jitter");

    // Cap is 3600s; jitter can add up to 10%, so the maximum a sane
    // implementation can return is 3960s. Anything above that means
    // the cap leaked. +1s slack absorbs integer-division rounding.
    const CAP_PLUS_JITTER: u64 = 3600 + 360 + 1;

    let bases = [1u64, 30, 60, 300, 600, 3600];

    for _ in 0..ITERS {
        let base_secs = *random::random_choice(&bases).unwrap_or(&60);
        let failures = (random::get_random() % 64) as u32; // up to 63
        let rand = random::get_random() as u32;
        let base = Duration::from_secs(base_secs);

        let result = algos::backoff_with_jitter(base, failures, rand);
        let result_secs = result.as_secs();

        // Cap holds: even with a runaway `failures` count, we must
        // never sleep longer than CAP * 1.1 (jitter). saturating_mul
        // + .min(BACKOFF_CAP) is what enforces this.
        check!(
            result_secs <= CAP_PLUS_JITTER,
            "backoff_with_jitter: respects 3600s cap (+jitter)",
            json!({
                "base": base_secs,
                "failures": failures,
                "rand": rand,
                "got": result_secs,
                "cap_plus_jitter": CAP_PLUS_JITTER,
            }),
        );

        // Result is always at least 1s (downstream of apply_jitter's
        // .max(1)). A 0-second sleep would turn the OTA loop into a
        // busy loop and torch the ESP32's heap.
        check!(
            result_secs >= 1,
            "backoff_with_jitter: result is at least 1s",
            json!({
                "base": base_secs,
                "failures": failures,
                "rand": rand,
                "got": result_secs,
            }),
        );

        // Monotonic-ish: doubling `failures` should never decrease the
        // un-jittered result. We compare against the same `rand` so
        // jitter isn't a confound.
        if failures < 32 {
            let doubled = algos::backoff_with_jitter(base, failures + 1, rand);
            // Both jittered with the same rand, so within-band
            // perturbation is identical. The pre-jitter sleep can
            // only grow or stay the same (capped), so post-jitter
            // result must too — modulo a 1s clamp at the floor.
            let lhs = result_secs;
            let rhs = doubled.as_secs();
            check!(
                rhs >= lhs || lhs <= 1,
                "backoff_with_jitter: monotonic in failures",
                json!({
                    "base": base_secs,
                    "failures": failures,
                    "rand": rand,
                    "lhs": lhs,
                    "rhs": rhs,
                }),
            );
        }
    }

    // Coverage: high failure counts should reach the cap (i.e. the
    // result lands within jitter range of 3600s).
    for _ in 0..ITERS {
        let rand = random::get_random() as u32;
        let result = algos::backoff_with_jitter(Duration::from_secs(60), 20, rand).as_secs();
        let near_cap = result >= 3600 - 360;
        let d = json!({"rand": rand, "got": result});
        assert_sometimes!(near_cap, "backoff_with_jitter: cap is reached on high failure counts", &d);
    }
}
