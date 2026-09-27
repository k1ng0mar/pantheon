//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral — SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem — lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Tests for `pantheon_runtime::operation::tests` — sibling file so sources stay test-free.
use pantheon_runtime::operation::*;
use pantheon_storage::{OperationStatus, OperationStore};
use pantheon_api::error::PantheonError;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct CountingAdapter(Arc<AtomicUsize>);
impl ToolOperationAdapter for CountingAdapter {
    fn translate(&self, request: &Value) -> Result<Value, PantheonError> {
        Ok(request.clone())
    }
    fn execute(&self, translated: &Value) -> Result<Value, PantheonError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(translated.clone())
    }
    fn translate_result(&self, result: &Value) -> Result<Value, PantheonError> {
        Ok(result.clone())
    }
}

#[test]
fn a_completed_operation_is_not_executed_twice() {
    let store = OperationStore::open_in_memory().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let adapter = CountingAdapter(count.clone());
    let first = run_tool_operation(
        &store,
        "op",
        "tool.execute",
        serde_json::json!({"x": 1}),
        &adapter,
    )
    .unwrap();
    let second = run_tool_operation(
        &store,
        "op",
        "tool.execute",
        serde_json::json!({"x": 1}),
        &adapter,
    )
    .unwrap();
    assert_eq!(first.status, OperationStatus::Completed);
    assert_eq!(second.status, OperationStatus::Completed);
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn canceling_operation_is_never_executed() {
    let store = OperationStore::open_in_memory().unwrap();
    let request = serde_json::json!({"x": 1});
    let op = store
        .create(
            "op-cancel",
            "tool.execute",
            serde_json::json!({
                "phase": "translate", "request": request
            }),
        )
        .unwrap();
    let op = store
        .transition(
            "op-cancel",
            op.version,
            OperationStatus::Awaiting,
            serde_json::json!({"phase": "execute", "request": request, "translated": request}),
        )
        .unwrap();
    let op = store
        .request_cancel(
            "op-cancel",
            op.version,
            serde_json::json!({
                "phase": "execute", "request": request, "cancel_reason": "user"
            }),
        )
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let resumed = run_tool_operation(
        &store,
        "op-cancel",
        "tool.execute",
        request,
        &CountingAdapter(count.clone()),
    )
    .unwrap();
    assert_eq!(resumed.status, OperationStatus::Canceling);
    assert_eq!(resumed.version, op.version);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[test]
fn operation_id_cannot_be_reused_for_another_request() {
    let store = OperationStore::open_in_memory().unwrap();
    let adapter = CountingAdapter(Arc::new(AtomicUsize::new(0)));
    run_tool_operation(
        &store,
        "op",
        "tool.execute",
        serde_json::json!({"x": 1}),
        &adapter,
    )
    .unwrap();
    let error = run_tool_operation(
        &store,
        "op",
        "tool.execute",
        serde_json::json!({"x": 2}),
        &adapter,
    )
    .unwrap_err();
    assert_eq!(error.code, "OPERATION_IDENTITY");
}
