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

// `dead_code` here covers consts the test crate doesn't reference but
// the firmware does (e.g. mediaType strings). Their values are still
// reachable via the `algos::` path for any test that wants them.
#[allow(dead_code)]
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
    test_is_valid_sha256_digest();
    test_strip_sha256_prefix();
    test_cosign_bundle_tag();
    test_ghcr_repo_path();
    test_digest_from_manifest_url();

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

// ---------------------------------------------------------------------------
// is_valid_sha256_digest
// ---------------------------------------------------------------------------

fn test_is_valid_sha256_digest() {
    eprintln!("antithesis-tests: is_valid_sha256_digest");

    // Hand-rolled vectors covering the canonical-good case and the
    // failure modes the firmware actually has to defend against.
    let cases: &[(&str, bool)] = &[
        // 64-char lowercase hex with prefix: the canonical accepted form.
        (
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            true,
        ),
        ("", false),
        ("sha256:", false),
        // 63 chars
        (
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcde",
            false,
        ),
        // 65 chars
        (
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0",
            false,
        ),
        // Uppercase hex — registries use lowercase canonically; mixing case
        // would mismatch on string comparison against `last_digest` in NVS.
        (
            "sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef",
            false,
        ),
        // Non-hex char (`g`)
        (
            "sha256:0123456789abcdeg0123456789abcdef0123456789abcdef0123456789abcdef",
            false,
        ),
        // Wrong algo prefix
        (
            "sha512:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            false,
        ),
        // Missing prefix entirely
        (
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            false,
        ),
    ];
    for (input, expected) in cases {
        let got = algos::is_valid_sha256_digest(input);
        check!(
            got == *expected,
            "is_valid_sha256_digest: vector matches expected",
            json!({"input": input, "expected": expected, "got": got}),
        );
    }

    // Property: a randomly built well-formed digest is always accepted.
    for _ in 0..ITERS {
        let mut hex = String::with_capacity(64);
        for _ in 0..64 {
            let nibble = random::get_random() % 16;
            hex.push(char::from_digit(nibble as u32, 16).unwrap());
        }
        let digest = format!("sha256:{}", hex);
        check!(
            algos::is_valid_sha256_digest(&digest),
            "is_valid_sha256_digest: random valid digest accepted",
            json!({"digest": digest}),
        );
    }

    // Property: flipping any single hex char to a non-hex byte rejects.
    for _ in 0..ITERS {
        let mut bytes: Vec<u8> = (0..64).map(|_| b"0123456789abcdef"[(random::get_random() as usize) & 0xf]).collect();
        let pos = (random::get_random() as usize) % bytes.len();
        // Pick a non-hex ASCII char.
        let bad = *random::random_choice(&[b'g' as u8, b'G', b'!', b':', b' ', b'-', b'X', b'.']).unwrap();
        bytes[pos] = bad;
        let hex = String::from_utf8(bytes).unwrap();
        let digest = format!("sha256:{}", hex);
        check!(
            !algos::is_valid_sha256_digest(&digest),
            "is_valid_sha256_digest: any non-hex char rejects",
            json!({"digest": digest, "bad_at": pos}),
        );
    }
}

// ---------------------------------------------------------------------------
// strip_sha256_prefix
// ---------------------------------------------------------------------------

fn test_strip_sha256_prefix() {
    eprintln!("antithesis-tests: strip_sha256_prefix");

    for _ in 0..ITERS {
        // Random hex content of varying length — strip is intentionally
        // lenient about the body, only the prefix is required.
        let len = (random::get_random() % 80) as usize;
        let body: String = (0..len)
            .map(|_| b"0123456789abcdef"[(random::get_random() as usize) & 0xf] as char)
            .collect();
        let with = format!("sha256:{}", body);

        let stripped = algos::strip_sha256_prefix(&with);
        check!(
            stripped == Some(body.as_str()),
            "strip_sha256_prefix: round-trips for sha256:<body>",
            json!({"input": with, "got": stripped}),
        );

        // Without the prefix → None.
        let stripped2 = algos::strip_sha256_prefix(&body);
        check!(
            stripped2.is_none() || body.starts_with("sha256:"),
            "strip_sha256_prefix: None when prefix is absent",
            json!({"input": body, "got": stripped2}),
        );

        // sha512:... or any other algo prefix is rejected (None).
        let other = format!("sha512:{}", body);
        check!(
            algos::strip_sha256_prefix(&other).is_none(),
            "strip_sha256_prefix: rejects non-sha256 algo prefix",
            json!({"input": other}),
        );
    }
}

// ---------------------------------------------------------------------------
// cosign_bundle_tag
// ---------------------------------------------------------------------------

fn test_cosign_bundle_tag() {
    eprintln!("antithesis-tests: cosign_bundle_tag");

    for _ in 0..ITERS {
        let mut hex = String::with_capacity(64);
        for _ in 0..64 {
            let nibble = random::get_random() % 16;
            hex.push(char::from_digit(nibble as u32, 16).unwrap());
        }
        let tag = algos::cosign_bundle_tag(&hex);

        // Required prefix: cosign and registries agree on this exact
        // mapping. A typo here would route the OTA verifier to a
        // non-existent tag and silently bork all signature lookups.
        check!(
            tag.starts_with("sha256-"),
            "cosign_bundle_tag: starts with sha256-",
            json!({"hex": hex, "tag": tag}),
        );
        check!(
            tag.ends_with(&hex),
            "cosign_bundle_tag: ends with the hex digest",
            json!({"hex": hex, "tag": tag}),
        );
        check!(
            tag.len() == "sha256-".len() + hex.len(),
            "cosign_bundle_tag: length is exact",
            json!({"hex_len": hex.len(), "tag_len": tag.len()}),
        );

        // Idempotent / pure.
        let tag2 = algos::cosign_bundle_tag(&hex);
        check!(
            tag == tag2,
            "cosign_bundle_tag: deterministic",
            json!({"hex": hex}),
        );
    }
}

// ---------------------------------------------------------------------------
// ghcr_repo_path
// ---------------------------------------------------------------------------

fn test_ghcr_repo_path() {
    eprintln!("antithesis-tests: ghcr_repo_path");

    let known: &[(&str, Option<&str>)] = &[
        ("ghcr.io/imjasonh/esp32", Some("imjasonh/esp32")),
        ("ghcr.io/", Some("")),
        ("docker.io/library/alpine", None),
        ("imjasonh/esp32", None),
        ("", None),
        // Substring match shouldn't fool us: prefix is anchored.
        ("xghcr.io/imjasonh/esp32", None),
    ];
    for (input, expected) in known {
        let got = algos::ghcr_repo_path(input);
        check!(
            got == *expected,
            "ghcr_repo_path: known vectors",
            json!({"input": input, "expected": expected, "got": got}),
        );
    }

    // Property: for any random suffix `s`, ghcr_repo_path("ghcr.io/" + s) == Some(s).
    for _ in 0..ITERS {
        let len = (random::get_random() % 64) as usize;
        let s: String = (0..len)
            .map(|i| {
                let pool = b"abcdefghijklmnopqrstuvwxyz0123456789-_/";
                pool[((random::get_random() as usize) ^ i) % pool.len()] as char
            })
            .collect();
        let full = format!("ghcr.io/{}", s);
        let got = algos::ghcr_repo_path(&full);
        check!(
            got == Some(s.as_str()),
            "ghcr_repo_path: prefix-strip round-trips for any suffix",
            json!({"suffix": s, "full": full, "got": got}),
        );
    }
}

// ---------------------------------------------------------------------------
// digest_from_manifest_url
// ---------------------------------------------------------------------------

fn test_digest_from_manifest_url() {
    eprintln!("antithesis-tests: digest_from_manifest_url");

    let known: &[(&str, Option<&str>)] = &[
        (
            "https://ghcr.io/v2/imjasonh/esp32/manifests/sha256:abc",
            Some("sha256:abc"),
        ),
        // Tag, not a digest — function still returns whatever's after the segment.
        (
            "https://ghcr.io/v2/imjasonh/esp32/manifests/latest",
            Some("latest"),
        ),
        // No /manifests/ segment.
        ("https://ghcr.io/v2/imjasonh/esp32/blobs/sha256:abc", None),
        ("", None),
        // Pathological: multiple /manifests/ segments → rsplit takes the last.
        (
            "https://ghcr.io/v2/imjasonh/manifests/repo/manifests/sha256:zzz",
            Some("sha256:zzz"),
        ),
    ];
    for (input, expected) in known {
        let got = algos::digest_from_manifest_url(input);
        check!(
            got == *expected,
            "digest_from_manifest_url: known vectors",
            json!({"input": input, "expected": expected, "got": got}),
        );
    }

    // Property: any URL we construct ourselves round-trips. This is
    // the actual integration we care about — `fetch_manifest` builds
    // these URLs and the publisher parses them back.
    for _ in 0..ITERS {
        let mut hex = String::with_capacity(64);
        for _ in 0..64 {
            hex.push(char::from_digit((random::get_random() % 16) as u32, 16).unwrap());
        }
        let digest = format!("sha256:{}", hex);
        let url = format!("https://ghcr.io/v2/imjasonh/esp32/manifests/{}", digest);
        let got = algos::digest_from_manifest_url(&url);
        check!(
            got == Some(digest.as_str()),
            "digest_from_manifest_url: round-trips constructed URL",
            json!({"digest": digest, "url": url, "got": got}),
        );
    }
}
