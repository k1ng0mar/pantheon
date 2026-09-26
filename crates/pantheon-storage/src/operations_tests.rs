//! Tests for `pantheon_storage::operations::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn state_machine_is_versioned_and_cas_safe() {
    let s = OperationStore::open_in_memory().unwrap();
    let op = s
        .create("op-1", "tool.execute", serde_json::json!({"tool":"shell"}))
        .unwrap();
    assert_eq!(op.status, OperationStatus::Ready);
    let op = s
        .transition(
            "op-1",
            0,
            OperationStatus::Awaiting,
            serde_json::json!({"reason":"approval"}),
        )
        .unwrap();
    assert_eq!(op.version, 1);
    let err = s
        .transition("op-1", 0, OperationStatus::Completed, Value::Null)
        .unwrap_err();
    assert_eq!(err.code, "OPERATION_CONFLICT");
    let done = s
        .complete("op-1", op.version, serde_json::json!({"ok":true}))
        .unwrap();
    assert_eq!(done.status, OperationStatus::Completed);
    assert_eq!(done.version, 2);
}

#[test]
fn cancellation_has_intent_then_terminal_state() {
    let s = OperationStore::open_in_memory().unwrap();
    let op = s.create("op-c", "tool.execute", Value::Null).unwrap();
    let op = s.request_cancel("op-c", op.version, Value::Null).unwrap();
    assert_eq!(op.status, OperationStatus::Canceling);
    let op = s.cancel("op-c", op.version, Value::Null).unwrap();
    assert!(op.status.is_terminal());
}
