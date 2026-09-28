//! Webhook HMAC contract: the scheduler's signing primitive round-trips.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_scheduler::webhook::{
    sign, verify_signature, WebhookAuth, SignatureError, SIGNATURE_HEADER,
};

const SECRET: &[u8] = b"test-shared-secret";
const BODY: &[u8] = br#"{"event":"push","repo":"pantheon"}"#;

#[test]
fn signed_payload_verifies_with_the_secret() {
    let header = sign(SECRET, BODY);
    assert!(
        header.starts_with("sha256="),
        "scheme prefix per the contract"
    );
    assert_eq!(header.len(), "sha256=".len() + 64);
    verify_signature(SECRET, BODY, Some(&header)).unwrap();
}

#[test]
fn verification_fails_with_the_wrong_secret() {
    let header = sign(SECRET, BODY);
    assert_eq!(
        verify_signature(b"wrong-secret", BODY, Some(&header)).unwrap_err(),
        SignatureError::Mismatch
    );
}

#[test]
fn verification_fails_when_the_body_changed() {
    let header = sign(SECRET, BODY);
    assert_eq!(
        verify_signature(SECRET, b"tampered", Some(&header)).unwrap_err(),
        SignatureError::Mismatch
    );
}

#[test]
fn verification_fails_without_a_signature() {
    assert_eq!(
        verify_signature(SECRET, BODY, None).unwrap_err(),
        SignatureError::Missing
    );
}

#[test]
fn verification_fails_on_malformed_header() {
    for bad in ["bogus", "sha256=zzz", "md5=abcd", "sha256="] {
        assert_eq!(
            verify_signature(SECRET, BODY, Some(bad)).unwrap_err(),
            SignatureError::Malformed,
            "header {bad:?}"
        );
    }
}

#[test]
fn empty_secret_cannot_build_auth() {
    assert!(WebhookAuth::new(b"").is_none());
    assert!(WebhookAuth::new(b"secret").is_some());
}

#[test]
fn signature_header_name_matches_contract() {
    assert_eq!(SIGNATURE_HEADER, "X-Pantheon-Signature");
}

#[test]
fn sign_is_deterministic_per_secret_and_body() {
    assert_eq!(sign(SECRET, BODY), sign(SECRET, BODY));
    assert_ne!(sign(SECRET, BODY), sign(SECRET, b"other"));
}
