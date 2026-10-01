//! Bounded fix loop: diagnose → revise → re-check, then escalate.
//!
//! When a skill/persona proposal fails eval-gating or replay-gating,
//! the pass doesn't just reject it — it tries to repair the DRAFT
//! first:
//!
//! - **Eval reject**: the eval failure detail goes to the repair aux
//!   model (Repair slot), which revises the draft body; the FULL eval
//!   tag set re-runs against the revised draft. Eval tags are immutable
//!   once the proposal enters the loop — the loop may not narrow the
//!   claim to dodge failing evals (that let broken drafts validate
//!   green).
//! - **Skipped evals** (no tags): escalates unless the proposal kind is
//!   explicitly allowlisted to skip eval-gating. Currently no kind is
//!   allowlisted — memory lessons never reach the gate at all (lib.rs
//!   handles them on a separate path: the promotion rule IS the gate).
//! - **Replay reject, fair measurement** ("no strict improvement"): one
//!   flakiness re-run of the A/B pair per attempt. With LLM steps
//!   enabled, the repair aux model (Repair slot) may instead sharpen
//!   the draft once, then both gates re-run on the revised draft.
//! - **Replay reject, infrastructure** (no tasks, spawn/timeout/scoring
//!   errors): not repairable by revision — escalate immediately.
//!
//! After `max_fix_attempts` the proposal is marked `NeedsAttention` and
//! recorded in `nightly-escalated.json`; it never loops forever and it
//! never reaches the approval queue unvalidated. Every attempt and
//! every escalation is audited (`FixAttempt`, `Escalated`).

use crate::audit::NightlyEvent;
use crate::gate::{gate, EvalRunner, EvalVerdict};
use crate::propose::{Proposal, ProposalKind, ProposalStatus};
use crate::replay::{replay_gate, ReplayRunner, ReplayStore, ReplayVerdict};
use crate::{NightlyConfig, NightlyLlm};
use pantheon_api::model::AuxiliaryModel;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many diagnose → revise → re-check iterations a failing proposal
/// gets before escalation. Default 3.
pub const DEFAULT_MAX_FIX_ATTEMPTS: usize = 3;

/// The fix loop's verdict on one proposal.
#[derive(Debug)]
pub enum FixOutcome {
    /// Validation eventually passed; the caller may queue for approval.
    Validated,
    /// The loop gave up (or hit unrepairable infrastructure). The
    /// proposal is marked `NeedsAttention` and recorded; the reason
    /// names the cause.
    Escalated { reason: String },
}

/// Where escalated proposals live: `<data_dir>/nightly/nightly-escalated.json`.
pub fn escalated_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nightly").join("nightly-escalated.json")
}

/// One escalation record.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Escalation {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub reason: String,
    pub attempts: usize,
    pub at_ms: i64,
}

/// Load escalations (empty when the file is absent).
pub fn load_escalated(data_dir: &Path) -> Result<Vec<Escalation>, String> {
    match std::fs::read_to_string(escalated_path(data_dir)) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("parse escalations: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("read escalations: {e}")),
    }
}

pub(crate) fn record_escalation(data_dir: &Path, esc: &Escalation) -> Result<(), String> {
    let mut all = load_escalated(data_dir)?;
    // One record per proposal: a re-escalation replaces the old one.
    if let Some(pos) = all.iter().position(|e| e.id == esc.id) {
        all[pos] = esc.clone();
    } else {
        all.push(esc.clone());
    }
    let path = escalated_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text =
        serde_json::to_string_pretty(&all).map_err(|e| format!("encode escalations: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Proposal kinds explicitly allowed to reach `Validated` with zero
/// evals run. Currently none: every kind that reaches `gate()` must
/// run its tagged evals. Memory lessons never reach the gate at all —
/// lib.rs handles them on a separate path ("the promotion rule IS the
/// gate") — so the empty allowlist is deliberate, not an oversight.
/// The match is exhaustive so adding a kind forces a conscious choice.
fn eval_skip_allowed(kind: &ProposalKind) -> bool {
    match kind {
        ProposalKind::MemoryLesson { .. }
        | ProposalKind::Skill { .. }
        | ProposalKind::Persona { .. } => false,
    }
}

/// Validate one skill/persona proposal through the bounded fix loop.
///
/// Returns `Validated` when eval-gating and replay-gating both pass
/// (possibly after repairs), `Escalated` when the loop gives up. Every
/// attempt and the escalation are pushed onto `events`; the caller
/// audits them with the rest of the pass.
///
/// `judge` scores replay transcripts (`ReplayCheck::Judge`); `repair`
/// revises drafts on eval/replay reject — all draft revision goes
/// through the Repair slot, never Reflection. `repair = None` means
/// revision is unavailable: the loop falls back to plain retries,
/// then escalation.
///
/// Eval tags are immutable here: an eval reject revises the draft via
/// the repair model and re-runs the full tag set; it never prunes
/// tags to dodge a failing eval.
#[allow(clippy::too_many_arguments)]
pub fn validate_with_fix_loop(
    proposal: &mut Proposal,
    eval_runner: &dyn EvalRunner,
    replay_runner: &dyn ReplayRunner,
    replay_store: &ReplayStore,
    judge: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    config: &NightlyConfig,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
) -> FixOutcome {
    let mut attempts = 0;
    loop {
        // ---- Eval gate: the FULL immutable tag set runs every pass. ----
        let tags: Vec<&str> = proposal.eval_tags.iter().map(String::as_str).collect();
        match gate(proposal, &tags, eval_runner, config) {
            EvalVerdict::Pass(_) => {}
            EvalVerdict::Skipped(reason) => {
                if !eval_skip_allowed(&proposal.kind) {
                    return escalate(
                        &config.data_dir,
                        proposal,
                        format!(
                            "eval gate skipped ({}); kind '{}' is not allowlisted to skip eval-gating",
                            reason,
                            proposal.kind_name()
                        ),
                        attempts,
                        events,
                        at_ms,
                    );
                }
            }
            EvalVerdict::Reject(reason) => {
                if attempts >= config.max_fix_attempts {
                    return escalate(
                        &config.data_dir,
                        proposal,
                        format!("eval gate rejected: {reason}"),
                        attempts,
                        events,
                        at_ms,
                    );
                }
                attempts += 1;
                let detail = match try_eval_sharpen(repair, proposal, &reason) {
                    Some(sharpened) => {
                        proposal.body = sharpened;
                        "sharpened draft via Repair slot; re-running gate"
                    }
                    None => "re-running eval gate (flakiness retry)",
                };
                events.push(NightlyEvent::FixAttempt {
                    id: proposal.id.clone(),
                    attempt: attempts,
                    phase: "eval".into(),
                    detail: detail.into(),
                    at_ms,
                });
                continue;
            }
        }

        // ---- Replay gate: strict improvement on held-out tasks. ----
        match replay_gate(proposal, replay_store, replay_runner, judge, config) {
            ReplayVerdict::Pass { summary } => {
                events.push(NightlyEvent::ReplayPassed {
                    id: proposal.id.clone(),
                    summary,
                    at_ms,
                });
                return FixOutcome::Validated;
            }
            ReplayVerdict::Fail { reason } => {
                // Infrastructure failures (no tasks, spawn/timeout/
                // scoring errors) are not repairable by revision:
                // escalate immediately, no attempts consumed.
                if !fair_measurement(&reason) {
                    return escalate(
                        &config.data_dir,
                        proposal,
                        format!("replay gate failed: {reason}"),
                        attempts,
                        events,
                        at_ms,
                    );
                }
                if attempts >= config.max_fix_attempts {
                    return escalate(
                        &config.data_dir,
                        proposal,
                        format!("replay gate rejected: {reason}"),
                        attempts,
                        events,
                        at_ms,
                    );
                }
                attempts += 1;
                let detail = match try_llm_sharpen(repair, proposal, &reason) {
                    Some(sharpened) => {
                        proposal.body = sharpened;
                        "sharpened draft via Repair slot; re-running gates"
                    }
                    None => "re-running replay A/B pair (flakiness retry)",
                };
                events.push(NightlyEvent::FixAttempt {
                    id: proposal.id.clone(),
                    attempt: attempts,
                    phase: "replay".into(),
                    detail: detail.into(),
                    at_ms,
                });
            }
        }
    }
}

/// A "fair measurement" failure is the one the fix loop can address:
/// the replay ran cleanly but the proposal didn't strictly improve.
/// Every other replay failure is infrastructure.
fn fair_measurement(reason: &str) -> bool {
    reason.contains("no strict improvement")
}

/// Ask the repair aux model to revise `proposal` per `prompt`; the
/// returned full draft becomes the new body. `None` when no repair
/// model is configured or the call fails — the caller falls back to a
/// plain retry.
fn sharpen_draft(
    judge: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    proposal: &Proposal,
    prompt: String,
) -> Option<String> {
    let (llm, model) = judge?;
    let draft = Proposal {
        body: prompt,
        ..proposal.clone()
    };
    match llm.refine_proposal(model, &draft) {
        Ok(body) if !body.trim().is_empty() => Some(body),
        _ => None,
    }
}

/// With LLM steps enabled, ask the repair aux model to revise a draft
/// that failed eval-gating, feeding it the eval failure detail. `None`
/// when no repair model is configured or the call fails — the caller
/// falls back to a plain gate re-run (flakiness retry).
fn try_eval_sharpen(
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    proposal: &Proposal,
    reason: &str,
) -> Option<String> {
    sharpen_draft(
        repair,
        proposal,
        format!(
            "The following skill/persona draft failed eval validation: {reason}.\n\nRevise it so every relevant eval passes with the draft applied. Keep it concrete and minimal.\n\nCurrent draft:\n{}\n\nReply with the full revised draft and nothing else.",
            proposal.body
        ),
    )
}

/// With LLM steps enabled, ask the repair aux model to sharpen a draft
/// that failed to improve replay. `None` when no repair model is
/// configured or the call fails — the caller falls back to a plain
/// retry.
fn try_llm_sharpen(
    repair: Option<(&dyn NightlyLlm, &AuxiliaryModel)>,
    proposal: &Proposal,
    reason: &str,
) -> Option<String> {
    sharpen_draft(
        repair,
        proposal,
        format!(
            "The following skill/persona draft failed replay validation: {reason}.\n\nRevise it so a held-out task replay strictly improves with the draft applied. Keep it concrete and minimal.\n\nCurrent draft:\n{}\n\nReply with the full revised draft and nothing else.",
            proposal.body
        ),
    )
}

fn escalate(
    data_dir: &Path,
    proposal: &mut Proposal,
    reason: String,
    attempts: usize,
    events: &mut Vec<NightlyEvent>,
    at_ms: i64,
) -> FixOutcome {
    proposal.status = ProposalStatus::NeedsAttention;
    events.push(NightlyEvent::Escalated {
        id: proposal.id.clone(),
        reason: reason.clone(),
        attempts,
        at_ms,
    });
    let esc = Escalation {
        id: proposal.id.clone(),
        kind: proposal.kind_name().to_string(),
        title: proposal.title.clone(),
        reason: reason.clone(),
        attempts,
        at_ms: now_ms(),
    };
    // A failed escalation write must not lose the verdict: the audit
    // event already records it; the JSON file is the readable surface.
    let _ = record_escalation(data_dir, &esc);
    FixOutcome::Escalated { reason }
}

// Small deterministic invariant tests only; behavioral coverage lives
// in eval/tests/nightly.rs (via the public `validate_with_fix_loop`
// API).
