//! Hook points.
//!
//! SCOPE (this comment is load-bearing — keep it true).
//!
//! Pantheon implements a *subset* of the Hermes hook surface, not a superset.
//! Hermes' `VALID_HOOKS` (`hermes_cli/plugins.py`) names 41 events; of those,
//! Pantheon wires 14 (below) and declares 1 unwired. Every wired hook has a
//! real fire site: either in `session.rs` (the tool-execution gate/transform
//! and the context-injection point) or via `event_bridge`, which maps
//! canonical `pantheon_core::Event`s onto hooks on the supervisor's
//! post-durable-write observer fan-out.
//!
//! The rule this file enforces: **no hook may be declared without a fire
//! site.** `Hook::is_wired()` is the single source of truth, and
//! `compat::map_hook` and `doctor` both consult it, so an unwired hook can
//! never be reported as `Mapped` (which would promise a plugin that its
//! handler will run) and a manifest that declares one is flagged loudly.
//! This mirrors Hermes' own constraint: "no inert VALID_HOOKS surface is
//! registered ahead of implementation."
//!
//! Hook classes matter because they have different contracts:
//! - [`HookClass::Context`] may inject text; the return value is used.
//! - [`HookClass::Gate`] may deny; **fails closed** on plugin error/timeout.
//! - [`HookClass::Transform`] may replace a payload; fails open.
//! - [`HookClass::Observer`] is notified; its return value is ignored.
use serde::{Deserialize, Serialize};

/// What a hook is allowed to do, which fixes its failure mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookClass {
    /// May inject context that the host adds to the prompt. Return value used.
    Context,
    /// May deny the action. Fails CLOSED: a plugin that errors or times out
    /// denies, because a security gate that fails open is not a gate.
    Gate,
    /// May return a replacement payload. Fails OPEN: first non-`None` wins,
    /// and a failing plugin leaves the payload unchanged.
    Transform,
    /// Notified only. Return values are ignored by the host.
    Observer,
}

/// Every extension hook point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    /// Before model inference. May inject context (anti-ai-writing, time-gap).
    PreLlmCall,
    /// Before an outbound provider API request.
    PreApiRequest,
    /// After a provider API response.
    PostApiRequest,
    /// Before a gateway dispatches an inbound message.
    PreGatewayDispatch,
    /// Once when a run starts. Observers: memory/session bootstrap.
    OnSessionStart,
    /// Once when a run reaches a terminal state (completed, failed, or
    /// canceled). GalaxyMem uses this for memory consolidation; disk-cleanup
    /// uses it. Terminal-once semantics matter: a crashed-then-recovered run
    /// must not double-consolidate, so the manager's `once_per_session`
    /// dedup is the enforcement point.
    OnSessionEnd,
    /// Before a gated tool call executes. May DENY the call.
    PreToolCall,
    /// After a tool call completes. Observer.
    PostToolCall,
    /// May REPLACE a tool's output before the model sees it. Redaction and
    /// secret-scrubbing live here.
    TransformToolResult,
    /// A sub-agent was spawned. Observer — this is what makes §3 swarm
    /// spawns visible to plugins.
    SubagentStart,
    /// A sub-agent finished. Observer.
    SubagentStop,
    /// A model response stream opened. Observer.
    OnStreamStart,
    /// A streamed text delta arrived. Observer, and **off by default**: the
    /// runner spawns a process per fire, and this fires per token. Enable
    /// with `PANTHEON_HOOK_STREAM_DELTA=1` only if a plugin truly needs it.
    /// Hermes makes the same call ("off the token path").
    OnStreamDelta,
    /// A model response stream closed. Observer.
    OnStreamEnd,
    /// Context compaction ran on this run (oldest rows trimmed or summarized
    /// into a memory-tier note). Observer.
    ///
    /// OMP emits this as a start/end pair (`auto_compaction_start` /
    /// `auto_compaction_end`); Pantheon fires once, after the fact, off the
    /// durable `ContextTrimmed` / `ContextCompressed` events. A plugin that
    /// needs "compaction is about to happen" gets "compaction happened"
    /// instead — the collapse is documented here and in `compat::map_hook`
    /// so nobody mistakes it for a pre-compaction gate.
    OnCompaction,
}

impl Hook {
    pub fn name(&self) -> &'static str {
        match self {
            Hook::PreLlmCall => "pre_llm_call",
            Hook::PreApiRequest => "pre_api_request",
            Hook::PostApiRequest => "post_api_request",
            Hook::PreGatewayDispatch => "pre_gateway_dispatch",
            Hook::OnSessionStart => "on_session_start",
            Hook::OnSessionEnd => "on_session_end",
            Hook::PreToolCall => "pre_tool_call",
            Hook::PostToolCall => "post_tool_call",
            Hook::TransformToolResult => "transform_tool_result",
            Hook::SubagentStart => "subagent_start",
            Hook::SubagentStop => "subagent_stop",
            Hook::OnStreamStart => "on_stream_start",
            Hook::OnStreamDelta => "on_stream_delta",
            Hook::OnStreamEnd => "on_stream_end",
            Hook::OnCompaction => "on_compaction",
        }
    }
    pub fn parse(name: &str) -> Option<Hook> {
        match name.trim() {
            "pre_llm_call" => Some(Hook::PreLlmCall),
            "pre_api_request" => Some(Hook::PreApiRequest),
            "post_api_request" => Some(Hook::PostApiRequest),
            "pre_gateway_dispatch" => Some(Hook::PreGatewayDispatch),
            "on_session_start" => Some(Hook::OnSessionStart),
            "on_session_end" => Some(Hook::OnSessionEnd),
            "pre_tool_call" => Some(Hook::PreToolCall),
            "post_tool_call" => Some(Hook::PostToolCall),
            "transform_tool_result" => Some(Hook::TransformToolResult),
            "subagent_start" => Some(Hook::SubagentStart),
            "subagent_stop" => Some(Hook::SubagentStop),
            "on_stream_start" => Some(Hook::OnStreamStart),
            "on_stream_delta" => Some(Hook::OnStreamDelta),
            "on_stream_end" => Some(Hook::OnStreamEnd),
            "on_compaction" => Some(Hook::OnCompaction),
            _ => None,
        }
    }
    /// What this hook is permitted to do, which fixes its failure mode.
    pub fn class(&self) -> HookClass {
        match self {
            Hook::PreLlmCall => HookClass::Context,
            Hook::PreToolCall => HookClass::Gate,
            Hook::TransformToolResult => HookClass::Transform,
            _ => HookClass::Observer,
        }
    }
    /// Whether this hook has a real fire site in the runtime today.
    ///
    /// `pre_gateway_dispatch` is declared but **unwired**: inbound messages
    /// are handled in `pantheon-gateway`, which deliberately depends only on
    /// `pantheon-core` + `pantheon-storage` and so cannot reach the extension
    /// manager without a new dependency edge. Until that exists the hook is
    /// reported as unsupported rather than falsely mapped.
    pub fn is_wired(&self) -> bool {
        !matches!(self, Hook::PreGatewayDispatch)
    }
    /// Fires often enough that a per-fire process spawn is a hazard.
    /// Gated behind an explicit opt-in by the event bridge.
    pub fn is_high_frequency(&self) -> bool {
        matches!(self, Hook::OnStreamDelta)
    }
    /// All known hooks (for doctor + docs).
    pub fn all() -> &'static [Hook] {
        &[
            Hook::PreLlmCall,
            Hook::PreApiRequest,
            Hook::PostApiRequest,
            Hook::PreGatewayDispatch,
            Hook::OnSessionStart,
            Hook::OnSessionEnd,
            Hook::OnCompaction,
            Hook::PreToolCall,
            Hook::PostToolCall,
            Hook::TransformToolResult,
            Hook::SubagentStart,
            Hook::SubagentStop,
            Hook::OnStreamStart,
            Hook::OnStreamDelta,
            Hook::OnStreamEnd,
        ]
    }
}
