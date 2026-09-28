//! Reflection — Pantheon's answer to the self-improvement loop.
//!
//! The concept, deliberately not a copy: instead of forking the agent to
//! re-read raw transcripts and edit markdown files, Reflection reads
//! **structured signals from the SQLite event ledger** (completed turns,
//! tool sequences, steering corrections, approval denials), turns them into
//! **proposals with provenance**, and routes each proposal through the
//! runtime's own governance:
//!
//! - `MemoryLesson` — low risk: auto-applies into the memory store at the
//!   `Memory` trust tier (it can never clobber a user-confirmed record).
//! - `Skill` / `Persona` — must first pass relevant `pantheon-eval` targets
//!   (eval-gated), then win explicit user approval (approval-gated).
//!   A self-change the user didn't approve never lands.
//!
//! Every lifecycle step is appended to a JSONL audit log
//! (`<data_dir>/reflect.jsonl`): what was proposed, what evals it passed,
//! who approved it, and which sessions it learned from.
//!
//! LLM-backed refinement is OFF by default (`ReflectConfig::enabled`):
//! it spends the user's tokens, so it must be opted into explicitly.

pub mod apply;
pub mod audit;
pub mod eval_gate;
pub mod propose;
pub mod signals;

pub use apply::{apply_proposal, ApplyOutcome};
pub use audit::{last_run_summary, AuditEvent, AuditRecord, ReflectAudit};
pub use eval_gate::{EvalOutcome, EvalRunner, EvalVerdict, SubprocessEvalRunner};
pub use propose::{Proposal, ProposalKind, ProposalStatus};
pub use signals::{Signal, TurnRef};

use pantheon_api::error::PantheonError;
use std::path::Path;
use std::time::Duration;

/// How far back a reflection pass looks for signals. Bounded so a pass
/// over years of ledger history can't spiral: 30 days.
pub const DEFAULT_LOOKBACK_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Knobs for a reflection pass. `enabled = false` is the default and the
/// safe one: deterministic signal extraction always runs, but anything
/// that would spend model tokens (the [`LlmRefiner`] hook, future
/// LLM-backed proposers) is refused unless this is explicitly flipped.
#[derive(Debug, Clone)]
pub struct ReflectConfig {
    /// Allow LLM-backed reflection steps. Default false.
    pub enabled: bool,
    /// Automatic reflection fires after this many completed turns in a
    /// session. Default 20. `0` disables the automatic trigger (manual
    /// `/reflect`, `pantheon reflect`, and scheduled passes still work).
    pub auto_turns: u32,
    /// Hard cap on proposals emitted per pass. Default 5.
    pub max_proposals: usize,
    /// Per-eval timeout for eval-gating. Default 120s.
    pub eval_timeout: Duration,
    /// Max eval targets run per proposal. Default 3.
    pub max_evals: usize,
}

impl Default for ReflectConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_turns: 20,
            max_proposals: 5,
            eval_timeout: Duration::from_secs(120),
            max_evals: 3,
        }
    }
}

/// Optional LLM hook to enrich proposal bodies. It is **never consulted**
/// unless the pass config has `enabled = true`: the deterministic
/// templates are the whole v1 pipeline, and this seam exists so a future
/// model-backed proposer can't silently start spending tokens.
///
/// Routing contract: implementations MUST place the call on the provided
/// [`AuxiliaryModel`] (the resolved `AuxiliaryKind::Reflection` slot) —
/// never on the chat/default model directly. That keeps reflection cheap
/// and keeps background self-improvement out of the interactive model's
/// context.
pub trait LlmRefiner {
    /// Polish or extend the draft body using `model`. Must not change the
    /// proposal's meaning, kind, or provenance — enrichment only.
    fn refine(
        &self,
        model: &pantheon_api::model::AuxiliaryModel,
        draft: &Proposal,
    ) -> Result<String, String>;
}

/// A proposal that cleared eval-gating and is waiting for a human
/// decision. Persisted to `<data_dir>/reflect-pending.json` so approval
/// can arrive later (`pantheon reflect --approve <id>`, TUI y/n).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingProposal {
    pub proposal: Proposal,
    pub eval_summary: String,
    /// Pass that produced the proposal; later approve/deny audit records
    /// carry this so `log`/`status` group the whole lifecycle together.
    #[serde(default)]
    pub pass_id: String,
}

/// Input to one reflection pass.
pub struct PassInput<'a> {
    pub data_dir: &'a Path,
    pub config: ReflectConfig,
    /// Override the lookback window; `None` = [`DEFAULT_LOOKBACK_MS`].
    pub lookback_ms: Option<i64>,
    /// If true, proposals are generated but nothing is applied and no
    /// audit records are written. For `pantheon reflect --dry-run`.
    pub dry_run: bool,
    /// Resolved model policy. Any LLM step looks up
    /// `AuxiliaryKind::Reflection` here and calls THAT model — never the
    /// chat model. `None` = LLM steps are refused outright (a
    /// deterministic-only pass); the deterministic pipeline is unaffected.
    pub model_policy: Option<&'a pantheon_api::model::ModelPolicy>,
    /// Optional LLM refiner. Consulted only when `config.enabled` AND a
    /// model policy is present to resolve the Reflection slot.
    pub llm: Option<&'a dyn LlmRefiner>,
}

/// Output of one reflection pass.
pub struct PassOutput {
    /// Pass id (`rfl_<ms>`); also tags every audit record from this pass.
    pub pass_id: String,
    /// All proposals generated this pass (in priority order).
    pub proposals: Vec<Proposal>,
    /// Memory lessons auto-applied this pass.
    pub applied: Vec<ApplyOutcome>,
    /// Skill/persona proposals awaiting approval.
    pub pending: Vec<PendingProposal>,
    /// Proposals rejected by eval-gating (with reasons).
    pub rejected: Vec<(Proposal, String)>,
}

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        pantheon_api::error::Layer::Runtime,
        false,
        cause,
        "check the data dir and ledger health (`pantheon doctor`)",
        String::new(),
    )
}

/// Run one bounded reflection pass over the ledger.
///
/// Pipeline: signals → proposals → (memory lessons auto-apply) →
/// (skill/persona: eval-gate → pending approval). Every step is audited.
/// Deterministic: the same ledger state yields the same proposals.
pub fn run_pass(
    input: PassInput<'_>,
    eval_runner: &dyn EvalRunner,
) -> Result<PassOutput, PantheonError> {
    let data_dir = input.data_dir;
    let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db"))
        .map_err(|e| err("RFL_LEDGER", format!("open ledger for reflection: {e}")))?;
    let pass_id = format!("rfl_{}", now_ms());
    let audit = ReflectAudit::open_for_pass(&data_dir.join("reflect.jsonl"), pass_id.clone())
        .map_err(|e| err("RFL_AUDIT", format!("open reflection audit log: {e}")))?;

    let lookback = input.lookback_ms.unwrap_or(DEFAULT_LOOKBACK_MS);
    let since_ms = now_ms().saturating_sub(lookback);
    let signals = signals::collect(&ledger, since_ms)?;
    let mut proposals = propose::from_signals(&signals);

    // Optional LLM enrichment: gated behind explicit opt-in AND routed
    // through the Reflection auxiliary slot. When disabled, or when no
    // model policy is available to resolve the slot, the refiner is not
    // even looked at — a misconfigured caller can't spend tokens by
    // accident, and reflection can never borrow the chat model directly.
    if input.config.enabled {
        if let (Some(llm), Some(policy)) = (input.llm, input.model_policy) {
            if let Some(aux) = policy.auxiliary(&pantheon_api::model::AuxiliaryKind::Reflection) {
                for p in &mut proposals {
                    if let Ok(better) = llm.refine(aux, p) {
                        if !better.trim().is_empty() {
                            p.body = better;
                        }
                    }
                }
            }
        }
    }

    proposals.truncate(input.config.max_proposals);

    let mut out = PassOutput {
        pass_id: pass_id.clone(),
        proposals: proposals.clone(),
        applied: Vec::new(),
        pending: Vec::new(),
        rejected: Vec::new(),
    };

    for p in proposals {
        if input.dry_run {
            // Dry-run is read-only: proposals are generated and returned,
            // but no audit records are written and nothing is applied or
            // queued for approval.
            continue;
        }
        audit.record(&p, AuditEvent::Proposed, None)?;
        match &p.kind {
            ProposalKind::MemoryLesson { .. } => {
                let outcome = apply::apply_proposal(data_dir, &p)?;
                audit.record(
                    &p,
                    AuditEvent::Applied {
                        detail: outcome.describe(),
                    },
                    None,
                )?;
                out.applied.push(outcome);
            }
            ProposalKind::Skill { .. } | ProposalKind::Persona { .. } => {
                let tags: Vec<&str> = p.eval_tags.iter().map(|s| s.as_str()).collect();
                let verdict = eval_gate::gate(&p, &tags, eval_runner, &input.config);
                match verdict {
                    EvalVerdict::Pass(summary) => {
                        audit.record(
                            &p,
                            AuditEvent::EvalPassed {
                                summary: summary.clone(),
                            },
                            None,
                        )?;
                        out.pending.push(PendingProposal {
                            proposal: p,
                            eval_summary: summary,
                            pass_id: pass_id.clone(),
                        });
                    }
                    EvalVerdict::Reject(reason) => {
                        audit.record(
                            &p,
                            AuditEvent::EvalFailed {
                                reason: reason.clone(),
                            },
                            None,
                        )?;
                        // Find the proposal again for the rejected list.
                        if let Some(prop) = out.proposals.iter().find(|q| q.id == p.id) {
                            out.rejected.push((prop.clone(), reason));
                        }
                    }
                }
            }
        }
    }

    if !input.dry_run {
        // Pass-level completion marker, written even for zero-proposal
        // passes so `status` can always summarize the latest run.
        audit.record_pass(
            out.proposals.len(),
            out.applied.len(),
            out.pending.len(),
            out.rejected.len(),
        )?;
        apply::save_pending(data_dir, &out.pending)
            .map_err(|e| err("RFL_PENDING", format!("persist pending proposals: {e}")))?;
    }
    Ok(out)
}

/// Approve a pending skill/persona proposal and apply it. The explicit
/// human action (CLI `--approve`, TUI y) is the approval; this records it
/// in the audit log alongside the eval summary.
pub fn approve_pending(
    data_dir: &Path,
    proposal_id: &str,
    approver: &str,
) -> Result<ApplyOutcome, PantheonError> {
    let mut pending = apply::load_pending(data_dir)
        .map_err(|e| err("RFL_PENDING", format!("load pending proposals: {e}")))?;
    let pos = pending
        .iter()
        .position(|p| p.proposal.id == proposal_id)
        .ok_or_else(|| {
            err(
                "RFL_UNKNOWN_PROPOSAL",
                format!("no pending reflection proposal '{proposal_id}'"),
            )
        })?;
    let pp = pending.remove(pos);
    let audit = ReflectAudit::open_for_pass(&data_dir.join("reflect.jsonl"), pp.pass_id.clone())
        .map_err(|e| err("RFL_AUDIT", format!("open reflection audit log: {e}")))?;
    audit.record(
        &pp.proposal,
        AuditEvent::Approved {
            by: approver.to_string(),
        },
        None,
    )?;
    let outcome = apply::apply_proposal(data_dir, &pp.proposal)?;
    audit.record(
        &pp.proposal,
        AuditEvent::Applied {
            detail: outcome.describe(),
        },
        Some(approver),
    )?;
    apply::save_pending(data_dir, &pending)
        .map_err(|e| err("RFL_PENDING", format!("persist pending proposals: {e}")))?;
    Ok(outcome)
}

/// Reject a pending proposal. It never applies; the denial is audited.
pub fn deny_pending(
    data_dir: &Path,
    proposal_id: &str,
    approver: &str,
) -> Result<(), PantheonError> {
    let mut pending = apply::load_pending(data_dir)
        .map_err(|e| err("RFL_PENDING", format!("load pending proposals: {e}")))?;
    let pos = pending
        .iter()
        .position(|p| p.proposal.id == proposal_id)
        .ok_or_else(|| {
            err(
                "RFL_UNKNOWN_PROPOSAL",
                format!("no pending reflection proposal '{proposal_id}'"),
            )
        })?;
    let pp = pending.remove(pos);
    let audit = ReflectAudit::open_for_pass(&data_dir.join("reflect.jsonl"), pp.pass_id.clone())
        .map_err(|e| err("RFL_AUDIT", format!("open reflection audit log: {e}")))?;
    audit.record(
        &pp.proposal,
        AuditEvent::Denied {
            by: approver.to_string(),
        },
        None,
    )?;
    apply::save_pending(data_dir, &pending)
        .map_err(|e| err("RFL_PENDING", format!("persist pending proposals: {e}")))?;
    Ok(())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// Small deterministic invariant tests only. Behavioral tests live in
// `eval/tests/reflect_*.rs` per the /eval-or-remove policy.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_opt_in() {
        let c = ReflectConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.auto_turns, 20);
        assert_eq!(c.max_proposals, 5);
    }

    #[test]
    fn audit_event_serializes() {
        let r = AuditRecord {
            ts_ms: 1,
            pass_id: "p".into(),
            proposal_id: "x".into(),
            kind: "skill".into(),
            title: "t".into(),
            event: AuditEvent::Proposed,
            provenance_runs: vec![],
            eval_summary: None,
            approver: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"event\":\"proposed\""));
    }
}
