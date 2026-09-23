//! The pipeline runner: walks stages, parks on gates, resumes cleanly.

use super::pipeline::{
    run_model_stage, stage_kind, StageEvaluator, StageExecutor, StageKind, STAGES,
};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_storage::{Operation, OperationStatus, OperationStore};
use serde_json::{json, Value};

fn perr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Agent,
        false,
        cause,
        "inspect the pipeline operation or grant/deny the gate scope",
        "",
    )
}

/// One pipeline execution. `run_id` namespaces every stage operation
/// (`{run_id}:{stage}`), so resume is per stage.
pub struct PipelineRunner<'a> {
    pub store: &'a OperationStore,
    pub run_id: String,
    pub executor: &'a dyn StageExecutor,
    pub evaluator: &'a dyn StageEvaluator,
    /// Max generator/evaluator iterations inside implement before failing.
    pub max_iterations: u32,
}

impl<'a> PipelineRunner<'a> {
    /// Run the whole pipeline. Returns the stage outputs keyed by stage.
    /// Parks: returns `Err(PIPELINE_GATE)` when a gate needs an answer; the
    /// caller grants/denies through the normal approval path, then re-calls
    /// `run` (resume is safe: completed stages are skipped).
    pub fn run(&self, initial_input: &str) -> Result<PipelineOutcome, PantheonError> {
        let mut input = initial_input.to_string();
        let mut outputs = std::collections::BTreeMap::new();
        for stage in STAGES {
            // Skip stages already completed on a previous resume.
            let op_id = format!("{}:{stage}", self.run_id);
            if let Some(existing) = self.store.get(&op_id)? {
                if existing.status == OperationStatus::Completed {
                    let out = existing
                        .state
                        .get("result")
                        .and_then(|r| r.get("output"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    outputs.insert(stage.to_string(), out.clone());
                    input = out;
                    continue;
                }
                if existing.status == OperationStatus::Canceling {
                    return Err(perr(
                        "PIPELINE_CANCELING",
                        format!("stage {stage} is canceling"),
                    ));
                }
            }
            match stage_kind(stage) {
                StageKind::Model => {
                    // Generator/evaluator loop inside implement.
                    if stage == "implement" {
                        let out = self.run_implement(&op_id, &input)?;
                        outputs.insert(stage.to_string(), out.clone());
                        input = out;
                        continue;
                    }
                    let op = run_model_stage(self.store, &op_id, stage, &input, self.executor)?;
                    let out = extract_output(&op)?;
                    outputs.insert(stage.to_string(), out.clone());
                    input = out;
                }
                StageKind::Gate => {
                    // Record the stage as awaiting with the pending decision,
                    // then park the whole pipeline.
                    let gate = self.ensure_gate(&op_id, stage, &input)?;
                    let decision = gate
                        .state
                        .get("decision")
                        .and_then(Value::as_str)
                        .unwrap_or("pending")
                        .to_string();
                    if gate.status != OperationStatus::Completed {
                        if decision == "denied" || gate.status == OperationStatus::Failed {
                            return Err(perr(
                                "PIPELINE_DENIED",
                                format!("stage {stage} was denied; pipeline stops"),
                            ));
                        }
                        if decision == "pending" {
                            return Err(perr(
                                "PIPELINE_GATE",
                                format!(
                                    "pipeline parked: approve or deny scope {op_id} to continue"
                                ),
                            ));
                        }
                    }
                    if decision == "denied" {
                        return Err(perr(
                            "PIPELINE_DENIED",
                            format!("stage {stage} was denied; pipeline stops"),
                        ));
                    }
                    let out = gate
                        .state
                        .get("result")
                        .and_then(|r| r.get("output"))
                        .and_then(Value::as_str)
                        .unwrap_or("approved")
                        .to_string();
                    outputs.insert(stage.to_string(), out.clone());
                    input = out;
                }
            }
        }
        Ok(PipelineOutcome { outputs })
    }

    /// Generator/evaluator loop: run implement, evaluate, iterate on the
    /// critique until accepted or the iteration cap. Each iteration is one
    /// durable operation so a crash resumes mid-loop.
    fn run_implement(&self, base_op_id: &str, input: &str) -> Result<String, PantheonError> {
        let mut current = input.to_string();
        for iteration in 0..self.max_iterations {
            let op_id = format!("{base_op_id}#{iteration}");
            let op = run_model_stage(self.store, &op_id, "implement", &current, self.executor)?;
            let out = extract_output(&op)?;
            let accepted = self.evaluator.accept("implement", &out)?;
            if accepted {
                return Ok(out);
            }
            // Not accepted: the critique becomes the next iteration's input.
            current = out;
        }
        Err(perr(
            "PIPELINE_EVAL_LOOP",
            format!(
                "implement did not converge in {} iterations",
                self.max_iterations
            ),
        ))
    }

    /// Create or reuse the gate operation. A gate whose state already
    /// carries a decision (resume of an approved/denied gate) is settled to
    /// its terminal status on sight; otherwise it is created awaiting.
    fn ensure_gate(
        &self,
        op_id: &str,
        stage: &str,
        input: &str,
    ) -> Result<Operation, PantheonError> {
        if let Some(existing) = self.store.get(op_id)? {
            if existing.status == OperationStatus::Ready {
                // A gate created ahead of time (or resumed) with a decision
                // in its state settles immediately.
                let decision = existing
                    .state
                    .get("decision")
                    .and_then(Value::as_str)
                    .unwrap_or("pending");
                if decision != "pending" {
                    let mut state = existing.state.clone();
                    state["result"] = json!({"output": decision});
                    return self.store.transition(
                        op_id,
                        existing.version,
                        if decision == "denied" {
                            OperationStatus::Failed
                        } else {
                            OperationStatus::Completed
                        },
                        state,
                    );
                }
                return Ok(existing);
            }
            return Ok(existing);
        }
        self.store.create(
            op_id,
            "pipeline.gate",
            json!({
                "phase": "gate",
                "request": {
                    "run_id": self.run_id,
                    "stage": stage,
                    "input": input,
                },
                "decision": "pending",
            }),
        )
    }
}

fn extract_output(op: &Operation) -> Result<String, PantheonError> {
    op.state
        .get("result")
        .and_then(|r| r.get("output"))
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| {
            perr(
                "PIPELINE_STATE",
                format!("stage op {} has no output", op.id),
            )
        })
}

/// What the pipeline produced, per stage.
#[derive(Debug, Clone, PartialEq)]
pub struct PipelineOutcome {
    pub outputs: std::collections::BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::super::pipeline::AcceptAllEvaluator;
    use super::super::pipeline::GATES_AFTER;
    use super::*;
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
                    &format!("run1:{stage}"),
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
                    &format!("run1:{stage}"),
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
                    &format!("run1:{stage}"),
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
}
