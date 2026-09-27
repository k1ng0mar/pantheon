//! Unit tests for the private `stamp_for` helper. Small, deterministic,
//! and needs the private function, so it stays beside the code.
use super::*;

#[test]
fn stamp_changes_when_content_changes() {
    let a = stamp_for("hello");
    let b = stamp_for("hello ");
    assert_ne!(a, b);
}
