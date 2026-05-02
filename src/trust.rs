//! Compile-time trust configuration for OTA signing verification.
//!
//! These values cannot be changed via OTA — only by editing the source
//! and reflashing over USB. That's the whole point: a compromised
//! signing identity must not be able to update its own allowlist.

/// Allowlist of (email, issuer) pairs the OTA verifier will accept as
/// signers. Identity comes from the Fulcio cert's SAN rfc822Name (email)
/// and the OIDC-issuer extension OID 1.3.6.1.4.1.57264.1.1 (issuer URL).
pub const TRUSTED_IDENTITIES: &[(&str, &str)] = &[
    ("imjasonh@gmail.com", "https://accounts.google.com"),
];

/// Sigstore Public Good Instance Fulcio intermediate CA (v1). Used to
/// verify the leaf signing cert was issued by Sigstore.
pub const SIGSTORE_INTERMEDIATE_PEM: &str =
    include_str!("../trust/fulcio_intermediate.pem");

/// Sigstore Public Good Instance Fulcio root CA (v1). Used to verify
/// the bundled intermediate hasn't been swapped (defense in depth).
pub const SIGSTORE_ROOT_PEM: &str = include_str!("../trust/fulcio_root.pem");
