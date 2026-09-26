//! Tests for `pantheon_secrets::value::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn debug_never_reveals_value() {
    let s = SecretValue::new("hunter2-dont-log-me");
    let rendered = format!("{s:?}");
    assert!(!rendered.contains("hunter2"));
    assert_eq!(rendered, "SecretValue(***)");
    // Length is safe to surface.
    assert_eq!(s.len(), "hunter2-dont-log-me".len());
}

#[test]
fn round_trips_value() {
    let s = SecretValue::new("sk-live-123");
    assert_eq!(s.expose(), "sk-live-123");
    assert_eq!(s, SecretValue::new("sk-live-123"));
    assert_ne!(s, SecretValue::new("sk-live-456"));
}
