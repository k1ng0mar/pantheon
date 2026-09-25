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
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Paragraph},
    DefaultTerminal,
    Frame,
};

use pantheon_core::model_event::ModelEvent;
use pantheon_core::events::Event as RuntimeErrorEvent;
use pantheon_runtime::session::Session;


/// A single block in the conversation transcript.
#[derive(Debug, Clone)]
pub enum BlockKind {
    UserMessage(String),
    AssistantMessage(String),
    Thinking { text: String, duration_ms: Option<u64>, tokens: Option<u32> },
    /// Tool card: running until the matching runtime completion event
    /// sets `ok`. Args are shown; output lands in ToolResult.
    ToolCall { name: String, args: String, ok: Option<bool> },
    ToolResult { name: String, output: String, ok: bool, duration_ms: Option<u64> },
    FileChange { path: String, added: u32, removed: u32, ok: bool },
    WebSearch { query: String, results: u32, relevant: u32 },
    Swarm { agents: u32, task: String },
    Status(String),
}

#[derive(Debug, Clone)]
pub struct TranscriptBlock {
    pub kind: BlockKind,
    pub timestamp: Instant,
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
        }
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
            ModelEvent::Attempt { provider, model, .. } => {
                // Track mid-session model switches (fallback chain).
                if self.model != model {
                    self.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(format!(
                            "routing: {} unavailable, falling back to {provider}/{model}",
                            self.model
                        )),
                        timestamp: Instant::now(),
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
                        timestamp: Instant::now(),
                    });
                }
            }
            ModelEvent::ReasoningDelta { text } => {
                self.bump_estimate(&text);
                if let Some(TranscriptBlock {
                    kind:
                        BlockKind::Thinking {
                            text: ref mut t,
                            ..
                        },
                    ..
                }) = self.blocks.last_mut()
                {
                    t.push_str(&text);
                } else {
                    self.blocks.push(TranscriptBlock {
                        kind: BlockKind::Thinking {
                            text,
                            duration_ms: None,
                            tokens: None,
                        },
                        timestamp: Instant::now(),
                    });
                }
            }
            ModelEvent::ToolCall { name, arguments, .. } => {
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::ToolCall {
                        name,
                        args: arguments,
                        ok: None,
                    },
                    timestamp: Instant::now(),
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
                    timestamp: Instant::now(),
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
                    timestamp: Instant::now(),
                });
            }
            _ => {}
        }
    }

    pub fn add_user_message(&mut self, text: String) {
        self.blocks.push(TranscriptBlock {
            kind: BlockKind::UserMessage(text),
            timestamp: Instant::now(),
        });
        self.scroll_to_bottom();
    }

    pub fn add_status(&mut self, text: String) {
        self.blocks.push(TranscriptBlock {
            kind: BlockKind::Status(text),
            timestamp: Instant::now(),
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

/// Icons matching the design spec.
mod icon {
    pub const PANTHEON: &str = "◈";
    pub const RUNNING: char = '●';
    pub const SUCCESS: char = '✓';
    pub const WARNING: char = '!';
    pub const FAILURE: char = '×';
    pub const THINKING: &str = "◇";
    pub const WEB: &str = "◎";
    pub const TOOL: &str = "⚙";
    pub const FILE: &str = "✎";
    pub const SWARM: &str = " Swarm";
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
            .title(Span::styled(
                " Input ",
                Style::default().fg(color::PRIMARY),
            )),
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
        format!("{:.1}k/{}k", live_tokens as f64 / 1000.0, state.tokens_max / 1000)
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
        format!("{:.1}k/{}k", state.tokens_used as f64 / 1000.0, state.tokens_max / 1000)
    } else {
        format!("{:.1}k", state.tokens_used as f64 / 1000.0)
    };
    let title = format!(
        "◈ PANTHEON  {}  {}  {:02}:{:02}:{:02}  {}  ${}",
        &state.session_id[..state.session_id.len().min(6)],
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
                Style::default().fg(color::PRIMARY).add_modifier(Modifier::BOLD),
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
fn render_block(
    lines: &mut Vec<Line>,
    block: &TranscriptBlock,
    is_last: bool,
    interrupted: bool,
) {
    match &block.kind {
        BlockKind::UserMessage(text) => {
            lines.push(Line::from(Span::styled(
                "╭─ You ──",
                Style::default().fg(color::PRIMARY).add_modifier(Modifier::BOLD),
            )));
            for line in text.lines() {
                lines.push(Line::from(format!("│  {line}")));
            }
        }
        BlockKind::AssistantMessage(text) => {
            lines.push(Line::from(Span::styled(
                "╭─ ◈ Pantheon ──",
                Style::default().fg(color::PRIMARY).add_modifier(Modifier::BOLD),
            )));
            for line in text.lines() {
                lines.push(Line::from(format!("│  {line}")));
            }
        }
        BlockKind::Thinking { text, .. } => {
            if is_last {
                lines.push(Line::from(Span::styled(
                    format!("┌─ ◇ Thinking ──"),
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
                None => ("●", color::RUNNING),
                Some(true) => ("✓", color::SUCCESS),
                Some(false) => ("×", color::FAILURE),
            };
            lines.push(Line::from(Span::styled(
                format!("┌─ ⚙ {name} ── {glyph}"),
                Style::default().fg(col),
            )));
            if !args.is_empty() {
                for line in args.lines().take(6) {
                    lines.push(Line::from(format!("│  {line}")));
                }
            }
        }
        BlockKind::ToolResult { name, output, ok, .. } => {
            let color = if *ok { color::SUCCESS } else { color::FAILURE };
            let icon = if *ok { icon::SUCCESS } else { icon::FAILURE };
            lines.push(Line::from(Span::styled(
                format!("└─ {icon} {name}"),
                Style::default().fg(color),
            )));
            let _ = output;
        }
        BlockKind::FileChange { path, added, removed, ok } => {
            let color = if *ok { color::SUCCESS } else { color::FAILURE };
            let icon = if *ok { icon::SUCCESS } else { icon::FAILURE };
            lines.push(Line::from(Span::styled(
                format!(
                    "┌─ ✎ {path}  +{added} −{removed}  {icon}"
                ),
                Style::default().fg(color),
            )));
        }
        BlockKind::WebSearch { query, results, relevant } => {
            lines.push(Line::from(Span::styled(
                format!("┌─ ◎ Web Search: {query}"),
                Style::default().fg(color::PRIMARY),
            )));
            lines.push(Line::from(format!(
                "│  {} results, {} relevant",
                results, relevant
            )));
        }
        BlockKind::Swarm { agents, task } => {
            lines.push(Line::from(Span::styled(
                format!("┌─→ Swarm · {agents} agents"),
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

    let mut session = match Session::new(
        crate::data_dir(),
        policy,
        model_policy,
        secrets,
    ) {
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

    let result = tui_loop(&mut terminal, &mut state, session, &tx, &run_id, &rx, &running);

    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(result?)
}

#[allow(unused_mut, unused_assignments)]
fn tui_loop(
    terminal: &mut DefaultTerminal,
    state: &mut TuiState,
    session: Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    run_id: &str,
    rx: &std::sync::mpsc::Receiver<TuiEvent>,
    running: &Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut worker: Option<std::thread::JoinHandle<()>> = None;

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
                    worker = None;
                }
                TuiEvent::Error(msg) => {
                    state.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(format!("error: {msg}")),
                        timestamp: Instant::now(),
                    });
                    state.ready = true;
                    worker = None;
                }
                TuiEvent::Canceled => {
                    // Honest report: the run was stopped by the user, and the
                    // ledger holds the partial transcript so it can be resumed.
                    state.ready = true;
                    state.interrupt_armed_at = None;
                    state.status_line = "interrupted".to_string();
                    state.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(
                            "interrupted \u{2014} run stopped, transcript saved"
                                .to_string(),
                        ),
                        timestamp: Instant::now(),
                    });
                    // Clear the token so the next turn starts clean.
                    session.reset_cancel();
                    worker = None;
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
                match key.code {
                    KeyCode::Char(c) => {
                        if state.pending_approval.is_some() {
                            // Permission card: y allow, n deny, Esc deny.
                            match c {
                                'y' | 'Y' => {
                                    if let Some((run, scope)) = state.pending_approval.take() {
                                        let _ = session
                                            .supervisor
                                            .grant(&run, &scope);
                                        state.status_line = "granted; resuming".into();
                                        state.ready = false;
                                        // Resume: chat_turn with empty message rebuilds
                                        // from the ledger and continues the loop.
                                        let tx3 = tx.clone();
                                        let run3 = run.clone();
                                        let sess3 = session.clone();
                                        worker = Some(std::thread::spawn(move || {
                                            match sess3.chat_turn(&run3, "", "") {
                                                Ok(_) => {
                                                    let _ = tx3.send(TuiEvent::TurnComplete);
                                                }
                                                Err(e) => {
                                                    let _ = tx3.send(TuiEvent::Error(e.to_string()));
                                                }
                                            }
                                        }));
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
                                _ => {}
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

                            state.blocks.push(TranscriptBlock {
                                kind: BlockKind::UserMessage(msg.clone()),
                                timestamp: Instant::now(),
                                                            });

                            state.ready = false;
                            state.status_line = "working".to_string();
                            state.interrupt_armed_at = None;
                            state.interrupted = false;
                            session.reset_cancel();
                            
                            let tx2 = tx.clone();
                            let run_id_owned = run_id.to_string();
                            let msg_owned = msg.clone();
                            let session_owned = session.clone();
                            worker = Some(std::thread::spawn(move || {
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
                            }));
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
                                    state.status_line =
                                        "interrupting\u{2026}".into();
                                    session.cancel_current_run(
                                        run_id,
                                        "user pressed esc twice",
                                    );
                                }
                                _ => {
                                    state.interrupt_armed_at = Some(Instant::now());
                                    state.status_line =
                                        "press esc again to interrupt".into();
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
    let mut push = |s: String| {
        state.blocks.push(TranscriptBlock {
            kind: BlockKind::Status(s),
            timestamp: Instant::now(),
        });
    };
    // /help shows the real command surface.
    if cmd == "/help" {
        push("commands:".into());
        push("  /help              this list".into());
        push("  /runs [N]          recent runs (default 10)".into());
        push("  /status <run_id>   run status line".into());
        push("  /cost              tokens and cost this session".into());
        push("  /clear             clear visible transcript".into());
        push("  /exit, /quit       leave pantheon".into());
        state.scroll_to_bottom();
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
        push(format!(
            "tokens {} \u{2022} cost ${:.2} \u{2022} ctx {:.1}k/{}k",
            state.tokens_used + state.turn_estimate,
            state.cost_cents as f64 / 100.0,
            (state.tokens_used + state.turn_estimate) as f64 / 1000.0,
            state.tokens_max / 1000
        ));
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
                    push("no runs yet".into());
                }
                for (run_id, status, _ts) in runs {
                    let glyph = match status.as_str() {
                        "completed" => "\u{2713}",
                        "failed" => "\u{d7}",
                        "running" => "\u{25cf}",
                        _ => "\u{25d0}",
                    };
                    let short: String = run_id.chars().skip(4).take(8).collect();
                    push(format!("{glyph} {short}  {status}"));
                }
            }
            Err(e) => push(format!("runs: {e}")),
        }
        return;
    }
    if let Some(id) = cmd.strip_prefix("/status ") {
        let id = id.trim();
        match supervisor.explain(id) {
            Ok(s) => push(s),
            Err(e) => push(format!("status: {e}")),
        }
        return;
    }
    push(format!("unknown command: {cmd} (try /help)"));
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_collapses_when_not_last() {
        let mut lines = Vec::new();
        let block = TranscriptBlock {
            kind: BlockKind::Thinking {
                text: "first line of reasoning\nsecond line\nthird".into(),
                duration_ms: None,
                tokens: None,
            },
            timestamp: Instant::now(),
        };
        render_block(&mut lines, &block, false, false);
        // Collapsed: exactly one line, contains the marker.
        assert_eq!(lines.len(), 1, "collapsed thinking is one line");
        let s = format!("{:?}", lines[0]);
        assert!(s.contains("Thought"), "summary marker present: {s}");
        assert!(s.contains("first line"), "summary carries head of text");
    }

    #[test]
    fn thinking_expands_when_last() {
        let mut lines = Vec::new();
        let block = TranscriptBlock {
            kind: BlockKind::Thinking {
                text: "line one\nline two".into(),
                duration_ms: None,
                tokens: None,
            },
            timestamp: Instant::now(),
        };
        render_block(&mut lines, &block, true, false);
        // Expanded: header + 2 content lines.
        assert_eq!(lines.len(), 3, "expanded thinking shows all lines");
    }

    #[test]
    fn tool_call_flips_status_glyph() {
        let mut lines = Vec::new();
        let mut block = TranscriptBlock {
            kind: BlockKind::ToolCall {
                name: "shell.exec".into(),
                args: "ls".into(),
                ok: None,
            },
            timestamp: Instant::now(),
        };
        render_block(&mut lines, &block, true, false);
        let running = format!("{:?}", lines[0]);
        assert!(running.contains('\u{25cf}'), "running glyph while ok=None: {running}");

        if let BlockKind::ToolCall { ok, .. } = &mut block.kind {
            *ok = Some(true);
        }
        lines.clear();
        render_block(&mut lines, &block, true, false);
        let done = format!("{:?}", lines[0]);
        assert!(done.contains('\u{2713}'), "done glyph after completion: {done}");
    }

    #[test]
    fn live_estimate_accumulates_and_snaps() {
        let mut state = TuiState::new("sess1234".into(), "opus".into(), 200_000);
        state.handle_model_event(ModelEvent::TextDelta { text: "x".repeat(40) });
        assert_eq!(state.turn_estimate, 10, "40 chars / 4 = 10 tokens");
        state.handle_model_event(ModelEvent::Usage {
            usage: pantheon_core::model_event::ModelUsage {
                input_tokens: 100,
                output_tokens: 10,
                total_tokens: 110,
                cost_usd: Some(0.01),
            },
        });
        assert_eq!(state.tokens_used, 110, "snapped to authoritative");
        assert_eq!(state.turn_estimate, 0, "estimate reset after snap");
        assert_eq!(state.cost_cents, 1);
    }
}

#[cfg(test)]
mod interrupt_tests {
    use super::*;

    /// Build a real running-session state (no hand-maintained field list,
    /// so this test cannot rot when the struct grows).
    fn state() -> TuiState {
        let mut s = TuiState::new("sess_test01".into(), "test".into(), 128_000);
        s.ready = false;
        s.is_inputting = true;
        s.status_line = "working".into();
        s
    }

    const ARM_WINDOW: Duration = Duration::from_millis(1500);

    /// First Esc arms; it must not claim the run is interrupted yet.
    #[test]
    fn first_esc_only_arms() {
        let mut s = state();
        assert!(s.interrupt_armed_at.is_none());
        s.interrupt_armed_at = Some(Instant::now());
        assert!(s.interrupt_armed_at.is_some(), "armed");
        assert!(!s.interrupted, "arming is not interruption");
    }

    /// A second Esc inside the window confirms the interrupt.
    #[test]
    fn second_esc_inside_window_interrupts() {
        let mut s = state();
        s.interrupt_armed_at = Some(Instant::now());
        let confirms = s
            .interrupt_armed_at
            .is_some_and(|t| t.elapsed() < ARM_WINDOW);
        assert!(confirms, "second esc inside the window is a confirm");
        s.interrupted = true;
        s.interrupt_armed_at = None;
        assert!(s.interrupted);
    }

    /// An arm that goes stale (user wandered off) must not fire later.
    #[test]
    fn stale_arm_does_not_interrupt() {
        let mut s = state();
        s.interrupt_armed_at = Some(Instant::now() - ARM_WINDOW - Duration::from_millis(50));
        let confirms = s
            .interrupt_armed_at
            .is_some_and(|t| t.elapsed() < ARM_WINDOW);
        assert!(!confirms, "stale arm is re-armed, not fired");
    }

    /// Idle sessions must not be interruptible: the arm path is gated on
    /// !ready, so an Esc while ready cannot cancel anything.
    #[test]
    fn idle_session_cannot_arm() {
        let mut s = state();
        s.ready = true;
        let armable = !s.ready && !s.interrupted;
        assert!(!armable, "no interrupt affordance while idle");
    }
}
