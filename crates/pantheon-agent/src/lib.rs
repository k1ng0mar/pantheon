//! Agent engine (spec section 2): the brain loop.
//!
//! receive input -> load state -> build context -> model inference ->
//! interpret response -> tool / delegation / response -> execute ->
//! observe result -> update state -> continue.
//!
//! Every transition emits a pantheon_api::events::Event through an
//! `EventSink`, so replay, debugging, and crash recovery come for free.
//! The model is behind `ModelTurn` - swapping providers never touches
//! this loop. Tools are behind `ToolRunner` and gated by
//! `crate::capability::enforce` before anything executes.
//!
//! The production path is `pantheon-runtime::Session::drive`, the CANONICAL
//! wired path (typed `Message` transcript + provenance + real provider
//! chain). It builds an `AgentLoop` but drives its own typed `Message`
//! turn loop over the same policy, budget, and run identity. Do not add
//! provider or network behavior here; keep it deterministic.
//!
//! The judge is host-side, not loop-side: `pantheon-runtime::Session` builds
//! its `AgentLoop` with `judge: None`, then resolves the `[judge]` auxiliary
//! itself (`Session::build_judge`) and drives approval verdicts through
//! `AgentLoop::consult_gate_advisory` in `PermissionMode::Smart`. Keeping the
//! client out of the loop is deliberate - this crate stays deterministic and
//! provider-free.
//!
//! Agent profiles (`[agents.*]` config tables) live at the API layer:
//! they are config-document types, and the runtime consumes them without
//! going through the agent crate. Re-exported here so
//! `pantheon_agent::agent_profile::...` paths keep resolving.
pub use pantheon_api::agent_profile;
pub mod core;

pub mod capability {
    pub use crate::core::capability::*;
}
pub mod engine {
    pub use crate::core::engine::*;
}
pub mod subagent {
    pub use crate::core::subagent::*;
}
pub mod tool {
    pub use crate::core::tool::*;
}

pub use core::engine::{
    AgentLoop, Budget, LoopOutcome, ModelTurn, SubagentSpawner, SwarmCtx, ToolCall, TurnOutcome,
};
pub use core::subagent::{SubagentCaps, SubagentHandle, SubagentRegistry, SubagentStatus};
pub use core::tool::{gate, EventSink, GateOutcome, ToolRunner};
