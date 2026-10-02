//! `pantheon-nightly`: the unified nightly self-improvement pass.
//!
//! One pipeline replaces the old `pantheon-reflect` + `pantheon-consolidate`
//! split:
//!
//! 1. **Scan** the structured ledger once ([`signals::collect`]).
//! 2. **Propose** deterministic proposals with run/turn provenance
//!    ([`propose::from_signals`]).
//! 3. Optionally **refine** drafts with an LLM - off by default, always
//!    through the `Reflection` auxiliary slot, never the chat model.
//! 4. **Eval-gate** skill/persona proposals: relevant `pantheon-eval`
//!    targets must pass ([`gate`]). Memory lessons skip this.
//! 5. **Replay-gate** skill/persona proposals: held-out recurring tasks
//!    replay with and without the candidate; the proposal ships only on
//!    *strict improvement* ([`replay`]). This is the SkillOpt learning
//!    rule, adapted to Pantheon's provenance and approval model.
//! 6. **Fix loop**: a proposal that fails a gate gets up to
//!    `max_fix_attempts` diagnose → revise → re-check iterations
//!    (deterministic repairs; LLM sharpening only when LLM steps are
//!    enabled), then escalates to the human as `NeedsAttention`
//!    ([`fixloop`]) - never an infinite loop, never silent.
//! 7. **Apply**: memory lessons auto-apply at trust tier `Memory`
//!    (after passing the frequency + recency promotion rule - no decay
//!    curves). Skills/personas wait in the pending queue for explicit
//!    human approval.
//! 7. **Repair** broken operational targets: MCP servers, scheduled
//!    jobs, and tools walk a bounded repair ladder (retry → `Repair`-slot
//!    diagnosis → config repair → disable/pause + escalate), audited with
//!    the same event shapes ([`repair_targets`]). Skipped when no repair
//!    targets are attached. In dry-run mode the phase detects and audits
//!    but mutates nothing.
//! 8. **Audit** every lifecycle event to JSONL; write the pass report.
//!
//! Approved persona notes are injected into fresh runs' system prompts
//! by the runtime ([`persona`]), so an approved persona actually shapes
//! future sessions.
//!
//! Memory promotion rule: a candidate promotes when seen in at least
//! `min_sessions` distinct runs with the newest observation inside
//! `max_age_days`. Plain frequency + recency - the old exponential
//! half-life formula is gone.

mod apply;
mod audit;
mod fixloop;
mod gate;
mod ideas;
mod llm;
mod persona;
mod propose;
mod repair_targets;
mod replay;
mod report;
mod signals;
mod state;

pub use apply::{
    apply_memory_lesson, apply_skill_or_persona, decide, load_pending, pending_path,
    queue_for_approval, route, ApplyOutcome,
};
pub use audit::{audit, audit_path, last_run_summary, read_events, NightlyEvent};
pub use fixloop::{
    escalated_path, load_escalated, validate_with_fix_loop, Escalation, FixOutcome,
    DEFAULT_MAX_FIX_ATTEMPTS,
};
pub use gate::{gate, EvalOutcome, EvalRunner, EvalVerdict, SubprocessEvalRunner};
pub use ideas::{
    add_days, downranked, general_from_failure, general_from_repair, median_hour_utc,
    run_ideas_phase, scheduled_from_sequence, today_utc, GeneratedIdea, MAX_IDEAS_PER_DAY,
    PENDING_KEEP_DAYS,
};
pub use llm::{
    resolve_distill, resolve_refiner, resolve_repair, DeterministicDistill, DistillBackend,
    NightlyLlm, ResolvedDistill,
};
pub use persona::{approved_notes, overlay_block, PERSONA_KEY_PREFIX, PERSONA_NAMESPACE};
pub use propose::{
    candidate_key, from_signals, memory_proposals, weigh_candidates, Proposal, ProposalKind,
    ProposalStatus, WeighedCandidate, MAX_FACT_CHARS,
};
pub use repair_targets::{
    run_repair_phase, McpRepairTarget, McpServerSnapshot, RepairOutcome, RepairReport,
    RepairTargets, ScheduleRepairTarget, ScheduledJobSnapshot, ToolCallStats, ToolRepairTarget,
};
pub use replay::{
    replay_gate, CompositeReplayRunner, ReplayCheck, ReplayRunner, ReplayStore, ReplayTask,
    ReplayVerdict, SubprocessReplayRunner, TaskExec, TaskSpecReplayRunner,
    UnconfiguredReplayRunner,
};
pub use report::render as render_report;
pub use signals::{
    collect as collect_signals, CandidateKind, MemoryCandidate, ScanResult, Signal, Source, TurnRef,
};
pub use state::{state_path, NightlyState};

use pantheon_api::error::PantheonError;
use pantheon_api::model::ModelPolicy;
use pantheon_api::provenance::TrustTier;
use pantheon_memory::MemoryBackend;
use pantheon_storage::Ledger;
use std::path::PathBuf;
use std::time::Duration;

/// All the knobs for one nightly pass.
#[derive(Debug, Clone)]
pub struct NightlyConfig {
    /// Pantheon data dir (`<data_dir>/nightly/*` is the pass workspace).
    pub data_dir: PathBuf,
    /// Memory lessons auto-apply at this tier. Defaults to `Memory`.
    /// Anything higher needs a human.
    pub memory_trust: TrustTier,
    /// `collect` knobs.
    pub since_ms: i64,
    pub max_runs: usize,
    pub min_sequence_repeats: usize,
    pub min_failure_repeats: usize,
    pub min_preference_hits: usize,
    /// Memory promotion rule.
    pub min_sessions: usize,
    pub max_age_days: i64,
    /// Eval gate knobs.
    pub eval_timeout: Duration,
    pub max_evals: usize,
    /// Headless agent command used to replay held-out validation tasks.
    /// Hosts build their [`ReplayRunner`](crate::ReplayRunner) from this
    /// (see [`SubprocessReplayRunner`](crate::SubprocessReplayRunner)).
    /// `None` = replays fail loudly (the replay gate rejects) rather
    /// than pass silently.
    pub replay_command: Option<String>,
    /// LLM steps. Off by default. When true AND the model policy
    /// resolves the auxiliary slot, proposal refinement and memory
    /// distillation use the auxiliary model.
    pub llm_enabled: bool,
    /// Diagnose → revise → re-check iterations per failing skill/persona
    /// proposal before it escalates to the human as `NeedsAttention`.
    /// Default 3. Repairs revise the draft through the `[repair]` aux
    /// slot (eval rejects; replay rejects re-run the A/B pair as a
    /// flakiness retry when no repair slot is configured; eval tags are
    /// immutable); infrastructure failures escalate immediately.
    pub max_fix_attempts: usize,
    /// Consecutive MCP server failures that mark a server broken
    /// (status `failed`/`backoff`). Default 5.
    pub mcp_max_failures: u32,
    /// Consecutive scheduled-job run failures that mark a job broken.
    /// Default 3.
    pub schedule_max_failures: u32,
    /// Minimum tool invocations in the scan window before the
    /// "every invocation errored" rule can fire. Default 3.
    pub tool_min_calls: usize,
    /// Tools the nightly smoke probe may execute (empty-args invocation
    /// behind the capability policy). Default empty: no probing. The
    /// pass never probes a tool not on this list.
    pub tool_probe_allowlist: Vec<String>,
    /// `true` → propose and audit, but don't apply or queue anything.
    pub dry_run: bool,
}

impl Default for NightlyConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("."),
            memory_trust: TrustTier::Memory,
            since_ms: 0,
            max_runs: 50,
            min_sequence_repeats: 3,
            min_failure_repeats: 3,
            min_preference_hits: 3,
            min_sessions: 3,
            max_age_days: 30,
            eval_timeout: Duration::from_secs(120),
            max_evals: 3,
            replay_command: None,
            llm_enabled: false,
            max_fix_attempts: DEFAULT_MAX_FIX_ATTEMPTS,
            mcp_max_failures: 5,
            schedule_max_failures: 3,
            tool_min_calls: 3,
            tool_probe_allowlist: Vec::new(),
            dry_run: false,
        }
    }
}

/// The world a pass runs against: the backends it reads and writes.
pub struct NightlyDeps<'a> {
    pub ledger: &'a Ledger,
    pub backend: &'a dyn MemoryBackend,
    pub capability_policy: &'a pantheon_api::capability::Policy,
    pub model_policy: &'a ModelPolicy,
    pub eval_runner: &'a dyn EvalRunner,
    pub replay_runner: &'a dyn ReplayRunner,
    pub llm: Option<&'a dyn NightlyLlm>,
    /// Operational repair targets (MCP servers, scheduled jobs, tools).
    /// `None` (the default) skips the repair phase. The phase mutates
    /// through these adapters, hence `run_pass` takes `&mut NightlyDeps`.
    pub repair: Option<RepairTargets<'a>>,
}

/// What one pass did.
#[derive(Debug, Default)]
pub struct PassResult {
    pub signals: usize,
    pub proposals: Vec<Proposal>,
    pub applied: usize,
    pub pending: usize,
    pub repairs: Vec<RepairReport>,
    pub events: Vec<NightlyEvent>,
    /// Ideas minted by this pass (the Ideas page backend).
    pub ideas_minted: usize,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run the full nightly pass: scan → propose → gate → apply → repair → audit.
///
/// The repair phase runs only as part of an enabled pass: `[nightly]
/// enabled` gating (workstream 1) decides whether `run_pass` is invoked
/// at all - this phase invents no second flag.
pub fn run_pass(
    config: &NightlyConfig,
    deps: &mut NightlyDeps<'_>,
) -> Result<PassResult, PantheonError> {
    let at = now_ms();
    let mut events = Vec::new();

    // 1. Single ledger scan → behavior signals + memory candidates.
    let policy = collect_policy(config);
    let scan = collect_signals(
        deps.ledger,
        deps.backend,
        deps.capability_policy,
        &policy,
        config.since_ms,
        config.max_runs,
    )?;
    let signals = scan.signals;
    let candidates = scan.candidates;
    for s in &signals {
        let (kind, hits) = signal_summary(s);
        events.push(NightlyEvent::SignalObserved {
            kind,
            hits,
            at_ms: at,
        });
    }

    // 2. Deterministic proposals with provenance. Memory lessons are
    // recurrence-gated here; skills/personas are replay-gated below.
    let mut proposals = from_signals(&signals, config.min_sessions, config.max_age_days, at);

    // Optional LLM refinement (off unless enabled + slot resolves).
    if config.llm_enabled {
        if let Some(llm) = deps.llm {
            if let Some((llm_impl, aux)) = resolve_refiner(llm, deps.model_policy) {
                for p in proposals.iter_mut() {
                    match llm_impl.refine_proposal(aux, p) {
                        Ok(body) if !body.trim().is_empty() => p.body = body,
                        // A failed refinement degrades to the deterministic
                        // draft, loudly.
                        Ok(_) | Err(_) => events.push(NightlyEvent::ProposalMade {
                            id: p.id.clone(),
                            kind: p.kind_name().into(),
                            title: "refinement skipped: empty/failed".into(),
                            at_ms: at,
                        }),
                    }
                }
            }
        }
    }

    // 3. Memory candidates → promotion rule → MemoryLesson proposals.
    let distill: Option<Box<dyn DistillBackend>> = if config.llm_enabled {
        deps.llm
            .and_then(|llm| resolve_distill(llm, deps.model_policy))
            .map(|r| Box::new(r) as Box<dyn DistillBackend>)
    } else {
        None
    };
    let weighed = weigh_candidates(
        candidates,
        config.min_sessions,
        config.max_age_days,
        at,
        distill.as_deref(),
    );
    proposals.extend(memory_proposals(&weighed));

    events.push(NightlyEvent::PassStarted {
        runs_scanned: scan.runs_scanned,
        at_ms: at,
    });
    for p in &proposals {
        events.push(NightlyEvent::ProposalMade {
            id: p.id.clone(),
            kind: p.kind_name().into(),
            title: p.title.clone(),
            at_ms: at,
        });
    }

    // 4-6. Validate and apply/queue each proposal.
    let replay_store = ReplayStore::open(&config.data_dir).map_err(nerr)?;
    let judge = if config.llm_enabled {
        deps.llm
            .and_then(|llm| resolve_refiner(llm, deps.model_policy))
    } else {
        None
    };
    // Fix-loop draft revision goes through the Repair slot - never
    // Reflection. `None` means revision is unavailable: the loop falls
    // back to plain retries, then escalation. A pass never fails for
    // lack of a repair model.
    let repair = if config.llm_enabled {
        deps.llm
            .and_then(|llm| resolve_repair(llm, deps.model_policy))
    } else {
        None
    };
    let mut applied = 0usize;
    let mut pending = 0usize;
    let mut finished: Vec<Proposal> = Vec::new();

    for mut p in proposals {
        match &p.kind {
            ProposalKind::MemoryLesson { .. } => {
                // No evals, no replay: the promotion rule IS the gate.
                if config.dry_run {
                    // Dry run: the proposal stands as proposed, nothing
                    // is written. Audit/state/report still record the
                    // pass - that is the point of a dry run.
                    p.status = ProposalStatus::Proposed;
                } else {
                    match apply_memory_lesson(&config.data_dir, &p) {
                        Ok(()) => {
                            p.status = ProposalStatus::Applied;
                            applied += 1;
                            events.push(NightlyEvent::LessonApplied {
                                id: p.id.clone(),
                                key: match &p.kind {
                                    ProposalKind::MemoryLesson { key } => key.clone(),
                                    _ => unreachable!(),
                                },
                                at_ms: at,
                            });
                        }
                        Err(e) => events.push(NightlyEvent::EvalRejected {
                            id: p.id.clone(),
                            reason: format!("apply failed: {e}"),
                            at_ms: at,
                        }),
                    }
                }
            }
            ProposalKind::Skill { .. } | ProposalKind::Persona { .. } => {
                // Eval gate (no-regression) then replay gate (strict
                // improvement), wrapped in the bounded fix loop:
                // diagnose → revise → re-check up to `max_fix_attempts`,
                // then escalate to the human as `NeedsAttention`.
                match validate_with_fix_loop(
                    &mut p,
                    deps.eval_runner,
                    deps.replay_runner,
                    &replay_store,
                    judge,
                    repair,
                    config,
                    &mut events,
                    at,
                ) {
                    FixOutcome::Validated => {}
                    FixOutcome::Escalated { .. } => {
                        // Audited and recorded in nightly-escalated.json;
                        // surfaced in the report. Never queued, never
                        // applied.
                        finished.push(p);
                        continue;
                    }
                }
                if config.dry_run {
                    finished.push(p);
                    continue;
                }
                match queue_for_approval(&config.data_dir, &p) {
                    Ok(_) => {
                        pending += 1;
                        // The queued copy is stored as ReplayPassed; the
                        // pass result must report the same status, not the
                        // pre-queue `Proposed`.
                        p.status = ProposalStatus::ReplayPassed;
                        events.push(NightlyEvent::QueuedForApproval {
                            id: p.id.clone(),
                            kind: p.kind_name().into(),
                            at_ms: at,
                        });
                    }
                    Err(e) => events.push(NightlyEvent::EvalRejected {
                        id: p.id.clone(),
                        reason: format!("queue failed: {e}"),
                        at_ms: at,
                    }),
                }
            }
        }
        finished.push(p);
    }

    // 7. Repair broken operational targets (MCP servers, scheduled
    // jobs, tools). Skipped when no repair targets are attached; in
    // dry-run mode the phase detects and audits but mutates nothing.
    // Diagnosis goes through the `Repair` slot (mirroring the fix
    // loop's slot resolution) - never Reflection, never chat. `None`
    // means deterministic-only repair; the pass never fails for a
    // missing repair model.
    let repair_model = if config.llm_enabled {
        deps.llm
            .and_then(|llm| resolve_repair(llm, deps.model_policy))
    } else {
        None
    };
    let mut repairs = Vec::new();
    if let Some(targets) = deps.repair.as_mut() {
        repairs = run_repair_phase(
            config,
            &config.data_dir,
            targets,
            &scan.tool_stats,
            repair_model,
            &mut events,
            at,
        );
    }

    // 7b. Ideas: proactive suggestions minted from this pass's signals
    // and repair outcomes. This runs only inside an enabled pass
    // `run_pass` is never invoked when the nightly master switch is off
    // (the single entry `run_one_pass` gates first) - so no second flag
    // is invented here. Dry runs mint nothing. Ideas are advisory: a
    // broken ideas store skips the phase instead of failing the pass.
    let mut ideas_minted = 0usize;
    if let Ok(store) = pantheon_storage::IdeaStore::open(&config.data_dir) {
        let today = ideas::today_utc(at);
        match ideas::run_ideas_phase(&store, &signals, &repairs, config.dry_run, &today, at) {
            Ok(n) => ideas_minted = n,
            Err(_) => {}
        }
    }

    // 8. Audit + report + state.
    events.push(NightlyEvent::PassFinished {
        proposals: finished.len(),
        applied,
        pending,
        at_ms: at,
    });
    for e in &events {
        if let Err(err) = audit(&config.data_dir, e) {
            return Err(nerr(format!("audit: {err}")));
        }
    }
    let state = NightlyState {
        last_run_ms: at,
        last_runs_scanned: scan.runs_scanned,
        last_signals: signals.len(),
        last_proposals: finished.len(),
        last_applied: applied,
        last_pending: pending,
        last_dry_run: config.dry_run,
        last_repairs_fixed: repairs
            .iter()
            .filter(|r| r.outcome == RepairOutcome::Repaired)
            .count(),
        last_repairs_contained: repairs
            .iter()
            .filter(|r| r.outcome == RepairOutcome::Contained)
            .count(),
    };
    state.save(&config.data_dir).map_err(nerr)?;
    let report = render_report(&state, &finished, &repairs, &events);
    let report_path = config.data_dir.join("nightly").join("nightly-report.md");
    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| nerr(e.to_string()))?;
    }
    std::fs::write(&report_path, report)
        .map_err(|e| nerr(format!("write {}: {e}", report_path.display())))?;

    Ok(PassResult {
        signals: signals.len(),
        proposals: finished,
        applied,
        pending,
        repairs,
        events,
        ideas_minted,
    })
}

fn signal_summary(s: &Signal) -> (String, usize) {
    match s {
        Signal::RepeatedSequence { hits, .. } => ("repeated_sequence".into(), hits.len()),
        Signal::UserCorrection { .. } => ("user_correction".into(), 1),
        Signal::ApprovalDenied { .. } => ("approval_denied".into(), 1),
        Signal::RepeatedFailure { count, .. } => ("repeated_failure".into(), *count),
        Signal::RepeatedPreference { hits, .. } => ("repeated_preference".into(), hits.len()),
    }
}

fn collect_policy(config: &NightlyConfig) -> signals::CollectPolicy {
    signals::CollectPolicy {
        min_sequence_repeats: config.min_sequence_repeats,
        min_failure_repeats: config.min_failure_repeats,
        min_preference_hits: config.min_preference_hits,
    }
}

fn nerr(cause: String) -> PantheonError {
    PantheonError::new(
        "NLY_IO",
        pantheon_api::error::Layer::Runtime,
        false,
        cause,
        "check the data dir and ledger health (`pantheon doctor`)",
        String::new(),
    )
}
