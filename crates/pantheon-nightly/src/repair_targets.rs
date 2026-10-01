//! Nightly repair of broken operational targets: MCP servers, scheduled
//! jobs, and tools.
//!
//! Detection is snapshot-driven. The host hands the pass one snapshot per
//! target through [`McpRepairTarget`], [`ScheduleRepairTarget`], and
//! [`ToolRepairTarget`]; the pass walks a bounded repair ladder per
//! broken target:
//!
//! 1. a cheap deterministic retry (re-probe the server / re-run the
//!    check) — the flapping-target second chance;
//! 2. LLM diagnosis through the `Repair` auxiliary slot
//!    ([`NightlyLlm::diagnose_repair`]) — advisory only, recorded in the
//!    audit log and any escalation; skipped silently when the slot is
//!    unconfigured or the call fails;
//! 3. a bounded config repair (re-resolve env/paths; normalize a broken
//!    cron expression);
//! 4. contain: **disable** the MCP server / tool, or **pause** the
//!    scheduled job, and escalate to `nightly-escalated.json` with kind
//!    `mcp-server` / `schedule` / `tool`.
//!
//! Step 4 is the floor: a target the pass cannot fix is contained and
//! escalated — never left retrying silently, never repaired into a worse
//! state. Every attempt is audited with the existing
//! [`NightlyEvent::FixAttempt`] shape; escalations reuse
//! [`NightlyEvent::Escalated`] and the fix loop's escalation file.
//!
//! The pass never touches `McpManager`, `TickDriver`, or `ToolRegistry`
//! directly: hosts implement the three traits (production adapters are
//! host wiring; tests use fakes), so this crate's dependency set is
//! unchanged. `Disabled` MCP servers and paused jobs are operator intent
//! and are never repair targets; unapproved MCP servers wait on a human
//! and are never touched either.
//!
//! Boundedness: at most two mutating attempts per target (retry, then
//! re-resolve) before containment; targets are visited in sorted order;
//! adapters must bound each call (suggested ≤ 30s) — a repair phase that
//! hangs the nightly pass is a bug.

use crate::audit::NightlyEvent;
use crate::fixloop::{record_escalation, Escalation};
use crate::{NightlyConfig, NightlyLlm};
use pantheon_api::model::AuxiliaryModel;
use pantheon_scheduler::cron::{normalize_cron_expr, CronSchedule};
use std::path::Path;

/// One MCP server as the repair phase sees it. The host builds these
/// from `McpManager::health()` / `check_health()`.
#[derive(Debug, Clone)]
pub struct McpServerSnapshot {
    pub name: String,
    /// `ServerStatus::as_str()`: "disabled" | "unapproved" |
    /// "connecting" | "ready" | "backoff" | "failed".
    pub status: String,
    /// Consecutive failures (the manager resets this on success).
    pub failures: u32,
    pub last_error: Option<String>,
}

/// One scheduled job as the repair phase sees it. The host builds these
/// from `schedule.json` plus [`pantheon_scheduler::RunHistory`].
#[derive(Debug, Clone)]
pub struct ScheduledJobSnapshot {
    pub id: String,
    /// "cron" | "interval" | "one-shot".
    pub kind: String,
    /// The cron expression, for cron jobs.
    pub cron_expr: Option<String>,
    pub paused: bool,
    /// Consecutive run failures from the run history.
    pub consecutive_failures: u32,
    pub last_error: Option<String>,
}

/// Per-tool invocation health over the pass's ledger scan window. Errors
/// are attributed via failed turns, exactly like the `RepeatedFailure`
/// signal: an "invocation error" is a call whose turn failed.
#[derive(Debug, Clone)]
pub struct ToolCallStats {
    pub name: String,
    pub calls: u64,
    pub errors: u64,
}

/// What the host lets the nightly pass do to its MCP servers.
pub trait McpRepairTarget {
    /// Fresh health snapshot of every managed server.
    fn servers(&mut self) -> Vec<McpServerSnapshot>;
    /// One more real connect attempt (the flapping-server second chance).
    fn retry_connect(&mut self, name: &str) -> Result<(), String>;
    /// Re-resolve the server's config — env vars and paths may have
    /// changed since it was configured — then retry the connection.
    fn re_resolve(&mut self, name: &str) -> Result<(), String>;
    /// Disable the server: stop reconnect attempts, keep the spec for the
    /// operator. The escalation tells the human to mirror the disable in
    /// config.
    fn disable(&mut self, name: &str, reason: &str) -> Result<(), String>;
}

/// What the host lets the nightly pass do to its scheduled jobs.
pub trait ScheduleRepairTarget {
    /// Snapshot of every job plus its run-history counters.
    fn jobs(&mut self) -> Vec<ScheduledJobSnapshot>;
    /// Apply a repaired cron expression (already validated by the pass).
    /// The host persists it to `schedule.json` and resets the job's run
    /// history.
    fn repair_cron(&mut self, id: &str, new_expr: &str) -> Result<(), String>;
    /// Pause the job: it stops firing until a human resumes it.
    fn pause(&mut self, id: &str, reason: &str) -> Result<(), String>;
}

/// What the host lets the nightly pass do to its tools.
pub trait ToolRepairTarget {
    /// Smoke-probe one tool: an empty-args invocation behind the
    /// capability policy. Called only for names in
    /// [`NightlyConfig::tool_probe_allowlist`]; hosts must refuse to
    /// probe anything else (probing an arbitrary tool executes it).
    fn probe(&mut self, name: &str) -> Result<(), String>;
    /// Re-resolve the tool's backing config (plugin path/manifest, MCP
    /// server env/paths) and probe again.
    fn re_resolve(&mut self, name: &str) -> Result<(), String>;
    /// Disable the tool: remove it from the live registry and record the
    /// disable so the next registry build skips it.
    fn disable(&mut self, name: &str, reason: &str) -> Result<(), String>;
}

/// The world the repair phase acts on. Every slot is `Option`: a host
/// that does not manage MCP servers (or schedules, or tools) leaves its
/// slot `None` and that category is skipped.
#[derive(Default)]
pub struct RepairTargets<'a> {
    pub mcp: Option<&'a mut dyn McpRepairTarget>,
    pub schedule: Option<&'a mut dyn ScheduleRepairTarget>,
    pub tools: Option<&'a mut dyn ToolRepairTarget>,
}

/// What the phase did to one target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairOutcome {
    /// Healthy (or below threshold): no action.
    Healthy,
    /// A repair step fixed it.
    Repaired,
    /// Disabled (MCP server / tool) or paused (scheduled job), always
    /// paired with an escalation.
    Contained,
    /// Dry run: detected, audited, not mutated.
    SkippedDryRun,
}

/// One target's repair result, for the pass report.
#[derive(Debug, Clone)]
pub struct RepairReport {
    /// "mcp:<name>" | "schedule:<id>" | "tool:<name>".
    pub target: String,
    pub outcome: RepairOutcome,
    pub detail: String,
}

/// Diagnose one broken target through the `Repair` auxiliary slot.
///
/// Advisory only: the returned text is recorded in the audit log and any
/// escalation; repair *actions* stay deterministic. `None` when no repair
/// model is configured, LLM steps are off, or the call fails — the
/// deterministic ladder proceeds either way. The pass never fails for a
/// missing repair model.
fn diagnose(
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    category: &str,
    target: &str,
    context: &str,
) -> Option<String> {
    let (llm, aux) = repair?;
    let prompt = format!(
        "Category: {category}\nTarget: {target}\n{context}\n\
         Diagnose the likely root cause and suggest the concrete, \
         non-destructive config fix (env var, path, command, args, \
         schedule expression). Keep it short and specific."
    );
    match llm.diagnose_repair(aux, &prompt) {
        Ok(text) => {
            let text = text.trim();
            if text.is_empty() {
                None
            } else {
                // Audit lines stay readable; the full text rides the
                // escalation reason.
                Some(truncate(text, 2000))
            }
        }
        Err(_) => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn fix_attempt(
    events: &mut Vec<NightlyEvent>,
    id: &str,
    attempt: usize,
    phase: &str,
    detail: String,
    at_ms: i64,
) {
    events.push(NightlyEvent::FixAttempt {
        id: id.to_string(),
        attempt,
        phase: phase.to_string(),
        detail,
        at_ms,
    });
}

fn escalate_target(events: &mut Vec<NightlyEvent>, data_dir: &Path, esc: Escalation) {
    events.push(NightlyEvent::Escalated {
        id: esc.id.clone(),
        reason: esc.reason.clone(),
        attempts: esc.attempts,
        at_ms: esc.at_ms,
    });
    // A failed escalation write must not lose the verdict: the audit
    // event already records it; the JSON file is the readable surface.
    let _ = record_escalation(data_dir, &esc);
}

fn with_diagnosis(base: String, diagnosis: &Option<String>) -> String {
    match diagnosis {
        Some(d) => format!("{base} Repair-model diagnosis: {d}"),
        None => base,
    }
}

/// Run the repair phase: MCP servers, then scheduled jobs, then tools.
///
/// `repair` is the resolved (`Repair`-slot) diagnoser, or `None` for a
/// deterministic-only pass. Every attempt and escalation is pushed onto
/// `events`; the caller audits them with the rest of the pass.
#[allow(clippy::too_many_arguments)]
pub fn run_repair_phase(
    config: &NightlyConfig,
    data_dir: &Path,
    targets: &mut RepairTargets<'_>,
    tool_stats: &[ToolCallStats],
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
) -> Vec<RepairReport> {
    let mut reports = Vec::new();
    if let Some(mcp) = targets.mcp.as_mut() {
        repair_mcp(config, data_dir, *mcp, repair, events, at_ms, &mut reports);
    }
    if let Some(sched) = targets.schedule.as_mut() {
        repair_schedules(
            config,
            data_dir,
            *sched,
            repair,
            events,
            at_ms,
            &mut reports,
        );
    }
    if let Some(tools) = targets.tools.as_mut() {
        repair_tools(
            config,
            data_dir,
            *tools,
            tool_stats,
            repair,
            events,
            at_ms,
            &mut reports,
        );
    }
    reports
}

fn is_broken_server(snap: &McpServerSnapshot, max_failures: u32) -> bool {
    (snap.status == "failed" || snap.status == "backoff") && snap.failures >= max_failures
}

#[allow(clippy::too_many_arguments)]
fn repair_mcp(
    config: &NightlyConfig,
    data_dir: &Path,
    mcp: &mut dyn McpRepairTarget,
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
    reports: &mut Vec<RepairReport>,
) {
    let mut snaps = mcp.servers();
    snaps.sort_by(|a, b| a.name.cmp(&b.name));
    for snap in snaps {
        let id = format!("mcp:{}", snap.name);
        // Operator intent or a human's pending decision: never targets.
        if snap.status == "disabled" || snap.status == "unapproved" {
            continue;
        }
        if !is_broken_server(&snap, config.mcp_max_failures) {
            reports.push(RepairReport {
                target: id,
                outcome: RepairOutcome::Healthy,
                detail: format!("status={} failures={}", snap.status, snap.failures),
            });
            continue;
        }
        let ctx = format!(
            "Status: {} ({} consecutive failures)\nLast error: {}",
            snap.status,
            snap.failures,
            snap.last_error.as_deref().unwrap_or("(none)")
        );
        if config.dry_run {
            fix_attempt(
                events,
                &id,
                0,
                "mcp",
                format!("dry run: would repair ({ctx})"),
                at_ms,
            );
            reports.push(RepairReport {
                target: id,
                outcome: RepairOutcome::SkippedDryRun,
                detail: ctx,
            });
            continue;
        }
        // Attempt 1: one more real connect — the flapping-server second
        // chance.
        match mcp.retry_connect(&snap.name) {
            Ok(()) => {
                fix_attempt(
                    events,
                    &id,
                    1,
                    "mcp",
                    format!(
                        "retry_connect succeeded after {} failures; server recovered",
                        snap.failures
                    ),
                    at_ms,
                );
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::Repaired,
                    detail: "retry_connect recovered the server".into(),
                });
                continue;
            }
            Err(e1) => {
                fix_attempt(
                    events,
                    &id,
                    1,
                    "mcp",
                    format!("retry_connect failed: {e1}"),
                    at_ms,
                );
                // Attempt 2 (advisory): repair-model diagnosis.
                let diagnosis = diagnose(repair, "mcp-server", &snap.name, &ctx);
                if let Some(d) = &diagnosis {
                    fix_attempt(
                        events,
                        &id,
                        2,
                        "mcp",
                        format!("repair-model diagnosis: {d}"),
                        at_ms,
                    );
                }
                // Attempt 3: re-resolve config (env/paths may have
                // changed), then retry.
                match mcp.re_resolve(&snap.name) {
                    Ok(()) => {
                        fix_attempt(
                            events,
                            &id,
                            3,
                            "mcp",
                            "re_resolve + retry succeeded; server recovered".into(),
                            at_ms,
                        );
                        reports.push(RepairReport {
                            target: id,
                            outcome: RepairOutcome::Repaired,
                            detail: "re_resolve recovered the server".into(),
                        });
                    }
                    Err(e2) => {
                        let reason = with_diagnosis(
                            format!(
                                "mcp server '{}' unrecoverable: retry failed ({e1}); \
                                 config re-resolve failed ({e2}). Disabled to stop \
                                 reconnect churn; re-enable in [mcp.servers] after fixing.",
                                snap.name
                            ),
                            &diagnosis,
                        );
                        match mcp.disable(&snap.name, &reason) {
                            Ok(()) => {}
                            Err(e) => {
                                // Containment failed too: still escalate —
                                // the human must know. The audit trail
                                // carries both failures.
                                fix_attempt(
                                    events,
                                    &id,
                                    4,
                                    "mcp",
                                    format!("disable failed: {e}; escalating anyway"),
                                    at_ms,
                                );
                            }
                        }
                        escalate_target(
                            events,
                            data_dir,
                            Escalation {
                                id: id.clone(),
                                kind: "mcp-server".into(),
                                title: format!("MCP server '{}' disabled", snap.name),
                                reason: reason.clone(),
                                attempts: 3,
                                at_ms,
                            },
                        );
                        reports.push(RepairReport {
                            target: id,
                            outcome: RepairOutcome::Contained,
                            detail: reason,
                        });
                    }
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn repair_schedules(
    config: &NightlyConfig,
    data_dir: &Path,
    sched: &mut dyn ScheduleRepairTarget,
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
    reports: &mut Vec<RepairReport>,
) {
    let mut jobs = sched.jobs();
    jobs.sort_by(|a, b| a.id.cmp(&b.id));
    for job in jobs {
        let id = format!("schedule:{}", job.id);
        if job.paused {
            continue;
        }
        // Config-shaped breakage first: a cron expression that cannot
        // parse means the job silently never fires.
        if job.kind == "cron" {
            if let Some(expr) = &job.cron_expr {
                if CronSchedule::validate(expr).is_err() {
                    repair_bad_cron(
                        config, data_dir, sched, repair, events, at_ms, reports, &job, expr, &id,
                    );
                    continue;
                }
            }
        }
        if job.consecutive_failures < config.schedule_max_failures {
            reports.push(RepairReport {
                target: id,
                outcome: RepairOutcome::Healthy,
                detail: format!("consecutive_failures={}", job.consecutive_failures),
            });
            continue;
        }
        let ctx = format!(
            "Kind: {}\nConsecutive run failures: {}\nLast error: {}",
            job.kind,
            job.consecutive_failures,
            job.last_error.as_deref().unwrap_or("(none)")
        );
        if config.dry_run {
            fix_attempt(
                events,
                &id,
                0,
                "schedule",
                format!("dry run: would pause ({ctx})"),
                at_ms,
            );
            reports.push(RepairReport {
                target: id,
                outcome: RepairOutcome::SkippedDryRun,
                detail: ctx,
            });
            continue;
        }
        // No retry-the-job ladder: re-firing a failing job is not the
        // nightly's job. Diagnose (advisory), then pause + escalate — a
        // failing cron spamming errors nightly is worse than a paused one.
        let diagnosis = diagnose(repair, "scheduled-job", &job.id, &ctx);
        if let Some(d) = &diagnosis {
            fix_attempt(
                events,
                &id,
                1,
                "schedule",
                format!("repair-model diagnosis: {d}"),
                at_ms,
            );
        }
        let reason = with_diagnosis(
            format!(
                "scheduled job '{}' failed {} consecutive run(s); last error: {}. \
                 Paused to stop the nightly error spam; resume after fixing.",
                job.id,
                job.consecutive_failures,
                job.last_error.as_deref().unwrap_or("(none)")
            ),
            &diagnosis,
        );
        match sched.pause(&job.id, &reason) {
            Ok(()) => {}
            Err(e) => fix_attempt(
                events,
                &id,
                2,
                "schedule",
                format!("pause failed: {e}; escalating anyway"),
                at_ms,
            ),
        }
        escalate_target(
            events,
            data_dir,
            Escalation {
                id: id.clone(),
                kind: "schedule".into(),
                title: format!("Scheduled job '{}' paused", job.id),
                reason: reason.clone(),
                attempts: 1,
                at_ms,
            },
        );
        reports.push(RepairReport {
            target: id,
            outcome: RepairOutcome::Contained,
            detail: reason,
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn repair_bad_cron(
    config: &NightlyConfig,
    data_dir: &Path,
    sched: &mut dyn ScheduleRepairTarget,
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
    reports: &mut Vec<RepairReport>,
    job: &ScheduledJobSnapshot,
    expr: &str,
    id: &str,
) {
    if config.dry_run {
        fix_attempt(
            events,
            id,
            0,
            "schedule",
            format!("dry run: cron expression '{expr}' does not parse; would attempt repair"),
            at_ms,
        );
        reports.push(RepairReport {
            target: id.to_string(),
            outcome: RepairOutcome::SkippedDryRun,
            detail: format!("unparseable cron '{expr}'"),
        });
        return;
    }
    // Bounded deterministic repair: normalize common shapes, then
    // re-validate. Anything else is not safely guessable.
    match normalize_cron_expr(expr).filter(|fixed| CronSchedule::validate(fixed).is_ok()) {
        Some(fixed) => match sched.repair_cron(&job.id, &fixed) {
            Ok(()) => {
                fix_attempt(
                    events,
                    id,
                    1,
                    "schedule",
                    format!("normalized cron '{expr}' → '{fixed}'; job repaired"),
                    at_ms,
                );
                reports.push(RepairReport {
                    target: id.to_string(),
                    outcome: RepairOutcome::Repaired,
                    detail: format!("cron '{expr}' → '{fixed}'"),
                });
            }
            Err(e) => {
                fix_attempt(
                    events,
                    id,
                    1,
                    "schedule",
                    format!("repair_cron failed for '{expr}' → '{fixed}': {e}"),
                    at_ms,
                );
                pause_bad_cron(data_dir, sched, None, events, at_ms, reports, job, expr, id);
            }
        },
        None => {
            let diagnosis = diagnose(
                repair,
                "scheduled-job",
                &job.id,
                &format!("Cron expression '{expr}' does not parse and has no safe normalization."),
            );
            if let Some(d) = &diagnosis {
                fix_attempt(
                    events,
                    id,
                    1,
                    "schedule",
                    format!("repair-model diagnosis: {d}"),
                    at_ms,
                );
            }
            pause_bad_cron(
                data_dir, sched, diagnosis, events, at_ms, reports, job, expr, id,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn pause_bad_cron(
    data_dir: &Path,
    sched: &mut dyn ScheduleRepairTarget,
    diagnosis: Option<String>,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
    reports: &mut Vec<RepairReport>,
    job: &ScheduledJobSnapshot,
    expr: &str,
    id: &str,
) {
    let reason = with_diagnosis(
        format!(
            "scheduled job '{}' has an unparseable cron expression '{expr}' and no safe \
             normalization exists; it would silently never fire. Paused; fix the expression.",
            job.id
        ),
        &diagnosis,
    );
    if let Err(e) = sched.pause(&job.id, &reason) {
        fix_attempt(
            events,
            id,
            2,
            "schedule",
            format!("pause failed: {e}; escalating anyway"),
            at_ms,
        );
    }
    escalate_target(
        events,
        data_dir,
        Escalation {
            id: id.to_string(),
            kind: "schedule".into(),
            title: format!("Scheduled job '{}' paused (bad cron)", job.id),
            reason: reason.clone(),
            attempts: 1,
            at_ms,
        },
    );
    reports.push(RepairReport {
        target: id.to_string(),
        outcome: RepairOutcome::Contained,
        detail: reason,
    });
}

#[allow(clippy::too_many_arguments)]
fn repair_tools(
    config: &NightlyConfig,
    data_dir: &Path,
    tools: &mut dyn ToolRepairTarget,
    tool_stats: &[ToolCallStats],
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
    reports: &mut Vec<RepairReport>,
) {
    // Broken by stats: every invocation in the window errored.
    let mut broken: Vec<(String, String)> = tool_stats
        .iter()
        .filter(|s| s.calls >= config.tool_min_calls as u64 && s.errors >= s.calls)
        .map(|s| {
            (
                s.name.clone(),
                format!("{} of {} recent invocation(s) errored", s.errors, s.calls),
            )
        })
        .collect();
    // Broken by smoke probe (opt-in allowlist only).
    for name in &config.tool_probe_allowlist {
        if broken.iter().any(|(n, _)| n == name) {
            continue;
        }
        match tools.probe(name) {
            Ok(()) => {}
            Err(e) => broken.push((name.clone(), format!("smoke probe failed: {e}"))),
        }
    }
    broken.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, why) in broken {
        let id = format!("tool:{name}");
        let ctx = format!("Tool '{name}': {why}.");
        if config.dry_run {
            fix_attempt(
                events,
                &id,
                0,
                "tool",
                format!("dry run: would repair ({ctx})"),
                at_ms,
            );
            reports.push(RepairReport {
                target: id,
                outcome: RepairOutcome::SkippedDryRun,
                detail: ctx,
            });
            continue;
        }
        // Attempt 1: re-resolve the backing config, then probe again.
        match tools.re_resolve(&name) {
            Ok(()) => {
                fix_attempt(
                    events,
                    &id,
                    1,
                    "tool",
                    format!("re_resolve + probe succeeded; tool '{name}' recovered"),
                    at_ms,
                );
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::Repaired,
                    detail: format!("re_resolve recovered '{name}'"),
                });
            }
            Err(e1) => {
                fix_attempt(
                    events,
                    &id,
                    1,
                    "tool",
                    format!("re_resolve failed for '{name}': {e1}"),
                    at_ms,
                );
                let diagnosis = diagnose(repair, "tool", &name, &ctx);
                if let Some(d) = &diagnosis {
                    fix_attempt(
                        events,
                        &id,
                        2,
                        "tool",
                        format!("repair-model diagnosis: {d}"),
                        at_ms,
                    );
                }
                let reason = with_diagnosis(
                    format!(
                        "tool '{name}' unrecoverable ({why}); re-resolve failed ({e1}). \
                         Disabled so the agent loop stops offering it; re-enable after fixing.",
                        why = why,
                        e1 = e1,
                    ),
                    &diagnosis,
                );
                match tools.disable(&name, &reason) {
                    Ok(()) => {}
                    Err(e) => fix_attempt(
                        events,
                        &id,
                        3,
                        "tool",
                        format!("disable failed: {e}; escalating anyway"),
                        at_ms,
                    ),
                }
                escalate_target(
                    events,
                    data_dir,
                    Escalation {
                        id: id.clone(),
                        kind: "tool".into(),
                        title: format!("Tool '{name}' disabled"),
                        reason: reason.clone(),
                        attempts: 2,
                        at_ms,
                    },
                );
                reports.push(RepairReport {
                    target: id,
                    outcome: RepairOutcome::Contained,
                    detail: reason,
                });
            }
        }
    }
}

// Small deterministic invariant tests only: id shapes, truncation,
// broken-server predicate. Behavioral tests live in
// `eval/tests/nightly_repair.rs`.
