//! Agent-engine event enum. Every meaningful transition becomes an event,
//! giving replayability, debugging, and crash recovery.

use crate::message::Message;
use crate::model::DecisionPoint;
use crate::provenance::Provenance;
use serde::{Deserialize, Serialize};

/// Compact summary of a decision answer for ledger events.
/// Full type lives in model.rs; this is the ledger-friendly version.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DecisionAnswerSummary {
    Route {
        choice: String,
        confidence: f32,
    },
    Gate {
        verdict: String,
        score: f32,
        confidence: f32,
    },
    Binary {
        accepted: bool,
        confidence: f32,
    },
    Threshold {
        passed: bool,
        value: f32,
    },
}

/// Compact summary of what the host actually did with a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum DecisionActionSummary {
    Accepted,
    Overridden { fallback_used: String },
    Denied { reason: String },
}

/// Canonical runtime events (§2 agent engine + §18 runtime API).
/// NOTE: no Eq derive — DecisionMade events carry f32 confidence scores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    RunStarted {
        run_id: String,
    },
    RunProgress {
        run_id: String,
        detail: String,
    },
    RunCompleted {
        run_id: String,
    },
    RunFailed {
        run_id: String,
        code: String,
    },
    /// Cancellation is terminal but distinct from failure: a user requested
    /// it, so replay and metrics can tell the two apart.
    RunCanceled {
        run_id: String,
        reason: String,
    },
    RunRecovered {
        run_id: String,
    },
    /// A run is bound to an agent profile identity.
    ///
    /// Separate from `RunStarted` on purpose: a run row can be created
    /// before any profile is resolved (a CLI pre-start, a TUI that opens a
    /// conversation, a resumed pre-profiles run), and the binding is what
    /// makes that run *belong* to an agent. Emitted once, and refused by
    /// the ledger if the run is already bound to a different agent, so a
    /// run can never silently change identity mid-life.
    AgentBound {
        run_id: String,
        agent_id: String,
        profile: String,
    },
    /// A user-initiated turn inside a durable conversation. A run may contain
    /// many turns over its lifetime; each turn owns its model/tool activity.
    TurnStarted {
        run_id: String,
        turn_id: String,
    },
    /// The turn yielded to an external continuation such as human approval.
    TurnParked {
        run_id: String,
        turn_id: String,
        reason: String,
    },
    TurnCompleted {
        run_id: String,
        turn_id: String,
        outcome: String,
    },
    TurnFailed {
        run_id: String,
        turn_id: String,
        code: String,
    },
    /// The operator rewound this turn (TUI double-Esc). History is never
    /// rewritten: the marker is appended and `Ledger::replay` excludes the
    /// named turn and everything after it up to this marker, so resume and
    /// the transcript rebuild behave as if the rewound turns never happened.
    /// Turns started after the marker replay normally.
    TurnRewound {
        run_id: String,
        turn_id: String,
    },
    /// The operator snapshotted the session at this turn (`/checkpoint`).
    /// A durable, named marker: `/checkpoints` lists them and `/restore`
    /// rewinds to one by emitting `TurnRewound` at the turn that follows
    /// the checkpoint's turn. Like rewind markers, this is a read-time
    /// projection aid — raw history is never rewritten.
    CheckpointCreated {
        run_id: String,
        turn_id: String,
        name: String,
    },
    /// The operator steered the running turn (`/steer`): mid-turn guidance
    /// injected without canceling or restarting. The guidance also lands in
    /// the transcript as a marked user message (see `rebuild_messages`), so
    /// resume stays faithful; this row is the durable steering record.
    SteeringProvided {
        run_id: String,
        text: String,
    },
    ModelRequested {
        run_id: String,
        model: String,
    },
    ModelDelta {
        run_id: String,
        delta: String,
    },
    ModelCompleted {
        run_id: String,
    },
    ToolRequested {
        run_id: String,
        tool: String,
    },
    ToolStarted {
        run_id: String,
        /// Stable per-call id (`call_{turn}_{i}`); resume keys grants on this.
        call_id: String,
        tool: String,
        /// Tool arguments as issued (persisted so resume re-executes exactly).
        args: String,
        /// Which entity/action issued this tool call (user, model, memory).
        provenance: Provenance,
    },
    ToolOutput {
        run_id: String,
        call_id: String,
        tool: String,
        truncated: bool,
        /// Provenance of the output (tool-derived, always Untrusted tier).
        provenance: Provenance,
    },
    ToolCompleted {
        run_id: String,
        call_id: String,
        tool: String,
        /// Provenance of the completion boundary.
        provenance: Provenance,
    },
    /// Assistant turn persisted so resume rebuilds messages without a model call.
    AssistantMessage {
        run_id: String,
        message: Message,
    },
    /// Tool result persisted so resume rebuilds messages without re-running tools.
    ToolMessage {
        run_id: String,
        message: Message,
    },
    /// A reasoning trace captured at import time (session migration).
    ///
    /// Foreign agents persist their thinking in transcript files; Pantheon
    /// keeps it durable so an imported session shows its original
    /// deliberation. Read-only: `rebuild_messages` skips it (it is never
    /// sent to the model as a conversation message); the TUI surfaces it
    /// through `rebuild_transcript` as a Thinking block.
    ImportedReasoning {
        run_id: String,
        turn_id: String,
        text: String,
    },
    AgentSpawned {
        run_id: String,
        agent: String,
    },
    AgentMessage {
        run_id: String,
        agent: String,
    },
    AgentCompleted {
        run_id: String,
        agent: String,
    },
    MemoryProposed {
        run_id: String,
    },
    ApprovalRequested {
        run_id: String,
        scope: String,
    },
    ApprovalGranted {
        run_id: String,
        scope: String,
    },
    /// A denial is scoped to one approval request. It must not turn the
    /// entire run into a failed run; another tool call may still proceed.
    ApprovalDenied {
        run_id: String,
        scope: String,
    },
    /// A decision-layer model was consulted at a specific insertion point.
    DecisionRequested {
        run_id: String,
        point: DecisionPoint,
        model: String,
    },
    /// The decision model produced a typed answer (Choice/Score/Noul).
    DecisionMade {
        run_id: String,
        point: DecisionPoint,
        model: String,
        answer: DecisionAnswerSummary,
    },
    /// The host validated the decision against live state and acted.
    DecisionRecorded {
        run_id: String,
        point: DecisionPoint,
        model: String,
        action: DecisionActionSummary,
    },
    /// The assembled context exceeded the model's window; the host trimmed
    /// it before the provider call (oldest tool rows re-compacted, oldest
    /// exchanges dropped). Ephemeral — ledger history is untouched, only
    /// what the model sees this turn. Persisted so `pantheon logs` shows why
    /// earlier turns are absent from the prompt.
    ContextTrimmed {
        run_id: String,
        /// Estimated input tokens after trimming.
        estimated: u32,
        /// Usable input window the fit targeted.
        window: u32,
        /// Rows dropped (oldest whole exchanges, oldest first).
        dropped_rows: u32,
        /// Tool rows re-compacted to the floor.
        compacted_rows: u32,
    },
    /// The compression aux model summarized the oldest exchanges during an
    /// overflow-triggered fit. The summary row is ephemeral prompt state
    /// (memory tier); ledger history is untouched. Emitted even when the
    /// subsequent deterministic fit still has to drop rows.
    ContextCompressed {
        run_id: String,
        /// The compression aux model (identifier for the audit trail).
        model: String,
        /// Exchanges absorbed into the summary.
        exchanges: u32,
        /// Rows removed (replaced by one summary row).
        rows: u32,
        /// Rendered transcript size in chars.
        chars_before: u32,
        /// Summary row size in chars.
        chars_after: u32,
    },
    /// The title-gen auxiliary (or the deterministic fallback) named this
    /// conversation from its first user prompt. The latest `SessionTitled`
    /// event is the run's display title — a later `/name` (manual) or a
    /// successful model call simply overwrites the previous one. Cosmetic:
    /// never affects execution, replay, or the transcript.
    SessionTitled {
        run_id: String,
        /// The bounded single-line title (at most TITLE_MAX_CHARS).
        title: String,
        /// Which model produced it (identifier for the audit trail);
        /// "deterministic" for the prompt-derived fallback.
        model: String,
        /// "model" when the aux call succeeded, "fallback" when the host
        /// derived the title locally from the first prompt.
        source: String,
    },
    /// One model call's token/cost accounting, persisted. `ModelEvent::Usage`
    /// used to be dropped by `to_event`, so historical spend was invisible;
    /// this event makes the event ledger the source of truth for
    /// `pantheon stats`. The model/provider are captured from the most
    /// recent `ModelRequested`/`Attempt` seen by the persisting sink.
    UsageRecorded {
        run_id: String,
        /// Model identifier, e.g. `anthropic:claude-opus-4-6` (provider
        /// prefix when known); "unknown" when the sink saw no request.
        model: String,
        /// Provider name, e.g. `anthropic`; empty when unknown.
        provider: String,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
        /// USD cost for this call, when the catalog priced it.
        cost_usd: Option<f64>,
    },
}
impl Event {
    /// A copy of this event addressed to a different run.
    ///
    /// Used by session fork: the forked run's history is the source run's
    /// event prefix verbatim, re-addressed. Every field except `run_id`
    /// is preserved, so replay, metrics, and resume see the fork as if
    /// the conversation had always lived there.
    pub fn with_run_id(&self, run_id: &str) -> Event {
        match self.clone() {
            Event::RunStarted { run_id: _ } => Event::RunStarted {
                run_id: run_id.to_string(),
            },
            Event::RunProgress { run_id: _, detail } => Event::RunProgress {
                run_id: run_id.to_string(),
                detail,
            },
            Event::RunCompleted { run_id: _ } => Event::RunCompleted {
                run_id: run_id.to_string(),
            },
            Event::RunFailed { run_id: _, code } => Event::RunFailed {
                run_id: run_id.to_string(),
                code,
            },
            Event::RunCanceled { run_id: _, reason } => Event::RunCanceled {
                run_id: run_id.to_string(),
                reason,
            },
            Event::RunRecovered { run_id: _ } => Event::RunRecovered {
                run_id: run_id.to_string(),
            },
            Event::AgentBound {
                run_id: _,
                agent_id,
                profile,
            } => Event::AgentBound {
                run_id: run_id.to_string(),
                agent_id,
                profile,
            },
            Event::TurnStarted { run_id: _, turn_id } => Event::TurnStarted {
                run_id: run_id.to_string(),
                turn_id,
            },
            Event::TurnParked {
                run_id: _,
                turn_id,
                reason,
            } => Event::TurnParked {
                run_id: run_id.to_string(),
                turn_id,
                reason,
            },
            Event::TurnCompleted {
                run_id: _,
                turn_id,
                outcome,
            } => Event::TurnCompleted {
                run_id: run_id.to_string(),
                turn_id,
                outcome,
            },
            Event::TurnFailed {
                run_id: _,
                turn_id,
                code,
            } => Event::TurnFailed {
                run_id: run_id.to_string(),
                turn_id,
                code,
            },
            Event::TurnRewound { run_id: _, turn_id } => Event::TurnRewound {
                run_id: run_id.to_string(),
                turn_id,
            },
            Event::CheckpointCreated {
                run_id: _,
                turn_id,
                name,
            } => Event::CheckpointCreated {
                run_id: run_id.to_string(),
                turn_id,
                name,
            },
            Event::SteeringProvided { run_id: _, text } => Event::SteeringProvided {
                run_id: run_id.to_string(),
                text,
            },
            Event::ModelRequested { run_id: _, model } => Event::ModelRequested {
                run_id: run_id.to_string(),
                model,
            },
            Event::ModelDelta { run_id: _, delta } => Event::ModelDelta {
                run_id: run_id.to_string(),
                delta,
            },
            Event::ModelCompleted { run_id: _ } => Event::ModelCompleted {
                run_id: run_id.to_string(),
            },
            Event::ToolRequested { run_id: _, tool } => Event::ToolRequested {
                run_id: run_id.to_string(),
                tool,
            },
            Event::ToolStarted {
                run_id: _,
                call_id,
                tool,
                args,
                provenance,
            } => Event::ToolStarted {
                run_id: run_id.to_string(),
                call_id,
                tool,
                args,
                provenance,
            },
            Event::ToolOutput {
                run_id: _,
                call_id,
                tool,
                truncated,
                provenance,
            } => Event::ToolOutput {
                run_id: run_id.to_string(),
                call_id,
                tool,
                truncated,
                provenance,
            },
            Event::ToolCompleted {
                run_id: _,
                call_id,
                tool,
                provenance,
            } => Event::ToolCompleted {
                run_id: run_id.to_string(),
                call_id,
                tool,
                provenance,
            },
            Event::AssistantMessage { run_id: _, message } => Event::AssistantMessage {
                run_id: run_id.to_string(),
                message,
            },
            Event::ToolMessage { run_id: _, message } => Event::ToolMessage {
                run_id: run_id.to_string(),
                message,
            },
            Event::ImportedReasoning {
                run_id: _,
                turn_id,
                text,
            } => Event::ImportedReasoning {
                run_id: run_id.to_string(),
                turn_id,
                text,
            },
            Event::AgentSpawned { run_id: _, agent } => Event::AgentSpawned {
                run_id: run_id.to_string(),
                agent,
            },
            Event::AgentMessage { run_id: _, agent } => Event::AgentMessage {
                run_id: run_id.to_string(),
                agent,
            },
            Event::AgentCompleted { run_id: _, agent } => Event::AgentCompleted {
                run_id: run_id.to_string(),
                agent,
            },
            Event::MemoryProposed { run_id: _ } => Event::MemoryProposed {
                run_id: run_id.to_string(),
            },
            Event::ApprovalRequested { run_id: _, scope } => Event::ApprovalRequested {
                run_id: run_id.to_string(),
                scope,
            },
            Event::ApprovalGranted { run_id: _, scope } => Event::ApprovalGranted {
                run_id: run_id.to_string(),
                scope,
            },
            Event::ApprovalDenied { run_id: _, scope } => Event::ApprovalDenied {
                run_id: run_id.to_string(),
                scope,
            },
            Event::DecisionRequested {
                run_id: _,
                point,
                model,
            } => Event::DecisionRequested {
                run_id: run_id.to_string(),
                point,
                model,
            },
            Event::DecisionMade {
                run_id: _,
                point,
                model,
                answer,
            } => Event::DecisionMade {
                run_id: run_id.to_string(),
                point,
                model,
                answer,
            },
            Event::DecisionRecorded {
                run_id: _,
                point,
                model,
                action,
            } => Event::DecisionRecorded {
                run_id: run_id.to_string(),
                point,
                model,
                action,
            },
            Event::ContextTrimmed {
                run_id: _,
                estimated,
                window,
                dropped_rows,
                compacted_rows,
            } => Event::ContextTrimmed {
                run_id: run_id.to_string(),
                estimated,
                window,
                dropped_rows,
                compacted_rows,
            },
            Event::ContextCompressed {
                run_id: _,
                model,
                exchanges,
                rows,
                chars_before,
                chars_after,
            } => Event::ContextCompressed {
                run_id: run_id.to_string(),
                model,
                exchanges,
                rows,
                chars_before,
                chars_after,
            },
            Event::SessionTitled {
                run_id: _,
                title,
                model,
                source,
            } => Event::SessionTitled {
                run_id: run_id.to_string(),
                title,
                model,
                source,
            },
            Event::UsageRecorded {
                run_id: _,
                model,
                provider,
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
            } => Event::UsageRecorded {
                run_id: run_id.to_string(),
                model,
                provider,
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
            },
        }
    }
}
