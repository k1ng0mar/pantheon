//! Session-end summary, printed when the user quits.
//!
//! Pantheon aesthetic: no box-drawing, dim labels, one amber accent. Two
//! renderings: [`SessionSummary::lines`] for in-TUI use and
//! [`SessionSummary::to_plain_text`] for printing after the alternate
//! screen is left (the quit path prints to the restored terminal).
//!
//! Wiring (pending the concurrent fix pass, which owns `session.rs`): in
//! `session.rs`, in the function that calls `tui_loop` (~line 5404),
//! after `execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;`,
//! add:
//!
//! ```ignore
//! let summary = crate::session_summary::SessionSummary::from_state(&state);
//! print!("{}", summary.to_plain_text());
//! ```
//!
//! Counts come from the transcript blocks: user messages, assistant
//! texts, and tool calls. The `Messages:` total folds all three in, with
//! the breakdown naming user messages and tool calls - the Hermes shape.

use std::time::Duration;

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
    /// Build from live TUI state at shutdown. `start_time.elapsed()` is
    /// the wall-clock session duration.
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
        let label = |t: &str| Span::styled(format!("{t:<11}"), Style::default().fg(th.dim));
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
        ]
    }

    /// Plain text for stdout after the TUI shuts down. Same layout, no
    /// ANSI - the terminal is already restored at that point.
    pub fn to_plain_text(&self) -> String {
        let title = self.title.as_deref().unwrap_or("untitled");
        format!(
            "Resume this session with:\n  pantheon --resume {}\n\n{:<11}{}\n{:<11}{}\n{:<11}{}\n{:<11}{}\n",
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
        assert!(t.contains("Duration:  1h 31m 21s"), "got:\n{t}");
        assert!(
            t.contains("Messages:  163 (3 user, 158 tool calls)"),
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
        assert!(s.to_plain_text().contains("Title:     untitled"));
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
