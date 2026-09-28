//! Phase 11: declarative orchestration pipeline.
//!
//! A pipeline is a fixed sequence of stages:
//! intake -> research -> plan -> implement -> review -> commit.
//!
//! Each stage is a durable operation (`tool.pipeline`) whose state records
//! the phase (translate/execute/translate_result) and stage inputs/outputs.
//! Two human gates are mandatory: after `plan` and after `review`, where
//! the stage parks in `awaiting` until an approval arrives. The
//! generator/evaluator loop lives inside `implement`: run, evaluate, and
//! only proceed when the evaluator accepts.
//!
//! The pipeline never spawns runs directly; it drives a `StageExecutor`
//! (the seam that maps a stage request to chat turns), so tests use a
//! scripted test executor and the CLI wires the real runtime session.

use pantheon_api::error::PantheonError;
use pantheon_storage::{Operation, OperationStore};
use serde_json::{json, Value};

/// The fixed stage order. Human gates sit after plan and review.
pub const STAGES: [&str; 6] = [
    "intake",
    "research",
    "plan",
    "implement",
    "review",
    "commit",
];
pub const GATES_AFTER: [&str; 2] = ["plan", "review"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageKind {
    Model,
    Gate,
}

pub fn stage_kind(stage: &str) -> StageKind {
    match stage {
        "plan" | "review" => StageKind::Gate,
        _ => StageKind::Model,
    }
}

/// Map a stage to the executor. Implementations drive chat turns with the
/// stage's prompt; the test double returns scripted answers.
pub trait StageExecutor: Send + Sync {
    fn run_stage(&self, stage: &str, input: &str) -> Result<String, PantheonError>;
}

/// Evaluate implement output: a real evaluator asks the model to critique
/// its own work; the test double is scripted. Returning Ok(true) accepts.
pub trait StageEvaluator: Send + Sync {
    fn accept(&self, stage: &str, output: &str) -> Result<bool, PantheonError>;
}

/// A no-op evaluator that accepts everything (used when no evaluator is
/// configured; the review gate still applies).
pub struct AcceptAllEvaluator;
impl StageEvaluator for AcceptAllEvaluator {
    fn accept(&self, _stage: &str, _output: &str) -> Result<bool, PantheonError> {
        Ok(true)
    }
}

/// Execute one model stage durably. Reuses the operation state machine:
/// on crash, the resumed run picks up from the persisted phase and never
/// repeats an executor call whose output already landed.
pub fn run_model_stage(
    store: &OperationStore,
    op_id: &str,
    stage: &str,
    input: &str,
    exec: &dyn StageExecutor,
) -> Result<Operation, PantheonError> {
    let adapter = StageAdapter {
        exec,
        stage: stage.to_string(),
        input: input.to_string(),
    };
    super::operation::run_tool_operation(
        store,
        op_id,
        "pipeline.stage",
        json!({
            "stage": stage,
            "input": input,
        }),
        &adapter,
    )
}

struct StageAdapter<'a> {
    exec: &'a dyn StageExecutor,
    stage: String,
    input: String,
}

impl super::operation::ToolOperationAdapter for StageAdapter<'_> {
    fn translate(&self, request: &Value) -> Result<Value, PantheonError> {
        Ok(request.clone())
    }
    fn execute(&self, translated: &Value) -> Result<Value, PantheonError> {
        let stage = translated
            .get("stage")
            .and_then(Value::as_str)
            .unwrap_or(&self.stage);
        let input = translated
            .get("input")
            .and_then(Value::as_str)
            .unwrap_or(&self.input);
        let output = self.exec.run_stage(stage, input)?;
        Ok(json!({"output": output}))
    }
    fn translate_result(&self, result: &Value) -> Result<Value, PantheonError> {
        Ok(result.clone())
    }
}
