//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem - lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Tests for `pantheon_runtime::pipeline_runner::tests` - sibling file so sources stay test-free.
use pantheon_api::error::PantheonError;
use pantheon_runtime::pipeline::GATES_AFTER;
use pantheon_runtime::pipeline::{AcceptAllEvaluator, StageEvaluator, StageExecutor};
use pantheon_runtime::pipeline_runner::*;
use pantheon_storage::{OperationStatus, OperationStore};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct ScriptedExec {
    calls: Arc<AtomicUsize>,
}
impl StageExecutor for ScriptedExec {
    fn run_stage(&self, stage: &str, input: &str) -> Result<String, PantheonError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(format!("{stage}<-{input}"))
    }
}

fn runner<'a>(
    store: &'a OperationStore,
    exec: &'a ScriptedExec,
    evaluator: &'a dyn StageEvaluator,
) -> PipelineRunner<'a> {
    PipelineRunner {
        store,
        run_id: "run1".into(),
        executor: exec,
        evaluator,
        max_iterations: 3,
    }
}

#[test]
fn full_pipeline_with_approved_gates_completes() {
    let store = OperationStore::open_in_memory().unwrap();
    let exec = ScriptedExec {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    // Pre-approve both gates so the run never parks.
    for stage in GATES_AFTER {
        store
            .create(
                format!("run1:{stage}"),
                "pipeline.gate",
                json!({
                    "phase": "gate",
                    "request": {"run_id": "run1", "stage": stage, "input": ""},
                    "decision": "approved",
                    "result": {"output": "approved"},
                }),
            )
            .unwrap();
    }
    let r = runner(&store, &exec, &AcceptAllEvaluator);
    let out = r.run("the spec").unwrap();
    assert_eq!(out.outputs.len(), 6);
    // Input chaining: research saw intake's output.
    assert!(out.outputs["research"].starts_with("research<-intake<-the spec"));
    // Gates contribute their decision text as the next stage's input.
    assert_eq!(out.outputs["commit"], "commit<-approved");
}

#[test]
fn pipeline_parks_at_the_plan_gate_then_resumes() {
    let store = OperationStore::open_in_memory().unwrap();
    let exec = ScriptedExec {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let r = runner(&store, &exec, &AcceptAllEvaluator);
    // First run parks at the plan gate.
    let err = r.run("spec").unwrap_err();
    assert_eq!(err.code, "PIPELINE_GATE");
    assert!(err.cause.contains("run1:plan"));
    // Approve the gate, resume. Now it parks at review.
    let gate = store.get("run1:plan").unwrap().unwrap();
    store
        .transition(
            "run1:plan",
            gate.version,
            OperationStatus::Completed,
            json!({
                "phase": "gate",
                "decision": "approved",
                "result": {"output": "approved"},
                "request": {"run_id": "run1", "stage": "plan", "input": ""},
            }),
        )
        .unwrap();
    let err = r.run("spec").unwrap_err();
    assert_eq!(err.code, "PIPELINE_GATE");
    assert!(err.cause.contains("run1:review"));
    // Approve review too; pipeline completes.
    let gate = store.get("run1:review").unwrap().unwrap();
    store
        .transition(
            "run1:review",
            gate.version,
            OperationStatus::Completed,
            json!({
                "phase": "gate",
                "decision": "approved",
                "result": {"output": "approved"},
                "request": {"run_id": "run1", "stage": "review", "input": ""},
            }),
        )
        .unwrap();
    let out = r.run("spec").unwrap();
    assert_eq!(out.outputs.len(), 6);
    // Completed stages were NOT re-executed across the three runs.
    let total = exec.calls.load(Ordering::SeqCst);
    assert_eq!(total, 4, "intake, research, implement, commit once each");
}

#[test]
fn denied_gate_stops_the_pipeline() {
    let store = OperationStore::open_in_memory().unwrap();
    let exec = ScriptedExec {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    store
        .create(
            "run1:plan",
            "pipeline.gate",
            json!({
                "phase": "gate",
                "request": {"run_id": "run1", "stage": "plan", "input": ""},
                "decision": "denied",
                "result": {"output": "denied"},
            }),
        )
        .unwrap();
    let r = runner(&store, &exec, &AcceptAllEvaluator);
    let err = r.run("spec").unwrap_err();
    assert_eq!(err.code, "PIPELINE_DENIED");
}

#[test]
fn implement_loops_until_the_evaluator_accepts() {
    struct RejectTwice(Arc<AtomicUsize>);
    impl StageEvaluator for RejectTwice {
        fn accept(&self, _s: &str, _o: &str) -> Result<bool, PantheonError> {
            // Reject the first two evaluations, accept the third.
            let n = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(n >= 2)
        }
    }
    let store = OperationStore::open_in_memory().unwrap();
    let exec = ScriptedExec {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    for stage in GATES_AFTER {
        store
            .create(
                format!("run1:{stage}"),
                "pipeline.gate",
                json!({
                    "phase": "gate",
                    "request": {"run_id": "run1", "stage": stage, "input": ""},
                    "decision": "approved",
                    "result": {"output": "approved"},
                }),
            )
            .unwrap();
    }
    let rejects = Arc::new(AtomicUsize::new(0));
    let ev = RejectTwice(rejects.clone());
    let r = runner(&store, &exec, &ev);
    let out = r.run("spec").unwrap();
    // Three evaluations: reject, reject, accept.
    assert_eq!(rejects.load(Ordering::SeqCst), 3);
    assert!(out.outputs["implement"].contains("implement<-"));
    // Iteration ops are distinct and durable.
    assert!(store.get("run1:implement#0").unwrap().is_some());
    assert!(store.get("run1:implement#1").unwrap().is_some());
    assert!(store.get("run1:implement#2").unwrap().is_some());
}

#[test]
fn implement_fails_after_max_iterations() {
    struct AlwaysReject;
    impl StageEvaluator for AlwaysReject {
        fn accept(&self, _: &str, _: &str) -> Result<bool, PantheonError> {
            Ok(false)
        }
    }
    let store = OperationStore::open_in_memory().unwrap();
    let exec = ScriptedExec {
        calls: Arc::new(AtomicUsize::new(0)),
    };
    for stage in GATES_AFTER {
        store
            .create(
                format!("run1:{stage}"),
                "pipeline.gate",
                json!({
                    "phase": "gate",
                    "request": {"run_id": "run1", "stage": stage, "input": ""},
                    "decision": "approved",
                    "result": {"output": "approved"},
                }),
            )
            .unwrap();
    }
    let r = runner(&store, &exec, &AlwaysReject);
    let err = r.run("spec").unwrap_err();
    assert_eq!(err.code, "PIPELINE_EVAL_LOOP");
}
