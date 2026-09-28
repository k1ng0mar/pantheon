//! Reflection signal → proposal evals: the deterministic pipeline turns
//! ledger signals into the right proposal kinds with provenance, without
//! any model call.

use pantheon_api::events::Event;
use pantheon_reflect::eval_gate::{EvalOutcome, EvalRunner};
use pantheon_reflect::{PassInput, ProposalKind, ReflectConfig};
use pantheon_storage::Ledger;
use std::path::Path;
use std::time::Duration;

/// Eval runner that passes everything. Keeps these evals about the
/// signal→proposal mapping, not the subprocess gate (covered in
/// reflect_gates.rs).
struct PassAll;
impl EvalRunner for PassAll {
    fn run_eval(&self, _target: &str, _timeout: Duration) -> EvalOutcome {
        EvalOutcome::Pass
    }
}

fn seed_ledger(path: &Path) -> Ledger {
    Ledger::open(path).expect("open ledger")
}

fn successful_sequence_turn(ledger: &Ledger, run: &str, turn: &str) {
    ledger
        .append(&Event::RunStarted { run_id: run.into() })
        .unwrap();
    ledger
        .append(&Event::TurnStarted {
            run_id: run.into(),
            turn_id: turn.into(),
        })
        .unwrap();
    for (i, tool) in ["shell", "read"].iter().enumerate() {
        ledger
            .append(&Event::ToolStarted {
                run_id: run.into(),
                call_id: format!("call_{turn}_{i}"),
                tool: tool.to_string(),
                args: "{}".into(),
                provenance: pantheon_api::provenance::Provenance::user("eval"),
            })
            .unwrap();
    }
    ledger
        .append(&Event::TurnCompleted {
            run_id: run.into(),
            turn_id: turn.into(),
            outcome: "answered".into(),
        })
        .unwrap();
}

fn run_pass_no_llm(data_dir: &Path) -> pantheon_reflect::PassOutput {
    let runner = PassAll;
    pantheon_reflect::run_pass(
        PassInput {
            data_dir,
            config: ReflectConfig::default(),
            lookback_ms: None,
            dry_run: false,
            model_policy: None,
            llm: None,
        },
        &runner,
    )
    .expect("reflection pass")
}

#[test]
fn repeated_successful_tool_sequence_becomes_skill_with_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = seed_ledger(&dir.path().join("ledger.db"));
    // The same [shell, read] sequence succeeding in 3 separate runs.
    for (i, run) in ["run_a", "run_b", "run_c"].iter().enumerate() {
        successful_sequence_turn(&ledger, run, &format!("turn_{i}"));
    }
    drop(ledger);

    let out = run_pass_no_llm(dir.path());
    let skills: Vec<_> = out
        .proposals
        .iter()
        .filter(|p| matches!(p.kind, ProposalKind::Skill { .. }))
        .collect();
    assert_eq!(skills.len(), 1, "one skill from the repeated sequence");
    let skill = skills[0];
    // Provenance: the skill names the runs it learned from.
    for run in ["run_a", "run_b", "run_c"] {
        assert!(
            skill.provenance_runs.contains(&run.to_string()),
            "skill provenance names {run}: {:?}",
            skill.provenance_runs
        );
    }
    assert!(
        !skill.provenance_turns.is_empty(),
        "skill carries turn-level provenance"
    );
    // Skill proposals are eval-gated, so they wait for approval, not
    // auto-applied.
    assert_eq!(out.pending.len(), 1, "skill waits for approval");
    assert!(out.applied.is_empty(), "no memory lessons in this scenario");
}

#[test]
fn user_correction_becomes_memory_lesson_not_skill() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = seed_ledger(&dir.path().join("ledger.db"));
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
            text: "stop summarizing, just show the diff".into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnCompleted {
            run_id: "run_x".into(),
            turn_id: "turn_0".into(),
            outcome: "answered".into(),
        })
        .unwrap();
    drop(ledger);

    let out = run_pass_no_llm(dir.path());
    assert!(
        out.proposals
            .iter()
            .all(|p| !matches!(p.kind, ProposalKind::Skill { .. })),
        "a user correction must never become a skill proposal"
    );
    let lessons: Vec<_> = out
        .proposals
        .iter()
        .filter(|p| matches!(p.kind, ProposalKind::MemoryLesson { .. }))
        .collect();
    assert_eq!(lessons.len(), 1, "the correction becomes one lesson");
    assert!(
        lessons[0].provenance_runs.contains(&"run_x".to_string()),
        "lesson provenance names the corrected run"
    );
    // Memory lessons auto-apply at the Memory trust tier.
    assert_eq!(out.applied.len(), 1, "lesson auto-applied");
    assert!(out.pending.is_empty(), "nothing awaits approval");
}

#[test]
fn repeated_preference_steering_becomes_persona_held_for_approval() {
    // Three steered turns across two runs, all asking for brevity.
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    let steer = |run: &str, turn: &str, text: &str| {
        ledger
            .append(&Event::RunStarted { run_id: run.into() })
            .unwrap();
        ledger
            .append(&Event::TurnStarted {
                run_id: run.into(),
                turn_id: turn.into(),
            })
            .unwrap();
        ledger
            .append(&Event::SteeringProvided {
                run_id: run.into(),
                text: text.into(),
            })
            .unwrap();
        ledger
            .append(&Event::TurnCompleted {
                run_id: run.into(),
                turn_id: turn.into(),
                outcome: "answered".into(),
            })
            .unwrap();
    };
    steer("run_a", "t1", "please be more concise");
    steer("run_a", "t2", "too long — be concise");
    steer("run_b", "t3", "concise answers only");
    drop(ledger);

    let out = run_pass_no_llm(dir.path());
    let persona: Vec<_> = out
        .proposals
        .iter()
        .filter(|p| matches!(p.kind, pantheon_reflect::ProposalKind::Persona { .. }))
        .collect();
    assert_eq!(
        persona.len(),
        1,
        "one persona proposal from repeated brevity steering"
    );
    // Persona proposals are NOT auto-applied: they wait for approval.
    // (The three corrections each also yield an auto-applied lesson —
    // that is the existing correction path, unaffected.)
    assert!(
        !out.applied.iter().any(|a| a.proposal_id == persona[0].id),
        "persona proposal is held, not auto-applied"
    );
    assert_eq!(out.pending.len(), 1);
    assert_eq!(out.pending[0].proposal.id, persona[0].id);
    assert!(
        !out.pending[0].pass_id.is_empty(),
        "pending carries the originating pass id"
    );
}
