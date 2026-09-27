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
    /// Promoting a record's trust tier (memory_confirm). Split from
    /// MemoryWrite because confirming is the user-vouch path: a model
    /// that may propose records must not be able to confirm its own
    /// into the trusted tier. Default policies mark this Approval so
    /// the run loop parks for a human before the promotion runs.
    MemoryConfirm,
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

impl Capability {
    /// Stable, log-safe token for a capability, e.g. `shell.execute`.
    /// Used for approval scopes and events, so it never carries payload
    /// values: a `MessageSend` token is the channel class, not the message.
    /// Single home for the token table — sandbox labels and MCP parsing
    /// both delegate here so a new variant touches one match.
    pub fn token(&self) -> String {
        match self {
            Capability::FilesystemRead => "filesystem.read".to_string(),
            Capability::FilesystemWrite => "filesystem.write".to_string(),
            Capability::ShellExecute => "shell.execute".to_string(),
            Capability::GitRead => "git.read".to_string(),
            Capability::GitWrite => "git.write".to_string(),
            Capability::GitPush => "git.push".to_string(),
            Capability::NetworkOutbound => "network.outbound".to_string(),
            Capability::Browser => "browser".to_string(),
            Capability::MessageSend(_) => "message.send".to_string(),
            Capability::MemoryRead => "memory.read".to_string(),
            Capability::MemoryWrite => "memory.write".to_string(),
            Capability::MemoryConfirm => "memory.confirm".to_string(),
            Capability::SecretsUse => "secrets.use".to_string(),
            Capability::AgentSpawn => "agent.spawn".to_string(),
            Capability::Other(name) => format!("other.{}", name.trim().replace(' ', ".")),
        }
    }

    /// Map a policy token string to a capability. Unknown tokens become
    /// `Other(name)` so policy can still gate them explicitly.
    /// Inverse of [`Capability::token`] for the fixed variants.
    pub fn from_token(token: &str) -> Capability {
        match token.trim() {
            "filesystem.read" => Capability::FilesystemRead,
            "filesystem.write" => Capability::FilesystemWrite,
            "shell.execute" => Capability::ShellExecute,
            "git.read" => Capability::GitRead,
            "git.write" => Capability::GitWrite,
            "git.push" => Capability::GitPush,
            "network.outbound" => Capability::NetworkOutbound,
            "browser" => Capability::Browser,
            "memory.read" => Capability::MemoryRead,
            "memory.write" => Capability::MemoryWrite,
            "memory.confirm" => Capability::MemoryConfirm,
            "secrets.use" => Capability::SecretsUse,
            "agent.spawn" => Capability::AgentSpawn,
            other => Capability::Other(other.to_string()),
        }
    }
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

    /// Coder preset plus the memory write capability. Confirming
    /// (promoting a record's trust tier) is a separate capability and
    /// needs human approval: the model may propose, but only a user
    /// vouches a record into the trusted tier.
    pub fn coder_with_memory() -> Self {
        Self::coder()
            .allow(Capability::MemoryWrite)
            .approval(Capability::MemoryConfirm)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_confirm_token_roundtrips() {
        assert_eq!(Capability::MemoryConfirm.token(), "memory.confirm");
        assert_eq!(
            Capability::from_token("memory.confirm"),
            Capability::MemoryConfirm
        );
        // Serialization carries the variant too (serde derive).
        let json = serde_json::to_string(&Capability::MemoryConfirm).unwrap();
        assert_eq!(
            serde_json::from_str::<Capability>(&json).unwrap(),
            Capability::MemoryConfirm
        );
    }

    #[test]
    fn coder_with_memory_requires_approval_for_confirm() {
        let p = Policy::coder_with_memory();
        assert_eq!(p.check(&Capability::MemoryConfirm), Decision::Approval);
        assert_eq!(p.check(&Capability::MemoryWrite), Decision::Allow);
        // Default-deny still holds for policies that never mention it.
        assert_eq!(
            Policy::coder().check(&Capability::MemoryConfirm),
            Decision::Deny
        );
    }
}
