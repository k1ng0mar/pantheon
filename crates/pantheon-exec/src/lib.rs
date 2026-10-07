//! Execution engine: process/fs/git surface + deterministic output
//! compaction (noisegate lesson as a core primitive, not a plugin).
//!
//! Rule: noisy tool output is compacted deterministically BEFORE it reaches
//! model context. No model summarization, no vibe-truncation. Keep exact
//! head/tail, and note what was dropped in the marker line.
//!
//! The callable-tool layer (registry, builtins, memory/vault/session-search
//! tools and their register helpers) lives in `pantheon-tools` - capability ≠
//! tool, and `pantheon-tools` depends *on* this crate (Tools → Exec).

pub mod bundled_skills;
pub mod confine;
pub mod context;
pub mod danger;
pub mod gitundo;
pub mod http;
pub mod lsp;
pub mod plugin_approval;
pub mod plugins;
pub mod process;
pub mod safewrite;
pub mod sandbox;
pub mod skill_exec;
pub mod skills;
pub mod supervisor;

// Re-exported at the crate root: these were `pantheon_sandbox::...` paths
// before the sandbox merged into exec.
pub use sandbox::{Enforcement, ExecutionBoundary, SandboxLevel, SandboxProfile};

use serde::{Deserialize, Serialize};

/// Compaction policy for one tool's output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPolicy {
    /// Max lines kept total (head + tail).
    pub max_lines: usize,
    /// Lines kept from the head (errors often at tail; keep both).
    pub head_lines: usize,
    /// Max bytes kept regardless of lines.
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

/// Deterministic compaction: keep head + tail, drop middle with a marker.
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
        text.truncate(policy.max_bytes);
        text.push_str("\n[... byte-cap ...]");
    }
    Compacted {
        truncated: true,
        text,
    }
}
