//! Tests for `pantheon_memory::http_backend::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn pct_passes_through_unreserved() {
    assert_eq!(pct("abc"), "abc");
    assert_eq!(pct("hello world"), "hello%20world");
    assert_eq!(pct("a/b"), "a%2Fb");
}

#[test]
fn layer_str_round_trips() {
    assert_eq!(layer_str(LayerKind::Global), "Global");
    assert_eq!(layer_str(LayerKind::Agent), "Agent");
    assert_eq!(layer_str(LayerKind::EphemeralTurn), "EphemeralTurn");
}
