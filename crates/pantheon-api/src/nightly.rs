//! Nightly self-improvement protocol types: the proposal DTOs and the
//! single LLM-gating contract.
//!
//! These live at the API (protocol) layer, not in `pantheon-nightly`:
//! `pantheon-providers` implements [`NightlyLlm`] but must not depend on
//! the nightly pipeline crate (that would invert the layering — a provider
//! leaf depending on a pipeline crate). The nightly crate re-exports these
//! types, and owns the pipeline that produces and consumes them.
use crate::model::{AuxiliaryKind, AuxiliaryModel, ModelPolicy};

/// (run, turn) reference: the provenance unit. `ts_ms` is the event
/// time, used for the recency half of the memory promotion rule.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnRef {
    pub run_id: String,
    pub turn_id: String,
    pub ts_ms: i64,
}

/// What the nightly pass wants to change.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProposalKind {
    /// A durable lesson for the memory store. Auto-applies at the `Memory`
    /// trust tier: informative, never authoritative, and it can never
    /// clobber a user-confirmed record.
    MemoryLesson { key: String },
    /// Create or update a SKILL.md under `<data_dir>/skills`. Eval-gated,
    /// replay-gated, then approval-gated.
    Skill { name: String, update: bool },
    /// A tweak to the agent's persona notes (stored as agent-layer memory
    /// under the `persona` namespace, so it shapes future sessions without
    /// editing user config files). Eval-gated, replay-gated, then
    /// approval-gated.
    Persona { topic: String },
}

/// Lifecycle state of a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProposalStatus {
    Proposed,
    EvalPassed,
    EvalFailed,
    ReplayPassed,
    ReplayFailed,
    Approved,
    Denied,
    Applied,
    /// Validation failed and the bounded fix loop gave up: a human
    /// should look at it. Never queued for approval, never applied.
    NeedsAttention,
}

/// One proposed self-improvement, with full provenance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Proposal {
    /// Deterministic id: `nly_<kind>_<fnv1a(title+body)[..8]>`. The same
    /// signal re-observed in a later pass yields the same id, which makes
    /// "already proposed / already applied" dedupe trivial.
    pub id: String,
    pub kind: ProposalKind,
    pub title: String,
    pub body: String,
    /// Runs this was learned from.
    pub provenance_runs: Vec<String>,
    /// (run, turn) pairs this was learned from.
    pub provenance_turns: Vec<TurnRef>,
    /// pantheon-eval test targets that must pass before this proposal can
    /// be approved. Empty for memory lessons (no evals; auto-applied).
    pub eval_tags: Vec<String>,
    pub status: ProposalStatus,
}

impl Proposal {
    pub fn kind_name(&self) -> &'static str {
        match &self.kind {
            ProposalKind::MemoryLesson { .. } => "lesson",
            ProposalKind::Skill { .. } => "skill",
            ProposalKind::Persona { .. } => "persona",
        }
    }
}

/// The one LLM seam for the nightly pass. Implementations must place
/// calls on the provided [`AuxiliaryModel`] — never on the chat model
/// directly. Every call is routed through an explicitly resolved
/// [`AuxiliaryModel`]: the proposal refiner uses the `Reflection` slot,
/// the memory distiller uses the `Consolidation` slot, and repair
/// diagnosis uses the `Repair` slot. Neither ever touches the chat model.
pub trait NightlyLlm: Send + Sync {
    /// Polish or extend a draft proposal body. Must not change the
    /// proposal's meaning, kind, or provenance — enrichment only.
    /// Routed through the `Reflection` auxiliary slot.
    fn refine_proposal(&self, model: &AuxiliaryModel, draft: &Proposal) -> Result<String, String>;

    /// Merge staged memory texts into durable facts. Must not invent new
    /// claims — merge and phrase only. Routed through the `Consolidation`
    /// slot.
    fn distill_memories(
        &self,
        model: &AuxiliaryModel,
        texts: &[String],
    ) -> Result<Vec<String>, String>;

    /// Diagnose a broken operational target (MCP server, scheduled job,
    /// tool) for the nightly repair loop. Advisory only: the caller
    /// records the returned text in the audit log and any escalation;
    /// repair *actions* stay deterministic and bounded. Must not suggest
    /// anything destructive. Routed through the `Repair` auxiliary slot —
    /// never the `Reflection` slot (repair diagnosis is a distinct
    /// workload from proposal refinement) and never the chat model.
    ///
    /// Default: unimplemented. Callers degrade gracefully to
    /// deterministic-only repair when the backend does not implement it
    /// or the `Repair` slot is unconfigured — the nightly pass never
    /// fails for a missing repair model.
    fn diagnose_repair(&self, model: &AuxiliaryModel, prompt: &str) -> Result<String, String> {
        let _ = (model, prompt);
        Err("repair diagnosis not implemented by this backend".to_string())
    }
}

/// Resolve the proposal refiner: `Some` only when the policy carries a
/// `Reflection` entry. The caller additionally gates on `config.enabled`.
pub fn resolve_refiner<'a>(
    llm: &'a dyn NightlyLlm,
    policy: &'a ModelPolicy,
) -> Option<(&'a dyn NightlyLlm, &'a AuxiliaryModel)> {
    policy
        .auxiliary(&AuxiliaryKind::Reflection)
        .map(|aux| (llm as &'a dyn NightlyLlm, aux))
}

/// Resolve the fix-loop repair model: `Some` only when the policy
/// carries a `Repair` entry. All fix-loop draft revision (eval-reject
/// and replay-reject paths) resolves through this slot — never the
/// Reflection slot. The caller additionally gates on `config.enabled`.
/// `None` means revision is unavailable: the loop falls back to plain
/// retries, then escalation. A pass never fails for lack of a repair
/// model.
pub fn resolve_repair<'a>(
    llm: &'a dyn NightlyLlm,
    policy: &'a ModelPolicy,
) -> Option<(&'a dyn NightlyLlm, &'a AuxiliaryModel)> {
    policy
        .auxiliary(&AuxiliaryKind::Repair)
        .map(|aux| (llm as &'a dyn NightlyLlm, aux))
}

// Small deterministic invariant tests only.
