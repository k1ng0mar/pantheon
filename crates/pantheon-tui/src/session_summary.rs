//! Session-end summary, printed when the user quits.
//!
//! Pantheon aesthetic: no box-drawing, dim labels, one amber accent. Two
//! renderings: [`SessionSummary::lines`] for in-TUI use and
//! [`SessionSummary::to_plain_text`] for printing after the alternate
//! screen is left (the quit path prints to the restored terminal).
//!
//! Message and tool-call totals use the ledger when available, because
//! resumed views do not rebuild every historical tool card and can render
//! saved tool results as assistant blocks. The `Messages:` total folds all
//! three in, with the breakdown naming user messages and tool calls.

use std::time::Duration;

use pantheon_api::events::Event;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::session::theme::Theme;
use crate::session::{BlockKind, TranscriptBlock, TuiState};

/// Count transcript blocks into (user messages, assistant messages, tool
/// calls). Read-only over the transcript: safe to call at shutdown.
pub fn count_blocks(blocks: &[TranscriptBlock]) -> (usize, usize, usize) {
    let mut user = 0;
    let mut assistant = 0;
    let mut tools = 0;
    for b in blocks {
        match &b.kind {
            BlockKind::UserMessage(_) => user += 1,
            BlockKind::AssistantMessage(_) => assistant += 1,
            BlockKind::ToolCall { .. } => tools += 1,
            _ => {}
        }
    }
    (user, assistant, tools)
}

/// Count the effective run history. Ledger replay applies turn rewinds, so
/// hidden turns stay excluded just as they do in the resumed transcript.
pub fn count_entries(entries: &[pantheon_storage::LedgerEntry]) -> (usize, usize, usize) {
    let mut raw_user_messages = 0;
    let mut persisted_user_messages = 0;
    let mut assistant = 0;
    let mut tool_call_ids = std::collections::BTreeSet::new();
    for entry in entries {
        match &entry.event {
            Event::UserMessage { .. } => raw_user_messages += 1,
            Event::AssistantMessage { message, .. } => {
                match message.role {
                    pantheon_api::message::Role::User => persisted_user_messages += 1,
                    pantheon_api::message::Role::Assistant
                        if !message.content.trim().is_empty() =>
                    {
                        assistant += 1;
                    }
                    _ => {}
                }
                tool_call_ids.extend(message.tool_calls.iter().map(|call| call.id.clone()));
            }
            Event::ToolStarted { call_id, .. } => {
                tool_call_ids.insert(call_id.clone());
            }
            _ => {}
        }
    }
    // Live turns record both the raw retry prompt and its replayable message;
    // imported sessions only have the replayable row. Count the larger side
    // so normal turns are not doubled and imports are not omitted.
    (
        raw_user_messages.max(persisted_user_messages),
        assistant,
        tool_call_ids.len(),
    )
}

/// `1h 31m 21s`; `4m 30s`; `45s`.
pub fn fmt_session_duration(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {}m {}s", s / 3600, (s % 3600) / 60, s % 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub title: Option<String>,
    pub duration: Duration,
    pub user_messages: usize,
    pub assistant_messages: usize,
    pub tool_calls: usize,
}

impl SessionSummary {
    /// Build from visible TUI state as a fallback if ledger replay fails.
    pub fn from_state(state: &TuiState) -> Self {
        let (user_messages, assistant_messages, tool_calls) = count_blocks(&state.blocks);
        Self {
            session_id: state.session_id.clone(),
            title: state.title.clone(),
            duration: state.start_time.elapsed(),
            user_messages,
            assistant_messages,
            tool_calls,
        }
    }

    /// Build from durable run counts and live TUI metadata.
    pub fn from_state_with_counts(
        state: &TuiState,
        (user_messages, assistant_messages, tool_calls): (usize, usize, usize),
    ) -> Self {
        let mut summary = Self::from_state(state);
        summary.user_messages = user_messages;
        summary.assistant_messages = assistant_messages;
        summary.tool_calls = tool_calls;
        summary
    }

    /// Build the goodbye summary from the effective durable history. The
    /// elapsed time begins at the first retained ledger event, so resuming a
    /// run does not reset its displayed duration to the current TUI visit.
    pub fn from_state_with_entries(
        state: &TuiState,
        entries: &[pantheon_storage::LedgerEntry],
    ) -> Self {
        let mut summary = Self::from_state_with_counts(state, count_entries(entries));
        if let Some(first_event_ms) = entries.iter().map(|e| e.ts_ms).filter(|ts| *ts > 0).min() {
            let elapsed_ms = pantheon_api::logging::now_ms()
                .saturating_sub(first_event_ms)
                .max(0) as u64;
            summary.duration = Duration::from_millis(elapsed_ms);
        }
        summary
    }

    pub fn total_messages(&self) -> usize {
        self.user_messages + self.assistant_messages + self.tool_calls
    }

    /// `Messages:  163 (3 user, 158 tool calls)` - labels padded to the
    /// same column as the reference.
    fn messages_value(&self) -> String {
        format!(
            "{} ({} user, {} tool calls)",
            self.total_messages(),
            self.user_messages,
            self.tool_calls
        )
    }

    /// Styled lines for in-TUI rendering. Labels dim, values body text;
    /// the resume command in the code color; the "Resume" header amber.
    pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
        let label = |t: &str| Span::styled(format!("{t:<16}"), Style::default().fg(th.dim));
        let value = |t: String| Span::styled(t, Style::default().fg(th.body));
        vec![
            Line::from(Span::styled(
                "Resume this session with:",
                Style::default()
                    .fg(th.emphasis)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("  pantheon --resume {}", self.session_id),
                Style::default().fg(th.code),
            )),
            Line::from(Span::styled(
                format!("  pantheon -r {} \"your next message\"", self.session_id),
                Style::default().fg(th.code),
            )),
            Line::from(Span::styled(
                "  The conversation transcript will reopen with the session.",
                Style::default().fg(th.dim),
            )),
            Line::from(""),
            Line::from(vec![label("Session:"), value(self.session_id.clone())]),
            Line::from(vec![
                label("Title:"),
                value(self.title.clone().unwrap_or_else(|| "untitled".to_string())),
            ]),
            Line::from(vec![
                label("Duration:"),
                value(fmt_session_duration(self.duration)),
            ]),
            Line::from(vec![label("Messages:"), value(self.messages_value())]),
            Line::from(""),
            Line::from(Span::styled(
                "Goodbye. Your session is saved.",
                Style::default().fg(th.dim),
            )),
        ]
    }

    /// Plain text for stdout after the TUI shuts down. Same layout, no
    /// ANSI - the terminal is already restored at that point.
    pub fn to_plain_text(&self) -> String {
        let title = self.title.as_deref().unwrap_or("untitled");
        format!(
            "Resume this session with:\n  pantheon --resume {}\n  pantheon -r {} \"your next message\"\n  The conversation transcript will reopen with the session.\n\n{:<16}{}\n{:<16}{}\n{:<16}{}\n{:<16}{}\n\nGoodbye. Your session is saved.\n",
            self.session_id,
            self.session_id,
            "Session:",
            self.session_id,
            "Title:",
            title,
            "Duration:",
            fmt_session_duration(self.duration),
            "Messages:",
            self.messages_value(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> SessionSummary {
        SessionSummary {
            session_id: "abc123".to_string(),
            title: Some("retry helper".to_string()),
            duration: Duration::from_secs(5481),
            user_messages: 3,
            assistant_messages: 2,
            tool_calls: 158,
        }
    }

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn fmt_session_duration_shapes() {
        assert_eq!(
            fmt_session_duration(Duration::from_secs(5481)),
            "1h 31m 21s"
        );
        assert_eq!(fmt_session_duration(Duration::from_secs(270)), "4m 30s");
        assert_eq!(fmt_session_duration(Duration::from_secs(45)), "45s");
    }

    #[test]
    fn plain_text_matches_reference_layout() {
        let t = summary().to_plain_text();
        assert!(t.contains("Resume this session with:"), "got:\n{t}");
        assert!(t.contains("pantheon --resume abc123"), "got:\n{t}");
        assert!(t.contains("Duration:       1h 31m 21s"), "got:\n{t}");
        assert!(
            t.contains("Messages:       163 (3 user, 158 tool calls)"),
            "got:\n{t}"
        );
    }

    #[test]
    fn styled_lines_carry_same_content() {
        let t = text(&summary().lines(&Theme::pantheon()));
        assert!(t.contains("pantheon --resume abc123"), "got:\n{t}");
        assert!(t.contains("retry helper"), "got:\n{t}");
        assert!(t.contains("1h 31m 21s"), "got:\n{t}");
    }

    #[test]
    fn untitled_when_no_title() {
        let mut s = summary();
        s.title = None;
        assert!(s.to_plain_text().contains("Title:          untitled"));
    }

    #[test]
    fn count_blocks_splits_kinds() {
        use crate::session::BlockKind;
        let blocks = vec![
            TranscriptBlock {
                kind: BlockKind::UserMessage("hi".to_string()),
            },
            TranscriptBlock {
                kind: BlockKind::AssistantMessage("hello".to_string()),
            },
            TranscriptBlock {
                kind: BlockKind::Status("x".to_string()),
            },
        ];
        assert_eq!(count_blocks(&blocks), (1, 1, 0));
    }
}
