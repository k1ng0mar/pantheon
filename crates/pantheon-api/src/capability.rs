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
    /// A low-confidence autonomous browser action (`browser_act` in
    /// pantheon-web's `browser` module). Split from Browser because clicking the top
    /// intent candidate is the one browser operation that acts on the
    /// model's behalf without a verified target: default policies mark
    /// this Approval so the run loop parks for a human before it runs.
    /// Mirrors the GitPush split from ShellExecute.
    BrowserAct,
    /// Filling a saved website-login credential into the browser
    /// (`browser_fill_login` in pantheon-web's `browser` module). Split
    /// from Browser because it touches the user's credential vault: the
    /// model never sees the secret (the tool result reports only which
    /// fields were filled), but the *decision* to fill is the user's —
    /// default policies mark this Approval so the run loop parks for a
    /// human before it runs. Mirrors the BrowserAct split.
    BrowserFillLogin,
    /// Desktop control through the CUA driver (`cua-driver`). Its own
    /// capability because it drives the user's real desktop: default
    /// policies mark this Approval so the run loop parks for a human
    /// before it acts.
    ComputerUse,
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
    /// Enabling a bundled plugin (`enable_plugin` tool). Split out so the
    /// agent can propose it but never self-authorize: default policies mark
    /// this Approval, so the run parks for a human before any plugin's
    /// code is switched on. Only bundled-catalog plugins are toggleable
    /// this way — there is no agent path to install arbitrary plugins.
    PluginEnable,
    /// Enabling a bundled MCP server (`enable_mcp` tool). The MCP twin
    /// of [`Capability::PluginEnable`]: default policies mark this
    /// Approval, so the run parks for a human before any server's tools
    /// are projected into the registry. Only bundled-catalog servers are
    /// toggleable this way — there is no agent path to put an arbitrary
    /// command on the spawn line (the supply-chain boundary).
    McpEnable,
    Other(String),
}

/// Tool name for the review-verdict tool. Registered only on reviewer
/// runs in staged review stages (see `pantheon_runtime::swarm_exec`):
/// the reviewer emits its verdict as a tool call and the orchestrator
/// extracts the structured args from the ledger, instead of parsing the
/// reviewer's prose.
pub const VERDICT_TOOL_NAME: &str = "verdict";

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
            Capability::BrowserAct => "browser.act".to_string(),
            Capability::BrowserFillLogin => "browser.fill_login".to_string(),
            Capability::ComputerUse => "computer.use".to_string(),
            Capability::MessageSend(_) => "message.send".to_string(),
            Capability::MemoryRead => "memory.read".to_string(),
            Capability::MemoryWrite => "memory.write".to_string(),
            Capability::MemoryConfirm => "memory.confirm".to_string(),
            Capability::SecretsUse => "secrets.use".to_string(),
            Capability::AgentSpawn => "agent.spawn".to_string(),
            Capability::PluginEnable => "plugin.enable".to_string(),
            Capability::McpEnable => "mcp.enable".to_string(),
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
            "browser.act" => Capability::BrowserAct,
            "browser.fill_login" => Capability::BrowserFillLogin,
            "computer.use" => Capability::ComputerUse,
            "memory.read" => Capability::MemoryRead,
            "memory.write" => Capability::MemoryWrite,
            "memory.confirm" => Capability::MemoryConfirm,
            "secrets.use" => Capability::SecretsUse,
            "agent.spawn" => Capability::AgentSpawn,
            "plugin.enable" => Capability::PluginEnable,
            "mcp.enable" => Capability::McpEnable,
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

    /// Coder preset: read/write/exec/git, push needs approval. Browser and
    /// outbound network are allowed (web access is a core assistant
    /// capability); autonomous low-confidence browser acts, filling saved
    /// login credentials, desktop control through the CUA driver, and
    /// enabling bundled plugins need approval.
    /// The `todo` planning tool is allowed: it only mutates the run's own
    /// in-memory task list, persisted to the ledger as a transcript event.
    pub fn coder() -> Self {
        Self::default()
            .allow(Capability::FilesystemRead)
            .allow(Capability::FilesystemWrite)
            .allow(Capability::ShellExecute)
            .allow(Capability::GitRead)
            .allow(Capability::GitWrite)
            .approval(Capability::GitPush)
            .allow(Capability::NetworkOutbound)
            .allow(Capability::Browser)
            .approval(Capability::BrowserAct)
            .approval(Capability::BrowserFillLogin)
            .approval(Capability::ComputerUse)
            .approval(Capability::PluginEnable)
            .approval(Capability::McpEnable)
            .allow(Capability::MemoryRead)
            .allow(Capability::AgentSpawn)
            .allow(Capability::Other(crate::todo::TODO_TOOL_NAME.into()))
            .allow(Capability::Other(VERDICT_TOOL_NAME.into()))
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
