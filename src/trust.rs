//! Compile-time trust configuration for OTA signing verification.
//!
//! These values cannot be changed via OTA — only by editing the source
//! and reflashing over USB. That's the whole point: a compromised
//! signing identity must not be able to update its own allowlist.

/// Allowlist of (identity, issuer) pairs the OTA verifier will accept
/// as signers. The identity is the Fulcio cert's SAN value — either an
/// rfc822Name (email, for OIDC issuers like Google) or a URI (for
/// workflow-based issuers like GitHub Actions). The issuer is read from
/// extension OID 1.3.6.1.4.1.57264.1.1 (the legacy Fulcio OIDC-issuer
/// extension), which carries the OIDC issuer URL.
pub const TRUSTED_IDENTITIES: &[(&str, &str)] = &[
    // Manual `make publish` from a developer's machine. Browser-based
    // OIDC against Google.
    ("imjasonh@gmail.com", "https://accounts.google.com"),

    // Automated publishes from the GitHub Actions workflow on push to
    // main. The URI pins the exact workflow file at the exact branch;
    // a malicious commit that adds a different workflow file won't
    // produce a matching SAN URI. Trust here is bounded by who can
    // push to imjasonh/esp32:main (i.e. the source-of-truth itself).
    (
        "https://github.com/imjasonh/esp32/.github/workflows/publish.yml@refs/heads/main",
        "https://token.actions.githubusercontent.com",
    ),
];

/// Sigstore Public Good Instance Fulcio intermediate CA (v1). Used to
/// verify the leaf signing cert was issued by Sigstore.
pub const SIGSTORE_INTERMEDIATE_PEM: &str =
    include_str!("../trust/fulcio_intermediate.pem");

/// Sigstore Public Good Instance Fulcio root CA (v1). Used to verify
/// the bundled intermediate hasn't been swapped (defense in depth).
pub const SIGSTORE_ROOT_PEM: &str = include_str!("../trust/fulcio_root.pem");
