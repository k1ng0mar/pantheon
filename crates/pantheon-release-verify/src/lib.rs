//! Release-signature verification for `pantheon update`.
//!
//! Auth for release archives is a minisign signature over the archive bytes,
//! checked offline against a public key pinned in this crate. The same-origin
//! `.sha256` file is corruption-check only: it proves the bytes match the
//! publisher's checksum file, which comes from the same origin as the binary,
//! so it can't prove who published it. The pinned key can.
//!
//! Fail-closed everywhere:
//! - missing/unreadable/malformed `.minisig` -> failure
//! - signature mismatch, wrong key id, or legacy (non-prehashed) -> failure
//!
//! No bypass flag: an unverified binary is never installed.
//!
//! Callers fetch `{asset_url}.minisig` themselves (transports differ) and pass
//! it to [`verify_signature`]; this crate stays offline and pure so it tests
//! without a network.

use minisign_verify::{PublicKey, Signature};

/// Base64 minisign public key pinned for release verification.
///
/// Generated once with `minisign -G`; the secret half lives only in the
/// `MINISIGN_SECRET_KEY` GitHub repo secret and is written to a runner temp
/// file at signing time (see `docs/release-signing.md`). Public, safe to ship.
pub const PINNED_PUBLIC_KEY: Option<&str> =
    Some("RWQfdih4Dd0BpT+16PEvcBm9h0F1LOmTY/mHksosHvt/sl0JUbIlXlpO");

/// Verify `bytes` against the pinned key and a `.minisig` text.
/// Fails closed when the key is absent or malformed.
pub fn verify_signature(bytes: &[u8], signature_text: &str) -> Result<(), String> {
    let key = PINNED_PUBLIC_KEY.ok_or_else(|| {
        "signature verification unavailable: no release signing key is pinned \
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
    // allow_legacy = false: modern signatures are prehashed (BLAKE2b then
    // Ed25519); legacy raw-Ed25519 ones are rejected.
    public_key.verify(bytes, &signature, false).map_err(|e| {
        format!("signature verification failed: signature does not match this release ({e})")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Published `minisign-verify` doc-test vector: public key and signature
    /// over `b"test"`.
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

    /// Made at provisioning time with the real release key, kept in scratch as
    /// the fixture. Proves the pinned key and the `minisign -S` format agree.
    const RELEASE_SIG: &str = "untrusted comment: signature from minisign secret key\nRUQfdih4Dd0Bpcd6T/WvTNanxXSsxtEpLVfKN+rI9U2uPrJgYpPofVBlHe6KIxwp7uZU7b/uvnQT86mMAVpacNGRcJw3jwLfyAs=\ntrusted comment: timestamp:1795638358\tfile:fixture.txt\thashed\nSLkJcU6mAwiROzLGTwsJ4fWcPfaGoz0XGwr9Q9YZiT5WdbbhyMJFwJzFX+yABRx129DUGg1uA0hnNtyC2y7yDg==";

    #[test]
    fn pinned_key_is_set_and_parses() {
        let key = PINNED_PUBLIC_KEY.expect("release key must be pinned");
        PublicKey::from_base64(key).expect("pinned key must parse");
    }

    #[test]
    fn pinned_key_verifies_a_real_signature() {
        verify_signature(b"pantheon release signing fixture\n", RELEASE_SIG)
            .expect("real release signature must verify against the pinned key");
    }

    #[test]
    fn pinned_key_rejects_tampered_bytes() {
        assert!(verify_signature(b"pantheon release signing fixture!", RELEASE_SIG).is_err());
    }
}
