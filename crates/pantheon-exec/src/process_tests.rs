//! Tests for `pantheon_exec::process::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn rejects_pid_zero_and_one() {
    assert!(ProcessGroup::new(0).is_none());
    assert!(ProcessGroup::new(1).is_none());
    assert!(ProcessGroup::new(42).is_some());
}
