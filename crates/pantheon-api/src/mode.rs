//! Agent operating mode: Build vs Plan.
//!
//! Plan mode is a host-enforced read-only posture for the agent loop.
//! The TUI flips it with Tab; the runtime stores it on the [`Session`]
//! (session-scoped, like `/goal`) and reads it at every tool gate,
//! refusing mutating tools with a model-facing message instead of
//! executing them. Default is Build, so existing behavior is unchanged
//! unless the operator opts in.
//!
//! [`Session`]: pantheon_runtime::session::Session
use serde::{Deserialize, Serialize};

/// Which posture the agent loop runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AgentMode {
    /// Normal operation: read and write tools run (subject to policy).
    #[default]
    Build,
    /// Planning only: mutating tools are refused at the gate with
    /// [`PLAN_MODE_REFUSAL`]; read-only tools run normally.
    Plan,
}

impl AgentMode {
    /// Stable string form (`"build"` / `"plan"`).
    pub fn as_str(self) -> &'static str {
        match self {
            AgentMode::Build => "build",
            AgentMode::Plan => "plan",
        }
    }

    /// Parse [`AgentMode::as_str`] output (case-insensitive).
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "build" => Some(AgentMode::Build),
            "plan" => Some(AgentMode::Plan),
            _ => None,
        }
    }
}

/// Model-facing refusal surfaced as a normal tool result when Plan mode
/// blocks a mutating tool. The run neither parks nor fails: the model
/// sees the refusal in its transcript and keeps planning.
pub const PLAN_MODE_REFUSAL: &str = "Plan mode is active: write/execute tools are disabled. \
    Describe your plan instead; do not attempt the change.";

/// Tools that are unambiguously read-only. Everything else is treated
/// as mutating (fail closed). Matching is EXACT (after trim + lowercase):
/// an earlier prefix rule lived here, but a prefix cannot tell
/// `read_file` from `read_write`, so any name merely starting with a
/// read-only word slipped through as read-only. Unknown names are
/// mutating, full stop — a provider-registered tool earns its entry by
/// being known read-only, not by naming luck.
const READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "preview_file",
    "list_dir",
    "list_checkpoints",
    "todo",
    "memory_recall",
    "memory_list",
    "session_search",
    "vault_read",
    "vault_list",
    "vault_search",
    "skill_read",
    "skills_list",
    "web_search",
    "ask_user",
    // Provider-registered read-only tools (previously covered by prefix
    // families; now listed exactly so similarly-named mutating tools
    // cannot ride the prefix).
    "glob_files",
    "grep_code",
    "web_fetch_page",
    "get_config",
    "find_refs",
    "think",
    "reflect_step",
];

/// True when the named tool can mutate state and must be refused while
/// [`AgentMode::Plan`] is active.
///
/// Classification is by name only — deliberately. Allowing `shell` for
/// "read-only" commands would require parsing arbitrary shell, which is
/// exactly the hole Plan mode closes: **all** exec is blocked in Plan
/// mode, including `ls`. Unknown names are treated as mutating (fail
/// closed); the [`READ_ONLY_TOOLS`] allowlist is the only way through.
pub fn is_mutating_tool(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    !READ_ONLY_TOOLS.iter().any(|t| *t == n)
}
