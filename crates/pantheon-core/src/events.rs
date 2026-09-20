//! Agent-engine event enum. Every meaningful transition becomes an event,
//! giving replayability, debugging, and crash recovery.

use serde::{Deserialize, Serialize};

/// Canonical runtime events (§2 agent engine + §18 runtime API).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    RunStarted { run_id: String },
    RunProgress { run_id: String, detail: String },
    RunCompleted { run_id: String },
    RunFailed { run_id: String, code: String },
    RunRecovered { run_id: String },
    ModelRequested { run_id: String, model: String },
    ModelDelta { run_id: String, delta: String },
    ModelCompleted { run_id: String },
    ToolRequested { run_id: String, tool: String },
    ToolStarted { run_id: String, tool: String },
    ToolOutput { run_id: String, tool: String, truncated: bool },
    ToolCompleted { run_id: String, tool: String },
    AgentSpawned { run_id: String, agent: String },
    AgentMessage { run_id: String, agent: String },
    AgentCompleted { run_id: String, agent: String },
    MemoryProposed { run_id: String },
    ApprovalRequested { run_id: String, scope: String },
    ApprovalGranted { run_id: String, scope: String },
}
