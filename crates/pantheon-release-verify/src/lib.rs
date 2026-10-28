//! Release-signature verification for `pantheon update`.
//!
//! Authentication for release archives is a minisign signature over the
//! archive bytes, checked offline against a public key pinned in this
//! crate. The same-origin `.sha256` file is a corruption check only:
//! it proves the bytes match the publisher's checksum file, and that
//! file comes from the same origin as the binary, so it can never prove
//! who published the binary. A signature from the pinned key can.
//!
//! Fail-closed, everywhere:
//! - no pinned key yet ([`PINNED_PUBLIC_KEY`] is `None` until the
//!   release keypair is provisioned - see `docs/release-signing.md`)
//!   → verification fails with a clear message; nothing installs.
//! - missing, unreadable, or malformed `.minisig` → failure.
//! - signature mismatch, wrong key id, or legacy (non-prehashed)
//!   signature → failure.
//! There is no bypass flag: an unverified binary is never installed.
//!
//! Callers fetch the `{asset_url}.minisig` text themselves (their
//! transports differ) and pass it to [`verify_signature`]; this crate
//! stays offline and pure so it is unit-testable without a network.

use minisign_verify::{PublicKey, Signature};

/// Base64 minisign public key pinned for release verification.
///
/// `None` until provisioning: no release keypair exists yet, so the
/// slot starts empty and verification fails closed with a message
/// telling the operator to install releases by hand until the key is
/// pinned (see `docs/release-signing.md`). A placeholder key would be
/// worse than none - it would look like authentication while
/// verifying against a key nobody controls.
pub const PINNED_PUBLIC_KEY: Option<&str> = None;

/// Verify `bytes` against the pinned release key and a `.minisig` text.
/// Fails closed when the key is absent.
pub fn verify_signature(bytes: &[u8], signature_text: &str) -> Result<(), String> {
    let key = PINNED_PUBLIC_KEY.ok_or_else(|| {
        "signature verification unavailable: no release signing key is pinned yet \
         (see docs/release-signing.md); install this release by hand instead of \
         running an unverified update"
            .to_string()
    })?;
    verify_with_key(key, bytes, signature_text)
}

/// Verify `bytes` against an explicit base64 minisign public key.
/// The test seam: unit tests use the published `minisign-verify` doc
/// vector through this function, since the pinned slot is empty.
pub fn verify_with_key(
    public_key_base64: &str,
    bytes: &[u8],
    signature_text: &str,
) -> Result<(), String> {
    let public_key = PublicKey::from_base64(public_key_base64)
        .map_err(|e| format!("signature verification failed: bad pinned public key ({e})"))?;
    let signature = Signature::decode(signature_text)
        .map_err(|e| format!("signature verification failed: unreadable .minisig ({e})"))?;
    // `allow_legacy = false`: modern minisign signatures are prehashed
    // (BLAKE2b, then Ed25519); legacy raw-Ed25519 signatures are
    // rejected rather than accepted.
    public_key.verify(bytes, &signature, false).map_err(|e| {
        format!("signature verification failed: signature does not match this release ({e})")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published `minisign-verify` doc-test vector: public key and a
    /// signature over `b"test"`.
    const DOC_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const DOC_SIG: &str = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1633700835\tfile:test\tprehashed\nwLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==";

    #[test]
    fn doc_vector_verifies() {
        verify_with_key(DOC_KEY, b"test", DOC_SIG).expect("doc vector must verify");
    }

    #[test]
    fn tampered_bytes_fail() {
        assert!(verify_with_key(DOC_KEY, b"Test", DOC_SIG).is_err());
        assert!(verify_with_key(DOC_KEY, b"test!", DOC_SIG).is_err());
    }

    #[test]
    fn wrong_key_fails() {
        // Same shape, different key id: key-id mismatch, not a pass.
        let other = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO4";
        assert!(verify_with_key(other, b"test", DOC_SIG).is_err());
    }

    #[test]
    fn malformed_signature_fails() {
        assert!(verify_with_key(DOC_KEY, b"test", "not a minisig").is_err());
        assert!(verify_with_key(DOC_KEY, b"test", "").is_err());
    }

    #[test]
    fn absent_pinned_key_fails_closed() {
        assert!(PINNED_PUBLIC_KEY.is_none());
        let err = verify_signature(b"test", DOC_SIG).unwrap_err();
        assert!(
            err.contains("no release signing key is pinned"),
            "unexpected message: {err}"
        );
    }
}
