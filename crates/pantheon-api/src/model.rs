//! Model policy: NO routing. Default + ordered fallbacks (failure-only,
//! runtime-controlled) + auxiliary models for scoped capabilities.
//!
//! Mirrors Hermes behavior: one configured default, a fallback chain, and
//! auxiliary models for helpers (embeddings, rerank, vision, extraction...).
//! The agent never selects. The runtime does.
//!
//! NOTE: this list is for things that ARE models - an endpoint you call
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
///
/// Aids that have a config section (`[judge]`, `[embeddings]`, ...)
/// resolve to the pinned target; the rest default to `auto` = the run's
/// default model. Exceptions where `auto` would be wrong are documented
/// on the variant (embeddings: absent = local embedder, never chat).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuxiliaryKind {
    Embeddings,
    /// Vision model (config `[vision]`): describes and answers about
    /// images attached to a turn. Host-orchestrated; never chat. Absent =
    /// `auto`: the run's default model. No call sites yet - there is no
    /// image-input pipeline; the slot exists so a model can be pinned
    /// ahead of it landing.
    Vision,
    /// Video-analysis model (config `[video]`): understands video attached
    /// to a turn - scene description, temporal question answering,
    /// keyframe summarization. Host-orchestrated; never chat. Absent =
    /// `auto`: the run's default model.
    ///
    /// Call-site proposal (the actual video-understanding pipeline is
    /// future work - this slot pins the model ahead of it landing):
    /// - the turn loop gains a `video` attachment kind next to the
    ///   (planned) image attachment kind; when a turn carries video, the
    ///   host samples keyframes (or hands the container/URL to a
    ///   provider-native video input) and calls this slot, never chat;
    /// - the `[tools]` `video_analysis` toggle gates the slot: off =
    ///   the entry never resolves, so a configured `[video]` cannot be
    ///   consulted by accident;
    /// - long video goes through the same summarization shape as
    ///   `[search_synthesis]` (chunk → note → context), so the window
    ///   never fills with raw frames.
    ///
    /// No call sites yet - the slot exists so a model can be pinned
    /// ahead of the video pipeline landing.
    Video,
    /// Search-synthesis model (config `[search_synthesis]`): turns raw
    /// web-search results into a short cited brief before they enter
    /// context. Host-orchestrated; never chat. Absent = `auto`: the run's
    /// default model. No call sites yet - search results currently enter
    /// context raw; the slot exists so a model can be pinned ahead of the
    /// synthesis workload landing.
    SearchSynthesis,
    /// Judge model (config `[judge]`): classifies, routes, scores. Never
    /// generates chat. Used for route selection, tool gating, verification
    /// thresholds. Replaces the old `DecisionRouter` name. No call sites
    /// yet - the engine supports a judge, but the production
    /// `Session::drive` path builds its `AgentLoop` with `judge: None`,
    /// so a configured `[judge]` is validated by `doctor` and then never
    /// consulted. The slot exists so a model can be pinned ahead of the
    /// judge being installed into the runtime loop.
    Judge,
    /// MCP tool-result synthesis model (config `[mcp_synthesis]`): bounds
    /// large MCP tool results into a note before they enter context.
    McpSynthesis,
    /// Model for scheduled (background) runs (config `[scheduled]`): when a
    /// scheduled job executes an agent turn it uses this model instead of
    /// the interactive default - cheap background runs. `auto` = default.
    Scheduled,
    /// Context-compression model: summarizes the oldest exchanges when the
    /// transcript overflows the window. Host-orchestrated; never chat.
    Compression,
    /// Session-title model: names a conversation from its first user prompt.
    /// Host-orchestrated, fire-and-forget beside the first turn; never chat.
    /// Absent entry = `auto`: the runtime falls back to the default model.
    TitleGen,
    /// Reflection-pass model (config `[reflect]`): the auxiliary model for
    /// LLM-backed reflection steps (proposal enrichment, and any future
    /// model-generated proposers). Host-orchestrated; never chat. Absent =
    /// `auto`: the run's default model. Every reflection LLM call resolves
    /// through this slot so background self-improvement stays cheap and
    /// never borrows the interactive model's context directly.
    Reflection,
    /// Consolidation-pass model (config `[consolidation]`): the auxiliary
    /// model for LLM-backed consolidation steps (candidate distillation
    /// merging near-duplicate staged candidates into single well-phrased
    /// facts). Host-orchestrated; never chat. Absent = `auto`: the run's
    /// default model. Every consolidation LLM call resolves through this
    /// slot so nightly memory consolidation stays cheap and never borrows
    /// the interactive model's context directly.
    Consolidation,
    /// Repair model (config `[repair]`): revises drafts and diagnoses
    /// broken things in the nightly self-improvement loop (eval-reject
    /// and replay-reject draft revision today; broken MCP servers,
    /// scheduled tasks, and tools next). Host-orchestrated; never chat.
    /// Absent = OFF: no policy entry, and fix-loop draft revision is
    /// unavailable. Every fix-loop revision resolves through this slot
    /// - never the Reflection slot - so repair work stays on a model
    /// pinned for the job.
    Repair,
    /// Structured-extraction model (config `[extraction]`): pulls fields
    /// and records out of prose and tool outputs (dates, amounts, names,
    /// entities) into typed values the runtime can act on.
    /// Host-orchestrated; never chat. Absent = `auto`: the run's default
    /// model. No call sites yet - the slot exists so a model can be pinned
    /// ahead of the extraction workload landing.
    Extraction,
    /// Rerank model (config `[rerank]`): scores and orders search and
    /// memory-retrieval candidates before they enter context, so the best
    /// evidence wins the window instead of the loudest keyword match.
    /// Host-orchestrated; never chat. Absent = `auto`: the run's default
    /// model. No call sites yet - the slot exists so a model can be pinned
    /// ahead of the reranking workload landing.
    Rerank,
    /// Planner model (config `[planner]`): decomposes a goal into a task
    /// plan for a future planner/worker split, where planning and execution
    /// run on different models. Host-orchestrated; never chat. Absent =
    /// `auto`: the run's default model. No call sites yet - the slot
    /// exists so a model can be pinned ahead of the planner workload
    /// landing.
    Planner,
    /// Adversarial verifier model (config `[verify]`): after a delegated
    /// sub-agent completes, it takes the task's goal plus the child's
    /// claimed result, assumes the goal was missed, and tries to falsify
    /// the claim from the evidence. Fail-closed: a falsified claim errors
    /// the delegation, an inconclusive one is marked unverified - neither
    /// counts as done. Host-orchestrated; never chat. Absent = the slot is
    /// OFF entirely (no entry in the policy), so verification only runs
    /// when the operator explicitly pins a cheap model here.
    Verify,
    Other(String),
}

impl AuxiliaryKind {
    /// Built-in per-capability request timeout in seconds, used when the
    /// aux section sets no explicit `timeout`. Matches the historical
    /// client timeouts so an unconfigured slot behaves exactly as before.
    pub fn default_timeout_secs(&self) -> u64 {
        match self {
            AuxiliaryKind::TitleGen => 10,
            AuxiliaryKind::Judge => 10,
            AuxiliaryKind::Embeddings => 15,
            AuxiliaryKind::Compression => 30,
            AuxiliaryKind::Verify => 30,
            AuxiliaryKind::Reflection => 60,
            AuxiliaryKind::Consolidation => 60,
            AuxiliaryKind::Repair => 60,
            _ => 120,
        }
    }
}

/// The kind of decision being routed through a Judge aux model.
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
    Gate {
        verdict: GateVerdict,
        confidence: f32,
        score: f32,
    },
    /// A binary accept/reject with confidence.
    Binary { accepted: bool, confidence: f32 },
    /// A numeric threshold check.
    Threshold { passed: bool, value: f32 },
}

impl DecisionAnswer {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuxiliaryModel {
    pub kind: AuxiliaryKind,
    pub provider: String,
    pub model: String,
    /// Per-aux request timeout in seconds. Resolved from the aux
    /// section's `timeout_secs` (default 120); call sites apply it to
    /// the aux HTTP transport.
    pub timeout_secs: u64,
    /// Compression only: summary target as a percentage (1-100) of the
    /// absorbed transcript chars (`[compression] target_percent`).
    /// `None` = the historic default (≈12%). Ignored by every other
    /// auxiliary kind.
    #[serde(default)]
    pub target_percent: Option<u8>,
}

/// Full model config for a run/policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub default: DefaultModel,
    pub fallbacks: FallbackChain,
    pub auxiliaries: Vec<AuxiliaryModel>,
    /// Reasoning effort for chat turns. `Off` sends no effort param and
    /// every provider behaves exactly as before. Applies to the default
    /// and to fallback attempts alike; aux turns (title, judge,
    /// compression) always run `Off` so helpers stay fast and cheap.
    /// `#[serde(default)]` keeps configs written before this existed
    /// loading as `Off`.
    #[serde(default)]
    pub reasoning: ReasoningLevel,
    /// Exact thinking budget in tokens, overriding the level mapping on
    /// budget wires (Anthropic). Ignored on effort-string wires (OpenAI).
    /// `None` (the default) means the level mapping decides; `Some(0)`
    /// disables thinking entirely, even with a level set.
    #[serde(default)]
    pub reasoning_budget: Option<u32>,
}

/// Reasoning effort for one turn. Levels are portable names; each wire
/// mode maps them to its own param at request time (OpenAI:
/// `reasoning_effort`, including GPT-5's `minimal` and Codex's `xhigh`;
/// Anthropic: a thinking budget). For exact control, pair a level with
/// `ModelPolicy::reasoning_budget`, which overrides the mapped budget on
/// budget wires and is ignored elsewhere. Endpoints that do not
/// understand the param ignore or reject it - setting effort is an
/// explicit request, and a rejection surfaces as a structured provider
/// error naming the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningLevel {
    /// No effort param sent. The provider default. Always safe.
    #[default]
    Off,
    /// GPT-5's lightest setting. Budget wires floor it at their minimum.
    Minimal,
    Low,
    Medium,
    High,
    /// Codex's top setting, sent verbatim on effort-string wires.
    Xhigh,
    /// As much as the window allows (budget wires: max_tokens − 1).
    /// Sent verbatim as `"max"` on effort-string wires; endpoints that
    /// do not know it reject with a structured error.
    Max,
}

impl ReasoningLevel {
    /// Parse a config/CLI spelling. Accepts case-insensitive `off`
    /// (`none`), `minimal` (`min`), `low`, `medium` (`med`), `high`,
    /// `xhigh` (`xhi`), `max`; anything else is `None`. Named `parse`,
    /// not `from_str`,
    /// so it can never be confused with the `FromStr` trait method.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" => Some(Self::Off),
            "minimal" | "min" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "xhi" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
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

/// Host-side interface for the judge model. Provider-agnostic:
/// implementations may talk to any typed classifier (hosted API, local
/// endpoint, small specialized model) - the host picks one via the
/// `Judge` auxiliary and validates every answer against live state
/// before acting.
pub trait Judge: Send + Sync {
    /// Returns the model identifier for logging/audit trail.
    fn model_name(&self) -> &str {
        "aux-judge-model"
    }

    /// Ask the judge a typed question. Returns a typed answer,
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
/// like `Judge`: the host renders the transcript, bounds the
/// summary, and falls back to deterministic dropping when this errors.
pub trait ContextCompressor: Send + Sync {
    /// Model identifier for logging/audit.
    fn model_name(&self) -> &str {
        "aux-compression-model"
    }

    /// Summarize the transcript. On `Err` the host proceeds with the
    /// deterministic fit - compression is an optimization, never a
    /// requirement for correctness.
    fn compress(&self, req: &CompressionRequest) -> Result<CompressionResult, PantheonError>;
}

/// Soft cap for a session title. Models overshoot; the host truncates at a
/// char boundary so list rows stay readable in the CLI, TUI, and gateways.
pub const TITLE_MAX_CHARS: usize = 60;

/// What the host asks a title model to do: name the conversation whose
/// first user prompt is `prompt`. One shot, no transcript - a title is
/// derived from the opening message, not the whole exchange.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleRequest {
    pub run_id: String,
    /// The first user prompt of the session (host-capped before sending).
    pub prompt: String,
}

/// A title model's output: the title text, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TitleResult {
    pub title: String,
}

/// Host-side interface for the session-title model. Provider-agnostic like
/// `ContextCompressor`: the host picks the target (config `[title_gen]`,
/// else `auto` = the run's default model), bounds the output, and falls
/// back to [`fallback_title`] when this errors. Titles are cosmetic - a
/// failed call never affects the conversation.
pub trait TitleGenerator: Send + Sync {
    /// Model identifier for logging/audit.
    fn model_name(&self) -> &str {
        "aux-title-model"
    }

    /// Name the session. On `Err` the host derives a deterministic title
    /// from the prompt instead.
    fn title(&self, req: &TitleRequest) -> Result<TitleResult, PantheonError>;
}

/// Normalize any raw text (a model's reply, or the first prompt itself in
/// the fallback path) into a single-line title: first non-empty line,
/// wrapping quotes stripped, a leading `Title:` label dropped, whitespace
/// collapsed, hard-truncated at `max_chars` on a char boundary.
/// Returns `""` when nothing usable remains.
pub fn bound_title(raw: &str, max_chars: usize) -> String {
    let mut s = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    // Strip one layer of wrapping quotes (ASCII or curly).
    for (a, b) in [('"', '"'), ('"', '"'), ('\'', '\'')] {
        if s.chars().count() >= 2 && s.starts_with(a) && s.ends_with(b) {
            let inner = s.strip_prefix(a).and_then(|t| t.strip_suffix(b));
            if let Some(inner) = inner {
                s = inner.trim().to_string();
            }
            break;
        }
    }
    // Models love to answer "Title: ..." despite instructions.
    for label in ["title:", "session title:", "name:"] {
        if let Some(head) = s.get(..label.len()) {
            if head.eq_ignore_ascii_case(label) {
                s = s[label.len()..].trim_start().to_string();
                break;
            }
        }
    }
    // Collapse remaining whitespace runs (including any inner newlines).
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
            }
            prev_ws = true;
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    let out = out.trim();
    if out.chars().count() <= max_chars {
        return out.to_string();
    }
    // Truncate on a char boundary at `max_chars` characters (not bytes
    // a byte cap would butcher multibyte scripts).
    let end = out
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(out.len());
    out[..end].trim_end().to_string()
}

/// Deterministic title for a session: the first prompt, bounded. This is
/// the fallback when the title aux model is absent, errors, or times out
/// history always shows something meaningful, and a later successful call
/// can replace it (last title wins).
pub fn fallback_title(prompt: &str) -> String {
    bound_title(prompt, TITLE_MAX_CHARS)
}
