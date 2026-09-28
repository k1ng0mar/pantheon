//! Reflection eval-gating and approval evals: a regressing tagged eval
//! rejects the proposal, and a denied approval blocks application.

use pantheon_api::events::Event;
use pantheon_reflect::eval_gate::{EvalOutcome, EvalRunner};
use pantheon_reflect::{PassInput, ProposalKind, ReflectConfig};
use pantheon_storage::Ledger;
use std::path::Path;
use std::time::Duration;

/// Fails every eval target: simulates a regressing tagged eval.
struct FailAll;
impl EvalRunner for FailAll {
    fn run_eval(&self, target: &str, _timeout: Duration) -> EvalOutcome {
        EvalOutcome::Fail(format!("regressing: {target}"))
    }
}

/// Passes every eval target.
struct PassAll;
impl EvalRunner for PassAll {
    fn run_eval(&self, _target: &str, _timeout: Duration) -> EvalOutcome {
        EvalOutcome::Pass
    }
}

fn seed_repeated_sequence(data_dir: &Path) {
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

fn run_pass(data_dir: &Path, runner: &dyn EvalRunner) -> pantheon_reflect::PassOutput {
    pantheon_reflect::run_pass(
        PassInput {
            data_dir,
            config: ReflectConfig::default(),
            lookback_ms: None,
            dry_run: false,
            model_policy: None,
            llm: None,
        },
        runner,
    )
    .expect("reflection pass")
}

fn skill_proposal_id(out: &pantheon_reflect::PassOutput) -> &str {
    &out.proposals
        .iter()
        .find(|p| matches!(p.kind, ProposalKind::Skill { .. }))
        .expect("a skill proposal")
        .id
}

#[test]
fn regressing_tagged_eval_rejects_proposal() {
    let dir = tempfile::tempdir().unwrap();
    seed_repeated_sequence(dir.path());

    let out = run_pass(dir.path(), &FailAll);
    assert_eq!(out.rejected.len(), 1, "the skill is rejected by the gate");
    assert!(
        out.pending.is_empty(),
        "a rejected proposal never awaits approval"
    );
    // Nothing reached the skills dir.
    assert!(
        !dir.path().join("skills").exists(),
        "rejected skill wrote nothing"
    );
    let (_, reason) = &out.rejected[0];
    assert!(
        reason.contains("regressing"),
        "rejection names the failing eval: {reason}"
    );
}

#[test]
fn denied_approval_blocks_skill_application() {
    let dir = tempfile::tempdir().unwrap();
    seed_repeated_sequence(dir.path());

    let out = run_pass(dir.path(), &PassAll);
    assert_eq!(out.pending.len(), 1, "eval-passed skill awaits approval");
    let id = skill_proposal_id(&out).to_string();

    pantheon_reflect::deny_pending(dir.path(), &id, "test-operator").expect("deny pending");
    // The skill file must not exist: denial blocked application.
    let skills_dir = dir.path().join("skills");
    let wrote_anything = walkdir_count(&skills_dir);
    assert_eq!(wrote_anything, 0, "denied proposal wrote no skill files");
    // And it left the pending queue.
    let pending = pantheon_reflect::apply::load_pending(dir.path()).expect("load pending");
    assert!(pending.is_empty(), "denied proposal left pending");
}

#[test]
fn approved_proposal_applies_skill() {
    let dir = tempfile::tempdir().unwrap();
    seed_repeated_sequence(dir.path());

    let out = run_pass(dir.path(), &PassAll);
    let id = skill_proposal_id(&out).to_string();

    let outcome =
        pantheon_reflect::approve_pending(dir.path(), &id, "test-operator").expect("approve");
    assert!(
        outcome.describe().contains("skill"),
        "approval applied the skill: {}",
        outcome.describe()
    );
    assert_eq!(
        walkdir_count(&dir.path().join("skills")),
        1,
        "approved skill wrote exactly one SKILL.md"
    );
}

/// Count files under a dir; 0 when the dir does not exist.
fn walkdir_count(dir: &Path) -> usize {
    let mut n = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                n += 1;
            }
        }
    }
    n
}

#[test]
fn dry_run_writes_and_applies_nothing() {
    // A correction would normally auto-apply a lesson; dry-run must not.
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    ledger
        .append(&Event::RunStarted {
            run_id: "run_x".into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnStarted {
            run_id: "run_x".into(),
            turn_id: "turn_0".into(),
        })
        .unwrap();
    ledger
        .append(&Event::SteeringProvided {
            run_id: "run_x".into(),
            text: "use tabs".into(),
        })
        .unwrap();
    drop(ledger);

    let runner = PassAll;
    let out = pantheon_reflect::run_pass(
        PassInput {
            data_dir: dir.path(),
            config: ReflectConfig::default(),
            lookback_ms: None,
            dry_run: true,
            model_policy: None,
            llm: None,
        },
        &runner,
    )
    .expect("reflection pass");

    assert!(
        !out.proposals.is_empty(),
        "dry-run still generates proposals"
    );
    assert!(out.applied.is_empty() && out.pending.is_empty() && out.rejected.is_empty());
    assert!(
        !dir.path().join("reflect.jsonl").exists(),
        "dry-run writes no audit records"
    );
    assert!(
        !dir.path().join("reflect-pending.json").exists(),
        "dry-run queues no pending proposals"
    );
    assert!(
        !dir.path().join("memory.db").exists(),
        "dry-run applies no memory lessons"
    );
}

#[test]
fn empty_pass_still_writes_completion_record() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    ledger
        .append(&Event::RunStarted {
            run_id: "run_q".into(),
        })
        .unwrap();
    drop(ledger);

    let runner = PassAll;
    let out = pantheon_reflect::run_pass(
        PassInput {
            data_dir: dir.path(),
            config: ReflectConfig::default(),
            lookback_ms: None,
            dry_run: false,
            model_policy: None,
            llm: None,
        },
        &runner,
    )
    .expect("reflection pass");

    assert!(out.proposals.is_empty(), "no signals, no proposals");
    let text = std::fs::read_to_string(dir.path().join("reflect.jsonl")).expect("audit log exists");
    assert!(
        text.contains("\"pass_completed\""),
        "empty pass still writes its completion record"
    );
    assert!(
        pantheon_reflect::last_run_summary(dir.path()).is_some(),
        "status can summarize the empty pass"
    );
}
