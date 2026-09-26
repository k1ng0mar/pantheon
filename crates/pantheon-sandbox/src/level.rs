//! Execution boundaries and the limits attached to each level (§10).
//!
//! The level is a policy value; the boundary is the mechanism that enforces
//! it on this host, and the profile is the concrete limit set handed to the
//! executor. Pattern we follow: capability-dropped containers, no-new-privs,
//! per-boundary rlimits — sandboxing as a runtime primitive, not a Docker
//! wrapper.

use crate::SandboxLevel;
use serde::{Deserialize, Serialize};

/// The mechanism that actually enforces a level.
///
/// Kept separate from [`SandboxLevel`] so a level can be compared and
/// persisted as policy while the boundary stays an implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionBoundary {
    /// Direct in-process call. LOW.
    InProcess,
    /// Forked child process under rlimits. MEDIUM.
    IsolatedProcess,
    /// Container with dropped capabilities and no-new-privs. HIGH.
    Container,
    /// VM / stronger sandbox boundary. VERY HIGH.
    Vm,
}

impl SandboxLevel {
    /// Boundary that enforces this level.
    pub fn boundary(self) -> ExecutionBoundary {
        match self {
            SandboxLevel::Low => ExecutionBoundary::InProcess,
            SandboxLevel::Medium => ExecutionBoundary::IsolatedProcess,
            SandboxLevel::High => ExecutionBoundary::Container,
            SandboxLevel::VeryHigh => ExecutionBoundary::Vm,
        }
    }

    /// Execution limits for this level.
    pub fn profile(self) -> SandboxProfile {
        profile_for(self)
    }
}

/// Concrete limits handed to the executor for one execution.
///
/// `None` means "this level does not impose that limit" — never "unlimited
/// by design". The runtime may tighten any field, never loosen it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProfile {
    pub level: SandboxLevel,
    pub boundary: ExecutionBoundary,
    /// Drop all capabilities from the bounding set before exec.
    pub drop_capabilities: bool,
    /// Set PR_SET_NO_NEW_PRIVS so setuid/setcap binaries cannot escalate.
    pub no_new_privs: bool,
    /// Whether this boundary may mediate network egress at all. Egress is
    /// still capability-gated (`network.outbound`); this is the outer limit.
    pub network: bool,
    pub max_memory_mb: Option<u64>,
    pub max_pids: Option<u32>,
    /// Wall-clock budget for the execution, in milliseconds.
    pub wall_clock_ms: u64,
}

/// Execution limits for a level.
pub fn profile_for(level: SandboxLevel) -> SandboxProfile {
    match level {
        SandboxLevel::Low => SandboxProfile {
            level,
            boundary: ExecutionBoundary::InProcess,
            drop_capabilities: false,
            no_new_privs: false,
            network: false,
            max_memory_mb: None,
            max_pids: None,
            wall_clock_ms: 30_000,
        },
        SandboxLevel::Medium => SandboxProfile {
            level,
            boundary: ExecutionBoundary::IsolatedProcess,
            drop_capabilities: false,
            no_new_privs: true,
            network: false,
            max_memory_mb: Some(2048),
            max_pids: Some(64),
            wall_clock_ms: 120_000,
        },
        SandboxLevel::High => SandboxProfile {
            level,
            boundary: ExecutionBoundary::Container,
            drop_capabilities: true,
            no_new_privs: true,
            network: true,
            max_memory_mb: Some(4096),
            max_pids: Some(256),
            wall_clock_ms: 600_000,
        },
        SandboxLevel::VeryHigh => SandboxProfile {
            level,
            boundary: ExecutionBoundary::Vm,
            drop_capabilities: true,
            no_new_privs: true,
            network: true,
            max_memory_mb: Some(2048),
            max_pids: Some(64),
            wall_clock_ms: 300_000,
        },
    }
}

impl From<SandboxLevel> for SandboxProfile {
    fn from(level: SandboxLevel) -> Self {
        profile_for(level)
    }
}

#[cfg(test)]
#[path = "level_tests.rs"]
mod tests;
