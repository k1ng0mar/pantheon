//! Nightly behavioral evals: the unified self-improvement pass over a
//! real ledger and memory store.
//!
//! Covers the SkillOpt learning rule end to end:
//! - strict improvement reaches pending approval; equal / lower /
//!   missing / erroring / non-finite replay scores reject
//! - recurring memory (N sessions in M days) promotes; one-offs don't
//! - human denial blocks application; approval applies
//! - one pass feeds proposals and memory from a single ledger scan
//! - dry run applies nothing
//!
//! The scripted eval/replay runners stand in for `pantheon-eval`
//! targets and a headless agent; the ledger, memory store, pending
//! queue, skill files, and audit log are all real.

use pantheon_api::capability::Policy;
use pantheon_api::events::Event;
use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, DefaultModel, ModelPolicy};
use pantheon_api::provenance::Provenance;
use pantheon_memory::LayerKind;
use pantheon_nightly::{
    decide, load_pending, run_pass, EvalOutcome, EvalRunner, NightlyConfig, NightlyDeps,
    NightlyEvent, NightlyLlm, Proposal, ProposalStatus, ReplayCheck, ReplayRunner, ReplayStore,
    ReplayTask,
};
use pantheon_storage::Ledger;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

struct Harness {
    dir: tempfile::TempDir,
    ledger: Ledger,
    backend: Arc<dyn pantheon_memory::MemoryBackend>,
    policy: Policy,
    model_policy: ModelPolicy,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    let backend = pantheon_memory::open_selected(dir.path()).unwrap();
    Harness {
        dir,
        ledger,
        backend,
        policy: Policy::coder_with_memory(),
        model_policy: ModelPolicy {
            default: DefaultModel {
                provider: "chat-provider".into(),
                model: "chat-model".into(),
            },
            fallbacks: Default::default(),
            auxiliaries: vec![],
            reasoning: Default::default(),
            reasoning_budget: None,
        },
    }
}

/// One successful turn running the read → exec tool sequence.
fn sequence_run(ledger: &Ledger, run_id: &str) {
    ledger
        .append(&Event::RunStarted {
            run_id: run_id.into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnStarted {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
        })
        .unwrap();
    for (i, tool) in ["read", "exec"].iter().enumerate() {
        ledger
            .append(&Event::ToolStarted {
                run_id: run_id.into(),
                call_id: format!("call_0_{i}"),
                tool: tool.to_string(),
                args: "{}".into(),
                provenance: Provenance::system("eval"),
            })
            .unwrap();
    }
    ledger
        .append(&Event::TurnCompleted {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
            outcome: "answered".into(),
        })
        .unwrap();
}

/// One user steering event in its own run.
fn steer(ledger: &Ledger, run_id: &str, text: &str) {
    ledger
        .append(&Event::RunStarted {
            run_id: run_id.into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnStarted {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
        })
        .unwrap();
    ledger
        .append(&Event::SteeringProvided {
            run_id: run_id.into(),
            text: text.into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnCompleted {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
            outcome: "answered".into(),
        })
        .unwrap();
}

struct PassEval;
impl EvalRunner for PassEval {
    fn run_eval(&self, _target: &str, _timeout: Duration) -> EvalOutcome {
        EvalOutcome::Pass
    }
}

/// Fixed transcripts for the baseline and with-proposal replays.
struct ScriptedReplay {
    base: String,
    with: String,
}
impl ReplayRunner for ScriptedReplay {
    fn run_transcript(
        &self,
        _task: &ReplayTask,
        with_proposal: Option<&Proposal>,
    ) -> Result<String, String> {
        Ok(if with_proposal.is_some() {
            self.with.clone()
        } else {
            self.base.clone()
        })
    }
}

struct ErrorReplay;
impl ReplayRunner for ErrorReplay {
    fn run_transcript(
        &self,
        task: &ReplayTask,
        _with_proposal: Option<&Proposal>,
    ) -> Result<String, String> {
        Err(format!("boom on {}", task.id))
    }
}

/// Judge LLM returning a fixed score string (e.g. "nan").
struct FixedJudge {
    score: String,
}
impl NightlyLlm for FixedJudge {
    fn refine_proposal(
        &self,
        _model: &AuxiliaryModel,
        _draft: &Proposal,
    ) -> Result<String, String> {
        Ok(self.score.clone())
    }
    fn distill_memories(
        &self,
        _model: &AuxiliaryModel,
        texts: &[String],
    ) -> Result<Vec<String>, String> {
        Ok(texts.to_vec())
    }
}

fn contains_task(dir: &Path) -> ReplayTask {
    let mut store = ReplayStore::open(dir).unwrap();
    let task = ReplayTask {
        id: "t1".into(),
        name: "t1".into(),
        prompt: "do the thing".into(),
        check: ReplayCheck::Contains {
            text: "done".into(),
            negate: false,
        },
        exec: None,
    };
    store.save(task.clone()).unwrap();
    task
}

fn judge_task(dir: &Path) -> ReplayTask {
    let mut store = ReplayStore::open(dir).unwrap();
    let task = ReplayTask {
        id: "j1".into(),
        name: "j1".into(),
        prompt: "do the thing".into(),
        check: ReplayCheck::Judge {
            rubric: "was it done well".into(),
        },
        exec: None,
    };
    store.save(task.clone()).unwrap();
    task
}

fn config(dir: &Path) -> NightlyConfig {
    NightlyConfig {
        data_dir: dir.to_path_buf(),
        ..NightlyConfig::default()
    }
}

fn deps<'a>(
    h: &'a Harness,
    eval: &'a dyn EvalRunner,
    replay: &'a dyn ReplayRunner,
    llm: Option<&'a dyn NightlyLlm>,
) -> NightlyDeps<'a> {
    NightlyDeps {
        ledger: &h.ledger,
        backend: h.backend.as_ref(),
        capability_policy: &h.policy,
        model_policy: &h.model_policy,
        eval_runner: eval,
        replay_runner: replay,
        llm,
        repair: None,
    }
}

/// Three successful read → exec runs: the skill signal.
fn seed_skill_signal(h: &Harness) {
    for i in 1..=3 {
        sequence_run(&h.ledger, &format!("run_{i}"));
    }
}

fn skill_proposals(out: &pantheon_nightly::PassResult) -> Vec<&Proposal> {
    out.proposals
        .iter()
        .filter(|p| p.kind_name() == "skill" && matches!(p.status, ProposalStatus::ReplayPassed))
        .collect()
}

#[test]
fn strict_improvement_reaches_pending_approval() {
    let h = harness();
    seed_skill_signal(&h);
    contains_task(h.dir.path());
    let replay = ScriptedReplay {
        base: "started".into(),
        with: "started\ndone".into(),
    };
    let cfg = config(h.dir.path());
    let out = run_pass(&cfg, &mut deps(&h, &PassEval, &replay, None)).unwrap();

    assert_eq!(skill_proposals(&out).len(), 1);
    let pending = load_pending(h.dir.path()).unwrap();
    assert_eq!(pending.len(), 1);
    // Queued, not applied: no SKILL.md yet.
    assert!(!h.dir.path().join("skills").exists());
}

#[test]
fn equal_replay_score_rejects() {
    let h = harness();
    seed_skill_signal(&h);
    contains_task(h.dir.path());
    let replay = ScriptedReplay {
        base: "done".into(),
        with: "done".into(),
    };
    let out = run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &replay, None),
    )
    .unwrap();

    assert!(skill_proposals(&out).is_empty());
    // Fair-measurement failure → bounded fix loop retries, then
    // escalates: never queued, never applied.
    let escalated: Vec<_> = out
        .proposals
        .iter()
        .filter(|p| matches!(p.status, ProposalStatus::NeedsAttention))
        .collect();
    assert_eq!(escalated.len(), 1);
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
    // The loop ran its attempts and recorded the escalation.
    let events = pantheon_nightly::read_events(h.dir.path());
    let attempts = events
        .iter()
        .filter(|e| matches!(e, NightlyEvent::FixAttempt { .. }))
        .count();
    assert_eq!(attempts, NightlyConfig::default().max_fix_attempts);
    assert!(events
        .iter()
        .any(|e| matches!(e, NightlyEvent::Escalated { .. })));
    let recorded = pantheon_nightly::load_escalated(h.dir.path()).unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].id, escalated[0].id);
}

#[test]
fn lower_replay_score_rejects() {
    let h = harness();
    seed_skill_signal(&h);
    contains_task(h.dir.path());
    let replay = ScriptedReplay {
        base: "done".into(),
        with: "started".into(),
    };
    let out = run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &replay, None),
    )
    .unwrap();

    assert!(skill_proposals(&out).is_empty());
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
}

#[test]
fn missing_replay_tasks_rejects() {
    let h = harness();
    seed_skill_signal(&h);
    // No tasks configured at all.
    let replay = ScriptedReplay {
        base: String::new(),
        with: String::new(),
    };
    let out = run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &replay, None),
    )
    .unwrap();

    // Infrastructure failure: not retryable, escalates immediately
    // with zero fix attempts.
    let escalated: Vec<_> = out
        .proposals
        .iter()
        .filter(|p| matches!(p.status, ProposalStatus::NeedsAttention))
        .collect();
    assert_eq!(escalated.len(), 1);
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
    let events = pantheon_nightly::read_events(h.dir.path());
    assert!(
        events
            .iter()
            .filter(|e| matches!(e, NightlyEvent::FixAttempt { .. }))
            .count()
            == 0
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, NightlyEvent::Escalated { .. })));
}

#[test]
fn replay_error_rejects() {
    let h = harness();
    seed_skill_signal(&h);
    contains_task(h.dir.path());
    let out = run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &ErrorReplay, None),
    )
    .unwrap();

    assert!(skill_proposals(&out).is_empty());
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
}

#[test]
fn nonfinite_judge_score_rejects() {
    let h = harness();
    seed_skill_signal(&h);
    judge_task(h.dir.path());
    let judge = FixedJudge {
        score: "nan".into(),
    };
    let mut cfg = config(h.dir.path());
    cfg.llm_enabled = true;
    // The Reflection slot must resolve for the judge to run.
    let mut policy = h.model_policy.clone();
    policy.auxiliaries.push(AuxiliaryModel {
        kind: AuxiliaryKind::Reflection,
        provider: "p".into(),
        model: "m".into(),
        timeout_secs: 60,
        target_percent: None,
    });
    let replay = ScriptedReplay {
        base: "x".into(),
        with: "x".into(),
    };
    let mut d = NightlyDeps {
        ledger: &h.ledger,
        backend: h.backend.as_ref(),
        capability_policy: &h.policy,
        model_policy: &policy,
        eval_runner: &PassEval,
        replay_runner: &replay,
        llm: Some(&judge),
        repair: None,
    };
    let out = run_pass(&cfg, &mut d).unwrap();

    assert!(skill_proposals(&out).is_empty());
    // Non-finite score = broken measurement = infrastructure failure:
    // escalates, never queued.
    assert!(out
        .proposals
        .iter()
        .any(|p| matches!(p.status, ProposalStatus::NeedsAttention)));
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
}

fn nightly_lessons(h: &Harness, query: &str) -> Vec<pantheon_memory::MemoryRecord> {
    h.backend
        .recall(&h.policy, &["nightly"], &[LayerKind::Agent], query, 10)
        .unwrap()
        .into_iter()
        .map(|r| r.record)
        .filter(|r| r.key.starts_with("nightly:"))
        .collect()
}

#[test]
fn recurring_memory_across_sessions_promotes() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
    let replay = ScriptedReplay {
        base: String::new(),
        with: String::new(),
    };
    let out = run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &replay, None),
    )
    .unwrap();

    // Propose-only: the unattended pass queues the lesson, it does not
    // write it. `applied` (auto-applies during the pass) is structurally
    // 0; the candidate sits in the pending queue with its evidence.
    assert_eq!(out.applied, 0);
    assert!(
        out.pending > 0,
        "lesson must be queued, got pending={}",
        out.pending
    );
    let pending = load_pending(h.dir.path()).unwrap();
    assert!(pending.iter().any(|p| p.kind_name() == "lesson"));
    // Nothing durable yet - the record lands only after an operator grant.
    assert!(nightly_lessons(&h, "tabs").is_empty());

    // Approving the queued lesson writes the durable record.
    let id = pending
        .iter()
        .find(|p| p.kind_name() == "lesson")
        .unwrap()
        .id
        .clone();
    decide(h.dir.path(), &config(h.dir.path()), &id, true).unwrap();
    assert_eq!(nightly_lessons(&h, "tabs").len(), 1);
}

#[test]
fn one_off_memory_does_not_promote() {
    let h = harness();
    steer(&h.ledger, "run_1", "always use tabs for indentation");
    let replay = ScriptedReplay {
        base: String::new(),
        with: String::new(),
    };
    let out = run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &replay, None),
    )
    .unwrap();

    // Below the promotion threshold: not even surfaced, so nothing
    // queues and nothing applies.
    assert_eq!(out.applied, 0);
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
    assert!(nightly_lessons(&h, "tabs").is_empty());
}

fn approve_setup() -> (Harness, String) {
    let h = harness();
    seed_skill_signal(&h);
    contains_task(h.dir.path());
    let replay = ScriptedReplay {
        base: "started".into(),
        with: "started\ndone".into(),
    };
    run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &replay, None),
    )
    .unwrap();
    let pending = load_pending(h.dir.path()).unwrap();
    assert_eq!(pending.len(), 1);
    let id = pending[0].id.clone();
    (h, id)
}

#[test]
fn human_denial_blocks_application() {
    let (h, id) = approve_setup();
    let cfg = config(h.dir.path());
    assert!(decide(h.dir.path(), &cfg, &id, false).unwrap());

    // Gone from the queue, never applied.
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
    assert!(!h.dir.path().join("skills").exists());
    // The denial is audited.
    let denied = pantheon_nightly::read_events(h.dir.path())
        .into_iter()
        .any(|e| {
            matches!(
                e,
                NightlyEvent::ApprovalDecided {
                    approved: false,
                    ..
                }
            )
        });
    assert!(denied);
}

#[test]
fn human_approval_applies_skill() {
    let (h, id) = approve_setup();
    let cfg = config(h.dir.path());
    assert!(decide(h.dir.path(), &cfg, &id, true).unwrap());

    // Gone from the queue, skill written with provenance.
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
    let skills: Vec<_> = std::fs::read_dir(h.dir.path().join("skills"))
        .unwrap()
        .collect();
    assert_eq!(skills.len(), 1);
    let applied = pantheon_nightly::read_events(h.dir.path())
        .into_iter()
        .any(|e| matches!(e, NightlyEvent::Applied { .. }));
    assert!(applied);
}

#[test]
fn one_pass_feeds_proposals_and_memory_from_single_scan() {
    let h = harness();
    // Same three runs carry both the skill signal and the memory
    // signal: one ledger scan must feed both pipelines.
    for i in 1..=3 {
        let run = format!("run_{i}");
        sequence_run(&h.ledger, &run);
        h.ledger
            .append(&Event::SteeringProvided {
                run_id: run,
                text: "always put imports at the top".into(),
            })
            .unwrap();
    }
    contains_task(h.dir.path());
    let replay = ScriptedReplay {
        base: "started".into(),
        with: "started\ndone".into(),
    };
    let out = run_pass(
        &config(h.dir.path()),
        &mut deps(&h, &PassEval, &replay, None),
    )
    .unwrap();

    // One scan feeds both pipelines. Propose-only: the skill queues for
    // approval and the lesson now queues too (it no longer auto-applies),
    // so both sit in the pending queue; nothing durable until a grant.
    assert_eq!(skill_proposals(&out).len(), 1);
    let pending = load_pending(h.dir.path()).unwrap();
    assert_eq!(pending.len(), 2, "skill + lesson both queue");
    assert!(pending.iter().any(|p| p.kind_name() == "skill"));
    assert!(pending.iter().any(|p| p.kind_name() == "lesson"));
    assert!(nightly_lessons(&h, "imports").is_empty());
    // Exactly one pass ran.
    let passes = pantheon_nightly::read_events(h.dir.path())
        .into_iter()
        .filter(|e| matches!(e, NightlyEvent::PassStarted { .. }))
        .count();
    assert_eq!(passes, 1);
}

#[test]
fn dry_run_applies_nothing() {
    let h = harness();
    for i in 1..=3 {
        steer(
            &h.ledger,
            &format!("run_{i}"),
            "always use tabs for indentation",
        );
    }
    seed_skill_signal(&h);
    contains_task(h.dir.path());
    let replay = ScriptedReplay {
        base: "started".into(),
        with: "started\ndone".into(),
    };
    let mut cfg = config(h.dir.path());
    cfg.dry_run = true;
    let out = run_pass(&cfg, &mut deps(&h, &PassEval, &replay, None)).unwrap();

    assert_eq!(out.applied, 0);
    assert_eq!(out.pending, 0);
    assert!(nightly_lessons(&h, "imports").is_empty());
    assert!(load_pending(h.dir.path()).unwrap().is_empty());
    // But the pass is still audited and reported.
    assert!(h.dir.path().join("nightly").join("nightly.jsonl").exists());
    assert!(h
        .dir
        .path()
        .join("nightly")
        .join("nightly-report.md")
        .exists());
}

#[test]
fn schedule_nightly_uses_nightly_marker_and_default_cron() {
    use pantheon_scheduler::ScheduleKind;
    use pantheon_tui::schedule::{build_nightly_job, NIGHTLY_TASK_MARKER};
    let dir = tempfile::tempdir().unwrap();
    let job = build_nightly_job(dir.path(), &[]).unwrap();
    assert_eq!(job.job.task, NIGHTLY_TASK_MARKER);
    assert!(
        matches!(&job.job.kind, ScheduleKind::Cron { expr } if expr == "0 3 * * *"),
        "unexpected kind: {:?}",
        job.job.kind
    );
}

#[test]
fn failed_approval_application_keeps_proposal_pending() {
    let (h, id) = approve_setup();
    // Sabotage the skills dir so application fails.
    std::fs::write(h.dir.path().join("skills"), b"not a dir").unwrap();
    let cfg = config(h.dir.path());
    let err = decide(h.dir.path(), &cfg, &id, true).unwrap_err();
    assert!(!err.is_empty());
    // Still queued for retry; the failure is audited.
    assert_eq!(load_pending(h.dir.path()).unwrap().len(), 1);
    let failed = pantheon_nightly::read_events(h.dir.path())
        .into_iter()
        .any(|e| matches!(e, NightlyEvent::ApplyFailed { .. }));
    assert!(failed);
}

/// Eval runner that fails one named target on its FIRST run and passes
/// afterwards: simulates a draft revision that fixes the eval.
struct FailOnceThenPass {
    bad: String,
    runs: std::cell::Cell<usize>,
}
impl EvalRunner for FailOnceThenPass {
    fn run_eval(&self, target: &str, _timeout: Duration) -> EvalOutcome {
        if target == self.bad && self.runs.get() == 0 {
            self.runs.set(1);
            EvalOutcome::Fail("boom".into())
        } else {
            EvalOutcome::Pass
        }
    }
}

/// Eval runner that fails every target: the original flaw (tag pruning +
/// vacuous pass) let this draft validate green.
struct FailAll;
impl EvalRunner for FailAll {
    fn run_eval(&self, _target: &str, _timeout: Duration) -> EvalOutcome {
        EvalOutcome::Fail("boom".into())
    }
}

/// Judge LLM returning a fixed revised draft; asserts the eval-failure
/// prompt reaches the Repair slot.
struct EvalSharpenJudge;
impl NightlyLlm for EvalSharpenJudge {
    fn refine_proposal(&self, _model: &AuxiliaryModel, draft: &Proposal) -> Result<String, String> {
        assert!(draft.body.contains("failed eval validation"));
        assert!(draft.body.contains("boom"));
        Ok("revised draft".into())
    }
    fn distill_memories(
        &self,
        _model: &AuxiliaryModel,
        texts: &[String],
    ) -> Result<Vec<String>, String> {
        Ok(texts.to_vec())
    }
}

/// Judge LLM whose revision never fixes anything: exhaustion must still
/// escalate.
struct NeverFixesJudge;
impl NightlyLlm for NeverFixesJudge {
    fn refine_proposal(
        &self,
        _model: &AuxiliaryModel,
        _draft: &Proposal,
    ) -> Result<String, String> {
        Ok("revised but still broken".into())
    }
    fn distill_memories(
        &self,
        _model: &AuxiliaryModel,
        texts: &[String],
    ) -> Result<Vec<String>, String> {
        Ok(texts.to_vec())
    }
}

fn aux_model() -> AuxiliaryModel {
    AuxiliaryModel {
        kind: AuxiliaryKind::Repair,
        provider: "p".into(),
        model: "m".into(),
        timeout_secs: 60,
        target_percent: None,
    }
}

fn proposal_with_tags(tags: &[&str]) -> Proposal {
    Proposal {
        id: "nly_skill_fixloop".into(),
        kind: pantheon_nightly::ProposalKind::Skill {
            name: "fixloop-test".into(),
            update: false,
        },
        title: "fixloop test".into(),
        body: "do the thing well".into(),
        provenance_runs: vec!["run_1".into()],
        provenance_turns: Vec::new(),
        eval_tags: tags.iter().map(|s| s.to_string()).collect(),
        status: ProposalStatus::Proposed,
    }
}

fn replay_task_with_exec(store: &mut ReplayStore) {
    store
        .save(ReplayTask {
            id: "exec1".into(),
            name: "exec1".into(),
            prompt: "go".into(),
            check: ReplayCheck::Contains {
                text: "started".into(),
                negate: false,
            },
            exec: Some(pantheon_nightly::TaskExec {
                command: "sh".into(),
                args: vec![
                    "-c".into(),
                    "if [ -n \"$PANTHEON_REPLAY_SKILL_DIR\" ]; then echo started; else echo idle; fi"
                        .into(),
                ],
                env: Default::default(),
            }),
        })
        .unwrap();
}

/// Eval reject revises the DRAFT (via the Repair slot), not the tags:
/// the full tag set re-runs against the revised draft.
#[test]
fn fix_loop_sharpens_draft_on_eval_reject_and_revalidates() {
    let h = harness();
    let mut store = ReplayStore::open(h.dir.path()).unwrap();
    // Task the built-in runner executes. The command sees
    // PANTHEON_REPLAY_SKILL_DIR only on the with-proposal replay, so
    // the A/B pair measures the proposal's effect: baseline prints
    // "idle" (score 0), with-proposal prints "started" (score 1).
    replay_task_with_exec(&mut store);
    let eval = FailOnceThenPass {
        bad: "bad".into(),
        runs: std::cell::Cell::new(0),
    };
    // No replay_command configured: the composite runner still replays
    // via the task's own exec spec.
    let replay = pantheon_nightly::CompositeReplayRunner::new(None, Duration::from_secs(30));
    let judge = EvalSharpenJudge;
    let aux = aux_model();
    let mut p = proposal_with_tags(&["good", "bad"]);
    let mut events = Vec::new();
    let outcome = pantheon_nightly::validate_with_fix_loop(
        &mut p,
        &eval,
        &replay,
        &store,
        None,                 // scoring judge: not needed (contains checks)
        Some((&judge, &aux)), // repair model: revises the draft
        &config(h.dir.path()),
        &mut events,
        1,
    );
    assert!(matches!(outcome, pantheon_nightly::FixOutcome::Validated));
    // The draft was revised; the tags were NOT pruned - the full set
    // re-ran against the new body.
    assert_eq!(p.body, "revised draft");
    assert_eq!(p.eval_tags, vec!["good".to_string(), "bad".to_string()]);
    assert!(events.iter().any(|e| matches!(
        e,
        NightlyEvent::FixAttempt { phase, detail, .. }
            if phase == "eval" && detail == "sharpened draft via Repair slot; re-running gate"
    )));
    assert!(events
        .iter()
        .any(|e| matches!(e, NightlyEvent::ReplayPassed { .. })));
}

/// Regression test for the original flaw: a draft that fails EVERY
/// eval must never validate. The old code pruned each failing tag and
/// then took the vacuous "no evals tagged" pass; now the tags are
/// immutable and the loop exhausts its attempts, then escalates.
#[test]
fn draft_failing_every_eval_cannot_validate() {
    let h = harness();
    let store = ReplayStore::open(h.dir.path()).unwrap();
    let replay = pantheon_nightly::CompositeReplayRunner::new(None, Duration::from_secs(30));
    let mut p = proposal_with_tags(&["bad1", "bad2"]);
    let mut events = Vec::new();
    let outcome = pantheon_nightly::validate_with_fix_loop(
        &mut p,
        &FailAll,
        &replay,
        &store,
        None,
        None, // no repair model: plain retries, then escalation
        &config(h.dir.path()),
        &mut events,
        1,
    );
    assert!(
        matches!(outcome, pantheon_nightly::FixOutcome::Escalated { .. }),
        "a draft failing every eval must escalate, never validate"
    );
    // The tags survived the loop: no pruning, no vacuous pass.
    assert_eq!(p.eval_tags, vec!["bad1".to_string(), "bad2".to_string()]);
    assert!(matches!(p.status, ProposalStatus::NeedsAttention));
    // Every attempt was consumed before escalation.
    let attempts = events
        .iter()
        .filter(|e| matches!(e, NightlyEvent::FixAttempt { .. }))
        .count();
    assert_eq!(attempts, NightlyConfig::default().max_fix_attempts);
    assert!(events
        .iter()
        .any(|e| matches!(e, NightlyEvent::Escalated { .. })));
    // The escalation is recorded for the human.
    let recorded = pantheon_nightly::load_escalated(h.dir.path()).unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].id, p.id);
    assert!(
        recorded[0].reason.contains("eval gate rejected"),
        "unexpected reason: {}",
        recorded[0].reason
    );
}

/// Exhaustion with a repair model: the judge revises but the evals
/// keep failing - each revision consumes one attempt, then the loop
/// escalates instead of validating.
#[test]
fn exhaustion_after_repair_sharpen_still_escalates() {
    let h = harness();
    let store = ReplayStore::open(h.dir.path()).unwrap();
    let replay = pantheon_nightly::CompositeReplayRunner::new(None, Duration::from_secs(30));
    let judge = NeverFixesJudge;
    let aux = aux_model();
    let mut cfg = config(h.dir.path());
    cfg.max_fix_attempts = 2;
    let mut p = proposal_with_tags(&["bad"]);
    let mut events = Vec::new();
    let outcome = pantheon_nightly::validate_with_fix_loop(
        &mut p,
        &FailAll,
        &replay,
        &store,
        None,
        Some((&judge, &aux)),
        &cfg,
        &mut events,
        1,
    );
    assert!(matches!(
        outcome,
        pantheon_nightly::FixOutcome::Escalated { .. }
    ));
    assert_eq!(p.body, "revised but still broken");
    assert_eq!(p.eval_tags, vec!["bad".to_string()]);
    let sharpens: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            NightlyEvent::FixAttempt { detail, .. } => Some(detail.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(sharpens.len(), 2);
    assert!(sharpens
        .iter()
        .all(|d| d == "sharpened draft via Repair slot; re-running gate"));
    assert!(events
        .iter()
        .any(|e| matches!(e, NightlyEvent::Escalated { .. })));
}

#[test]
fn builtin_runner_captures_stdout_and_rejects_nonzero_exit() {
    let runner = pantheon_nightly::TaskSpecReplayRunner::new(Duration::from_secs(30));
    let ok_task = ReplayTask {
        id: "ok".into(),
        name: "ok".into(),
        prompt: "go".into(),
        check: ReplayCheck::Contains {
            text: "hello".into(),
            negate: false,
        },
        exec: Some(pantheon_nightly::TaskExec {
            command: "sh".into(),
            args: vec!["-c".into(), "echo hello".into()],
            env: Default::default(),
        }),
    };
    let tx = runner.run_transcript(&ok_task, None).unwrap();
    assert!(tx.contains("hello"));
    let bad_task = ReplayTask {
        id: "bad".into(),
        name: "bad".into(),
        prompt: "go".into(),
        check: ReplayCheck::Contains {
            text: "x".into(),
            negate: false,
        },
        exec: Some(pantheon_nightly::TaskExec {
            command: "sh".into(),
            args: vec!["-c".into(), "exit 3".into()],
            env: Default::default(),
        }),
    };
    assert!(runner.run_transcript(&bad_task, None).is_err());
}

#[test]
fn composite_runner_fails_loudly_with_no_exec_and_no_command() {
    let runner = pantheon_nightly::CompositeReplayRunner::new(None, Duration::from_secs(30));
    let task = ReplayTask {
        id: "t".into(),
        name: "t".into(),
        prompt: "p".into(),
        check: ReplayCheck::Contains {
            text: "x".into(),
            negate: false,
        },
        exec: None,
    };
    let err = runner.run_transcript(&task, None).unwrap_err();
    assert!(err.contains("replay_command"), "unexpected: {err}");
}

/// LLM sharpen path: with a repair model available, a fair-measurement
/// failure sharpens the draft via the Repair slot before the final
/// escalation.
struct SharpenJudge;
impl NightlyLlm for SharpenJudge {
    fn refine_proposal(&self, _model: &AuxiliaryModel, draft: &Proposal) -> Result<String, String> {
        assert!(draft.body.contains("failed replay validation"));
        Ok("sharpened draft".into())
    }
    fn distill_memories(
        &self,
        _model: &AuxiliaryModel,
        texts: &[String],
    ) -> Result<Vec<String>, String> {
        Ok(texts.to_vec())
    }
}

#[test]
fn fix_loop_sharpens_draft_with_llm_then_escalates() {
    let h = harness();
    seed_skill_signal(&h);
    contains_task(h.dir.path());
    let replay = ScriptedReplay {
        base: "done".into(),
        with: "done".into(),
    };
    let judge = SharpenJudge;
    let aux = aux_model();
    // Tagged so the eval gate passes and the replay path runs.
    let mut p = proposal_with_tags(&["some_eval"]);
    let store = ReplayStore::open(h.dir.path()).unwrap();
    let mut events = Vec::new();
    let outcome = pantheon_nightly::validate_with_fix_loop(
        &mut p,
        &PassEval,
        &replay,
        &store,
        None,                 // scoring judge: not needed (contains checks)
        Some((&judge, &aux)), // repair model: sharpens the draft
        &config(h.dir.path()),
        &mut events,
        1,
    );
    assert!(matches!(
        outcome,
        pantheon_nightly::FixOutcome::Escalated { .. }
    ));
    assert!(matches!(p.status, ProposalStatus::NeedsAttention));
    assert!(events.iter().any(|e| matches!(
        e,
        NightlyEvent::FixAttempt { detail, .. } if detail.contains("sharpened draft")
    )));
    assert!(events
        .iter()
        .any(|e| matches!(e, NightlyEvent::Escalated { .. })));
}

/// Trust-tier rule end to end: a persona proposal reaches the live
/// session's persona overlay only through explicit human approval.
#[test]
fn approved_persona_reaches_overlay_denied_does_not() {
    let h = harness();
    let persona = Proposal {
        id: "nly_persona_tone".into(),
        kind: pantheon_nightly::ProposalKind::Persona {
            topic: "tone".into(),
        },
        title: "persona: prefers terse".into(),
        body: "keep replies terse".into(),
        provenance_runs: vec!["run_1".into()],
        provenance_turns: Vec::new(),
        eval_tags: Vec::new(),
        status: ProposalStatus::ReplayPassed,
    };
    let cfg = config(h.dir.path());

    // Approval writes the note to the persona namespace.
    pantheon_nightly::queue_for_approval(h.dir.path(), &persona).unwrap();
    assert!(decide(h.dir.path(), &cfg, &persona.id, true).unwrap());
    let store = pantheon_memory::MemoryStore::open(&h.dir.path().join("memory.db")).unwrap();
    let notes = pantheon_nightly::approved_notes(&store);
    assert_eq!(notes.len(), 1);
    let block = pantheon_nightly::overlay_block(&notes);
    assert!(block.contains("keep replies terse"));

    // A denied persona never lands in the namespace.
    let denied = Proposal {
        id: "nly_persona_other".into(),
        ..persona.clone()
    };
    pantheon_nightly::queue_for_approval(h.dir.path(), &denied).unwrap();
    assert!(decide(h.dir.path(), &cfg, &denied.id, false).unwrap());
    let notes = pantheon_nightly::approved_notes(&store);
    assert_eq!(notes.len(), 1, "denied persona must not inject");
    assert!(!pantheon_nightly::overlay_block(&notes).contains("other"));
}

// ------------------------------------------------- enable-path matrix ---
//
// The four enable paths (model pin, `/nightly on`, config edit,
// dashboard toggle) share one rule - `pantheon_api::config::
// nightly_enabled`: explicit `false` always wins; explicit `true`
// forces on; an absent flag is on iff a model pin is present (the
// `[nightly.model]` table or the `PANTHEON_NIGHTLY_PROVIDER` /
// `PANTHEON_NIGHTLY_MODEL` env overrides); otherwise off. The matrix
// below exercises the rule through real TOML → `Config` parses so the
// wire shape is covered, then the TUI persist path and the status
// surface round-trip through real files.

use pantheon_api::config::{
    nightly_enabled, nightly_enabled_reason, Config as ApiConfig, NightlySection,
};

/// Serializes the enable-matrix tests: they share the process env
/// (the `PANTHEON_NIGHTLY_*` pin overrides), and Rust runs tests on
/// threads in one process. The lock is held for microseconds.
static NIGHTLY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Lock the env mutex, recovering from poisoning: a sibling test's
/// assertion failure must not cascade into unrelated tests.
fn nightly_env_guard() -> std::sync::MutexGuard<'static, ()> {
    NIGHTLY_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clear the env-pin overrides so the table/flag matrix is
/// deterministic: the test process owns its env and no other test sets
/// these, but a developer shell might.
fn clear_nightly_env() {
    std::env::remove_var("PANTHEON_NIGHTLY_PROVIDER");
    std::env::remove_var("PANTHEON_NIGHTLY_MODEL");
}

fn parse_nightly_section(toml: &str) -> NightlySection {
    let cfg: ApiConfig = toml::from_str(toml).expect("test TOML parses");
    cfg.nightly.expect("test TOML has [nightly]")
}

#[test]
fn nightly_enable_matrix_explicit_false_beats_model_pin() {
    let _guard = nightly_env_guard();
    clear_nightly_env();
    let s = parse_nightly_section(
        "[nightly]\nenabled = false\n[nightly.model]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\n",
    );
    assert!(!nightly_enabled(&s), "explicit false must win over the pin");
    assert_eq!(nightly_enabled_reason(&s), "explicit flag off");
}

#[test]
fn nightly_enable_matrix_pin_alone_enables() {
    let _guard = nightly_env_guard();
    clear_nightly_env();
    let s = parse_nightly_section(
        "[nightly]\n[nightly.model]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\n",
    );
    assert!(nightly_enabled(&s), "absent flag + model pin = on");
    assert_eq!(nightly_enabled_reason(&s), "on via [nightly.model] pin");
}

#[test]
fn nightly_enable_matrix_explicit_true_without_pin_enables() {
    let _guard = nightly_env_guard();
    clear_nightly_env();
    let s = parse_nightly_section("[nightly]\nenabled = true\n");
    assert!(
        nightly_enabled(&s),
        "explicit true enables even with no pin (status must warn)"
    );
    assert_eq!(nightly_enabled_reason(&s), "explicit flag on");
}

#[test]
fn nightly_enable_matrix_absent_flag_and_no_pin_is_off() {
    let _guard = nightly_env_guard();
    clear_nightly_env();
    let s = parse_nightly_section("[nightly]\nauto_turns = 20\n");
    assert!(!nightly_enabled(&s), "off by default");
    assert_eq!(nightly_enabled_reason(&s), "off (no flag, no model pin)");
    // No `[nightly]` section at all: the default section is off too.
    assert!(!nightly_enabled(&NightlySection::default()));
}

#[test]
fn nightly_enable_matrix_env_pin_enables_without_flag() {
    let _guard = nightly_env_guard();
    clear_nightly_env();
    std::env::set_var("PANTHEON_NIGHTLY_MODEL", "gpt-4o-mini");
    let s = parse_nightly_section("[nightly]\n");
    assert!(
        nightly_enabled(&s),
        "env pin counts as a pin for the absent-flag rule"
    );
    assert_eq!(nightly_enabled_reason(&s), "on via PANTHEON_NIGHTLY_* env");
    // ...but explicit false still wins over the env pin.
    let s = parse_nightly_section("[nightly]\nenabled = false\n");
    assert!(!nightly_enabled(&s));
    std::env::remove_var("PANTHEON_NIGHTLY_MODEL");
}

#[test]
fn nightly_enable_matrix_empty_pin_table_is_no_pin() {
    let _guard = nightly_env_guard();
    clear_nightly_env();
    let s = parse_nightly_section("[nightly]\n[nightly.model]\n");
    assert!(
        !nightly_enabled(&s),
        "an empty [nightly.model] table is not a pin"
    );
}

// ------------------------------------------- TUI persist + status -------

#[test]
fn nightly_tui_persist_round_trips_through_config_file() {
    use pantheon_tui::nightly_cli;
    let _guard = nightly_env_guard();
    clear_nightly_env();
    let dir = tempfile::tempdir().unwrap();
    // Seed an unrelated knob: persist must preserve everything else.
    std::fs::write(
        dir.path().join("config.toml"),
        "[nightly]\nauto_turns = 42\n",
    )
    .unwrap();

    nightly_cli::persist_nightly_enabled(dir.path(), true).expect("persist on");
    let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(raw.contains("enabled = true"), "flag written: {raw}");
    assert!(raw.contains("auto_turns = 42"), "other keys kept: {raw}");
    assert!(
        nightly_cli::nightly_enabled(dir.path()),
        "resolver sees the persisted flag"
    );

    nightly_cli::persist_nightly_enabled(dir.path(), false).expect("persist off");
    assert!(!nightly_cli::nightly_enabled(dir.path()));
    // The explicit false survives as a flag, not as a deleted key: it
    // must keep beating a later-added pin.
    let raw = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
    assert!(raw.contains("enabled = false"), "explicit off kept: {raw}");
}

#[test]
fn nightly_subcommand_parsing() {
    use pantheon_tui::nightly_cli::NightlySub;
    assert_eq!(NightlySub::parse(""), NightlySub::Run { dry_run: false });
    assert_eq!(
        NightlySub::parse("--dry-run"),
        NightlySub::Run { dry_run: true }
    );
    assert_eq!(NightlySub::parse("-n"), NightlySub::Run { dry_run: true });
    assert_eq!(NightlySub::parse("  on  "), NightlySub::On);
    assert_eq!(NightlySub::parse("off"), NightlySub::Off);
    assert_eq!(NightlySub::parse("status"), NightlySub::Status);
    assert!(matches!(NightlySub::parse("bogus"), NightlySub::Unknown(_)));
}

#[test]
fn nightly_status_reports_reason_pin_and_next_run() {
    use pantheon_tui::nightly_cli;
    let _guard = nightly_env_guard();
    clear_nightly_env();
    let dir = tempfile::tempdir().unwrap();
    // A config with [nightly] but no flag and no pin: the reason comes
    // from the enable rule, not the no-config fallback.
    std::fs::write(
        dir.path().join("config.toml"),
        "[nightly]\nauto_turns = 42\n",
    )
    .unwrap();

    // Off by default: the reason names the default, no job scheduled.
    let s = nightly_cli::status_line(dir.path());
    assert!(s.contains("nightly pass: off"), "state: {s}");
    assert!(s.contains("no flag, no model pin"), "reason: {s}");
    assert!(s.contains("next scheduled run: none"), "next run: {s}");

    // Schedule a nightly job: status names its next fire time.
    let job = pantheon_tui::schedule::build_nightly_job(dir.path(), &[]).unwrap();
    let text = serde_json::to_string_pretty(&vec![job]).unwrap();
    std::fs::write(pantheon_scheduler::job_store_path(dir.path()), text).unwrap();
    let s = nightly_cli::status_line(dir.path());
    assert!(s.contains("next scheduled run: 2"), "next run: {s}");

    // Explicit on with no pin: on, with the reason - and the pin
    // guidance explains how to pin a model.
    nightly_cli::persist_nightly_enabled(dir.path(), true).unwrap();
    let s = nightly_cli::status_line(dir.path());
    assert!(
        s.contains("nightly pass: on (explicit flag on)"),
        "state: {s}"
    );
    assert!(!nightly_cli::nightly_pin_present(dir.path()));
    assert!(
        nightly_cli::pin_guidance().contains("[nightly.model]"),
        "guidance names the key path"
    );
    assert!(
        nightly_cli::pin_guidance().contains("api_key_env"),
        "guidance names the env-var key"
    );
}
