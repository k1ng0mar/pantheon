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
    event::{self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEvent,
     KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Paragraph, Scrollbar},
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
    ToolCall { name: String, args: String },
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
    pub tokens_used: u32,
    pub tokens_max: u32,
    pub cost_cents: u32,
    pub blocks: Vec<TranscriptBlock>,
    pub input: String,
    pub scroll_offset: usize,
    pub ready: bool,
    pub shutting_down: bool,
    pub status_line: String,
    pub is_inputting: bool,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            model: String::new(),
            elapsed: Duration::ZERO,
            tokens_used: 0,
            tokens_max: 0,
            cost_cents: 0,
            blocks: Vec::new(),
            input: String::new(),
            scroll_offset: 0,
            ready: false,
            shutting_down: false,
            status_line: String::from("ready"),
            is_inputting: false,
        }
    }
}

impl TuiState {
    fn new(session_id: String, model: String, tokens_max: u32) -> Self {
        Self {
            session_id,
            model,
            elapsed: Duration::ZERO,
            tokens_used: 0,
            tokens_max,
            cost_cents: 0,
            blocks: Vec::new(),
            input: String::new(),
            scroll_offset: 0,
            ready: true,
            shutting_down: false,
            status_line: String::from("ready"),
            is_inputting: false,
        }
    }

    /// Process a model event into a transcript block or state update.
    pub fn handle_model_event(&mut self, ev: ModelEvent) {
        match ev {
            ModelEvent::TextDelta { text } => {
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
                    },
                    timestamp: Instant::now(),
                });
            }
            ModelEvent::Usage { usage } => {
                self.tokens_used = usage.total_tokens as u32;
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
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::ToolCall {
                        name: tool.clone(),
                        args: args.clone(),
                    },
                    timestamp: Instant::now(),
                });
            }
            RuntimeErrorEvent::ToolOutput {
                tool,
                truncated,
                ..
            } => {
                let block = self.blocks.last_mut();
                if let Some(TranscriptBlock {
                    kind: BlockKind::ToolCall { .. },
                    ..
                }) = block
                {
                    // Tool result will come as assistant text, then we update
                }
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
        self.elapsed = Instant::now().duration_since(
            self.blocks
                .first()
                .map(|b| b.timestamp)
                .unwrap_or_else(Instant::now),
        );
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

/// Render the full TUI frame.
pub fn render(state: &TuiState, f: &mut Frame) {
    let outer = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(3),
        Constraint::Min(1),
        Constraint::Length(3),
    ]);
    let [main_area, header_area, chat_area, input_area] = outer.areas(f.size());

    // Header (persistent top bar).
    render_header(f, header_area, state);

    // Main transcript area with a visual block.
    let chat_chunks: [Rect; 1] = Layout::vertical([Constraint::Min(1)])
        .margin(1)
    .areas(chat_area);

    render_transcript(f, chat_chunks[0], state);
}

/// Draw the persistent top header bar.
fn render_header(f: &mut Frame, area: Rect, state: &TuiState) {
    let progress_pct = if state.tokens_max > 0 {
        (state.tokens_used as f32 / state.tokens_max as f32) * 100.0
    } else {
        0.0
    };
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

    // Progress bar under the header.
    let progress_area = Rect {
        x: area.x,
        y: area.y + 1,
        width: area.width,
        height: 1,
    };
    if state.tokens_max > 0 {
        let width = ((progress_pct / 100.0) * (area.width as f32 - 2.0)) as u16;
        let bar = "▰".repeat(width as usize);
        let blank = "▱".repeat((area.width as usize - 2).saturating_sub(width as usize));
        let progress_text = format!("{bar}{blank} {:.0}% ", progress_pct);
        let progress = Paragraph::new(progress_text)
            .style(Style::default().fg(color::RUNNING));
        f.render_widget(progress, progress_area);
    }
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

    let visible_height = area.height as usize;
    let mut lines: Vec<Line> = Vec::new();
    let skip = state.scroll_offset;

    // Render from oldest visible to newest.
    let start = state.blocks.len().saturating_sub(skip + 1);
    for block in state.blocks.iter().rev().skip(start).rev() {
        render_block(&mut lines, block);
        lines.push(Line::from(""));
    }

    let text = Text::from(lines);
    let para = Paragraph::new(text)
        .block(Block::bordered().title(" Transcript "))
        .scroll((state.scroll_offset as u16, 0));
    f.render_widget(para, area);
}

/// Render a single transcript block as Lines.
fn render_block(lines: &mut Vec<Line>, block: &TranscriptBlock) {
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
            lines.push(Line::from(Span::styled(
                format!("┌─ ◇ Thinking ──"),
                Style::default().fg(color::WARNING),
            )));
            for line in text.lines() {
                lines.push(Line::from(format!("│  {line}")));
            }
        }
        BlockKind::ToolCall { name, args } => {
            lines.push(Line::from(Span::styled(
                format!("┌─ ⚙ {name} ──"),
                Style::default().fg(color::RUNNING),
            )));
            if !args.is_empty() {
                for line in args.lines() {
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
    TurnComplete,
    Error(String),
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
        if !running.load(Ordering::SeqCst) {
            break;
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                TuiEvent::Model(me) => state.handle_model_event(me),
                TuiEvent::TurnComplete => {
                    state.ready = true;
                    state.status_line = "ready".to_string();
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
            }
        }

        terminal.draw(|f| render(state, f))?;

        if event::poll(Duration::from_millis(100))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char(c) => {
                        if state.is_inputting {
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
                                handle_slash(state, &msg);
                                continue;
                            }

                            state.blocks.push(TranscriptBlock {
                                kind: BlockKind::UserMessage(msg.clone()),
                                timestamp: Instant::now(),
                                                            });

                            state.ready = false;
                            state.status_line = "working".to_string();
                            
                            let tx2 = tx.clone();
                            let run_id_owned = run_id.to_string();
                            let msg_owned = msg.clone();
                            let session_owned = session.clone();
                            worker = Some(std::thread::spawn(move || {
                                match session_owned.chat(&run_id_owned, &msg_owned) {
                                    Ok(_outcome) => {
                                        let _ = tx2.send(TuiEvent::TurnComplete);
                                    }
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
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn handle_slash(state: &mut TuiState, cmd: &str) {
    match cmd {
        "/help" => {
            state.blocks.push(TranscriptBlock {
                kind: BlockKind::Status("commands: /help /new /runs /exit".to_string()),
        timestamp: Instant::now(),
                            });
        }
        "/exit" | "/quit" => {
            state.blocks.push(TranscriptBlock {
                kind: BlockKind::Status("exiting...".to_string()),
        timestamp: Instant::now(),
                            });
        }
        _ => {
            state.blocks.push(TranscriptBlock {
                kind: BlockKind::Status(format!("unknown command: {cmd}")),
        timestamp: Instant::now(),
                            });
        }
    }
}
