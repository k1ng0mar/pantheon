//! Execution engine: process/fs/git + deterministic output compaction.
//!
//! Rule: noisy tool output gets compacted deterministically BEFORE it hits
//! model context. no model summarization, no vibe-truncation - exact head/tail
//! and a marker saying what got dropped (noisegate lesson, as a core primitive).
//!
//! The callable-tool layer (registry, builtins, memory/vault/session-search
//! tools and register helpers) is in `pantheon-tools` - capability != tool,
//! and that crate depends on this one (Tools -> Exec).

pub mod plugin_approval;
pub mod plugins;
pub mod sandbox;
pub mod sandbox_group;
pub mod skills_group;
pub mod tools_group;

// re-exported at crate root: these were `pantheon_sandbox::...` paths before
// the sandbox merged into exec.
pub use sandbox::{Enforcement, ExecutionBoundary, SandboxLevel, SandboxProfile};

// crate-root shims for modules moved into groups by the exec merge. external
// paths (`pantheon_exec::confine::...` etc.) keep working; same as providers.

pub mod confine {
    pub use crate::sandbox_group::confine::*;
}
pub mod danger {
    pub use crate::sandbox_group::danger::*;
}
pub mod supervisor {
    pub use crate::sandbox_group::supervisor::*;
}
pub mod safewrite {
    pub use crate::sandbox_group::safewrite::*;
}
pub mod prestate {
    pub use crate::sandbox_group::prestate::*;
}

pub mod skills {
    pub use crate::skills_group::skills::*;
}
pub mod skills_scan {
    pub use crate::skills_group::skills_scan::*;
}
pub mod bundled_skills {
    pub use crate::skills_group::bundled_skills::*;
}
pub mod skill_exec {
    pub use crate::skills_group::skill_exec::*;
}

pub mod cloudflare {
    pub use crate::tools_group::cloudflare::*;
}
pub mod http {
    pub use crate::tools_group::http::*;
}
pub mod lsp {
    pub use crate::tools_group::lsp::*;
}
pub mod codegraph {
    pub use crate::tools_group::codegraph::*;
}
pub mod gitundo {
    pub use crate::tools_group::gitundo::*;
}
pub mod worktree {
    pub use crate::tools_group::worktree::*;
}
pub mod process {
    pub use crate::tools_group::process::*;
}
pub mod context {
    pub use crate::tools_group::context::*;
}

use serde::{Deserialize, Serialize};

/// compaction policy for one tool's output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPolicy {
    /// max lines kept total (head + tail)
    pub max_lines: usize,
    /// lines kept from the head (errors usually live at the tail, keep both)
    pub head_lines: usize,
    /// max bytes kept regardless of lines
    pub max_bytes: usize,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            max_lines: 200,
            head_lines: 60,
            max_bytes: 64 * 1024,
        }
    }
}

/// Result of compacting one tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compacted {
    pub text: String,
    pub truncated: bool,
}

/// deterministic compaction: keep head + tail, drop the middle w/ a marker
pub fn compact_output(raw: &str, policy: &CompactionPolicy) -> Compacted {
    let raw_bytes = raw.as_bytes();
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= policy.max_lines && raw_bytes.len() <= policy.max_bytes {
        return Compacted {
            text: raw.to_string(),
            truncated: false,
        };
    }
    let tail_lines = policy.max_lines.saturating_sub(policy.head_lines);
    let head: Vec<&str> = lines.iter().take(policy.head_lines).copied().collect();
    let tail: Vec<&str> = if tail_lines == 0 {
        vec![]
    } else {
        lines.iter().rev().take(tail_lines).rev().copied().collect()
    };
    let dropped: Vec<&str> = lines
        .iter()
        .skip(head.len())
        .take(lines.len().saturating_sub(head.len() + tail.len()))
        .copied()
        .collect();
    let dropped_text = dropped.join("\n");
    let mut text = head.join("\n");
    text.push_str(&format!(
        "\n[... compacted: dropped {} lines, {} bytes ...]\n",
        dropped.len(),
        dropped_text.len()
    ));
    text.push_str(&tail.join("\n"));
    if text.len() > policy.max_bytes {
        // truncate on a char boundary: `String::truncate` panics mid-char, so
        // multi-byte output (CJK, emoji) at the cap crashed the tool path
        // (audit COR-03).
        let mut cut = policy.max_bytes;
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[... byte-cap ...]");
    }
    Compacted {
        truncated: true,
        text,
    }
}
