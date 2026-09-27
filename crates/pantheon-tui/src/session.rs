//! The conversation view: the base screen every overlay stacks above.
//!
//! The block model, the streaming event handling, and the
//! permission card move here from `tui.rs` unchanged in behavior.

use crate::app::{RawHandler, ScreenResult};
use crate::widget::Key;
use ratatui::text::Line;

/// One block in the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    UserMessage(String),
    AssistantMessage(String),
    Thinking(String),
    ToolCall {
        name: String,
        args: String,
        ok: Option<bool>,
    },
    Status(String),
}

/// Session state the view owns.
#[derive(Debug, Clone, Default)]
pub struct SessionView {
    pub session_id: String,
    /// The *configured* model, read from the resolved model policy. The
    /// previous header hardcoded a string here, so a `local/llama3.2` session
    /// displayed a model it was not running.
    pub model: String,
    pub provider: String,
    pub title: Option<String>,
    pub blocks: Vec<BlockKind>,
    pub tokens_used: u32,
    pub turn_estimate: u32,
    /// From the catalog, or `None` when the catalog does not know the model.
    /// `None` is not the same as zero: it means "we do not know the window",
    /// and the header must say so rather than invent a budget.
    pub tokens_max: Option<u32>,
    pub elapsed_secs: u64,
    pub ready: bool,
    pub status_line: String,
    /// Set when a run parks on approval: (run_id, scope).
    pub pending_approval: Option<(String, String)>,
    pub last_tool: Option<(String, String)>,
    pub interrupt_armed: bool,
    pub interrupted: bool,
    pub shutting_down: bool,
}

impl SessionView {
    /// Live token count: the authoritative total plus this turn's estimate.
    pub fn live_tokens(&self) -> u32 {
        self.tokens_used + self.turn_estimate
    }

    /// The context indicator. An unknown window is stated, not guessed.
    pub fn context_label(&self) -> String {
        let live = self.live_tokens() as f64 / 1000.0;
        match self.tokens_max {
            Some(max) if max > 0 => format!("{live:.1}k/{}k", max / 1000),
            _ => format!("{live:.1}k/unknown"),
        }
    }

    /// Add to this turn's live estimate, ~4 chars per token.
    pub fn bump_estimate(&mut self, chars: usize) {
        self.turn_estimate += (chars as u32).div_ceil(4);
    }

    /// Fold the estimate into the authoritative count when Usage lands.
    pub fn snap_usage(&mut self, total: u32) {
        self.tokens_used = total;
        self.turn_estimate = 0;
    }

    /// How long the first Esc stays armed.
    pub const ARM_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);

    /// The double-Esc interrupt state machine. Returns true when the caller
    /// must cancel the run in flight.
    pub fn press_esc(&mut self, now: std::time::Instant) -> bool {
        if self.pending_approval.is_some() {
            // The permission card owns Esc: it denies the call. Otherwise one
            // Esc would both arm an interrupt and refuse an approval.
            return false;
        }
        if !self.ready {
            if self.interrupt_armed {
                self.interrupt_armed = false;
                self.interrupted = true;
                self.status_line = "interrupting...".into();
                return true;
            }
            self.interrupt_armed = true;
            self.status_line = "press esc again to interrupt".into();
        } else if self.interrupt_armed {
            self.interrupt_armed = false;
            self.status_line = "ready".into();
        }
        let _ = now;
        false
    }
}

/// The session screen. Owns the view and answers keys while no overlay is up.
pub struct SessionScreen {
    pub view: SessionView,
    /// What an entered composer line should do. The session does not run a
    /// turn itself; the app wires this to the runtime.
    pub on_submit: Option<Box<dyn Fn(String) + Send>>,
    /// Whether a run is in flight, which is what the composer's Enter means
    /// "steer" rather than "send".
    pub composer: String,
    pub composer_active: bool,
}

impl SessionScreen {
    pub fn new(view: SessionView) -> Self {
        Self {
            view,
            on_submit: None,
            composer: String::new(),
            composer_active: false,
        }
    }
}

impl RawHandler for SessionScreen {
    fn draw(&mut self, f: &mut ratatui::Frame) {
        use ratatui::layout::{Constraint, Layout};
        use ratatui::style::{Modifier, Style};
        use ratatui::text::Span;
        use ratatui::widgets::{Block, BorderType, Paragraph};

        let area = f.area();
        let [header_area, transcript_area, input_area, status_area] = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(area);

        let title = match self.view.title.as_deref().filter(|t| !t.is_empty()) {
            Some(t) => format!("PANTHEON {} {}", self.view.session_id, t),
            None => format!("PANTHEON {}", self.view.session_id),
        };
        let header = Paragraph::new(Line::from(Span::styled(
            format!(
                "{}  {}  {}  {}",
                title,
                self.view.model,
                self.view.context_label(),
                human(self.view.elapsed_secs)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )))
        .block(
            Block::bordered()
                .border_type(BorderType::Rounded)
                .title(" "),
        );
        f.render_widget(header, header_area);

        let mut lines: Vec<Line> = Vec::new();
        for b in &self.view.blocks {
            lines.extend(block_lines(b));
            lines.push(Line::from(""));
        }
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().title(" Transcript ")),
            transcript_area,
        );

        let cursor = if self.composer_active { "_" } else { " " };
        let box_line = format!("> {}{}", self.composer, cursor);
        f.render_widget(
            Paragraph::new(box_line).block(
                Block::bordered()
                    .border_type(BorderType::Rounded)
                    .title(" Input "),
            ),
            input_area,
        );

        let status = if let Some((_, scope)) = &self.view.pending_approval {
            format!("permission required  {scope}")
        } else if self.view.ready {
            "ready".to_string()
        } else {
            self.view.status_line.clone()
        };
        f.render_widget(Paragraph::new(status), status_area);
    }

    fn key(&mut self, key: Key) -> Option<ScreenResult> {
        // A pending approval replaces the composer entirely: y grants, n
        // denies, and nothing else is typed into a card.
        if self.view.pending_approval.is_some() {
            return match key {
                Key::Char('y') | Key::Char('Y') | Key::Char('n') | Key::Char('N') => {
                    // The app resolves the grant/deny against the supervisor;
                    // clearing here is what un-sticks the card.
                    self.view.pending_approval = None;
                    self.view.status_line = "settling approval".into();
                    None
                }
                _ => None,
            };
        }
        match key {
            Key::CtrlC | Key::CtrlD => Some(ScreenResult::Quit),
            Key::Esc => {
                self.composer_active = false;
                if self.view.press_esc(std::time::Instant::now()) {
                    self.view.status_line = "interrupting...".into();
                }
                None
            }
            Key::Enter => {
                if !self.composer_active || self.composer.trim().is_empty() {
                    return None;
                }
                let msg = std::mem::take(&mut self.composer);
                self.composer_active = false;
                if let Some(f) = self.on_submit.take() {
                    f(msg);
                }
                None
            }
            Key::Char(c) => {
                self.composer_active = true;
                self.composer.push(c);
                None
            }
            Key::Space => {
                self.composer_active = true;
                self.composer.push(' ');
                None
            }
            Key::Backspace => {
                if self.composer_active {
                    self.composer.pop();
                }
                None
            }
            _ => None,
        }
    }
}

fn block_lines(b: &BlockKind) -> Vec<Line<'static>> {
    let mut out = vec![Line::from("")];
    match b {
        BlockKind::UserMessage(t) => {
            out.push(Line::from("--- you"));
            for l in t.lines() {
                out.push(Line::from(format!("  {l}")));
            }
        }
        BlockKind::AssistantMessage(t) => {
            out.push(Line::from("--- pantheon"));
            for l in t.lines() {
                out.push(Line::from(format!("  {l}")));
            }
        }
        BlockKind::Thinking(t) => {
            let words = t.split_whitespace().count();
            let head: String = t.lines().next().unwrap_or("").chars().take(80).collect();
            out.push(Line::from(format!("thought  {words} words  {head}")));
        }
        BlockKind::ToolCall { name, args, ok } => {
            let glyph = match ok {
                Some(true) => "ok",
                Some(false) => "failed",
                None => "running",
            };
            out.push(Line::from(format!("tool {name}  [{glyph}]")));
            for l in args.lines().take(4) {
                out.push(Line::from(format!("  {l}")));
            }
        }
        BlockKind::Status(t) => out.push(Line::from(t.clone())),
    }
    out
}

fn human(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
