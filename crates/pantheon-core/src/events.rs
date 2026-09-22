//! Agent-engine event enum. Every meaningful transition becomes an event,
//! giving replayability, debugging, and crash recovery.

use crate::message::Message;
use serde::{Deserialize, Serialize};

/// Canonical runtime events (§2 agent engine + §18 runtime API).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    RunRecovered {
        run_id: String,
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
    },
    ToolOutput {
        run_id: String,
        call_id: String,
        tool: String,
        truncated: bool,
    },
    ToolCompleted {
        run_id: String,
        call_id: String,
        tool: String,
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
}
