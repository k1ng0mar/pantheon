//! Sandbox hierarchy (spec section 10). Policy chooses the boundary:
//! LOW = in-process/restricted, MEDIUM = isolated process + limits,
//! HIGH = container, VERY HIGH = stronger sandbox/VM.
//!
//! [`level`] turns a level into the concrete limits the executor must
//! apply; [`enforce`] maps the core capability policy (Allow/Deny/Approval)
//! onto those boundaries. Levels are ordered weakest -> strongest.
pub mod enforce;
pub mod level;
pub mod runner;

pub use enforce::{capability_label, enforce, Enforcement};
pub use level::{profile_for, ExecutionBoundary, SandboxProfile};

use pantheon_api::capability::Capability;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SandboxLevel {
    Low,
    Medium,
    High,
    VeryHigh,
}

impl SandboxLevel {
    /// Pick the minimum level for a capability use.
    pub fn for_capability(cap: &Capability) -> SandboxLevel {
        match cap {
            Capability::FilesystemRead | Capability::MemoryRead => SandboxLevel::Low,
            Capability::FilesystemWrite
            | Capability::MemoryWrite
            | Capability::GitRead
            | Capability::MessageSend(_) => SandboxLevel::Medium,
            Capability::ShellExecute | Capability::GitWrite | Capability::NetworkOutbound => {
                SandboxLevel::High
            }
            Capability::GitPush
            | Capability::Browser
            | Capability::SecretsUse
            | Capability::AgentSpawn
            | Capability::Other(_) => SandboxLevel::VeryHigh,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            SandboxLevel::Low => "low",
            SandboxLevel::Medium => "medium",
            SandboxLevel::High => "high",
            SandboxLevel::VeryHigh => "very-high",
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
