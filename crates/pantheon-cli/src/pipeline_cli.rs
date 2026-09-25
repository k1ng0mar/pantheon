//! `pantheon pipeline`: run the orchestration pipeline against the runtime.
//!
//! The StageExecutor drives `pantheon chat`-style sessions per stage; the
//! evaluator is a second session asked to critique. Human gates park the
//! pipeline; `pantheon pipeline <run_id> --approve <stage>` / `--deny
//! <stage>` resolve them and resume.

use crate::config_doc::Config;
use crate::config_schema::PolicyPreset;
use pantheon_runtime::pipeline::{AcceptAllEvaluator, StageEvaluator, StageExecutor};
use pantheon_runtime::pipeline_runner::PipelineRunner;
use pantheon_runtime::Supervisor;
use pantheon_storage::OperationStatus;
use std::path::PathBuf;

fn policy_for(cfg: Option<&Config>) -> pantheon_core::capability::Policy {
    match cfg.and_then(|c| c.policy) {
        Some(PolicyPreset::Reader) => pantheon_core::capability::Policy::researcher_readonly(),
        Some(PolicyPreset::CoderMemory) => pantheon_core::capability::Policy::coder_with_memory(),
        _ => pantheon_core::capability::Policy::coder(),
    }
}

/// Real executor: one Session per stage call, prompt framed per stage.
struct RuntimeExecutor {
    data_dir: PathBuf,
    policy: pantheon_core::capability::Policy,
}

impl StageExecutor for RuntimeExecutor {
    fn run_stage(
        &self,
        stage: &str,
        input: &str,
    ) -> Result<String, pantheon_core::error::PantheonError> {
        let session = open_session(&self.data_dir, self.policy.clone())?;
        let run_id = format!("pipe-{stage}-{}", pantheon_runtime::new_run_id());
        let prompt = match stage {
            "intake" => format!("Restate the following task as a precise, testable specification. Output only the specification.\n\n{input}"),
            "research" => format!("Gather the context and constraints needed for this specification. List facts, unknowns, and risks. Output only the list.\n\n{input}"),
            "plan" => format!("Produce a step-by-step implementation plan for this specification. Output only the plan.\n\n{input}"),
            "implement" => format!("Execute this plan. Produce the work product and a summary of what you did.\n\n{input}"),
            "review" => format!("Review this work against the plan. List defects and confirm or reject. Output the verdict and reasons.\n\n{input}"),
            "commit" => format!("Finalize: produce the commit message and summary for this work.\n\n{input}"),
            other => format!("{other}: {input}"),
        };
        match session.chat(&run_id, &prompt) {
            Ok(pantheon_agent::LoopOutcome::Answered { text, .. }) => Ok(text),
            Ok(other) => Ok(format!("[stage {stage} ended: {other:?}]")),
            Err(e) => Err(e),
        }
    }
}

/// Real evaluator: a separate session asked to accept or reject.
struct RuntimeEvaluator {
    data_dir: PathBuf,
    policy: pantheon_core::capability::Policy,
}

impl StageEvaluator for RuntimeEvaluator {
    fn accept(
        &self,
        stage: &str,
        output: &str,
    ) -> Result<bool, pantheon_core::error::PantheonError> {
        let session = open_session(&self.data_dir, self.policy.clone())?;
        let run_id = format!("pipe-eval-{}", pantheon_runtime::new_run_id());
        let prompt = format!(
            "You are a strict evaluator. Does the following {stage} output meet its goal? \
Reply with exactly ACCEPT or REJECT followed by one reason line.\n\n{output}"
        );
        let answer = match session.chat(&run_id, &prompt) {
            Ok(pantheon_agent::LoopOutcome::Answered { text: t, .. }) => t.to_ascii_lowercase(),
            _ => return Ok(false),
        };
        Ok(answer.starts_with("accept"))
    }
}

fn open_session(
    data_dir: &PathBuf,
    policy: pantheon_core::capability::Policy,
) -> Result<pantheon_runtime::session::Session, pantheon_core::error::PantheonError> {
    let cfg = Config::load(data_dir).ok();
    let (provider, model) = cfg
        .as_ref()
        .and_then(|c| c.model.clone())
        .map(|m| (m.provider, m.model))
        .unwrap_or_else(|| {
            (
                std::env::var("PANTHEON_PROVIDER").unwrap_or_else(|_| "local".into()),
                std::env::var("PANTHEON_MODEL").unwrap_or_else(|_| "llama3.2".into()),
            )
        });
    let model_policy = pantheon_core::model::ModelPolicy {
        default: pantheon_core::model::DefaultModel { provider, model },
        fallbacks: pantheon_core::model::FallbackChain::default(),
        auxiliaries: crate::config_doc::auxiliaries(cfg.as_ref()),
    };
    let secrets = crate::config_doc::chat_secrets(cfg.as_ref());
    pantheon_runtime::session::Session::new(data_dir.clone(), policy, model_policy, secrets)
}

pub fn cmd_pipeline(args: &[String]) {
    let parsed = crate::cli_args::Args::parse(&args[2.min(args.len())..]);
    // First positional (if any) is the run id.
    let run_id = parsed
        .positional(0)
        .unwrap_or_else(|| format!("pipe-{}", pantheon_runtime::new_run_id()));
    let approve = parsed.flag("approve");
    let deny = parsed.flag("deny");
    let spec = parsed.flag("spec");

    let data_dir = crate::data_dir();
    let sup = Supervisor::open(data_dir.clone()).unwrap_or_else(|e| {
        eprintln!("pipeline: open runtime: {e}");
        std::process::exit(1);
    });

    // Gate resolution mode.
    let denied_flag = approve.is_none();
    if let Some(stage) = approve.clone().or_else(|| deny.clone()) {
        let denied = denied_flag;
        let op_id = format!("{run_id}:{stage}");
        let store = sup.operations();
        let op = match store.get(&op_id) {
            Ok(Some(op)) => op,
            _ => {
                eprintln!("pipeline: no gate {op_id}");
                std::process::exit(1);
            }
        };
        if op.status == OperationStatus::Completed || op.status == OperationStatus::Failed {
            eprintln!("pipeline: gate {op_id} already settled");
            std::process::exit(1);
        }
        let mut state = op.state.clone();
        state["decision"] = serde_json::json!(if denied { "denied" } else { "approved" });
        let next = if denied {
            OperationStatus::Failed
        } else {
            OperationStatus::Completed
        };
        match store.transition(&op_id, op.version, next, state) {
            Ok(_) => {
                println!(
                    "gate {op_id} {}",
                    if denied { "denied" } else { "approved" }
                );
                return;
            }
            Err(e) => {
                eprintln!("pipeline: {e}");
                std::process::exit(1);
            }
        }
    }

    // Run mode.
    let Some(spec) = spec else {
        eprintln!(
            "usage: pantheon pipeline [--spec \"task\"] | <run_id> --approve <stage> | <run_id> --deny <stage>"
        );
        std::process::exit(2);
    };

    // Record the run in the ledger for observability.
    sup.start_run(&run_id).unwrap_or_else(|e| {
        eprintln!("pipeline: start run: {e}");
        std::process::exit(1);
    });

    let policy = policy_for(Config::load(&data_dir).ok().as_ref());
    let exec = RuntimeExecutor {
        data_dir: data_dir.clone(),
        policy: policy.clone(),
    };
    // The evaluator runs only when PANTHEON_PIPELINE_EVAL=1; otherwise the
    // accept-all evaluator applies (the review gate still gates).
    let eval_enabled = std::env::var("PANTHEON_PIPELINE_EVAL")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let noop = AcceptAllEvaluator;
    let strict = RuntimeEvaluator {
        data_dir: data_dir.clone(),
        policy,
    };
    let runner = PipelineRunner {
        store: sup.operations(),
        run_id: run_id.clone(),
        executor: &exec,
        evaluator: if eval_enabled { &strict } else { &noop },
        max_iterations: 3,
    };
    match runner.run(&spec) {
        Ok(outcome) => {
            for (stage, text) in &outcome.outputs {
                println!("== {stage} ==");
                println!("{text}");
                println!();
            }
            sup.complete(&run_id).unwrap_or_else(|e| {
                eprintln!("pipeline: complete run: {e}");
            });
        }
        Err(e) => match e.code.as_str() {
            "PIPELINE_GATE" => {
                println!("run id: {run_id}");
                println!("pipeline parked: {e}");
                println!("approve with: pantheon pipeline {run_id} --approve <stage>");
                println!("deny with:    pantheon pipeline {run_id} --deny <stage>");
            }
            _ => {
                eprintln!("pipeline: {e}");
                sup.fail(&run_id, &e.code).unwrap_or_else(|x| {
                    eprintln!("pipeline: mark failed: {x}");
                });
                std::process::exit(1);
            }
        },
    }
}
