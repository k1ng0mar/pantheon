//! transcript search: Ctrl+F filters the conversation, n/N walks the hits.
//!
//! Search reads the transcript, never the ledger: a rewind that drops blocks
//! also drops their hits, so `n` can never jump to a block that is gone. The
//! hit list is rebuilt on every query keystroke and kept after the panel
//! closes, which is what makes `n`/`N` work from the transcript.

use super::{BlockKind, TranscriptBlock};

/// one matching block plus the row the panel shows for it
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// index into `TuiState::blocks`
    pub block: usize,
    /// what kind of block this is, for the panel's leading label
    pub label: &'static str,
    /// one line of context around the first match in that block
    pub snippet: String,
}

/// open search panel. `sel` indexes `hits` and clamps, never wraps.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    pub query: String,
    pub hits: Vec<Hit>,
    pub sel: usize,
}

impl SearchState {
    pub fn open() -> Self {
        Self::default()
    }

    /// replace the query and recompute hits, keeping the selection in range.
    /// Empty query -> no hits (not "everything"), so the panel opens calm.
    pub fn set_query(&mut self, blocks: &[TranscriptBlock], query: &str) {
        self.query = query.to_string();
        self.hits = find(blocks, query);
        self.sel = 0;
    }

    pub fn move_sel(&mut self, n: isize) {
        if self.hits.is_empty() {
            self.sel = 0;
            return;
        }
        let sel = self.sel as isize + n;
        self.sel = sel.clamp(0, self.hits.len() as isize - 1) as usize;
    }

    /// block index of the selected hit, if any
    pub fn current(&self) -> Option<usize> {
        self.hits.get(self.sel).map(|h| h.block)
    }
}

/// every block whose searchable text contains `query`, case-insensitive.
/// an empty or whitespace-only query matches nothing.
pub fn find(blocks: &[TranscriptBlock], query: &str) -> Vec<Hit> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let text = searchable_text(&b.kind);
            if !text.to_lowercase().contains(&needle) {
                return None;
            }
            Some(Hit {
                block: i,
                label: label(&b.kind),
                snippet: snippet(&text, &needle, 64),
            })
        })
        .collect()
}

/// the text a block contributes to search. Deliberately includes what the
/// user can read on screen (args, shell output, errors), never internal ids.
pub fn searchable_text(kind: &BlockKind) -> String {
    match kind {
        BlockKind::UserMessage(t) | BlockKind::AssistantMessage(t) | BlockKind::Status(t) => {
            t.clone()
        }
        BlockKind::Thinking { text, .. } => text.clone(),
        BlockKind::ToolCall {
            name,
            args,
            error,
            output,
            ..
        } => {
            let mut out = format!("{name} {args}");
            if let Some(e) = error {
                out.push(' ');
                out.push_str(e);
            }
            for line in output {
                out.push(' ');
                out.push_str(line);
            }
            out
        }
        BlockKind::Swarm { agents, .. } => agents
            .iter()
            .map(|a| format!("{} {:?}", a.name, a.status))
            .collect::<Vec<_>>()
            .join(" "),
        BlockKind::Todos(items) => items
            .iter()
            .map(|i| i.content.clone())
            .collect::<Vec<_>>()
            .join(" "),
        BlockKind::BgResult { label, output, .. } => format!("{label} {output}"),
        BlockKind::Diff { lines, .. } => lines
            .iter()
            .map(|l| match l {
                crate::diffview::DiffLine::FileHeader(s)
                | crate::diffview::DiffLine::Hunk(s)
                | crate::diffview::DiffLine::Context(s)
                | crate::diffview::DiffLine::Add(s)
                | crate::diffview::DiffLine::Del(s) => s.clone(),
                crate::diffview::DiffLine::Truncated(_)
                | crate::diffview::DiffLine::TooLarge { .. } => String::new(),
            })
            .collect::<Vec<_>>()
            .join(" "),
        BlockKind::VerifyGate(findings) => findings
            .iter()
            .map(|f| format!("{} {}", f.sentence, f.reason))
            .collect::<Vec<_>>()
            .join(" "),
        BlockKind::AgentNote { sender, kind, text } => {
            format!("{sender} {kind} {text}")
        }
        BlockKind::Approval {
            tool,
            sentence,
            target,
            ..
        } => format!("{tool} {sentence} {target}"),
        BlockKind::Clarify {
            question, options, ..
        } => format!("{question} {}", options.join(" ")),
        BlockKind::Command { cmd, lines, .. } => format!("{cmd} {}", lines.join(" ")),
        BlockKind::Confirm {
            label, sentence, ..
        } => format!("{label} {sentence}"),
        BlockKind::ProviderError {
            provider,
            model,
            message,
            ..
        } => format!("{provider} {model} {message}"),
        BlockKind::Ui { document, .. } => format!("{document:?}"),
    }
}

/// short label the panel puts before each hit
pub fn label(kind: &BlockKind) -> &'static str {
    match kind {
        BlockKind::UserMessage(_) => "you",
        BlockKind::AssistantMessage(_) => "reply",
        BlockKind::Thinking { .. } => "thought",
        BlockKind::ToolCall { .. } => "tool",
        BlockKind::Swarm { .. } => "agents",
        BlockKind::Status(_) => "status",
        BlockKind::Todos(_) => "todos",
        BlockKind::BgResult { .. } => "btw",
        BlockKind::Diff { .. } => "diff",
        BlockKind::VerifyGate(_) => "unverified",
        BlockKind::AgentNote { .. } => "agent",
        BlockKind::Approval { .. } => "approval",
        BlockKind::Clarify { .. } => "question",
        BlockKind::Command { .. } => "command",
        BlockKind::Confirm { .. } => "confirm",
        BlockKind::ProviderError { .. } => "error",
        BlockKind::Ui { .. } => "ui",
    }
}

/// one line of context centred on the first match, ellipsised at both ends.
/// operates on chars, so a match in non-ASCII text never slices mid-codepoint.
pub fn snippet(text: &str, needle: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = flat.to_lowercase();
    let char_pos = lower
        .find(needle)
        .map(|byte| lower[..byte].chars().count())
        .unwrap_or(0);
    let back = 24.min(char_pos);
    let start = char_pos - back;
    // Never cut the match itself: leading context may not eat the needle.
    let span = max_chars.max(back + needle.chars().count());
    let mut out: String = flat.chars().skip(start).take(span).collect();
    if start > 0 {
        out.insert(0, '\u{2026}');
    }
    if flat.chars().count() > start + span {
        out.push('\u{2026}');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks() -> Vec<TranscriptBlock> {
        vec![
            TranscriptBlock {
                kind: BlockKind::UserMessage("fix the parser".into()),
            },
            TranscriptBlock {
                kind: BlockKind::AssistantMessage("I changed src/parser.rs and reran it".into()),
            },
            TranscriptBlock {
                kind: BlockKind::ToolCall {
                    name: "shell".into(),
                    args: "cargo test".into(),
                    ok: Some(true),
                    call_id: "c1".into(),
                    started: None,
                    duration: None,
                    error: None,
                    tokens: None,
                    expanded: false,
                    output: vec!["parsing 12 records".into()],
                },
            },
        ]
    }

    #[test]
    fn finds_across_block_kinds_case_insensitively() {
        let hits = find(&blocks(), "PARSER");
        assert_eq!(hits.len(), 2, "user text and the assistant path both match");
        assert_eq!(hits[0].label, "you");
        assert_eq!(hits[1].label, "reply");
        let hits = find(&blocks(), "parsing");
        assert_eq!(hits.len(), 1, "shell output is searchable");
        assert_eq!(hits[0].block, 2);
        assert_eq!(hits[0].label, "tool");
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(find(&blocks(), "").is_empty());
        assert!(find(&blocks(), "   ").is_empty());
    }

    #[test]
    fn selection_clamps_and_current_tracks_it() {
        let mut s = SearchState::open();
        s.set_query(&blocks(), "a");
        assert!(s.hits.len() >= 2);
        s.move_sel(-5);
        assert_eq!(s.sel, 0);
        s.move_sel(1000);
        assert_eq!(s.sel, s.hits.len() - 1);
        assert_eq!(s.current(), Some(s.hits[s.hits.len() - 1].block));
    }

    #[test]
    fn no_hits_leaves_selection_at_zero_and_no_current() {
        let mut s = SearchState::open();
        s.set_query(&blocks(), "zzz-not-there");
        s.move_sel(3);
        assert_eq!(s.sel, 0);
        assert_eq!(s.current(), None);
    }

    #[test]
    fn snippet_centres_on_the_match_and_survives_multibyte() {
        let text = "αβγδέ ζηθικ λμνξο αργός κώδικας πλήρης δοκιμών";
        let s = snippet(text, "κώδικας", 30);
        assert!(s.contains("κώδικας"), "{s}");
        let s = snippet("short", "short", 40);
        assert_eq!(s, "short", "no needless ellipsis");
    }

    #[test]
    fn search_reads_diff_lines() {
        let diff = crate::diffview::unified_diff("old line\n", "new line\n");
        let b = vec![TranscriptBlock {
            kind: BlockKind::Diff {
                lines: diff,
                expanded: false,
            },
        }];
        assert_eq!(find(&b, "new line").len(), 1);
    }
}
