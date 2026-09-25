//! Model policy: NO routing. Default + ordered fallbacks (failure-only,
//! runtime-controlled) + auxiliary models for scoped capabilities.
//!
//! Mirrors Hermes behavior: one configured default, a fallback chain, and
//! auxiliary models for helpers (embeddings, rerank, vision, extraction...).
//! The agent never selects. The runtime does.
//!
//! NOTE: this list is for things that ARE models — an endpoint you call
//! with a prompt or tensors and get inference back. Service capabilities
//! (STT, TTS, search, browser) belong to the provider plane: they are
//! swappable services or local binaries selected by capability, not
//! entries in the model policy (ARCHITECTURE §14).

use crate::error::PantheonError;
use serde::{Deserialize, Serialize};

/// Which model a run uses. Single configured default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultModel {
    pub provider: String,
    pub model: String,
}

/// Ordered fallback chain. Used ONLY on retryable default-model failure
/// (unavailable, quota, 5xx, timeout). Never agent-chosen, never scored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FallbackChain {
    pub fallbacks: Vec<DefaultModel>,
}

impl FallbackChain {
    pub fn next_after(&self, failed_index: Option<usize>) -> Option<(usize, &DefaultModel)> {
        let i = match failed_index {
            Some(i) => i + 1,
            None => 0,
        };
        self.fallbacks.get(i).map(|m| (i, m))
    }
}

/// Auxiliary model for a scoped capability (NOT a chat substitute).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuxiliaryKind {
    Embeddings,
    Rerank,
    Vision,
    Extraction,
    SearchSynthesis,
    /// Decision-layer model: classifies, routes, scores. Never generates chat.
    /// Used for route selection, tool gating, verification thresholds.
    DecisionRouter,
    /// Context-compression model: summarizes the oldest exchanges when the
    /// transcript overflows the window. Host-orchestrated; never chat.
    Compression,
    /// Typed output from a decision model. Answer is a single token or enum label.
    Decision,
    Other(String),
}

/// The kind of decision being routed through a DecisionRouter aux model.
/// Each maps to a narrow insertion point in the host code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionPoint {
    /// Before a chat turn: which provider/model to use for this request.
    RouteSelect,
    /// Before a tool executes: score risk + decide allow/deny/approve.
    ToolGate,
    /// After a stage completes: verify output quality.
    TaskVerify,
    /// Before spawning a sub-agent: pick the right specialist.
    DelegateSelect,
    /// Custom decision point.
    Other(String),
}

/// A decision model's typed output. Never free text.
///
/// ADVISORY-ONLY CONTRACT (locked): the decision model proposes, the host
/// validates against live state and enforces. Specifically:
/// - Route answers may only select from the caller-supplied `choices`
///   (the configured default + ordered fallbacks). Anything else is
///   recorded as `Overridden` and ignored.
/// - Gate answers may only ESCALATE: Allow -> Approval/Deny is honored;
///   Deny -> Allow is never honored. The deterministic `Policy` is the
///   floor; the classifier can raise the bar, never lower it.
/// - Confidence is a routing/escalation signal, never a permission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DecisionAnswer {
    /// A model/provider identifier for route selection.
    /// Must match one entry of the caller's `choices`; else overridden.
    Route { choice: String, confidence: f32 },
    /// A risk score and gate verdict for tool gating.
    Gate { verdict: GateVerdict, confidence: f32, score: f32 },
    /// A binary accept/reject with confidence.
    Binary { accepted: bool, confidence: f32 },
    /// A numeric threshold check.
    Threshold { passed: bool, value: f32 },
}

impl DecisionAnswer {
    /// Confidence signal 0.0-1.0. Never a permission.
    pub fn confidence(&self) -> f32 {
        match self {
            DecisionAnswer::Route { confidence, .. } => *confidence,
            DecisionAnswer::Gate { confidence, .. } => *confidence,
            DecisionAnswer::Binary { confidence, .. } => *confidence,
            DecisionAnswer::Threshold { value, .. } => *value,
        }
    }

    /// Validate a route choice against the allowed set (default + fallbacks
    /// supplied as `choices` by the caller). Returns the choice if allowed.
    pub fn validated_route(&self, allowed: &[String]) -> Option<String> {
        match self {
            DecisionAnswer::Route { choice, .. } => {
                if allowed.iter().any(|a| a == choice) {
                    Some(choice.clone())
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// What the gate decided for a tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateVerdict {
    Allow,
    Deny { reason: String },
    NeedsApproval { reason: String },
}

impl GateVerdict {
    /// Escalation order: Allow < NeedsApproval < Deny.
    /// The classifier may only move UP this ladder relative to the
    /// deterministic host policy. Moving down is dropped by the host.
    pub fn escalation_level(&self) -> u8 {
        match self {
            GateVerdict::Allow => 0,
            GateVerdict::NeedsApproval { .. } => 1,
            GateVerdict::Deny { .. } => 2,
        }
    }

    /// Returns true if `self` (classifier proposal) is at or above the
    /// host policy floor. Host `Allow` + classifier `Deny` => honor.
    /// Host `Deny` + classifier `Allow` => reject (return false).
    pub fn escalates_or_matches(&self, host_floor: &GateVerdict) -> bool {
        self.escalation_level() >= host_floor.escalation_level()
    }
}

/// The recorded outcome of a decision, for the ledger.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionReceipt {
    pub run_id: String,
    pub point: DecisionPoint,
    pub model: String,
    pub answer: DecisionAnswer,
    /// Conservative: the host's final action, validated against live state.
    pub action_taken: DecisionAction,
    pub timestamp_ms: u128,
}

/// What the host actually did with the decision, after validating against live state.
/// The decision model proposes; the host validates and enforces.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionAction {
    /// Decision accepted, host executed the model's choice.
    Accepted,
    /// Decision rejected: host used a hardcoded fallback instead.
    Overridden { fallback_used: String },
    /// Decision denied: host blocked the action entirely.
    Denied { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuxiliaryModel {
    pub kind: AuxiliaryKind,
    pub provider: String,
    pub model: String,
}

/// Full model config for a run/policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub default: DefaultModel,
    pub fallbacks: FallbackChain,
    pub auxiliaries: Vec<AuxiliaryModel>,
}

impl ModelPolicy {
    pub fn auxiliary(&self, kind: &AuxiliaryKind) -> Option<&AuxiliaryModel> {
        self.auxiliaries.iter().find(|a| &a.kind == kind)
    }
}

/// What the host tells the decision model: enough context to make the call.
/// The model returns a Choice/Score/Noul typed answer, not free text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub run_id: String,
    pub point: DecisionPoint,
    /// The specific question for this decision point.
    pub query: String,
    /// Options the model can choose from, when the point is a choice problem.
    /// For scoring/gate decisions, this carries the items being scored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
    /// Optional context: the transcript prefix, tool args, etc.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Host-side interface for a decision-layer model. Provider-agnostic:
/// implementations may talk to any typed classifier (hosted API, local
/// endpoint, small specialized model) — the host picks one via the
/// `DecisionRouter` auxiliary and validates every answer against live
/// state before acting.
pub trait DecisionRouter: Send + Sync {
    /// Returns the model identifier for logging/audit trail.
    fn model_name(&self) -> &str {
        "aux-decision-model"
    }

    /// Ask the decision model a typed question. Returns a typed answer,
    /// never free text.
    fn decide(&self, req: &DecisionRequest) -> Result<DecisionAnswer, PantheonError>;
}

/// What the host asks a compression model to do: summarize `transcript`
/// (the oldest exchanges, pre-rendered and row-capped by the host) into a
/// handoff note of roughly `target_chars`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionRequest {
    pub run_id: String,
    /// Pre-rendered oldest exchanges, role-tagged, per-row capped.
    pub transcript: String,
    /// Soft cap for the summary length in chars. The host hard-bounds the
    /// result regardless of what the model returns.
    pub target_chars: usize,
}

/// A compression model's output: the summary text, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionResult {
    pub summary: String,
}

/// Host-side interface for a context-compression model. Provider-agnostic
/// like `DecisionRouter`: the host renders the transcript, bounds the
/// summary, and falls back to deterministic dropping when this errors.
pub trait ContextCompressor: Send + Sync {
    /// Model identifier for logging/audit.
    fn model_name(&self) -> &str {
        "aux-compression-model"
    }

    /// Summarize the transcript. On `Err` the host proceeds with the
    /// deterministic fit — compression is an optimization, never a
    /// requirement for correctness.
    fn compress(&self, req: &CompressionRequest) -> Result<CompressionResult, PantheonError>;
}
