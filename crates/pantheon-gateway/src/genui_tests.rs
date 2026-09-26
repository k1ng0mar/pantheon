//! Tests for `pantheon_gateway::genui::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn task_ids_are_safe_path_components() {
    assert!(valid_task_id("task-1_A"));
    assert!(!valid_task_id("task/1"));
    assert!(!valid_task_id("task?x"));
    assert!(!valid_task_id(""));
}

#[test]
fn sha256_matches_nist_vector() {
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
#[test]
fn round_trip_sign_verify() {
    let s = GenUiSigner::new("https://ui.example/blobs", b"test-secret".to_vec());
    let r = s.sign("task-1", "image/png", 60_000);
    assert!(r.url.contains("task-1"));
    let q = r.url.split('?').nth(1).unwrap();
    let mut exp = 0;
    let mut sig = String::new();
    for kv in q.split('&') {
        if let Some(v) = kv.strip_prefix("exp=") {
            exp = v.parse().unwrap();
        }
        if let Some(v) = kv.strip_prefix("sig=") {
            sig = v.into();
        }
    }
    assert!(s.verify("task-1", exp, &sig));
    assert!(!s.verify("task-1", exp, "00"));
    assert!(!s.verify("other", exp, &sig));
}
#[test]
fn expired_urls_rejected() {
    let s = GenUiSigner::new("https://ui.example/blobs", b"s".to_vec());
    assert!(!s.verify("t", now_ms() - 1, "ab"));
}
