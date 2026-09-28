//! Reflection LLM-routing evals: the `LlmRefiner` seam is dead unless
//! `[reflect] enabled = true`, and every call it ever makes runs on the
//! resolved `AuxiliaryKind::Reflection` model — never the chat model.

use pantheon_api::events::Event;
use pantheon_api::model::{
    AuxiliaryKind, AuxiliaryModel, DefaultModel, FallbackChain, ModelPolicy, ReasoningLevel,
};
use pantheon_reflect::eval_gate::{EvalOutcome, EvalRunner};
use pantheon_reflect::{LlmRefiner, PassInput, Proposal, ReflectConfig};
use pantheon_storage::Ledger;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

struct PassAll;
impl EvalRunner for PassAll {
    fn run_eval(&self, _target: &str, _timeout: Duration) -> EvalOutcome {
        EvalOutcome::Pass
    }
}

/// Records every refine call: (kind, provider, model).
struct RecordingRefiner {
    calls: Mutex<Vec<(AuxiliaryKind, String, String)>>,
}
impl RecordingRefiner {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
    fn calls(&self) -> Vec<(AuxiliaryKind, String, String)> {
        self.calls.lock().unwrap().clone()
    }
}
impl LlmRefiner for RecordingRefiner {
    fn refine(&self, model: &AuxiliaryModel, draft: &Proposal) -> Result<String, String> {
        self.calls.lock().unwrap().push((
            model.kind.clone(),
            model.provider.clone(),
            model.model.clone(),
        ));
        Ok(format!("{} (polished)", draft.body))
    }
}

/// One repeated successful sequence so the pass has proposals to refine.
fn seed_sequence(data_dir: &Path) {
    let ledger = Ledger::open(&data_dir.join("ledger.db")).expect("open ledger");
    for (i, run) in ["run_a", "run_b", "run_c"].iter().enumerate() {
        let turn = format!("turn_{i}");
        ledger
            .append(&Event::RunStarted {
                run_id: run.to_string(),
            })
            .unwrap();
        ledger
            .append(&Event::TurnStarted {
                run_id: run.to_string(),
                turn_id: turn.clone(),
            })
            .unwrap();
        for (j, tool) in ["shell", "read"].iter().enumerate() {
            ledger
                .append(&Event::ToolStarted {
                    run_id: run.to_string(),
                    call_id: format!("call_{turn}_{j}"),
                    tool: tool.to_string(),
                    args: "{}".into(),
                    provenance: pantheon_api::provenance::Provenance::untrusted("test"),
                })
                .unwrap();
        }
        ledger
            .append(&Event::TurnCompleted {
                run_id: run.to_string(),
                turn_id: turn,
                outcome: "answered".into(),
            })
            .unwrap();
    }
}

fn policy_with_reflection_pin() -> ModelPolicy {
    ModelPolicy {
        default: DefaultModel {
            provider: "chat-provider".into(),
            model: "chat-model".into(),
        },
        fallbacks: FallbackChain::default(),
        auxiliaries: vec![AuxiliaryModel {
            kind: AuxiliaryKind::Reflection,
            provider: "aux-provider".into(),
            model: "aux-model".into(),
        }],
        reasoning: ReasoningLevel::Off,
        reasoning_budget: None,
    }
}

#[test]
fn disabled_by_default_config_causes_zero_llm_calls() {
    let dir = tempfile::tempdir().unwrap();
    seed_sequence(dir.path());
    let refiner = RecordingRefiner::new();
    let runner = PassAll;

    let out = pantheon_reflect::run_pass(
        PassInput {
            data_dir: dir.path(),
            // Default: enabled = false.
            config: ReflectConfig::default(),
            lookback_ms: None,
            dry_run: false,
            model_policy: None,
            llm: Some(&refiner),
        },
        &runner,
    )
    .expect("reflection pass");

    assert!(
        !out.proposals.is_empty(),
        "the pass still produced proposals deterministically"
    );
    assert!(
        refiner.calls().is_empty(),
        "disabled config must cause zero LLM calls, got {:?}",
        refiner.calls()
    );
}

#[test]
fn enabled_but_no_policy_refuses_llm() {
    let dir = tempfile::tempdir().unwrap();
    seed_sequence(dir.path());
    let refiner = RecordingRefiner::new();
    let runner = PassAll;

    let config = ReflectConfig {
        enabled: true,
        ..ReflectConfig::default()
    };
    let out = pantheon_reflect::run_pass(
        PassInput {
            data_dir: dir.path(),
            config,
            lookback_ms: None,
            dry_run: false,
            // No policy: the Reflection slot cannot be resolved, so the
            // refiner must not be consulted at all.
            model_policy: None,
            llm: Some(&refiner),
        },
        &runner,
    )
    .expect("reflection pass");

    assert!(!out.proposals.is_empty());
    assert!(
        refiner.calls().is_empty(),
        "without a policy the Reflection slot is unresolvable: zero LLM calls, got {:?}",
        refiner.calls()
    );
}

#[test]
fn every_llm_call_runs_on_the_reflection_aux_model() {
    let dir = tempfile::tempdir().unwrap();
    seed_sequence(dir.path());
    let refiner = RecordingRefiner::new();
    let runner = PassAll;
    let policy = policy_with_reflection_pin();

    let config = ReflectConfig {
        enabled: true,
        ..ReflectConfig::default()
    };
    let out = pantheon_reflect::run_pass(
        PassInput {
            data_dir: dir.path(),
            config,
            lookback_ms: None,
            dry_run: false,
            model_policy: Some(&policy),
            llm: Some(&refiner),
        },
        &runner,
    )
    .expect("reflection pass");

    let calls = refiner.calls();
    assert_eq!(
        calls.len(),
        out.proposals.len(),
        "every proposal was refined exactly once"
    );
    for (kind, provider, model) in &calls {
        assert_eq!(
            *kind,
            AuxiliaryKind::Reflection,
            "refine ran on the Reflection aux slot"
        );
        assert_eq!(provider, "aux-provider", "refine used the pinned provider");
        assert_eq!(model, "aux-model", "refine used the pinned model");
        assert_ne!(provider, "chat-provider", "never the chat provider");
        assert_ne!(model, "chat-model", "never the chat model");
    }
    // The enrichment actually landed on the bodies.
    assert!(
        out.proposals.iter().all(|p| p.body.ends_with("(polished)")),
        "refined bodies carried the aux model's output"
    );
}
