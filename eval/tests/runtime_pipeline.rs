//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem - lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Tests for `pantheon_runtime::pipeline::tests` - sibling file so sources stay test-free.
use pantheon_api::error::PantheonError;
use pantheon_runtime::pipeline::*;
use pantheon_storage::{OperationStatus, OperationStore};
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct ScriptedExec {
    calls: Arc<AtomicUsize>,
}
impl StageExecutor for ScriptedExec {
    fn run_stage(&self, stage: &str, _input: &str) -> Result<String, PantheonError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("{stage} output"))
    }
}

#[test]
fn stage_order_and_gates() {
    assert_eq!(STAGES.len(), 6);
    assert_eq!(GATES_AFTER, ["plan", "review"]);
    assert_eq!(stage_kind("plan"), StageKind::Gate);
    assert_eq!(stage_kind("intake"), StageKind::Model);
}

#[test]
fn model_stage_completes_durably_and_records_output() {
    let store = OperationStore::open_in_memory().unwrap();
    let exec = ScriptedExec {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let op = run_model_stage(&store, "run:plan", "plan", "spec", &exec).unwrap();
    assert_eq!(op.status, OperationStatus::Completed);
    assert_eq!(
        op.state
            .get("result")
            .and_then(|r| r.get("output"))
            .and_then(Value::as_str),
        Some("plan output")
    );
}

#[test]
fn crashed_stage_resumes_without_rerunning_the_executor() {
    let store = OperationStore::open_in_memory().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let exec = ScriptedExec {
        calls: calls.clone(),
    };
    // First run completes the execute phase and persists raw_result.
    let op = run_model_stage(&store, "run:research", "research", "in", &exec).unwrap();
    assert_eq!(op.status, OperationStatus::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // A resumed run for the same op id is a no-op: result already durable.
    let op2 = run_model_stage(&store, "run:research", "research", "in", &exec).unwrap();
    assert_eq!(op2.status, OperationStatus::Completed);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "executor must not rerun");
}
