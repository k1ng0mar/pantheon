//! The model/tool loop, kept independent of providers and I/O.
//!
//! Runtime does the real session wiring; this crate keeps the loop testable.
//! Policy runs before tools, turns emit through `EventSink`.
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
