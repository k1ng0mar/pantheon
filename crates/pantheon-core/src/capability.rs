//! Capability system (§9). Security backbone: granular capabilities gated by
//! policy, never bare `coder = yes`.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Granular capability tokens.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    FilesystemRead,
    FilesystemWrite,
    ShellExecute,
    GitRead,
    GitWrite,
    GitPush,
    NetworkOutbound,
    Browser,
    MessageSend(String),
    MemoryRead,
    MemoryWrite,
    SecretsUse,
    AgentSpawn,
    Other(String),
}

/// A policy decision for one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    Deny,
    Approval,
}

/// Policy: capability -> decision, default-deny.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Policy {
    rules: std::collections::HashMap<Capability, Decision>,
}

impl Policy {
    pub fn allow(mut self, cap: Capability) -> Self {
        self.rules.insert(cap, Decision::Allow);
        self
    }

    pub fn approval(mut self, cap: Capability) -> Self {
        self.rules.insert(cap, Decision::Approval);
        self
    }

    pub fn deny(mut self, cap: Capability) -> Self {
        self.rules.insert(cap, Decision::Deny);
        self
    }

    pub fn check(&self, cap: &Capability) -> Decision {
        self.rules.get(cap).copied().unwrap_or(Decision::Deny)
    }

    /// Coder preset: read/write/exec/git, push needs approval.
    pub fn coder() -> Self {
        Self::default()
            .allow(Capability::FilesystemRead)
            .allow(Capability::FilesystemWrite)
            .allow(Capability::ShellExecute)
            .allow(Capability::GitRead)
            .allow(Capability::GitWrite)
            .approval(Capability::GitPush)
            .allow(Capability::MemoryRead)
            .allow(Capability::AgentSpawn)
    }

    /// Coder preset plus the memory write capability.
    pub fn coder_with_memory() -> Self {
        Self::coder().allow(Capability::MemoryWrite)
    }

    /// Read-only researcher preset.
    pub fn researcher_readonly() -> Self {
        Self::default()
            .allow(Capability::FilesystemRead)
            .allow(Capability::MemoryRead)
    }

    pub fn granted(&self) -> HashSet<Capability> {
        self.rules
            .iter()
            .filter(|(_, d)| **d == Decision::Allow)
            .map(|(c, _)| c.clone())
            .collect()
    }

    /// Capabilities parked on approval. Resume treats scopes recorded in
    /// ApprovalGranted events as one-shot allows for the matching call.
    pub fn approval_caps(&self) -> HashSet<Capability> {
        self.rules
            .iter()
            .filter(|(_, d)| **d == Decision::Approval)
            .map(|(c, _)| c.clone())
            .collect()
    }
}
