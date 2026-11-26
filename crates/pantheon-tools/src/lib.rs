//! named + schema'd + capability-gated callable ops.
//!
//! capability != tool, and the distinction was deliberate:
//! - capability = what policy lets you do (`filesystem.write`); types in
//!   `pantheon-api::capability`, role maps/resolution in `pantheon-agent::capability`
//! - tool = the actual callable thing (`write_file`); that's this crate
//!
//! the registry is where enforcement happens: `execute_gated` checks policy
//! before any closure runs, so new callers get it without having to remember.
//! tools go *through* `pantheon-exec` (process/fs engines) and exec knows
//! nothing about the registry - that's what keeps `capability → tools → exec` acyclic.

pub mod tools;
pub mod tools_group;

// shims for modules moved into the tools group. old paths like
// `pantheon_tools::builtins::...` still work. same trick as exec/providers.

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
