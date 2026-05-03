//! Pure-Rust algorithms shared between the firmware and the host-side
//! Antithesis test crate (`tools/antithesis/`). Anything in here MUST
//! NOT depend on `esp_idf_svc` or any other ESP-IDF-only crate — the
//! test crate includes this file verbatim via `#[path]` and builds for
//! the host triple.

use std::time::Duration;

/// Maximum sleep between OTA polls when backing off after failures.
pub const BACKOFF_CAP: Duration = Duration::from_secs(3600);

/// OCI mediaType for the firmware blob layer in our artifacts.
/// `tools/publisher` writes this and `src/ota.rs` checks for it.
pub const FIRMWARE_LAYER_MEDIA_TYPE: &str = "application/vnd.esp32.firmware.bin";

/// Sigstore bundle layers carry a versioned mediaType
/// (`...bundle.v0.3+json`, etc.); we accept any version so we don't have
/// to chase cosign upgrades.
pub const SIGSTORE_BUNDLE_MEDIA_TYPE_PREFIX: &str = "application/vnd.dev.sigstore.bundle.";

/// Length of a SHA-256 digest rendered in lowercase hex (no `sha256:` prefix).
pub const SHA256_HEX_LEN: usize = 64;

// --- OCI / digest helpers ---------------------------------------------------

/// Returns true if `s` is the canonical OCI digest form
/// `"sha256:" + 64 lowercase hex chars`. Strict — uppercase hex,
/// trailing whitespace, or wrong length all return false.
pub fn is_valid_sha256_digest(s: &str) -> bool {
    let Some(hex) = s.strip_prefix("sha256:") else {
        return false;
    };
    is_lowercase_sha256_hex(hex)
}

/// Same as `is_valid_sha256_digest` but for a bare hex string with no
/// `sha256:` prefix (e.g. the manifest digest used to build the cosign
/// bundle tag).
pub fn is_lowercase_sha256_hex(hex: &str) -> bool {
    hex.len() == SHA256_HEX_LEN
        && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Strip the `"sha256:"` prefix from an OCI digest. Returns `None` if
/// the prefix is missing. Lenient — does not validate that the
/// remainder is hex; pair with `is_lowercase_sha256_hex` if you need
/// that. (Kept lenient because the OTA download path already
/// fingerprints the blob bytes against this string and would catch
/// malformed hex via the SHA mismatch bail.)
pub fn strip_sha256_prefix(digest: &str) -> Option<&str> {
    digest.strip_prefix("sha256:")
}

/// Cosign publishes the Sigstore bundle for a given OCI artifact at a
/// tag derived from the artifact's manifest digest:
/// `sha256-<64 hex chars>`. Used by the OTA verifier to locate the
/// bundle without a referrers API call.
pub fn cosign_bundle_tag(manifest_digest_hex: &str) -> String {
    format!("sha256-{}", manifest_digest_hex)
}

/// Return the `<owner>/<name>` portion of a `ghcr.io/<owner>/<name>`
/// repo string. `None` if the input doesn't start with `ghcr.io/`.
/// Other registries aren't supported yet; once they are this can grow.
pub fn ghcr_repo_path(repo: &str) -> Option<&str> {
    repo.strip_prefix("ghcr.io/")
}

/// Extract a digest like `sha256:abc...` from an OCI manifest URL of
/// the shape `https://<host>/v2/<path>/manifests/<digest>`. `None` if
/// the URL doesn't contain `/manifests/`.
pub fn digest_from_manifest_url(url: &str) -> Option<&str> {
    url.rsplit_once("/manifests/").map(|(_, d)| d)
}

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
