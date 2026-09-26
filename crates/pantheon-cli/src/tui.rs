//! Pantheon TUI: the agent cockpit.
//!
//! Visual blocks for each event type, persistent header, status bar.
//! Uses ratatui + crossterm. Falls back to plain streaming output if the
//! terminal doesn't support alternate screen mode.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Paragraph},
    DefaultTerminal, Frame,
};

use pantheon_core::events::Event as RuntimeErrorEvent;
use pantheon_core::model_event::ModelEvent;
use pantheon_runtime::session::Session;

/// A single block in the conversation transcript.
#[derive(Debug, Clone)]
pub enum BlockKind {
    UserMessage(String),
    AssistantMessage(String),
    Thinking(String),
    /// Tool card: running until the matching runtime completion event
    /// sets `ok`. Args are shown inline.
    ToolCall {
        name: String,
        args: String,
        ok: Option<bool>,
    },
    Swarm {
        agents: u32,
        task: String,
    },
    Status(String),
}

#[derive(Debug, Clone)]
pub struct TranscriptBlock {
    pub kind: BlockKind,
}

/// Runtime state for the TUI session.
pub struct TuiState {
    pub session_id: String,
    pub model: String,
    pub elapsed: Duration,
    pub start_time: Instant,
    /// Authoritative cumulative tokens (snapped on each Usage event).
    pub tokens_used: u32,
    /// Live estimate of THIS turn's streamed tokens (chars / 4).
    /// Added to tokens_used for the live counter; snapped away when Usage lands.
    pub turn_estimate: u32,
    pub tokens_max: u32,
    pub cost_cents: u32,
    pub blocks: Vec<TranscriptBlock>,
    pub input: String,
    pub scroll_offset: usize,
    pub ready: bool,
    pub shutting_down: bool,
    pub status_line: String,
    pub is_inputting: bool,
    /// Set when a run parks on approval: (run_id, scope/call_id).
    /// Drives the permission card; cleared by grant/deny.
    pub pending_approval: Option<(String, String)>,
    /// Last tool that started (name, args) — shown in the permission card
    /// because ApprovalRequested only carries the opaque call id.
    pub last_tool: Option<(String, String)>,
    /// Double-Esc state: None = not interrupting, Some(t) = armed at t.
    /// The second Esc inside the window actually cancels the run.
    pub interrupt_armed_at: Option<Instant>,
    /// True once the user confirmed; shows the canceled card.
    pub interrupted: bool,
    /// Open session-history overlay: Some(runs) while /history is open.
    /// runs: (run_id, status, created_ms, title), newest first.
    pub history: Option<Vec<pantheon_storage::RunListing>>,
    /// Live filter typed into the history overlay.
    pub history_input: String,
    /// Selected index into the filtered history list.
    pub history_sel: usize,
    /// The conversation's current title: set by the title auxiliary
    /// (SessionTitled), by /name, and refreshed when resuming a run.
    pub title: Option<String>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            model: String::new(),
            elapsed: Duration::ZERO,
            start_time: Instant::now(),
            tokens_used: 0,
            turn_estimate: 0,
            tokens_max: 0,
            cost_cents: 0,
            blocks: Vec::new(),
            input: String::new(),
            scroll_offset: 0,
            ready: false,
            shutting_down: false,
            status_line: String::from("ready"),
            is_inputting: false,
            pending_approval: None,
            last_tool: None,
            interrupt_armed_at: None,
            interrupted: false,
            history: None,
            history_input: String::new(),
            history_sel: 0,
            title: None,
        }
    }
}

impl TuiState {
    fn new(session_id: String, model: String, tokens_max: u32) -> Self {
        Self {
            session_id,
            model,
            elapsed: Duration::ZERO,
            start_time: Instant::now(),
            tokens_used: 0,
            turn_estimate: 0,
            tokens_max,
            cost_cents: 0,
            blocks: Vec::new(),
            input: String::new(),
            scroll_offset: 0,
            ready: true,
            shutting_down: false,
            status_line: String::from("ready"),
            is_inputting: false,
            pending_approval: None,
            last_tool: None,
            interrupt_armed_at: None,
            interrupted: false,
            history: None,
            history_input: String::new(),
            history_sel: 0,
            title: None,
        }
    }

    /// Runs matching the current filter, in list order.
    pub fn filtered_history(&self) -> Vec<pantheon_storage::RunListing> {
        match &self.history {
            None => Vec::new(),
            Some(runs) => {
                let f = self.history_input.to_lowercase();
                if f.is_empty() {
                    runs.clone()
                } else {
                    runs.iter()
                        .filter(|(id, status, _, title)| {
                            id.to_lowercase().contains(&f)
                                || status.to_lowercase().contains(&f)
                                || title
                                    .as_deref()
                                    .map(|t| t.to_lowercase().contains(&f))
                                    .unwrap_or(false)
                        })
                        .cloned()
                        .collect()
                }
            }
        }
    }

    /// Move the overlay selection by n, clamped to the filtered list.
    pub fn history_move(&mut self, n: isize) {
        let len = self.filtered_history().len();
        if len == 0 {
            self.history_sel = 0;
            return;
        }
        let sel = self.history_sel as isize + n;
        self.history_sel = sel.clamp(0, len as isize - 1) as usize;
    }

    /// Close the overlay and reset its state.
    pub fn history_close(&mut self) {
        self.history = None;
        self.history_input.clear();
        self.history_sel = 0;
    }

    /// Live token estimate: ~4 chars per token, updated on every streamed
    /// delta so the counter ticks like Claude Code's. Snapped to the
    /// authoritative number when Usage arrives.
    fn bump_estimate(&mut self, text: &str) {
        self.turn_estimate += (text.len() as u32 + 3) / 4;
    }

    /// Process a model event into a transcript block or state update.
    pub fn handle_model_event(&mut self, ev: ModelEvent) {
        match ev {
            ModelEvent::Attempt {
                provider, model, ..
            } => {
                // Track mid-session model switches (fallback chain).
                if self.model != model {
                    self.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(format!(
                            "routing: {} unavailable, falling back to {provider}/{model}",
                            self.model
                        )),
                    });
                    self.model = model;
                }
            }
            ModelEvent::TextDelta { text } => {
                self.bump_estimate(&text);
                if let Some(TranscriptBlock {
                    kind: BlockKind::AssistantMessage(ref mut buf),
                    ..
                }) = self.blocks.last_mut()
                {
                    buf.push_str(&text);
                } else {
                    self.blocks.push(TranscriptBlock {
                        kind: BlockKind::AssistantMessage(text),
                    });
                }
            }
            ModelEvent::ReasoningDelta { text } => {
                self.bump_estimate(&text);
                if let Some(TranscriptBlock {
                    kind: BlockKind::Thinking(ref mut t),
                    ..
                }) = self.blocks.last_mut()
                {
                    t.push_str(&text);
                } else {
                    self.blocks.push(TranscriptBlock {
                        kind: BlockKind::Thinking(text),
                    });
                }
            }
            ModelEvent::ToolCall {
                name, arguments, ..
            } => {
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::ToolCall {
                        name,
                        args: arguments,
                        ok: None,
                    },
                });
            }
            ModelEvent::Usage { usage } => {
                // Snap: fold the live estimate into the authoritative count.
                self.tokens_used = usage.total_tokens as u32;
                self.turn_estimate = 0;
                if let Some(cost) = usage.cost_usd {
                    self.cost_cents = (cost * 100.0) as u32;
                }
            }
            _ => {}
        }
    }

    /// Process a runtime event into a transcript block.
    pub fn handle_runtime_event(&mut self, ev: &RuntimeErrorEvent) {
        match ev {
            RuntimeErrorEvent::ToolStarted { tool, args, .. } => {
                self.last_tool = Some((tool.clone(), args.clone()));
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::ToolCall {
                        name: tool.clone(),
                        args: args.clone(),
                        ok: None,
                    },
                });
            }
            RuntimeErrorEvent::ToolCompleted { tool, .. } => {
                // Flip the most recent still-running card for this tool.
                if let Some(block) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|b| matches!(&b.kind, BlockKind::ToolCall { name, ok: None, .. } if name == tool))
                {
                    if let BlockKind::ToolCall { ok, .. } = &mut block.kind {
                        *ok = Some(true);
                    }
                }
            }
            RuntimeErrorEvent::ToolOutput {
                tool: _,
                truncated: _,
                ..
            } => {
                // Output content rides the ToolMessage into the transcript;
                // the card itself flips on ToolCompleted.
            }
            RuntimeErrorEvent::ApprovalRequested { run_id, scope } => {
                self.pending_approval = Some((run_id.clone(), scope.clone()));
                self.status_line = "permission required".into();
            }
            RuntimeErrorEvent::RunProgress { detail, .. } => {
                self.status_line = detail.clone();
            }
            RuntimeErrorEvent::AgentSpawned { agent, .. } => {
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::Swarm {
                        agents: 1,
                        task: agent.clone(),
                    },
                });
            }
            // Title generation (aux or /name) lands as a durable event;
            // the header follows it with no polling.
            RuntimeErrorEvent::SessionTitled { title, .. } => {
                self.title = Some(title.clone());
            }
            _ => {}
        }
    }

    pub fn add_user_message(&mut self, text: String) {
        self.blocks.push(TranscriptBlock {
            kind: BlockKind::UserMessage(text),
        });
        self.scroll_to_bottom();
    }

    pub fn add_status(&mut self, text: String) {
        self.blocks.push(TranscriptBlock {
            kind: BlockKind::Status(text),
        });
        self.scroll_to_bottom();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }

    pub fn tick(&mut self) {
        self.elapsed = self.start_time.elapsed();
    }
}

/// Icons for transcript cards and the status bar. Every entry is used
/// by `render_block` or `render_status` below — no speculative glyphs.
mod icon {
    pub const PANTHEON: &str = "◈";
    pub const RUNNING: &str = "●";
    pub const SUCCESS: &str = "✓";
    pub const WARNING: &str = "!";
    pub const FAILURE: &str = "×";
    pub const THINKING: &str = "◇";
    pub const TOOL: &str = "⚙";
    pub const AGENT: &str = "→";
}

/// Color helpers.
mod color {
    use ratatui::style::Color;
    pub const PRIMARY: Color = Color::Cyan;
    pub const RUNNING: Color = Color::Yellow;
    pub const SUCCESS: Color = Color::Green;
    pub const WARNING: Color = Color::Yellow;
    pub const FAILURE: Color = Color::Red;
}

/// Render the full TUI frame: header, transcript, input, status bar.
pub fn render(state: &TuiState, f: &mut Frame) {
    let outer = Layout::vertical([
        Constraint::Length(3), // header
        Constraint::Min(1),    // transcript
        Constraint::Length(3), // input
        Constraint::Length(1), // status bar
    ]);
    let [header_area, chat_area, input_area, status_area] = outer.areas(f.area());

    if state.history.is_some() {
        render_history(f, f.area(), state);
        return;
    }
    if state.pending_approval.is_some() {
        // Steal the input row: permission card replaces it until resolved.
        render_permission(f, input_area, state);
    } else {
        render_input(f, input_area, state);
    }
    render_header(f, header_area, state);
    render_transcript(f, chat_area, state);
    render_status(f, status_area, state);
}

/// Searchable scrollable history overlay: /history. Type-to-filter,
/// Up/Down to move, Enter to resume, Esc to close. Mirrors the REPL's
/// /history picker, rendered as a centered list.
fn render_history(f: &mut Frame, area: Rect, state: &TuiState) {
    let w = area.width.min(80).max(40);
    let h = area.height.min(20).max(7);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let runs = state.filtered_history();
    let mut lines = vec![
        Line::from(Span::styled(
            "  type to filter, Up/Down to move, Enter to resume, Esc to close",
            Style::default().fg(color::PRIMARY),
        )),
        Line::from(""),
    ];
    if runs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(color::FAILURE),
        )));
    }
    for (i, (id, status, ts, title)) in runs.iter().enumerate() {
        let glyph = match status.as_str() {
            "completed" => "\u{2713}",
            "failed" => "\u{d7}",
            "running" => "\u{25cf}",
            _ => "\u{25d0}",
        };
        let short: String = id.chars().skip(4).take(8).collect();
        // The generated session title leads; untitled runs fall back to
        // the run id stem so the row still reads as an identity.
        let label = match title.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => t.chars().take(38).collect::<String>(),
            None => short.clone(),
        };
        let style = if i == state.history_sel {
            Style::default()
                .fg(color::PRIMARY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(
                "  {:>3}  {glyph} {:<10} {:<38} {}",
                i,
                status,
                label,
                fmt_age(*ts)
            ),
            style,
        )));
    }
    if !state.history_input.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("  filter: {}", state.history_input)));
    }
    let title = " conversations (/history) ";
    let card = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(color::PRIMARY))
            .title(Span::styled(
                title,
                Style::default()
                    .fg(color::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, area);
}

/// Local-relative age for the history overlay.
fn fmt_age(ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let s = (now - ms).max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86400 {
        format!("{}h", s / 3600)
    } else {
        format!("{}d", s / 86400)
    }
}

/// Permission-required card. Shown when a run parked on approval.
fn render_permission(f: &mut Frame, area: Rect, state: &TuiState) {
    let (_, scope) = state
        .pending_approval
        .as_ref()
        .map(|(r, s)| (r.as_str(), s.as_str()))
        .unwrap_or(("", ""));
    let (tool_name, tool_args) = state
        .last_tool
        .as_ref()
        .map(|(n, a)| (n.as_str(), a.as_str()))
        .unwrap_or(("unknown tool", ""));
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Pantheon wants to run a gated operation.",
            Style::default()
                .fg(color::WARNING)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  tool: {tool_name}"),
            Style::default().fg(color::WARNING),
        )),
    ];
    if !tool_args.is_empty() {
        for line in tool_args.lines().take(3) {
            text.push(Line::from(format!("  args: {line}")));
        }
    }
    text.extend([
        Line::from(format!("  scope: {scope}")),
        Line::from(""),
        Line::from(Span::styled(
            "  [y] Allow    [n] Deny",
            Style::default().fg(color::SUCCESS),
        )),
    ]);
    let card = Paragraph::new(text).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(color::WARNING))
            .title(Span::styled(
                " \u{26a0} Permission required ",
                Style::default()
                    .fg(color::WARNING)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, area);
}

/// Draw the input box at the bottom.
fn render_input(f: &mut Frame, area: Rect, state: &TuiState) {
    let cursor = if state.is_inputting { "_" } else { " " };
    let line = format!("› {}{}", state.input, cursor);
    let input = Paragraph::new(line).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Input ", Style::default().fg(color::PRIMARY))),
    );
    f.render_widget(input, area);
}

/// Draw the one-line status bar under the input.
fn render_status(f: &mut Frame, area: Rect, state: &TuiState) {
    // Interrupt state takes over the status word: an armed interrupt is a
    // call to action, a settled one reports the truth.
    let (status_icon, status_color, status_word) = if state.interrupted {
        ("\u{25CB}".to_string(), color::WARNING, "interrupted")
    } else if !state.ready && state.interrupt_armed_at.is_some() {
        (
            icon::WARNING.to_string(),
            color::WARNING,
            "esc to interrupt",
        )
    } else if state.ready {
        (icon::SUCCESS.to_string(), color::SUCCESS, "ready")
    } else {
        (icon::RUNNING.to_string(), color::RUNNING, "working")
    };
    let live_tokens = state.tokens_used + state.turn_estimate;
    let ctx = if state.tokens_max > 0 {
        format!(
            "{:.1}k/{}k",
            live_tokens as f64 / 1000.0,
            state.tokens_max / 1000
        )
    } else {
        format!("{:.1}k", live_tokens as f64 / 1000.0)
    };
    let secs = state.elapsed.as_secs();
    let text = format!(
        "{} {}  │  {}  │  {}  │  {:02}:{:02}:{:02}  │  session {}",
        status_icon,
        status_word,
        state.model,
        ctx,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        &state.session_id[..state.session_id.len().min(4)],
    );
    let bar = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(status_color),
    )));
    f.render_widget(bar, area);
}

/// Draw the persistent top header bar.
fn render_header(f: &mut Frame, area: Rect, state: &TuiState) {
    let tokens_display = if state.tokens_max > 0 {
        format!(
            "{:.1}k/{}k",
            state.tokens_used as f64 / 1000.0,
            state.tokens_max / 1000
        )
    } else {
        format!("{:.1}k", state.tokens_used as f64 / 1000.0)
    };
    let session_label = match state.title.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!(
            "{} \u{2022} {}",
            &state.session_id[..state.session_id.len().min(6)],
            t.chars().take(40).collect::<String>()
        ),
        None => state.session_id[..state.session_id.len().min(6)].to_string(),
    };
    let title = format!(
        "◈ PANTHEON  {}  {}  {:02}:{:02}:{:02}  {}  ${}",
        session_label,
        state.model,
        state.elapsed.as_secs() / 3600,
        (state.elapsed.as_secs() % 3600) / 60,
        state.elapsed.as_secs() % 60,
        tokens_display,
        format!("{:.2}", state.cost_cents as f64 / 100.0),
    );
    let header = Paragraph::new(Line::from(Span::styled(
        title,
        Style::default()
            .fg(color::PRIMARY)
            .add_modifier(Modifier::BOLD),
    )))
    .block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(" PANTHEON "),
    );
    f.render_widget(header, area);
}

/// Draw the conversation transcript with visual blocks per event type.
fn render_transcript(f: &mut Frame, area: Rect, state: &TuiState) {
    if state.blocks.is_empty() {
        let welcome = Paragraph::new(vec![
            Line::from(""),
            Line::from(Span::styled(
                "◈ PANTHEON",
                Style::default()
                    .fg(color::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("Ask me anything. /help for commands."),
        ])
        .block(Block::bordered().title(" Transcript "));
        f.render_widget(welcome, area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    let skip = state.scroll_offset;

    // Render from oldest visible to newest.
    let start = state.blocks.len().saturating_sub(skip + 1);
    let block_count = state.blocks.len();
    for (i, block) in state.blocks.iter().rev().skip(start).rev().enumerate() {
        let is_last = i + start + 1 == block_count;
        render_block(&mut lines, block, is_last, state.interrupted);
        lines.push(Line::from(""));
    }

    let text = Text::from(lines);
    let para = Paragraph::new(text)
        .block(Block::bordered().title(" Transcript "))
        .scroll((state.scroll_offset as u16, 0));
    f.render_widget(para, area);
}

/// Render a single transcript block as Lines. `is_last` marks the streaming
/// head: thinking blocks stay expanded while they are the live block and
/// collapse to a summary line once anything else lands after them.
fn render_block(lines: &mut Vec<Line>, block: &TranscriptBlock, is_last: bool, interrupted: bool) {
    match &block.kind {
        BlockKind::UserMessage(text) => {
            lines.push(Line::from(Span::styled(
                "╭─ You ──",
                Style::default()
                    .fg(color::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            )));
            for line in text.lines() {
                lines.push(Line::from(format!("│  {line}")));
            }
        }
        BlockKind::AssistantMessage(text) => {
            lines.push(Line::from(Span::styled(
                format!("╭─ {} Pantheon ──", icon::PANTHEON),
                Style::default()
                    .fg(color::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            )));
            for line in text.lines() {
                lines.push(Line::from(format!("│  {line}")));
            }
        }
        BlockKind::Thinking(text) => {
            if is_last {
                lines.push(Line::from(Span::styled(
                    format!("┌─ {} Thinking ──", icon::THINKING),
                    Style::default().fg(color::WARNING),
                )));
                for line in text.lines().take(40) {
                    lines.push(Line::from(format!("│  {line}")));
                }
            } else {
                // Collapsed summary: first line of the thought + size hint.
                let words = text.split_whitespace().count();
                let summary = text
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>();
                lines.push(Line::from(Span::styled(
                    format!("◇ Thought · {words} words · {summary}..."),
                    Style::default().fg(color::WARNING),
                )));
            }
        }
        BlockKind::ToolCall { name, args, ok } => {
            let (glyph, col) = match ok {
                // A tool still marked running after an interrupt was stopped
                // from the outside: show that honestly instead of spinning.
                None if interrupted => ("■", color::WARNING),
                None => (icon::RUNNING, color::RUNNING),
                Some(true) => (icon::SUCCESS, color::SUCCESS),
                Some(false) => (icon::FAILURE, color::FAILURE),
            };
            lines.push(Line::from(Span::styled(
                format!("┌─ {} {name} ── {glyph}", icon::TOOL),
                Style::default().fg(col),
            )));
            if !args.is_empty() {
                for line in args.lines().take(6) {
                    lines.push(Line::from(format!("│  {line}")));
                }
            }
        }
        BlockKind::Swarm { agents, task } => {
            lines.push(Line::from(Span::styled(
                format!("┌─{} Swarm · {agents} agents", icon::AGENT),
                Style::default().fg(color::PRIMARY),
            )));
            lines.push(Line::from(format!("│  {task}")));
        }
        BlockKind::Status(text) => {
            lines.push(Line::from(Span::styled(
                format!("… {text}"),
                Style::default().fg(color::WARNING),
            )));
        }
    }
}

/// Internal event types that flow from the worker thread to the TUI loop.
enum TuiEvent {
    Model(pantheon_core::model_event::ModelEvent),
    Runtime(pantheon_core::events::Event),
    TurnComplete,
    Error(String),
    /// The run stopped because the user interrupted it (not a failure).
    Canceled,
}

/// Entry point for the Pantheon agent cockpit TUI.
pub fn run_tui_session() -> Result<(), Box<dyn std::error::Error>> {
    use crate::config_doc;
    use crate::session_cli::build_model_policy;
    use std::sync::mpsc;

    let file_cfg = config_doc::Config::load(&crate::data_dir()).ok();
    let model_policy = build_model_policy(&file_cfg, None, None);
    let allow_memory = file_cfg
        .as_ref()
        .map(|c| c.policy == Some(crate::config_schema::PolicyPreset::CoderMemory))
        .unwrap_or_else(|| {
            std::env::var("PANTHEON_ALLOW_MEMORY")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false)
        });
    let policy = if allow_memory {
        pantheon_core::capability::Policy::coder_with_memory()
    } else {
        pantheon_core::capability::Policy::coder()
    };
    let secrets = config_doc::chat_secrets(file_cfg.as_ref());

    let mut session = match Session::new(crate::data_dir(), policy, model_policy, secrets) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open session: {e}");
            std::process::exit(1);
        }
    };

    let (tx, rx) = mpsc::channel::<TuiEvent>();
    let running = Arc::new(AtomicBool::new(true));

    {
        let r = running.clone();
        ctrlc::set_handler(move || {
            r.store(false, Ordering::SeqCst);
        })
        .ok();
    }

    session.on_event = Some(Box::new({
        let tx = tx.clone();
        move |ev| {
            let _ = tx.send(TuiEvent::Model(ev));
        }
    }));

    // Runtime events (tool lifecycle, approvals, progress) flow through the
    // supervisor observer into the same channel. The guard is kept alive
    // until the TUI exits by leaking it — the process is shutting down anyway.
    let obs_tx = tx.clone();
    let _observer_guard = session.supervisor.register_observer(Arc::new(move |ev| {
        let _ = obs_tx.send(TuiEvent::Runtime(ev.clone()));
    }));

    let session = Arc::new(session);

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(stdout))?;

    let mut state = TuiState::new(
        pantheon_runtime::new_run_id(),
        "opus-4.1".to_string(),
        200_000,
    );
    state.tokens_max = 200_000;

    let run_id = pantheon_runtime::new_run_id();
    let _ = session.supervisor.start_run(&run_id);

    let result = tui_loop(
        &mut terminal,
        &mut state,
        session,
        &tx,
        &run_id,
        &rx,
        &running,
    );

    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(result?)
}

fn tui_loop(
    terminal: &mut DefaultTerminal,
    state: &mut TuiState,
    session: Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    run_id: &str,
    rx: &std::sync::mpsc::Receiver<TuiEvent>,
    running: &Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        if !running.load(Ordering::SeqCst) || state.shutting_down {
            break;
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                TuiEvent::Model(me) => state.handle_model_event(me),
                TuiEvent::Runtime(re) => state.handle_runtime_event(&re),
                TuiEvent::TurnComplete => {
                    state.ready = true;
                    state.status_line = "ready".to_string();
                    state.interrupt_armed_at = None;
                    state.interrupted = false;
                    session.reset_cancel();
                }
                TuiEvent::Error(msg) => {
                    state.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(format!("error: {msg}")),
                    });
                    state.ready = true;
                }
                TuiEvent::Canceled => {
                    // Honest report: the run was stopped by the user, and the
                    // ledger holds the partial transcript so it can be resumed.
                    state.ready = true;
                    state.interrupt_armed_at = None;
                    state.status_line = "interrupted".to_string();
                    state.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(
                            "interrupted \u{2014} run stopped, transcript saved".to_string(),
                        ),
                    });
                    // Clear the token so the next turn starts clean.
                    session.reset_cancel();
                }
            }
        }

        state.tick();
        terminal.draw(|f| render(state, f))?;

        if event::poll(Duration::from_millis(100))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if state.history.is_some() {
                    match key.code {
                        KeyCode::Char(c) => state.history_input.push(c),
                        KeyCode::Backspace => {
                            state.history_input.pop();
                        }
                        KeyCode::Up => state.history_move(-1),
                        KeyCode::Down => state.history_move(1),
                        KeyCode::Esc => state.history_close(),
                        KeyCode::Enter => {
                            let sel = state.history_sel;
                            let target = state.filtered_history().get(sel).map(|r| r.0.clone());
                            state.history_close();
                            if let Some(id) = target {
                                // Switch the run: reopen a terminal run and
                                // rebuild the transcript from the ledger.
                                if id != run_id {
                                    let _ = session.supervisor.ledger_reopen_run(&id);
                                    if let Ok(entries) = session.supervisor.replay(&id) {
                                        state.blocks.clear();
                                        state.session_id = id.clone();
                                        state.title =
                                            session.supervisor.ledger_title(&id).ok().flatten();
                                        for m in
                                            pantheon_runtime::session::rebuild_messages(entries)
                                        {
                                            let kind = match m.role {
                                                pantheon_core::message::Role::User => {
                                                    BlockKind::UserMessage(m.content.clone())
                                                }
                                                _ => BlockKind::AssistantMessage(m.content.clone()),
                                            };
                                            state.blocks.push(TranscriptBlock { kind });
                                        }
                                        state.scroll_to_bottom();
                                    }
                                    state.blocks.push(TranscriptBlock {
                                        kind: BlockKind::Status(format!("resumed {}", id)),
                                    });
                                    state.ready = true;
                                }
                            }
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                match key.code {
                    KeyCode::Char(c) => {
                        if state.pending_approval.is_some() {
                            // Permission card: y allow, n deny, Esc deny.
                            match c {
                                'y' | 'Y' => {
                                    if let Some((run, scope)) = state.pending_approval.take() {
                                        let _ = session.supervisor.grant(&run, &scope);
                                        state.status_line = "granted; resuming".into();
                                        state.ready = false;
                                        // Resume: chat_turn with empty message rebuilds
                                        // from the ledger and continues the loop.
                                        let tx3 = tx.clone();
                                        let run3 = run.clone();
                                        let sess3 = session.clone();
                                        std::thread::spawn(move || {
                                            match sess3.chat_turn(&run3, "", "") {
                                                Ok(_) => {
                                                    let _ = tx3.send(TuiEvent::TurnComplete);
                                                }
                                                Err(e) => {
                                                    let _ =
                                                        tx3.send(TuiEvent::Error(e.to_string()));
                                                }
                                            }
                                        });
                                    }
                                }
                                'n' | 'N' => {
                                    if let Some((run, scope)) = state.pending_approval.take() {
                                        let _ = session.supervisor.deny(&run, &scope);
                                        state.status_line = "denied".into();
                                        state.ready = true;
                                    }
                                }
                                _ => {}
                            }
                        } else if state.is_inputting {
                            state.input.push(c);
                        } else {
                            match c {
                                'q' | 'Q' => break,
                                // Start typing into the input box on the first
                                // printable char. Without this the box never
                                // becomes active, so slash commands never
                                // reached the handler.
                                _ => {
                                    state.is_inputting = true;
                                    state.input.push(c);
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if state.is_inputting {
                            state.input.pop();
                        }
                    }
                    KeyCode::Enter => {
                        if state.is_inputting {
                            let msg = state.input.trim().to_string();
                            state.input.clear();
                            state.is_inputting = false;
                            if msg.is_empty() {
                                continue;
                            }

                            if msg.starts_with('/') {
                                handle_slash(state, &session.supervisor, &msg);
                                continue;
                            }

                            state.add_user_message(msg.clone());

                            state.ready = false;
                            state.status_line = "working".to_string();
                            state.interrupt_armed_at = None;
                            state.interrupted = false;
                            session.reset_cancel();

                            let tx2 = tx.clone();
                            let run_id_owned = run_id.to_string();
                            let msg_owned = msg.clone();
                            let session_owned = session.clone();
                            std::thread::spawn(move || {
                                match session_owned.chat(&run_id_owned, &msg_owned) {
                                    Ok(outcome) => match outcome {
                                        pantheon_agent::LoopOutcome::AwaitingApproval {
                                            ..
                                        } => {
                                            // The ApprovalRequested runtime event
                                            // (via the observer) carries scope; the
                                            // worker just marks the turn parked.
                                            let _ = tx2.send(TuiEvent::TurnComplete);
                                        }
                                        pantheon_agent::LoopOutcome::Canceled { .. } => {
                                            let _ = tx2.send(TuiEvent::Canceled);
                                        }
                                        _ => {
                                            let _ = tx2.send(TuiEvent::TurnComplete);
                                        }
                                    },
                                    Err(e) => {
                                        let _ = tx2.send(TuiEvent::Error(e.to_string()));
                                    }
                                }
                            });
                        } else {
                            state.is_inputting = true;
                        }
                    }
                    KeyCode::Esc => {
                        state.is_inputting = false;
                        // Double-Esc interrupts the active run. First Esc arms,
                        // second Esc inside the window cancels for real.
                        if !state.ready && !state.interrupted {
                            const ARM_WINDOW: Duration = Duration::from_millis(1500);
                            match state.interrupt_armed_at {
                                Some(t) if t.elapsed() < ARM_WINDOW => {
                                    state.interrupt_armed_at = None;
                                    state.interrupted = true;
                                    state.status_line = "interrupting\u{2026}".into();
                                    session.cancel_current_run(run_id, "user pressed esc twice");
                                }
                                _ => {
                                    state.interrupt_armed_at = Some(Instant::now());
                                    state.status_line = "press esc again to interrupt".into();
                                }
                            }
                        } else if state.interrupt_armed_at.is_some() {
                            // Disarm if the run finished before the second Esc.
                            state.interrupt_armed_at = None;
                            state.status_line = "ready".into();
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn handle_slash(state: &mut TuiState, supervisor: &pantheon_runtime::Supervisor, cmd: &str) {
    // /help shows the real command surface.
    if cmd == "/help" {
        state.add_status("commands:".into());
        state.add_status("  /help              this list".into());
        state.add_status("  /runs [N]          recent runs (default 10)".into());
        state.add_status(
            "  /history           interactive searchable history (pick + resume)".into(),
        );
        state.add_status("  /resume [ID]       resume a run by id".into());
        state
            .add_status("  /name [TITLE]      show this conversation's title, or rename it".into());
        state.add_status("  /status <run_id>   run status line".into());
        state.add_status("  /cost              tokens and cost this session".into());
        state.add_status("  /clear             clear visible transcript".into());
        state.add_status("  /exit, /quit       leave pantheon".into());
        return;
    }
    if cmd == "/exit" || cmd == "/quit" {
        state.shutting_down = true;
        return;
    }
    if cmd == "/clear" {
        state.blocks.clear();
        return;
    }
    if cmd == "/cost" {
        state.add_status(format!(
            "tokens {} \u{2022} cost ${:.2} \u{2022} ctx {:.1}k/{}k",
            state.tokens_used + state.turn_estimate,
            state.cost_cents as f64 / 100.0,
            (state.tokens_used + state.turn_estimate) as f64 / 1000.0,
            state.tokens_max / 1000
        ));
        return;
    }
    if cmd == "/name" || cmd.starts_with("/name ") {
        let rest = cmd.strip_prefix("/name").unwrap_or("").trim();
        if rest.is_empty() {
            match state.title.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => state.add_status(format!("title: {t}")),
                None => state.add_status("(untitled)".into()),
            }
            state.add_status("rename with: /name <new title>".into());
            return;
        }
        // Same normalization contract as the aux: one bounded line.
        let title = pantheon_core::model::bound_title(rest, pantheon_core::model::TITLE_MAX_CHARS);
        if title.is_empty() {
            state.add_status("/name: nothing to title with".into());
            return;
        }
        // A never-chatted run has no row yet; create it so the rename
        // survives and shows up in /history.
        if supervisor
            .ledger_status(&state.session_id)
            .ok()
            .flatten()
            .is_none()
        {
            let _ = supervisor.start_run(&state.session_id);
        }
        let ev = pantheon_core::events::Event::SessionTitled {
            run_id: state.session_id.clone(),
            title: title.clone(),
            model: "user".into(),
            source: "manual".into(),
        };
        match supervisor.emit(ev) {
            Ok(()) => {
                state.title = Some(title.clone());
                state.add_status(format!("renamed \u{201c}{title}\u{201d}"));
            }
            Err(e) => state.add_status(format!("/name: {e}")),
        }
        return;
    }
    if cmd == "/history" {
        // Open the interactive history overlay; key handling lives in
        // tui_loop while state.history is Some.
        match supervisor.ledger_list_runs(50) {
            Ok(runs) => {
                if runs.is_empty() {
                    state.add_status("no runs yet".into());
                } else {
                    state.history = Some(runs);
                    state.history_input.clear();
                    state.history_sel = 0;
                }
            }
            Err(e) => state.add_status(format!("runs: {e}")),
        }
        return;
    }
    if let Some(id) = cmd.strip_prefix("/resume ") {
        let id = id.trim();
        // Prove the run exists before switching; reopen a terminal run.
        match supervisor.ledger_status(id) {
            Ok(Some(_)) => {
                let _ = supervisor.ledger_reopen_run(id);
                state.session_id = id.to_string();
                state.title = supervisor.ledger_title(id).ok().flatten();
                match supervisor.replay(id) {
                    Ok(entries) => {
                        state.blocks.clear();
                        for m in pantheon_runtime::session::rebuild_messages(entries) {
                            let kind = match m.role {
                                pantheon_core::message::Role::User => {
                                    BlockKind::UserMessage(m.content.clone())
                                }
                                _ => BlockKind::AssistantMessage(m.content.clone()),
                            };
                            state.blocks.push(TranscriptBlock { kind });
                        }
                        state.scroll_to_bottom();
                    }
                    Err(e) => state.add_status(format!("replay: {e}")),
                }
                state.blocks.push(TranscriptBlock {
                    kind: BlockKind::Status(format!("resumed {id}")),
                });
                state.ready = true;
            }
            Ok(None) => state.add_status(format!("no run {id}")),
            Err(e) => state.add_status(format!("status: {e}")),
        }
        return;
    }
    if cmd == "/resume" {
        state.add_status("resume which? give an id, or /history to pick".into());
        return;
    }
    if cmd == "/runs" || cmd.starts_with("/runs ") {
        let n = cmd
            .strip_prefix("/runs ")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(10);
        match supervisor.ledger_list_runs(n) {
            Ok(runs) => {
                if runs.is_empty() {
                    state.add_status("no runs yet".into());
                }
                for (run_id, status, _ts, title) in runs {
                    let glyph = match status.as_str() {
                        "completed" => "\u{2713}",
                        "failed" => "\u{d7}",
                        "running" => "\u{25cf}",
                        _ => "\u{25d0}",
                    };
                    let short: String = run_id.chars().skip(4).take(8).collect();
                    let label = title.filter(|t| !t.is_empty()).unwrap_or(short);
                    state.add_status(format!("{glyph} {label}  {status}"));
                }
            }
            Err(e) => state.add_status(format!("runs: {e}")),
        }
        return;
    }
    if let Some(id) = cmd.strip_prefix("/status ") {
        let id = id.trim();
        match supervisor.explain(id) {
            Ok(s) => state.add_status(s),
            Err(e) => state.add_status(format!("status: {e}")),
        }
        return;
    }
    state.add_status(format!("unknown command: {cmd} (try /help)"));
}
#[cfg(test)]
#[path = "tui_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tui_interrupt_tests.rs"]
mod interrupt_tests;
