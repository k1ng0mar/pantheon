//! Nightly repair-loop behavioral evals: broken MCP servers, scheduled
//! jobs, and tools.
//!
//! Fakes stand in for the host adapters (`McpManager`, the schedule
//! store, the tool registry); the ledger, audit log, escalation file,
//! and (for the tool-stats test) the scan are all real.
//!
//! Covered:
//! - flapping MCP server (fails, then recovers on retry) → repaired, not
//!   disabled, no escalation
//! - dead MCP server (always fails) → disabled + escalated (`mcp-server`)
//! - disabled / unapproved servers are never repair targets
//! - repeatedly-failing schedule → paused + escalated (`schedule`)
//! - bad cron with a safe normalization → repaired in place
//! - unfixable cron → paused + escalated
//! - always-erroring tool (ledger scan → stats) → disabled + escalated
//!   (`tool`), audit events asserted
//! - repair-model diagnosis is recorded when the `Repair` slot resolves,
//!   and skipped gracefully when it does not
//! - dry run detects without mutating; no targets attached skips the phase

use pantheon_api::capability::Policy;
use pantheon_api::events::Event;
use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, DefaultModel, ModelPolicy};
use pantheon_api::nightly::NightlyLlm;
use pantheon_api::provenance::Provenance;
use pantheon_nightly::{
    load_escalated, run_pass, run_repair_phase, EvalOutcome, EvalRunner, McpRepairTarget,
    McpServerSnapshot, NightlyConfig, NightlyDeps, NightlyEvent, Proposal, RepairOutcome,
    RepairTargets, ReplayRunner, ReplayTask, ScheduleRepairTarget, ScheduledJobSnapshot,
    ToolRepairTarget,
};
use pantheon_storage::Ledger;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

struct FakeMcp {
    snaps: Vec<McpServerSnapshot>,
    retries: VecDeque<bool>,
    re_resolves: VecDeque<bool>,
    pub retried: Vec<String>,
    pub re_resolved: Vec<String>,
    pub disabled: Vec<String>,
}

impl FakeMcp {
    fn new(snaps: Vec<McpServerSnapshot>) -> Self {
        Self {
            snaps,
            retries: VecDeque::new(),
            re_resolves: VecDeque::new(),
            retried: Vec::new(),
            re_resolved: Vec::new(),
            disabled: Vec::new(),
        }
    }

    fn snap(name: &str, status: &str, failures: u32) -> McpServerSnapshot {
        McpServerSnapshot {
            name: name.into(),
            status: status.into(),
            failures,
            last_error: Some("connection refused".into()),
        }
    }
}

impl McpRepairTarget for FakeMcp {
    fn servers(&mut self) -> Vec<McpServerSnapshot> {
        self.snaps.clone()
    }
    fn retry_connect(&mut self, name: &str) -> Result<(), String> {
        self.retried.push(name.into());
        match self.retries.pop_front() {
            Some(true) => Ok(()),
            _ => Err("connection refused".into()),
        }
    }
    fn re_resolve(&mut self, name: &str) -> Result<(), String> {
        self.re_resolved.push(name.into());
        match self.re_resolves.pop_front() {
            Some(true) => Ok(()),
            _ => Err("env var MCP_TOKEN still missing".into()),
        }
    }
    fn disable(&mut self, name: &str, _reason: &str) -> Result<(), String> {
        self.disabled.push(name.into());
        Ok(())
    }
}

struct FakeSchedule {
    jobs: Vec<ScheduledJobSnapshot>,
    pub repaired_cron: Vec<(String, String)>,
    pub paused: Vec<String>,
}

impl FakeSchedule {
    fn job(id: &str, kind: &str, cron_expr: Option<&str>, failures: u32) -> ScheduledJobSnapshot {
        ScheduledJobSnapshot {
            id: id.into(),
            kind: kind.into(),
            cron_expr: cron_expr.map(str::to_string),
            paused: false,
            consecutive_failures: failures,
            last_error: Some("executor error".into()),
        }
    }
}

impl ScheduleRepairTarget for FakeSchedule {
    fn jobs(&mut self) -> Vec<ScheduledJobSnapshot> {
        self.jobs.clone()
    }
    fn repair_cron(&mut self, id: &str, new_expr: &str) -> Result<(), String> {
        self.repaired_cron.push((id.into(), new_expr.into()));
        Ok(())
    }
    fn pause(&mut self, id: &str, _reason: &str) -> Result<(), String> {
        self.paused.push(id.into());
        Ok(())
    }
}

struct FakeTools {
    pub probed: Vec<String>,
    pub re_resolved: Vec<String>,
    pub disabled: Vec<String>,
}

impl ToolRepairTarget for FakeTools {
    fn probe(&mut self, name: &str) -> Result<(), String> {
        self.probed.push(name.into());
        Ok(())
    }
    fn re_resolve(&mut self, name: &str) -> Result<(), String> {
        self.re_resolved.push(name.into());
        Err("plugin dir gone".into())
    }
    fn disable(&mut self, name: &str, _reason: &str) -> Result<(), String> {
        self.disabled.push(name.into());
        Ok(())
    }
}

struct DiagLlm {
    text: String,
    calls: AtomicUsize,
}

impl DiagLlm {
    fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            calls: AtomicUsize::new(0),
        }
    }
}

impl NightlyLlm for DiagLlm {
    fn refine_proposal(
        &self,
        _model: &AuxiliaryModel,
        _draft: &pantheon_api::nightly::Proposal,
    ) -> Result<String, String> {
        Err("unused".into())
    }
    fn distill_memories(
        &self,
        _model: &AuxiliaryModel,
        _texts: &[String],
    ) -> Result<Vec<String>, String> {
        Err("unused".into())
    }
    fn diagnose_repair(&self, _model: &AuxiliaryModel, prompt: &str) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(
            prompt.contains("mcp-server")
                || prompt.contains("scheduled-job")
                || prompt.contains("tool"),
            "diagnosis prompt names the category"
        );
        // The repair slot must never be asked to do proposal work.
        assert!(!prompt.contains("proposal"), "diagnosis is not refinement");
        Ok(self.text.clone())
    }
}

fn repair_aux() -> AuxiliaryModel {
    AuxiliaryModel {
        kind: AuxiliaryKind::Repair,
        provider: "p".into(),
        model: "m".into(),
        timeout_secs: 60,
        target_percent: None,
    }
}

fn phase_config(dir: &std::path::Path) -> NightlyConfig {
    NightlyConfig {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

fn fix_attempts(events: &[NightlyEvent], id: &str) -> Vec<(usize, String, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            NightlyEvent::FixAttempt {
                id: i,
                attempt,
                phase,
                detail,
                ..
            } if i == id => Some((*attempt, phase.clone(), detail.clone())),
            _ => None,
        })
        .collect()
}

fn escalated(events: &[NightlyEvent], id: &str) -> Option<(String, usize)> {
    events.iter().find_map(|e| match e {
        NightlyEvent::Escalated {
            id: i,
            reason,
            attempts,
            ..
        } if i == id => Some((reason.clone(), *attempts)),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

#[test]
fn flapping_mcp_server_recovers_on_retry() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut mcp = FakeMcp::new(vec![FakeMcp::snap("flap", "failed", 5)]);
    mcp.retries.push_back(true);
    let mut targets = RepairTargets {
        mcp: Some(&mut mcp),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Repaired);
    assert!(
        mcp.disabled.is_empty(),
        "recovered server must not be disabled"
    );
    assert_eq!(mcp.retried, vec!["flap".to_string()]);
    let attempts = fix_attempts(&events, "mcp:flap");
    assert_eq!(attempts.len(), 1);
    assert!(attempts[0].2.contains("recovered"), "got: {:?}", attempts);
    assert!(escalated(&events, "mcp:flap").is_none());
    assert!(load_escalated(dir.path()).unwrap().is_empty());
}

#[test]
fn dead_mcp_server_disabled_and_escalated() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut mcp = FakeMcp::new(vec![FakeMcp::snap("dead", "backoff", 9)]);
    mcp.retries.push_back(false);
    mcp.re_resolves.push_back(false);
    let mut targets = RepairTargets {
        mcp: Some(&mut mcp),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Contained);
    assert_eq!(mcp.disabled, vec!["dead".to_string()]);
    assert_eq!(mcp.retried, vec!["dead".to_string()]);
    assert_eq!(mcp.re_resolved, vec!["dead".to_string()]);
    let (reason, attempts) = escalated(&events, "mcp:dead").expect("escalation audited");
    assert_eq!(attempts, 3);
    assert!(reason.contains("unrecoverable"), "got: {reason}");
    let escs = load_escalated(dir.path()).unwrap();
    assert_eq!(escs.len(), 1);
    assert_eq!(escs[0].kind, "mcp-server");
    assert_eq!(escs[0].id, "mcp:dead");
}

#[test]
fn disabled_and_unapproved_servers_are_never_targets() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut mcp = FakeMcp::new(vec![
        FakeMcp::snap("off", "disabled", 99),
        FakeMcp::snap("waiting", "unapproved", 99),
        FakeMcp::snap("ok", "ready", 0),
    ]);
    let mut targets = RepairTargets {
        mcp: Some(&mut mcp),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    // Only the ready server yields a report (Healthy); the other two are
    // skipped silently - operator intent / pending human decision.
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Healthy);
    assert!(mcp.retried.is_empty());
    assert!(mcp.disabled.is_empty());
    assert!(events.is_empty());
}

#[test]
fn below_threshold_server_is_healthy() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = phase_config(dir.path());
    config.mcp_max_failures = 5;
    let mut mcp = FakeMcp::new(vec![FakeMcp::snap("flaky", "failed", 2)]);
    let mut targets = RepairTargets {
        mcp: Some(&mut mcp),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Healthy);
    assert!(mcp.retried.is_empty());
    assert!(events.is_empty());
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

#[test]
fn repeatedly_failing_schedule_paused_and_escalated() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut sched = FakeSchedule {
        jobs: vec![FakeSchedule::job("nightly-digest", "interval", None, 3)],
        repaired_cron: Vec::new(),
        paused: Vec::new(),
    };
    let mut targets = RepairTargets {
        schedule: Some(&mut sched),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Contained);
    assert_eq!(sched.paused, vec!["nightly-digest".to_string()]);
    assert!(sched.repaired_cron.is_empty(), "no config to repair");
    let (reason, _) = escalated(&events, "schedule:nightly-digest").expect("escalated");
    assert!(reason.contains("Paused"), "got: {reason}");
    let escs = load_escalated(dir.path()).unwrap();
    assert_eq!(escs[0].kind, "schedule");
}

#[test]
fn bad_cron_with_safe_normalization_is_repaired() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut sched = FakeSchedule {
        jobs: vec![FakeSchedule::job(
            "every5",
            "cron",
            Some("0 */5 * * * *"),
            0,
        )],
        repaired_cron: Vec::new(),
        paused: Vec::new(),
    };
    let mut targets = RepairTargets {
        schedule: Some(&mut sched),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Repaired);
    assert_eq!(
        sched.repaired_cron,
        vec![("every5".to_string(), "*/5 * * * *".to_string())]
    );
    assert!(sched.paused.is_empty());
    assert!(escalated(&events, "schedule:every5").is_none());
    let attempts = fix_attempts(&events, "schedule:every5");
    assert!(attempts
        .iter()
        .any(|(_, p, d)| p == "schedule" && d.contains("*/5 * * * *")));
}

#[test]
fn unfixable_cron_paused_and_escalated() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut sched = FakeSchedule {
        jobs: vec![FakeSchedule::job("broken", "cron", Some("blah blah"), 0)],
        repaired_cron: Vec::new(),
        paused: Vec::new(),
    };
    let mut targets = RepairTargets {
        schedule: Some(&mut sched),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Contained);
    assert_eq!(sched.paused, vec!["broken".to_string()]);
    assert!(sched.repaired_cron.is_empty(), "nothing safe to apply");
    let (reason, _) = escalated(&events, "schedule:broken").expect("escalated");
    assert!(reason.contains("unparseable"), "got: {reason}");
}

// ---------------------------------------------------------------------------
// Tools (via run_repair_phase with stats; scan→stats covered by run_pass test)
// ---------------------------------------------------------------------------

#[test]
fn always_erroring_tool_disabled_and_escalated() {
    use pantheon_nightly::ToolCallStats;
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut tools = FakeTools {
        probed: Vec::new(),
        re_resolved: Vec::new(),
        disabled: Vec::new(),
    };
    let mut targets = RepairTargets {
        tools: Some(&mut tools),
        ..RepairTargets::default()
    };
    let stats = vec![ToolCallStats {
        name: "badtool".into(),
        calls: 3,
        errors: 3,
    }];
    let mut events = Vec::new();
    let reports = run_repair_phase(
        &config,
        dir.path(),
        &mut targets,
        &stats,
        None,
        &mut events,
        1,
    );
    assert_eq!(reports[0].outcome, RepairOutcome::Contained);
    assert_eq!(tools.re_resolved, vec!["badtool".to_string()]);
    assert_eq!(tools.disabled, vec!["badtool".to_string()]);
    let (reason, _) = escalated(&events, "tool:badtool").expect("escalated");
    assert!(reason.contains("unrecoverable"), "got: {reason}");
    let escs = load_escalated(dir.path()).unwrap();
    assert_eq!(escs[0].kind, "tool");
    // Audit trail: the re-resolve attempt and the escalation.
    let attempts = fix_attempts(&events, "tool:badtool");
    assert!(attempts
        .iter()
        .any(|(_, _, d)| d.contains("re_resolve failed")));
}

#[test]
fn healthy_tool_stats_produce_no_action() {
    use pantheon_nightly::ToolCallStats;
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut tools = FakeTools {
        probed: Vec::new(),
        re_resolved: Vec::new(),
        disabled: Vec::new(),
    };
    let mut targets = RepairTargets {
        tools: Some(&mut tools),
        ..RepairTargets::default()
    };
    // 2 of 3 calls errored: not "every invocation".
    let stats = vec![ToolCallStats {
        name: "flaky".into(),
        calls: 3,
        errors: 2,
    }];
    let mut events = Vec::new();
    let reports = run_repair_phase(
        &config,
        dir.path(),
        &mut targets,
        &stats,
        None,
        &mut events,
        1,
    );
    assert!(reports.is_empty());
    assert!(tools.disabled.is_empty());
    assert!(events.is_empty());
}

// ---------------------------------------------------------------------------
// Repair-model diagnosis (Repair slot, never Reflection)
// ---------------------------------------------------------------------------

#[test]
fn repair_model_diagnosis_is_recorded_when_slot_resolves() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let llm = DiagLlm::new("the token env var is unset");
    let aux = repair_aux();
    let mut mcp = FakeMcp::new(vec![FakeMcp::snap("dead", "failed", 5)]);
    mcp.retries.push_back(false);
    mcp.re_resolves.push_back(false);
    let mut targets = RepairTargets {
        mcp: Some(&mut mcp),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(
        &config,
        dir.path(),
        &mut targets,
        &[],
        Some((&llm, &aux)),
        &mut events,
        1,
    );
    assert_eq!(reports[0].outcome, RepairOutcome::Contained);
    assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    let attempts = fix_attempts(&events, "mcp:dead");
    assert!(
        attempts
            .iter()
            .any(|(_, _, d)| d.contains("the token env var is unset")),
        "diagnosis audited, got: {attempts:?}"
    );
    let (reason, _) = escalated(&events, "mcp:dead").expect("escalated");
    assert!(
        reason.contains("the token env var is unset"),
        "diagnosis rides the escalation, got: {reason}"
    );
}

#[test]
fn missing_repair_model_degrades_to_deterministic_only() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    // Slot unconfigured → None diagnoser: the deterministic ladder still
    // runs to containment.
    let mut mcp = FakeMcp::new(vec![FakeMcp::snap("dead", "failed", 5)]);
    mcp.retries.push_back(false);
    mcp.re_resolves.push_back(false);
    let mut targets = RepairTargets {
        mcp: Some(&mut mcp),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert_eq!(reports[0].outcome, RepairOutcome::Contained);
    assert_eq!(mcp.disabled, vec!["dead".to_string()]);
    let attempts = fix_attempts(&events, "mcp:dead");
    assert!(
        !attempts.iter().any(|(_, _, d)| d.contains("diagnosis")),
        "no diagnosis without a model, got: {attempts:?}"
    );
}

#[test]
fn resolve_repair_uses_the_repair_slot() {
    // The repair diagnosis resolves through AuxiliaryKind::Repair
    // mirroring resolve_refiner, never the Reflection slot.
    let llm = DiagLlm::new("x");
    let policy = ModelPolicy {
        default: DefaultModel {
            provider: "p".into(),
            model: "m".into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![repair_aux()],
        reasoning: Default::default(),
        reasoning_budget: None,
    };
    let resolved = pantheon_api::nightly::resolve_repair(&llm, &policy);
    assert!(resolved.is_some());
    assert_eq!(resolved.unwrap().1.kind, AuxiliaryKind::Repair);
    let bare = ModelPolicy {
        auxiliaries: vec![],
        ..policy
    };
    assert!(pantheon_api::nightly::resolve_repair(&llm, &bare).is_none());
}

// ---------------------------------------------------------------------------
// Dry run, skipped phase
// ---------------------------------------------------------------------------

#[test]
fn dry_run_detects_without_mutating() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = phase_config(dir.path());
    config.dry_run = true;
    let mut mcp = FakeMcp::new(vec![FakeMcp::snap("dead", "failed", 5)]);
    let mut sched = FakeSchedule {
        jobs: vec![FakeSchedule::job("j", "interval", None, 5)],
        repaired_cron: Vec::new(),
        paused: Vec::new(),
    };
    let mut targets = RepairTargets {
        mcp: Some(&mut mcp),
        schedule: Some(&mut sched),
        ..RepairTargets::default()
    };
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert!(reports
        .iter()
        .all(|r| r.outcome == RepairOutcome::SkippedDryRun));
    assert!(mcp.disabled.is_empty() && mcp.retried.is_empty());
    assert!(sched.paused.is_empty());
    assert!(load_escalated(dir.path()).unwrap().is_empty());
    assert!(fix_attempts(&events, "mcp:dead")
        .iter()
        .any(|(_, _, d)| d.contains("dry run")));
}

#[test]
fn phase_skipped_when_no_targets_attached() {
    let dir = tempfile::tempdir().unwrap();
    let config = phase_config(dir.path());
    let mut targets = RepairTargets::default();
    let mut events = Vec::new();
    let reports = run_repair_phase(&config, dir.path(), &mut targets, &[], None, &mut events, 1);
    assert!(reports.is_empty());
    assert!(events.is_empty());
}

// ---------------------------------------------------------------------------
// run_pass integration: ledger scan → tool stats → containment
// ---------------------------------------------------------------------------

struct PassEval;
impl EvalRunner for PassEval {
    fn run_eval(&self, _target: &str, _timeout: std::time::Duration) -> EvalOutcome {
        EvalOutcome::Pass
    }
}

struct NoReplay;
impl ReplayRunner for NoReplay {
    fn run_transcript(
        &self,
        _task: &ReplayTask,
        _with_proposal: Option<&Proposal>,
    ) -> Result<String, String> {
        Err("no replay tasks in repair tests".into())
    }
}

fn seed_failing_tool_runs(ledger: &Ledger) {
    for i in 0..3 {
        let run_id = format!("run_{i}");
        ledger
            .append(&Event::RunStarted {
                run_id: run_id.clone(),
            })
            .unwrap();
        ledger
            .append(&Event::TurnStarted {
                run_id: run_id.clone(),
                turn_id: "t1".into(),
            })
            .unwrap();
        ledger
            .append(&Event::ToolStarted {
                run_id: run_id.clone(),
                call_id: format!("c{i}"),
                tool: "badtool".into(),
                args: "{}".into(),
                provenance: Provenance::system("eval"),
            })
            .unwrap();
        ledger
            .append(&Event::TurnFailed {
                run_id: run_id.clone(),
                turn_id: "t1".into(),
                code: "TOOL_ERROR".into(),
            })
            .unwrap();
    }
}

#[test]
fn run_pass_contains_always_erroring_tool() {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    seed_failing_tool_runs(&ledger);
    let backend = pantheon_memory::open_selected(dir.path()).unwrap();
    let policy = Policy::coder_with_memory();
    let model_policy = ModelPolicy {
        default: DefaultModel {
            provider: "p".into(),
            model: "m".into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![],
        reasoning: Default::default(),
        reasoning_budget: None,
    };
    let mut tools = FakeTools {
        probed: Vec::new(),
        re_resolved: Vec::new(),
        disabled: Vec::new(),
    };
    let targets = RepairTargets {
        tools: Some(&mut tools),
        ..RepairTargets::default()
    };
    let config = NightlyConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    let mut deps = NightlyDeps {
        ledger: &ledger,
        backend: backend.as_ref(),
        capability_policy: &policy,
        model_policy: &model_policy,
        eval_runner: &PassEval,
        replay_runner: &NoReplay,
        llm: None,
        repair: Some(targets),
    };
    let out = run_pass(&config, &mut deps).unwrap();
    // The scan attributed 3 failures to badtool over 3 calls → broken →
    // re-resolve failed → disabled + escalated.
    assert_eq!(tools.disabled, vec!["badtool".to_string()]);
    assert!(out
        .repairs
        .iter()
        .any(|r| r.target == "tool:badtool" && r.outcome == RepairOutcome::Contained));
    let escs = load_escalated(dir.path()).unwrap();
    assert!(escs
        .iter()
        .any(|e| e.kind == "tool" && e.id == "tool:badtool"));
    // And the audit log carries the attempt trail.
    let audit_events = pantheon_nightly::read_events(dir.path());
    assert!(audit_events.iter().any(|e| matches!(
        e,
        NightlyEvent::FixAttempt { id, phase, .. } if id == "tool:badtool" && phase == "tool"
    )));
    assert!(audit_events.iter().any(|e| matches!(
        e,
        NightlyEvent::Escalated { id, .. } if id == "tool:badtool"
    )));
}

#[test]
fn run_history_counts_consecutive_failures_for_repair() {
    use pantheon_scheduler::{RunHistory, RunOutcome};
    let dir = tempfile::tempdir().unwrap();
    let mut h = RunHistory::open(dir.path()).unwrap();
    h.record("j", RunOutcome::Panicked, Some("boom".into()), 1)
        .unwrap();
    h.record("j", RunOutcome::TimedOut, None, 2).unwrap();
    h.record("j", RunOutcome::Panicked, None, 3).unwrap();
    assert_eq!(h.stats("j").unwrap().consecutive_failures, 3);
    // The nightly schedule adapter would now see a broken job: simulate
    // the detection half with a snapshot built from the history.
    let snap = ScheduledJobSnapshot {
        id: "j".into(),
        kind: "interval".into(),
        cron_expr: None,
        paused: false,
        consecutive_failures: h.stats("j").unwrap().consecutive_failures,
        last_error: h.stats("j").unwrap().last_error.clone(),
    };
    assert!(snap.consecutive_failures >= NightlyConfig::default().schedule_max_failures);
    // A success resets the streak: the job is no longer broken.
    h.record("j", RunOutcome::Completed, None, 4).unwrap();
    assert_eq!(h.stats("j").unwrap().consecutive_failures, 0);
}
