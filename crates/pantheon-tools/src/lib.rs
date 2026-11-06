//! Tool layer: named, schema'd, capability-gated callable operations.
//!
//! Deliberate workspace change (capability ≠ tool):
//!
//! - a **capability** is what a policy *allows or is able to do*
//!   (`filesystem.write`) - its types live in `pantheon-api::capability`,
//!   its role maps and resolution in `pantheon-agent::capability`;
//! - a **tool** is one actual callable operation (`write_file`) - here.
//!
//! The registry is the choke point: `execute_gated` applies the policy
//! before any closure runs, so a new caller gets enforcement by default
//! rather than by remembering. Tools execute *through* `pantheon-exec`
//! (process/fs engines); `pantheon-exec` itself no longer knows about the
//! registry - that is what keeps `capability → tools → exec` acyclic.

pub mod tools;
pub mod tools_group;

// Crate-root shims for modules moved into the tools group by the tools
// merge. External paths (`pantheon_tools::builtins::...` etc.) keep working;
// same pattern as the exec and providers merges.

pub mod builtins {
    pub use crate::tools_group::builtins::*;
}
pub mod code_mode {
    pub use crate::tools_group::code_mode::*;
}
pub mod codegraph_tools {
    pub use crate::tools_group::codegraph_tools::*;
}
pub mod format {
    pub use crate::tools_group::format::*;
}
pub mod gitundo_tools {
    pub use crate::tools_group::gitundo_tools::*;
}
pub mod lsp_tools {
    pub use crate::tools_group::lsp_tools::*;
}
pub mod memory_tools {
    pub use crate::tools_group::memory_tools::*;
}
pub mod oauth_tools {
    pub use crate::tools_group::oauth_tools::*;
}
pub mod plugin_tools {
    pub use crate::tools_group::plugin_tools::*;
}
pub mod pty_tools {
    pub use crate::tools_group::pty_tools::*;
}
pub mod safewrite_tools {
    pub use crate::tools_group::safewrite_tools::*;
}
pub mod session_search_tools {
    pub use crate::tools_group::session_search_tools::*;
}
pub mod skill_tools {
    pub use crate::tools_group::skill_tools::*;
}
pub mod todo_tools {
    pub use crate::tools_group::todo_tools::*;
}
pub mod ui_tool {
    pub use crate::tools_group::ui_tool::*;
}
pub mod vault_tools {
    pub use crate::tools_group::vault_tools::*;
}
pub mod verdict_tool {
    pub use crate::tools_group::verdict_tool::*;
}
pub mod worktree_tools {
    pub use crate::tools_group::worktree_tools::*;
}
