//! Tests for `crate::gateway::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_gateway::EventSink;

fn sink_with(allow: Option<&[&str]>) -> RuntimeSink {
    RuntimeSink {
        data_dir: std::path::PathBuf::from("/tmp"),
        policy: Policy::coder(),
        threads: Mutex::new(std::collections::HashMap::new()),
        outbound: Arc::new(Mutex::new(Vec::new())),
        allow: allow.map(|ids| ids.iter().map(|s| s.to_string()).collect()),
    }
}

#[test]
fn allowlist_admits_listed_senders_only() {
    let s = sink_with(Some(&["111", "222"]));
    assert!(s.allowed(Some("111")));
    assert!(!s.allowed(Some("333")));
    // No sender identity with an active allowlist: refuse.
    assert!(!s.allowed(None));
}

#[test]
fn unset_allowlist_admits_everyone_local_only() {
    let s = sink_with(None);
    assert!(s.allowed(Some("anyone")));
    assert!(s.allowed(None));
}

#[test]
fn empty_allowlist_denies_everyone() {
    let s = sink_with(Some(&[]));
    assert!(!s.allowed(Some("111")));
}

#[test]
fn denied_sender_never_reaches_the_runtime() {
    // on_message from a non-listed sender must not bind a thread or
    // touch a session; outbound gets the refusal instead.
    let dir = std::env::temp_dir().join(format!("pantheon-gw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let outbound = Arc::new(Mutex::new(Vec::new()));
    let s = RuntimeSink {
        data_dir: dir,
        policy: Policy::coder(),
        threads: Mutex::new(std::collections::HashMap::new()),
        outbound: outbound.clone(),
        allow: Some(["111".to_string()].into_iter().collect()),
    };
    s.on_message("chat1", Some("999"), "rm -rf /");
    let out = outbound.lock().unwrap();
    assert_eq!(out.len(), 1, "exactly the refusal");
    assert!(out[0].text.contains("not authorized"));
}
