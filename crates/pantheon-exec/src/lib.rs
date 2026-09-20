//! Execution engine: tool/process/fs/git surface + deterministic output
//! compaction (noisegate lesson as a core primitive, not a plugin).
//!
//! Rule: noisy tool output is compacted deterministically BEFORE it reaches
//! model context. No model summarization, no vibe-truncation. Keep exact
//! head/tail, hash the dropped middle, record what was dropped in the ledger.
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
        Self { max_lines: 200, head_lines: 60, max_bytes: 64 * 1024 }
    }
}

/// Result of compacting one tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Compacted {
    pub text: String,
    pub truncated: bool,
    pub kept_lines: usize,
    pub dropped_lines: usize,
    pub kept_bytes: usize,
    pub dropped_bytes: usize,
    /// FNV-1a hash of the dropped middle (integrity, like artifact verify).
    pub dropped_hash: String,
}

fn fnv1a_hex(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Deterministic compaction: keep head + tail, drop middle with a marker.
pub fn compact_output(raw: &str, policy: &CompactionPolicy) -> Compacted {
    let raw_bytes = raw.as_bytes();
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() <= policy.max_lines && raw_bytes.len() <= policy.max_bytes {
        return Compacted {
            text: raw.to_string(),
            truncated: false,
            kept_lines: lines.len(),
            dropped_lines: 0,
            kept_bytes: raw_bytes.len(),
            dropped_bytes: 0,
            dropped_hash: fnv1a_hex(&[]),
        };
    }
    let tail_lines = policy.max_lines.saturating_sub(policy.head_lines);
    let head: Vec<&str> = lines.iter().take(policy.head_lines).copied().collect();
    let tail: Vec<&str> = if tail_lines == 0 { vec![] } else {
        lines.iter().rev().take(tail_lines).rev().copied().collect()
    };
    let dropped: Vec<&str> = lines.iter().skip(head.len())
        .take(lines.len().saturating_sub(head.len() + tail.len()))
        .copied().collect();
    let dropped_text = dropped.join("\n");
    let mut text = head.join("\n");
    text.push_str(&format!(
        "\n[... compacted: dropped {} lines, {} bytes, hash {} ...]\n",
        dropped.len(), dropped_text.len(), fnv1a_hex(dropped_text.as_bytes())
    ));
    text.push_str(&tail.join("\n"));
    if text.len() > policy.max_bytes {
        text.truncate(policy.max_bytes);
        text.push_str("\n[... byte-cap ...]");
    }
    Compacted {
        kept_lines: head.len() + tail.len(),
        dropped_lines: dropped.len(),
        kept_bytes: head.join("\n").len() + tail.join("\n").len(),
        dropped_bytes: dropped_text.len(),
        dropped_hash: fnv1a_hex(dropped_text.as_bytes()),
        truncated: true,
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_output_untouched() {
        let c = compact_output("a\nb\n", &CompactionPolicy::default());
        assert!(!c.truncated);
        assert_eq!(c.text, "a\nb\n");
    }
    #[test]
    fn wall_compacted_with_marker() {
        let raw: String = (0..1000).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let c = compact_output(&raw, &CompactionPolicy::default());
        assert!(c.truncated);
        assert!(c.text.contains("line 0"));
        assert!(c.text.contains("line 999"));
        assert!(c.text.contains("compacted: dropped"));
        assert_eq!(c.kept_lines + c.dropped_lines, 1000);
    }
}
