//! The Pantheon session: the agent cockpit.
//!
//! Visual blocks for each event type, persistent header, status bar.
//! Uses ratatui + crossterm. There is no streaming-text fallback: if the
//! terminal cannot host the alternate screen, entry refuses to start rather
//! than becoming a second, worse interface.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as CtEvent, KeyCode, KeyEventKind,
        KeyModifiers,
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

use pantheon_api::events::Event as RuntimeErrorEvent;
use pantheon_providers::model_event::ModelEvent;
use pantheon_runtime::session::Session;

/// TUI-B feature modules. Declared here (not in lib.rs) so a sibling
/// worker editing lib.rs cannot conflict with this branch.
mod statusbar;
mod timeline;
mod editor;

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

/// One selectable row in the /models browser: a provider/model pair plus
/// the context window the catalog declares (`None` = unknown, shown as ?).
/// A row with an empty `model_id` is a provider with no curated models;
/// Enter on it explains how to switch with an explicit id instead.
#[derive(Debug, Clone)]
pub struct ModelRow {
    pub provider_id: String,
    pub provider_label: String,
    pub model_id: String,
    pub ctx: Option<u32>,
}

/// A pending rewind confirmation: the last turn the operator could roll
/// back to, captured while the session is idle. Confirming appends a
/// rewind marker to the ledger (history is never rewritten) and truncates
/// the live transcript view back to the pre-turn state.
#[derive(Debug, Clone)]
pub struct RewindOffer {
    /// 1-based turn number being rewound.
    pub turn_no: usize,
    /// Block index of the turn's user message in `TuiState::blocks`.
    pub block_index: usize,
    /// First line of the discarded user message, for the confirm prompt.
    pub preview: String,
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
    /// Open model-browser overlay: Some(rows) while /models is open.
    pub models: Option<Vec<ModelRow>>,
    /// Live filter typed into the models overlay.
    pub models_input: String,
    /// Selected index into the filtered models list.
    pub models_sel: usize,
    /// The conversation's current title: set by the title auxiliary
    /// (SessionTitled), by /name, and refreshed when resuming a run.
    pub title: Option<String>,
    /// Message typed while a turn was running. Enter queues it instead of
    /// spawning a second agent loop; it fires when the turn ends.
    pub queued_message: Option<String>,
    /// Run id of the turn currently in flight, if any. Esc interrupts this
    /// run (not the selected session) so a mid-turn resume cannot retarget
    /// the interrupt at the wrong loop.
    pub active_run: Option<String>,
    /// This turn's input tokens, from the latest Usage event. `None` until
    /// the provider reports usage — feeds the live status bar.
    pub turn_in: Option<u64>,
    /// This turn's output tokens, from the latest Usage event.
    pub turn_out: Option<u64>,
    /// When the current turn started; drives tokens/sec. `None` while idle.
    pub turn_started_at: Option<Instant>,
    /// Completed turns. The status bar shows this + 1 while a turn runs.
    pub turns_completed: u32,
    /// Background-turn contract for the tab layer: set when a turn
    /// finishes while this session's tab is not focused. The tab UI
    /// renders it as a badge. Cleared by `attention_clear` when the tab
    /// regains focus. Turn completion never touches `input`/`is_inputting`,
    /// so a background finish cannot steal input focus.
    pub attention: bool,
    /// Human note for the badge, e.g. "turn 3 finished".
    pub attention_note: Option<String>,
    /// Open turn-timeline rail: `Some(nav)` while Ctrl+O/F2 is open.
    pub timeline: Option<timeline::TimelineNav>,
    /// Transcript block to pin to the viewport top on the next render
    /// (set by timeline Enter). Consumed by `render_transcript`.
    pub jump_to_block: Option<usize>,
    /// Open fullscreen draft editor: `Some(ed)` while Ctrl+E is open.
    pub editor: Option<editor::DraftEditor>,
    /// Pending double-Esc rewind confirmation. `Some` while the operator
    /// decides; `y` confirms, `n`/Esc dismisses.
    pub rewind_offer: Option<RewindOffer>,
    /// Session tabs (opencode-style top bar). Rebuilt from
    /// `ledger_list_runs`; refreshed on /new, resume, tab switch and turn
    /// completion so titles and busy badges stay current.
    pub tabs: crate::tabs::TabList,
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
            models: None,
            models_input: String::new(),
            models_sel: 0,
            title: None,
            queued_message: None,
            active_run: None,
            turn_in: None,
            turn_out: None,
            turn_started_at: None,
            turns_completed: 0,
            attention: false,
            attention_note: None,
            timeline: None,
            jump_to_block: None,
            editor: None,
            rewind_offer: None,
            tabs: crate::tabs::TabList::default(),
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
            models: None,
            models_input: String::new(),
            models_sel: 0,
            title: None,
            queued_message: None,
            active_run: None,
            turn_in: None,
            turn_out: None,
            turn_started_at: None,
            turns_completed: 0,
            attention: false,
            attention_note: None,
            timeline: None,
            jump_to_block: None,
            editor: None,
            rewind_offer: None,
            tabs: crate::tabs::TabList::default(),
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

    /// Rows matching the current filter, in catalog order. Matches
    /// provider id, provider label, and model id, so `anth` finds the
    /// provider and `mini` finds the model.
    pub fn filtered_models(&self) -> Vec<ModelRow> {
        match &self.models {
            None => Vec::new(),
            Some(rows) => {
                let f = self.models_input.to_lowercase();
                if f.is_empty() {
                    rows.clone()
                } else {
                    rows.iter()
                        .filter(|r| {
                            r.provider_id.to_lowercase().contains(&f)
                                || r.provider_label.to_lowercase().contains(&f)
                                || r.model_id.to_lowercase().contains(&f)
                        })
                        .cloned()
                        .collect()
                }
            }
        }
    }

    /// Move the overlay selection by n, clamped to the filtered list.
    pub fn models_move(&mut self, n: isize) {
        let len = self.filtered_models().len();
        if len == 0 {
            self.models_sel = 0;
            return;
        }
        let sel = self.models_sel as isize + n;
        self.models_sel = sel.clamp(0, len as isize - 1) as usize;
    }

    /// Close the overlay and reset its state.
    pub fn models_close(&mut self) {
        self.models = None;
        self.models_input.clear();
        self.models_sel = 0;
    }

    /// Live token estimate: ~4 chars per token, updated on every streamed
    /// delta so the counter ticks like Claude Code's. Snapped to the
    /// authoritative number when Usage arrives.
    fn bump_estimate(&mut self, text: &str) {
        self.turn_estimate += (text.len() as u32).div_ceil(4);
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
                // Per-turn in/out for the live status bar. A provider that
                // reports only totals leaves these at the total split it
                // gave; a provider that reports nothing leaves them None.
                self.turn_in = Some(usage.input_tokens);
                self.turn_out = Some(usage.output_tokens);
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
            RuntimeErrorEvent::ToolOutput { .. } => {
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

    /// How long the first Esc stays armed. The second Esc inside this
    /// window is what actually cancels the run.
    pub const ARM_WINDOW: Duration = Duration::from_millis(1500);

    /// Handle an Esc press. Returns true when the caller must cancel the
    /// run in flight, which happens only on a confirmed second Esc.
    ///
    /// While a turn runs, the second Esc interrupts (existing behavior).
    /// While idle, the first Esc arms and the second Esc inside the arm
    /// window offers to rewind the last turn instead of just disarming:
    /// the confirm prompt is explicit (`y`/`n`), so nothing destructive
    /// happens on a stray keypress. An idle Esc never interrupts.
    ///
    /// Extracted from the event loop so the arm/confirm/stale rules are
    /// testable against the shipped code rather than a copy of it.
    pub fn press_esc(&mut self) -> bool {
        if !self.ready && !self.interrupted {
            match self.interrupt_armed_at {
                Some(t) if t.elapsed() < Self::ARM_WINDOW => {
                    self.interrupt_armed_at = None;
                    self.interrupted = true;
                    self.status_line = "interrupting…".into();
                    return true;
                }
                _ => {
                    self.interrupt_armed_at = Some(Instant::now());
                    self.status_line = "press esc again to interrupt".into();
                }
            }
        } else if let Some(t) = self.interrupt_armed_at {
            // Second Esc while idle: inside the window, offer rewind when
            // there is a turn to roll back to, otherwise disarm as before.
            // A stale arm re-arms instead of firing.
            self.interrupt_armed_at = None;
            if t.elapsed() < Self::ARM_WINDOW {
                match self.rewind_candidate() {
                    Some(offer) => {
                        self.rewind_offer = Some(offer);
                        self.status_line = "rewind last turn? [y]es [n]o".into();
                    }
                    None => {
                        self.status_line = "ready".into();
                    }
                }
            } else {
                self.interrupt_armed_at = Some(Instant::now());
            }
        } else if !self.interrupted {
            // First Esc while idle: arm the rewind offer path. This never
            // interrupts anything; begin_turn clears the arm.
            self.interrupt_armed_at = Some(Instant::now());
        }
        false
    }

    /// Begin a turn for `msg`. Returns true when the caller should start
    /// the agent loop now; returns false and queues the message when a turn
    /// is already running, so Enter never spawns a second loop on the same
    /// run. A newer queued message replaces an older one: the single slot
    /// holds the latest intent, and it drains on the next TurnComplete.
    ///
    /// Extracted from the event loop so the guard rules are testable
    /// against the shipped code rather than a copy of it.
    pub fn begin_turn(&mut self, msg: String) -> bool {
        if !self.ready {
            self.queued_message = Some(msg);
            return false;
        }
        self.ready = false;
        self.status_line = "working".to_string();
        self.interrupt_armed_at = None;
        self.interrupted = false;
        // Fresh per-turn telemetry: the last turn's in/out must not leak
        // into this turn's status bar.
        self.turn_in = None;
        self.turn_out = None;
        self.turn_started_at = Some(Instant::now());
        true
    }

    /// Take the message queued while a turn was running, if any.
    pub fn take_queued(&mut self) -> Option<String> {
        self.queued_message.take()
    }

    /// Which run an interrupt should cancel: the in-flight turn's run when
    /// there is one, else the selected session. Pure, tested below.
    pub fn interrupt_target(&self) -> &str {
        self.active_run.as_deref().unwrap_or(&self.session_id)
    }

    /// Mark a turn finished. `focused` tells whether this session's tab is
    /// the visible one: a background finish raises the tab badge instead
    /// of touching anything the user is doing.
    ///
    /// This deliberately never touches `input` or `is_inputting`: a turn
    /// completing in the background must not steal input focus. The queued
    /// message still drains (via `drain_queued_message` in the loop) because
    /// it belongs to this session's turn machinery, not to the tab.
    pub fn on_turn_complete(&mut self, focused: bool) {
        self.ready = true;
        self.status_line = "ready".to_string();
        self.interrupt_armed_at = None;
        self.interrupted = false;
        self.active_run = None;
        self.turn_started_at = None;
        self.turns_completed += 1;
        if focused {
            self.attention = false;
            self.attention_note = None;
        } else {
            self.attention = true;
            self.attention_note = Some(format!("turn {} finished", self.turns_completed));
        }
    }

    /// Clear the background-completion badge: the tab is focused again.
    pub fn attention_clear(&mut self) {
        self.attention = false;
        self.attention_note = None;
    }

    /// Toggle the turn-timeline rail. Opening selects the latest turn.
    pub fn toggle_timeline(&mut self) {
        match self.timeline.take() {
            Some(_) => {}
            None => {
                let n = timeline::build_turns(&self.blocks).len();
                self.timeline = Some(timeline::TimelineNav::open(n));
            }
        }
    }

    /// Open the fullscreen draft editor, carrying the current input draft.
    pub fn open_editor(&mut self) {
        let draft = std::mem::take(&mut self.input);
        self.is_inputting = false;
        self.editor = Some(editor::DraftEditor::from_text(&draft));
    }

    /// The turn a rewind would roll back to, or `None` when rewind is not
    /// available: no turn has completed, a turn is running, an approval is
    /// pending, or a confirm is already open.
    pub fn rewind_candidate(&self) -> Option<RewindOffer> {
        if !self.ready
            || self.pending_approval.is_some()
            || self.active_run.is_some()
            || self.rewind_offer.is_some()
        {
            return None;
        }
        let mut last_user: Option<(usize, usize, String)> = None;
        for (i, block) in self.blocks.iter().enumerate() {
            if let BlockKind::UserMessage(text) = &block.kind {
                let turn_no = last_user.map(|(_, n, _)| n + 1).unwrap_or(1);
                let preview: String = text.lines().next().unwrap_or("").chars().take(60).collect();
                last_user = Some((i, turn_no, preview));
            }
        }
        last_user.map(|(block_index, turn_no, preview)| RewindOffer {
            turn_no,
            block_index,
            preview,
        })
    }

    /// This turn's throughput in tokens/sec. Prefers the authoritative
    /// Usage numbers; falls back to the live streamed estimate while the
    /// turn runs; `None` when there is nothing honest to divide.
    pub fn turn_rate(&self) -> Option<f64> {
        let elapsed = self.turn_started_at?.elapsed().as_secs_f64();
        if elapsed < 0.5 {
            return None;
        }
        match (self.turn_in, self.turn_out) {
            (Some(i), Some(o)) => Some((i + o) as f64 / elapsed),
            _ if self.turn_estimate > 0 => Some(self.turn_estimate as f64 / elapsed),
            _ => None,
        }
    }

    /// The turn number the status bar shows: completed turns + 1 while a
    /// turn runs; `None` before the first turn so the bar shows `—`.
    pub fn display_turn_no(&self) -> Option<u32> {
        let n = self.turns_completed + if self.ready { 0 } else { 1 };
        if n == 0 { None } else { Some(n) }
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

    /// Scroll up into history by `lines`. New blocks reset via
    /// `scroll_to_bottom`; streaming follows the tail while the offset is 0.
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_add(lines).min(10_000);
    }

    /// Scroll back down toward the live tail.
    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_offset = self.scroll_offset.saturating_sub(lines);
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
    /// Reasoning text: dimmer than answers, never competing with them.
    pub const DIM: Color = Color::DarkGray;
}

/// Render the full TUI frame: header, transcript, input, status bar.
///
/// Takes `&mut` because a timeline jump pins the viewport to a turn's
/// first block during this render (consuming `jump_to_block`); nothing
/// else about the transcript is mutated.
/// Rebuild the session tab bar from the run ledger. Cheap local query;
/// called on loop start, /new, resume, tab switch and turn completion so
/// titles and busy badges stay current.
fn refresh_tabs(state: &mut TuiState, session: &Arc<Session>) {
    if let Ok(runs) = session.supervisor.ledger_list_runs(50) {
        let active = state.session_id.clone();
        state.tabs.refresh_from_runs(
            &runs,
            |id| session.supervisor.has_active_lease(id).unwrap_or(false),
            &active,
        );
    }
}

/// Switch the visible session to run `id`: reopen the run and rebuild the
/// transcript from the ledger. Shared by /history resume and tab keys.
/// Never forces `ready`: a turn may still run for another session and the
/// Enter guard must keep applying to it.
fn switch_to_run(state: &mut TuiState, session: &Arc<Session>, id: &str) {
    if id == state.session_id {
        state.attention_clear();
        return;
    }
    let _ = session.supervisor.ledger_reopen_run(id);
    if let Ok(entries) = session.supervisor.replay(id) {
        state.blocks.clear();
        state.session_id = id.to_string();
        state.title = session.supervisor.ledger_title(id).ok().flatten();
        for m in pantheon_runtime::session::rebuild_messages(entries) {
            let kind = match m.role {
                pantheon_api::message::Role::User => BlockKind::UserMessage(m.content.clone()),
                _ => BlockKind::AssistantMessage(m.content.clone()),
            };
            state.blocks.push(TranscriptBlock { kind });
        }
        state.scroll_to_bottom();
    }
    state.attention_clear();
    state.blocks.push(TranscriptBlock {
        kind: BlockKind::Status(format!("resumed {}", id)),
    });
}

pub fn render(state: &mut TuiState, f: &mut Frame) {
    let outer = Layout::vertical([
        Constraint::Length(1), // session tabs
        Constraint::Length(3), // header
        Constraint::Min(1),    // transcript
        Constraint::Length(3), // input
        Constraint::Length(1), // status bar
    ]);
    let [tab_area, header_area, chat_area, input_area, status_area] = outer.areas(f.area());

    if state.history.is_some() {
        render_history(f, f.area(), state);
        return;
    }
    if state.models.is_some() {
        render_models(f, f.area(), state);
        return;
    }
    if state.editor.is_some() {
        render_editor(f, f.area(), state);
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
    crate::tabs::render_tab_bar(f, tab_area, &state.tabs);
    if state.timeline.is_some() {
        render_timeline(f, f.area(), state);
    }
}

/// Searchable scrollable history overlay: /history. Type-to-filter,
/// Up/Down to move, Enter to resume, Esc to close. Mirrors the REPL's
/// /history picker, rendered as a centered list.
fn render_history(f: &mut Frame, area: Rect, state: &TuiState) {
    let w = area.width.clamp(40, 80);
    let h = area.height.clamp(7, 20);
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

/// Searchable model browser overlay: /models. Type-to-filter across
/// provider and model names, Up/Down to move, Enter to switch the live
/// session's default model, Esc to close. Same overlay contract as
/// /history, rendered as a centered list.
fn render_models(f: &mut Frame, area: Rect, state: &TuiState) {
    let w = area.width.clamp(48, 88);
    let h = area.height.clamp(7, 22);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    let rows = state.filtered_models();
    let mut lines = vec![
        Line::from(Span::styled(
            "  type to filter, Up/Down to move, Enter to switch, Esc to close",
            Style::default().fg(color::PRIMARY),
        )),
        Line::from(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(color::FAILURE),
        )));
    }
    for (i, row) in rows.iter().enumerate() {
        let current = format!("{}/{}", row.provider_id, row.model_id) == state.model;
        let glyph = if current { "\u{25cf}" } else { " " };
        let model = if row.model_id.is_empty() {
            "(no curated models — Enter for how to switch)".to_string()
        } else {
            row.model_id.clone()
        };
        let ctx = match row.ctx {
            Some(c) if c >= 1000 => format!("{}k ctx", c / 1000),
            Some(c) => format!("{c} ctx"),
            None => "ctx ?".to_string(),
        };
        let style = if i == state.models_sel {
            Style::default()
                .fg(color::PRIMARY)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!(
                "  {glyph} {:<18} {:<32} {}",
                row.provider_label.chars().take(18).collect::<String>(),
                model.chars().take(32).collect::<String>(),
                ctx,
            ),
            style,
        )));
    }
    if !state.models_input.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("  filter: {}", state.models_input)));
    }
    let title = " models (/models) ";
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

/// Turn-timeline rail: a right-side panel listing one row per user turn
/// (conversation order). Arrow keys move the selection, Enter jumps the
/// transcript viewport to that turn, Esc/F2 closes. Strictly read-only:
/// navigating never mutates the transcript.
fn render_timeline(f: &mut Frame, area: Rect, state: &TuiState) {
    let turns = timeline::build_turns(&state.blocks);
    let sel = state.timeline.as_ref().map(|n| n.sel()).unwrap_or(0);
    let w = 46.min(area.width.max(20));
    let panel = Rect {
        x: area.x + area.width.saturating_sub(w),
        y: area.y,
        width: w,
        height: area.height,
    };
    f.render_widget(ratatui::widgets::Clear, panel);

    let mut lines = vec![
        Line::from(Span::styled(
            "  turns  ↑/↓ move · Enter jump · Esc close",
            Style::default().fg(color::PRIMARY),
        )),
        Line::from(""),
    ];
    if turns.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no turns yet)",
            Style::default().fg(color::DIM),
        )));
    }
    // Show the tail when there are more turns than rows: the latest turn
    // is what the selection opens on.
    let inner = w.saturating_sub(4) as usize;
    let rows = panel.height.saturating_sub(5) as usize;
    let start = turns.len().saturating_sub(rows.max(1));
    for (i, turn) in turns.iter().enumerate().skip(start) {
        let row = format!(
            "  {:>3}  {}",
            turn.turn_no,
            turn.preview.chars().take(inner.saturating_sub(8)).collect::<String>(),
        );
        let style = if i == sel {
            Style::default()
                .fg(color::PRIMARY)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(row, style)));
    }
    let card = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(color::PRIMARY))
            .title(Span::styled(
                " timeline ",
                Style::default()
                    .fg(color::PRIMARY)
                    .add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, panel);
}

/// Fullscreen draft editor. Line numbers, current-line highlight, live
/// line/char stats. `Ctrl+Enter`/`Ctrl+S` sends, `Esc` cancels and keeps
/// the draft in the input box.
fn render_editor(f: &mut Frame, area: Rect, state: &TuiState) {
    let Some(ed) = state.editor.as_ref() else {
        return;
    };
    f.render_widget(ratatui::widgets::Clear, area);
    let (crow, ccol) = ed.cursor();
    let mut lines: Vec<Line> = Vec::new();
    let num_w = ed.line_count().to_string().len().max(2);
    // Keep the cursor row visible: scroll the editor viewport to it.
    let view_h = area.height.saturating_sub(5) as usize;
    let top = crow.saturating_sub(view_h.saturating_sub(1));
    for (i, line) in ed.lines().iter().enumerate().skip(top).take(view_h.max(1)) {
        let mut spans = vec![Span::styled(
            format!("{:>num_w$} │ ", i + 1),
            Style::default().fg(color::DIM),
        )];
        if i == crow {
            // Draw the block cursor inside the current line.
            let chars: Vec<char> = line.chars().collect();
            let c = ccol.min(chars.len());
            let (head, tail) = (chars[..c].iter().collect::<String>(), chars[c..].iter().collect::<String>());
            spans.push(Span::raw(head));
            spans.push(Span::styled("▌", Style::default().fg(color::PRIMARY).add_modifier(Modifier::BOLD)));
            spans.push(Span::raw(tail));
            lines.push(Line::from(spans).style(Style::default().add_modifier(Modifier::REVERSED)));
        } else {
            spans.push(Span::raw(line.clone()));
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "  {} lines · {} chars    Ctrl+Enter send · Esc cancel (keeps draft)",
            ed.line_count(),
            ed.char_count(),
        ),
        Style::default().fg(color::PRIMARY),
    )));
    let card = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(color::PRIMARY))
            .title(Span::styled(
                " draft editor ",
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

/// Draw the one-line live status bar under the input.
///
/// Telemetry is real or absent: context %, per-turn in/out, tok/s, model,
/// turn count, and cost come from `ModelEvent::Usage` and turn timing;
/// anything the runtime did not expose renders as `—` (see `statusbar`).
/// A pending rewind confirmation takes over the bar so the question is
/// impossible to miss.
fn render_status(f: &mut Frame, area: Rect, state: &TuiState) {
    if let Some(offer) = &state.rewind_offer {
        let text = format!(
            "↩ rewind turn {} ({}…)?  [y] yes   [n] no",
            offer.turn_no,
            offer.preview.chars().take(40).collect::<String>(),
        );
        let bar = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default()
                .fg(color::WARNING)
                .add_modifier(Modifier::BOLD),
        )));
        f.render_widget(bar, area);
        return;
    }
    // Interrupt state takes over the status word: an armed interrupt is a
    // call to action, a settled one reports the truth.
    let (icon, bar_color, status_word) = if state.interrupted {
        (icon::WARNING, color::WARNING, "interrupted")
    } else if !state.ready && state.interrupt_armed_at.is_some() {
        (icon::WARNING, color::WARNING, "esc to interrupt")
    } else if state.ready
        && state.interrupt_armed_at.is_some()
        && state.rewind_candidate().is_some()
    {
        // Double-Esc is armed while idle: make the rewind discoverable.
        (icon::WARNING, color::WARNING, "esc again: rewind?")
    } else if state.ready {
        (icon::SUCCESS, color::SUCCESS, "ready")
    } else {
        (icon::RUNNING, color::RUNNING, "working")
    };
    let live_tokens = state.tokens_used + state.turn_estimate;
    let (context_frac, context_label) = if state.tokens_max > 0 {
        (
            Some(live_tokens as f64 / state.tokens_max as f64),
            Some(format!(
                "{:.1}k/{}k",
                live_tokens as f64 / 1000.0,
                state.tokens_max / 1000
            )),
        )
    } else {
        // No declared window is not a zero-sized window: the fraction is
        // unknown, and the label says so rather than inventing a budget.
        (
            None,
            Some(format!("{:.1}k/unknown", live_tokens as f64 / 1000.0)),
        )
    };
    let data = statusbar::StatusBarData {
        status_word: status_word.to_string(),
        icon: icon.to_string(),
        model: state.model.clone(),
        context_frac,
        context_label,
        turn_in: state.turn_in,
        turn_out: state.turn_out,
        tokens_per_sec: state.turn_rate(),
        // The runtime's ModelUsage carries no cache counters; `None`
        // renders as `—` rather than a fabricated hit rate.
        cache_hit_rate: None,
        turn_no: state.display_turn_no(),
        session_prefix: state.session_id.chars().take(6).collect(),
        cost_usd: if state.cost_cents > 0 {
            Some(state.cost_cents as f64 / 100.0)
        } else {
            None
        },
    };
    let text = statusbar::render(&data, area.width as usize);
    let bar = Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().fg(bar_color),
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
        format!("{:.1}k/unknown", state.tokens_used as f64 / 1000.0)
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
        "◈ PANTHEON  {}  {}  {:02}:{:02}:{:02}  {}  ${:.2}",
        session_label,
        state.model,
        state.elapsed.as_secs() / 3600,
        (state.elapsed.as_secs() % 3600) / 60,
        state.elapsed.as_secs() % 60,
        tokens_display,
        state.cost_cents as f64 / 100.0,
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
///
/// Consumes `jump_to_block` (set by the timeline's Enter): the viewport
/// pins so the target turn's first block sits at the top. Read-only
/// otherwise — jumping never mutates the transcript.
fn render_transcript(f: &mut Frame, area: Rect, state: &mut TuiState) {
    if state.blocks.is_empty() {
        let mut welcome = crate::terminal::splash_lines(area.width);
        welcome.push(Line::from(""));
        welcome.push(Line::from("Ask me anything. /help for commands."));
        let welcome = Paragraph::new(welcome).block(Block::bordered().title(" Transcript "));
        f.render_widget(welcome, area);
        return;
    }

    let mut lines: Vec<Line> = Vec::new();
    // Line index where each block starts: the timeline jump target.
    let mut block_starts: Vec<usize> = Vec::new();

    // Every block, oldest first. The viewport pins to the bottom (the live
    // tail) minus how far the user scrolled up: an earlier version rendered
    // only the oldest block, so every turn after the first was invisible.
    let block_count = state.blocks.len();
    for (i, block) in state.blocks.iter().enumerate() {
        block_starts.push(lines.len());
        let is_last = i + 1 == block_count;
        render_block(&mut lines, block, is_last, state.interrupted);
        lines.push(Line::from(""));
    }

    // Pin to the bottom: offset 0 shows the tail, and scrolling up reveals
    // history. ratatui clips the viewport, so this is exact, not an estimate.
    let visible = area.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    if let Some(target_block) = state.jump_to_block.take() {
        // Pin the target block's first line to the viewport top: the
        // paragraph scrolls `off` lines from the top, and
        // off = max_scroll - scroll_offset.
        let target_line = block_starts.get(target_block).copied().unwrap_or(0);
        state.scroll_offset = max_scroll.saturating_sub(target_line);
    }
    let off = max_scroll.saturating_sub(state.scroll_offset) as u16;
    let text = Text::from(lines);
    let para = Paragraph::new(text)
        .block(Block::bordered().title(" Transcript "))
        .scroll((off, 0));
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
            // Reasoning renders distinctly from final text: dim italic,
            // labeled as thought rather than answer. The live (streaming)
            // block stays expanded; settled ones collapse to a summary so
            // old reasoning never crowds out answers.
            let dim = Style::default()
                .fg(color::DIM)
                .add_modifier(Modifier::ITALIC);
            if is_last {
                lines.push(Line::from(Span::styled(
                    format!("┌─ {} reasoning ──", icon::THINKING),
                    Style::default()
                        .fg(color::DIM)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                )));
                for line in text.lines().take(40) {
                    lines.push(Line::from(Span::styled(format!("│  {line}"), dim)));
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
                    dim,
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
    Model(pantheon_providers::model_event::ModelEvent),
    Runtime(pantheon_api::events::Event),
    TurnComplete,
    /// The assistant's final text for a turn, rendered into the transcript.
    Answered(String),
    Error(String),
    /// The run stopped because the user interrupted it (not a failure).
    Canceled,
}

/// Run the session view on a real terminal.
///
/// The data dir and the resume id arrive from the caller rather than being
/// read from the process environment here, so this function has one way to be
/// called and a test can drive it with a temp dir.
pub fn run_tui_session_with(
    data_dir: std::path::PathBuf,
    resume: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    use crate::config;
    use crate::config::build_model_policy;
    use std::sync::mpsc;

    let file_cfg = config::Config::load_or_report(&data_dir);
    let model_policy = build_model_policy(file_cfg.as_ref(), None, None);

    // The header states the model this session is actually running. It used to
    // be the literal "opus-4.1", so a session on a local llama displayed a
    // frontier model it never called. Read here, before the policy is moved
    // into the Session below.
    let header_model = if model_policy.default.model.trim().is_empty() {
        "no model configured".to_string()
    } else {
        format!(
            "{}/{}",
            model_policy.default.provider, model_policy.default.model
        )
    };
    // An uncataloged model declares no window. Zero is not the same as
    // unknown, and the status line says "unknown" rather than inventing one.
    let tokens_max = pantheon_providers::catalog::model_meta(
        &model_policy.default.provider,
        &model_policy.default.model,
    )
    .context_limit
    .unwrap_or(0);
    let policy = crate::config_schema::policy_for_config(&file_cfg);
    let secrets = config::chat_secrets(file_cfg.as_ref());

    let mut session = match Session::new(data_dir.clone(), policy, model_policy, secrets) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open session: {e}");
            std::process::exit(1);
        }
    };

    // Attach the configured agent profile, if the config declares any. A
    // config with no `[agents]` table is not an error: those runs stay
    // anonymous, exactly as before profiles existed. A *named* agent that
    // does not resolve is a hard error, because silently running as the
    // default agent would attribute the conversation to the wrong identity.
    match file_cfg
        .as_ref()
        .and_then(|c| c.resolve_profile(None).ok().flatten().map(|eff| (c, eff)))
    {
        Some((cfg, effective)) => {
            match cfg
                .profile_registry()
                .map_err(pantheon_runtime::profile_err)
                .and_then(|reg| {
                    pantheon_runtime::AgentRuntime::new(
                        session.supervisor.clone(),
                        reg,
                        effective,
                        data_dir,
                    )
                }) {
                Ok(agent) => {
                    if let Err(e) = session.with_agent(agent) {
                        eprintln!("attach agent: {e}");
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("pantheon: agent profile: {}", e.cause);
                    eprintln!("pantheon: fix: {}", e.remediation);
                    std::process::exit(2);
                }
            }
        }
        None => {
            if let Some(cfg) = file_cfg.as_ref() {
                if let Some(named) = cfg.agent.as_deref() {
                    eprintln!("pantheon: agent {named:?} is not declared in config");
                    eprintln!("pantheon: fix: add [agents.{named}] to config.toml");
                    std::process::exit(2);
                }
            }
        }
    }

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

    let mut state = TuiState::new(pantheon_runtime::new_run_id(), header_model, tokens_max);

    // A resume id means the caller already validated the run exists, so the
    // session opens onto it. Otherwise this is a new run.
    let run_id = resume.unwrap_or_else(pantheon_runtime::new_run_id);
    if let Err(e) = session.supervisor.start_run(&run_id) {
        // The TUI is not up yet, so this goes straight to stderr and the
        // process exits: running on a supervisor that cannot open the run
        // would take turns against a broken session.
        eprintln!("pantheon: cannot start run: {e}");
        std::process::exit(1);
    }
    // The send path resolves state.session_id at send time, so it must
    // start as the run the loop opens on — not the throwaway id new() made.
    state.session_id = run_id.clone();

    let result = tui_loop(
        &mut terminal,
        &mut state,
        session,
        &tx,
        &rx,
        &running,
    );

    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)?;
    result
}

/// The run id the next turn sends to: the currently selected session.
/// Kept as a helper (not inlined) so tests pin the resolution point:
/// history-resume, /resume and /new all write `state.session_id`, and the
/// send path must follow the selection, never the run the loop opened on.
fn resolve_send_run_id(state: &TuiState) -> &str {
    &state.session_id
}

/// Spawn the worker thread for one agent turn. The run id arrives resolved
/// at send time from the caller; it is never captured from loop state, so
/// a mid-turn /resume, /new, or history-resume cannot address the turn at a
/// stale run.
fn spawn_turn(
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    session: &Arc<Session>,
    run_id: &str,
    msg: &str,
) {
    let tx2 = tx.clone();
    let run_id_owned = run_id.to_string();
    let msg_owned = msg.to_string();
    let session_owned = session.clone();
    std::thread::spawn(move || {
        match session_owned.chat(&run_id_owned, &msg_owned) {
            Ok(outcome) => match outcome {
                pantheon_agent::LoopOutcome::AwaitingApproval { .. } => {
                    // The ApprovalRequested runtime event (via the observer)
                    // carries scope; the worker just marks the turn parked.
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
}

/// Start an agent turn for `msg`, or queue it when one is already running.
/// Returns true when a turn was started. The run id resolves from the
/// currently selected session at send time.
fn start_turn(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    msg: String,
) -> bool {
    if !state.begin_turn(msg.clone()) {
        return false;
    }
    session.reset_cancel();
    let run_id = resolve_send_run_id(state).to_string();
    state.active_run = Some(run_id.clone());
    spawn_turn(tx, session, &run_id, &msg);
    true
}

/// Fire a message queued while a turn was running, now that the turn is
/// over. Skipped while an approval is pending: the parked run must be
/// resolved first, and the queue waits for the turn that follows it.
fn drain_queued_message(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
) {
    if state.pending_approval.is_some() {
        return;
    }
    // The ApprovalRequested event and the worker's TurnComplete race on the
    // channel; the supervisor's pending list is the ground truth for
    // whether the selected run is parked, so a queued message never starts
    // a turn on a run that is waiting for a decision.
    let parked = session
        .supervisor
        .pending_approvals(&state.session_id)
        .map(|p| !p.is_empty())
        .unwrap_or(false);
    if parked {
        return;
    }
    if let Some(msg) = state.take_queued() {
        let _ = start_turn(state, session, tx, msg);
    }
}

/// Send raw text as a message: slash commands dispatch, anything else
/// becomes a user turn. Shared by the Enter key and the fullscreen draft
/// editor's submit, so both paths obey the same Enter guard.
fn submit_text(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    raw: &str,
) {
    let msg = raw.trim().to_string();
    state.input.clear();
    state.is_inputting = false;
    if msg.is_empty() {
        return;
    }

    if msg.starts_with('/') {
        handle_slash(state, session, &msg, tx);
        return;
    }

    state.add_user_message(msg.clone());

    // No second loop on a running turn: begin_turn queues the message
    // instead, and the queue drains when the turn ends. start_turn
    // resolves the run id at send time, so a resume that happened
    // mid-turn sends to the run that is selected now, not the one the
    // turn used.
    if !start_turn(state, session, tx, msg) {
        state.add_status("already working — queued for after this turn".into());
    }
}

/// Confirm the pending rewind: append the marker to the ledger, then
/// truncate the live view back to the pre-turn state.
///
/// The ledger is append-only — history is hidden from the view, never
/// rewritten — and the discarded user message returns as the input draft
/// so the turn can be redone. The marker write gates the truncation: if
/// it fails, the view keeps the turn and says why.
fn do_rewind(state: &mut TuiState, session: &Arc<Session>) {
    let Some(offer) = state.rewind_offer.take() else {
        return;
    };
    let detail = format!(
        "rewind: operator discarded turn {} from the live view; ledger history retained",
        offer.turn_no
    );
    if let Err(e) = session.supervisor.emit(RuntimeErrorEvent::RunProgress {
        run_id: state.session_id.clone(),
        detail,
    }) {
        state.add_status(format!("rewind: ledger marker failed, turn kept: {e}"));
        return;
    }
    let draft = match state.blocks.get(offer.block_index) {
        Some(TranscriptBlock {
            kind: BlockKind::UserMessage(text),
        }) => text.clone(),
        _ => String::new(),
    };
    state.blocks.truncate(offer.block_index);
    state.input = draft;
    state.is_inputting = false;
    state.turn_in = None;
    state.turn_out = None;
    state.turn_estimate = 0;
    state.turn_started_at = None;
    state.turns_completed = state.turns_completed.saturating_sub(1);
    state.timeline = None;
    state.jump_to_block = None;
    state.scroll_to_bottom();
    state.status_line = "ready".into();
    state.add_status(format!(
        "↩ rewound turn {} — hidden from view; ledger keeps full history",
        offer.turn_no
    ));
}

/// Keys while the fullscreen draft editor is open. Submit sends through
/// the same path as Enter; Esc cancels and keeps the draft in the input
/// box.
fn handle_editor_key(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    key: crossterm::event::KeyEvent,
) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => {
            // Cancel: the draft survives in the input box.
            if let Some(ed) = state.editor.take() {
                state.input = ed.text();
            }
            state.is_inputting = false;
            state.status_line = "ready".into();
        }
        KeyCode::Enter if ctrl => {
            if let Some(ed) = state.editor.take() {
                let text = ed.text();
                submit_text(state, session, tx, &text);
            }
        }
        KeyCode::Char('s') | KeyCode::Char('S') if ctrl => {
            if let Some(ed) = state.editor.take() {
                let text = ed.text();
                submit_text(state, session, tx, &text);
            }
        }
        _ => {
            let Some(ed) = state.editor.as_mut() else {
                return;
            };
            match key.code {
                KeyCode::Char(c) if !ctrl => ed.insert_char(c),
                KeyCode::Enter => ed.newline(),
                KeyCode::Backspace => ed.backspace(),
                KeyCode::Delete => ed.delete(),
                KeyCode::Left => ed.move_left(),
                KeyCode::Right => ed.move_right(),
                KeyCode::Up => ed.move_up(),
                KeyCode::Down => ed.move_down(),
                KeyCode::Home => ed.home(),
                KeyCode::End => ed.end(),
                KeyCode::Tab => {
                    ed.insert_char(' ');
                    ed.insert_char(' ');
                }
                _ => {}
            }
        }
    }
}

fn tui_loop(
    terminal: &mut DefaultTerminal,
    state: &mut TuiState,
    session: Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    rx: &std::sync::mpsc::Receiver<TuiEvent>,
    running: &Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error>> {
    refresh_tabs(state, &session);
    loop {
        if !running.load(Ordering::SeqCst) || state.shutting_down {
            break;
        }

        while let Ok(ev) = rx.try_recv() {
            match ev {
                TuiEvent::Model(me) => state.handle_model_event(me),
                TuiEvent::Runtime(re) => state.handle_runtime_event(&re),
                TuiEvent::Answered(text) => {
                    if !text.trim().is_empty() {
                        state.blocks.push(TranscriptBlock {
                            kind: BlockKind::AssistantMessage(text),
                        });
                    }
                }
                TuiEvent::TurnComplete => {
                    // Focused: the single-session loop is always the
                    // visible tab. The tab layer calls on_turn_complete
                    // with `focused = false` for background sessions, which
                    // raises the tab badge instead of touching input.
                    state.on_turn_complete(true);
                    session.reset_cancel();
                    // A message typed while the turn ran goes out now, to
                    // the currently selected run.
                    drain_queued_message(state, &session, tx);
                    // Titles/busy badges may have changed.
                    refresh_tabs(state, &session);
                }
                TuiEvent::Error(msg) => {
                    state.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(format!("error: {msg}")),
                    });
                    state.ready = true;
                    state.active_run = None;
                    drain_queued_message(state, &session, tx);
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
                    state.active_run = None;
                    // The user stopped the work; do not surprise them by
                    // firing a queued message. Say it was dropped.
                    if state.take_queued().is_some() {
                        state.blocks.push(TranscriptBlock {
                            kind: BlockKind::Status("interrupted — dropped queued message".into()),
                        });
                    }
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
                if state.models.is_some() {
                    match key.code {
                        KeyCode::Char(c) => state.models_input.push(c),
                        KeyCode::Backspace => {
                            state.models_input.pop();
                        }
                        KeyCode::Up => state.models_move(-1),
                        KeyCode::Down => state.models_move(1),
                        KeyCode::Esc => state.models_close(),
                        KeyCode::Enter => {
                            let sel = state.models_sel;
                            let target = state.filtered_models().get(sel).cloned();
                            state.models_close();
                            if let Some(row) = target {
                                if row.model_id.is_empty() {
                                    state.add_status(format!(
                                        "{} lists no curated models; switch with: /model {} <id>",
                                        row.provider_id, row.provider_id
                                    ));
                                } else {
                                    apply_model_switch(
                                        state,
                                        &session,
                                        &row.provider_id,
                                        &row.model_id,
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
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
                                // Compare against the selected session, not
                                // the run the loop opened on.
                                switch_to_run(state, &session, &id);
                                refresh_tabs(state, &session);
                            }
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                if state.editor.is_some() {
                    handle_editor_key(state, &session, tx, key);
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                if state.rewind_offer.is_some() {
                    // Explicit confirm: y rewinds, n/Esc dismisses.
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => do_rewind(state, &session),
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            state.rewind_offer = None;
                            state.status_line = "ready".into();
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                if state.timeline.is_some() {
                    let n = timeline::build_turns(&state.blocks).len();
                    match key.code {
                        KeyCode::Up => {
                            if let Some(nav) = state.timeline.as_mut() {
                                nav.move_sel(-1, n);
                            }
                        }
                        KeyCode::Down => {
                            if let Some(nav) = state.timeline.as_mut() {
                                nav.move_sel(1, n);
                            }
                        }
                        KeyCode::Enter => {
                            let target = state.timeline.as_ref().and_then(|nav| {
                                timeline::build_turns(&state.blocks)
                                    .get(nav.sel())
                                    .map(|t| t.block_start)
                            });
                            state.timeline = None;
                            // Read-only jump: the viewport moves, the
                            // transcript does not.
                            state.jump_to_block = target;
                            state.status_line = "ready".into();
                        }
                        KeyCode::Esc | KeyCode::F(2) => {
                            state.timeline = None;
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('o') | KeyCode::Char('O') => state.toggle_timeline(),
                        KeyCode::Char('e') | KeyCode::Char('E') => {
                            if state.pending_approval.is_none() {
                                state.open_editor();
                            }
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                // Session tabs: Ctrl+Tab cycle, Alt+1..9 jump. Checked before
                // the Char handler (Alt+1 arrives as Char('1')+ALT).
                if let Some(action) = crate::tabs::tab_key_action(key.code, key.modifiers) {
                    let target: Option<String> = match action {
                        crate::tabs::TabAction::Next => {
                            state.tabs.cycle_next().map(str::to_string)
                        }
                        crate::tabs::TabAction::Prev => {
                            state.tabs.cycle_prev().map(str::to_string)
                        }
                        crate::tabs::TabAction::Jump(n) => {
                            if state.tabs.jump(n) {
                                state.tabs.active_run_id().map(str::to_string)
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(id) = target {
                        switch_to_run(state, &session, &id);
                        refresh_tabs(state, &session);
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
                                        state.active_run = Some(run3.clone());
                                        std::thread::spawn(move || {
                                            match sess3.chat_turn(&run3, "", "") {
                                                Ok(outcome) => {
                                                    // Carry the answer through the
                                                    // same event the normal
                                                    // worker path uses, so a
                                                    // resumed turn renders its
                                                    // result instead of going
                                                    // silent once the approval
                                                    // clears.
                                                    let text = match outcome {
                                                        pantheon_agent::LoopOutcome::Answered {
                                                            text,
                                                            ..
                                                        } => text,
                                                        _ => String::new(),
                                                    };
                                                    let _ = tx3.send(TuiEvent::Answered(text));
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
                                        state.active_run = None;
                                        // Deny ends the parked turn without a
                                        // TurnComplete event, so drain here.
                                        drain_queued_message(state, &session, tx);
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
                            let raw = state.input.clone();
                            submit_text(state, &session, tx, &raw);
                        } else {
                            state.is_inputting = true;
                        }
                    }
                    KeyCode::Esc => {
                        state.is_inputting = false;
                        // Double-Esc interrupts the active run. The arm and
                        // confirm rules live in `press_esc` so they are
                        // testable without a terminal.
                        if state.press_esc() {
                            let target = state.interrupt_target().to_string();
                            session.cancel_current_run(&target, "user pressed esc twice");
                        }
                    }
                    KeyCode::PageUp if state.pending_approval.is_none() => {
                        // The permission card owns the keyboard while it is
                        // up; anything else would type into a decision.
                        state.scroll_up(10);
                    }
                    KeyCode::PageDown if state.pending_approval.is_none() => {
                        state.scroll_down(10);
                    }
                    KeyCode::F(2) => state.toggle_timeline(),
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

/// Switch the session to a named agent profile.
///
/// Reloads the config so `/agent` reflects an edit the user just made to
/// `config.toml` without needing a restart, which is what makes profile
/// setup feel like a live operation rather than a reinstall.
fn switch_agent(state: &mut TuiState, session: &Arc<Session>, name: &str) {
    let cfg = match crate::config::Config::load_or_report(&crate::terminal::data_dir()) {
        Some(c) => c,
        None => {
            state.add_status("/agent: no config; declare [agents.<name>] first".into());
            return;
        }
    };
    let registry = match cfg.profile_registry() {
        Ok(r) => r,
        Err(e) => {
            state.add_status(format!("/agent: {e}"));
            return;
        }
    };
    let agent = match agent_for(session, &cfg, registry, name) {
        Ok(a) => a,
        Err(msg) => {
            state.add_status(format!("/agent: {msg}"));
            return;
        }
    };
    match session.switch_agent(agent) {
        Ok(()) => state.add_status(format!("now talking to {name}")),
        Err(e) => state.add_status(format!("/agent: {}", e.cause)),
    }
}

/// Build a runtime for `name` from a freshly loaded config.
fn agent_for(
    session: &Arc<Session>,
    cfg: &crate::config::Config,
    registry: pantheon_agent::agent_profile::ProfileRegistry,
    name: &str,
) -> Result<pantheon_runtime::AgentRuntime, String> {
    let preset = cfg.policy.map(|p| p.as_str()).unwrap_or("coder");
    let effective = registry.resolve(name, preset).map_err(|e| e.to_string())?;
    pantheon_runtime::AgentRuntime::new(
        session.supervisor.clone(),
        registry,
        effective,
        crate::terminal::data_dir(),
    )
    .map_err(|e| e.cause)
}

/// Declared profiles, flagged with the current one.
fn agent_profiles(session: &Arc<Session>) -> Vec<(String, bool)> {
    let Some(cfg) = crate::config::Config::load_or_report(&crate::terminal::data_dir()) else {
        return Vec::new();
    };
    let Ok(registry) = cfg.profile_registry() else {
        return Vec::new();
    };
    let current = session.agent().map(|a| a.profile().name.clone());
    registry
        .names()
        .into_iter()
        .map(|n| {
            let cur = Some(n.clone()) == current;
            (n, cur)
        })
        .collect()
}

/// Active collaborations with a one-line progress summary each.
fn show_collaborations(state: &mut TuiState) {
    let store = match collaboration_store() {
        Ok(s) => s,
        Err(msg) => {
            state.add_status(format!("/collab: {msg}"));
            return;
        }
    };
    let active = match store.active_collaborations() {
        Ok(c) => c,
        Err(e) => {
            state.add_status(format!("/collab: {}", e.cause));
            return;
        }
    };
    if active.is_empty() {
        state.add_status("no active collaborations".into());
        return;
    }
    for c in active {
        let tasks = store
            .tasks_in_collaboration(&c.collaboration_id)
            .unwrap_or_default();
        let done = tasks.iter().filter(|t| t.status.is_terminal()).count();
        state.add_status(format!(
            "{} \u{2022} {} \u{2022} {done}/{} tasks done",
            c.collaboration_id,
            c.objective,
            tasks.len()
        ));
        for t in tasks {
            state.add_status(format!(
                "  {} {} \u{2192} {} [{}]",
                t.task_id,
                t.origin_agent,
                t.assigned_agent.as_deref().unwrap_or("unassigned"),
                t.status
            ));
        }
    }
}

/// One agent's open tasks.
fn show_tasks(state: &mut TuiState, agent: &str) {
    let store = match collaboration_store() {
        Ok(s) => s,
        Err(msg) => {
            state.add_status(format!("/tasks: {msg}"));
            return;
        }
    };
    let tasks = match store.tasks_for_agent(agent, None) {
        Ok(t) => t,
        Err(e) => {
            state.add_status(format!("/tasks: {}", e.cause));
            return;
        }
    };
    if tasks.is_empty() {
        state.add_status(format!("{agent} has no open tasks"));
        return;
    }
    for t in tasks {
        state.add_status(format!(
            "{} [{}] {}",
            t.task_id,
            t.status,
            t.objective.chars().take(70).collect::<String>()
        ));
    }
}

/// Unread messages addressed to the current agent.
fn show_inbox(state: &mut TuiState, session: &Arc<Session>) {
    let Some(agent) = session.agent() else {
        state.add_status("/inbox: no agent profile attached".into());
        return;
    };
    match agent.inbox() {
        Ok(messages) if messages.is_empty() => {
            state.add_status("inbox is empty".into());
        }
        Ok(messages) => {
            for m in messages {
                state.add_status(format!(
                    "from {} [{}] {}",
                    m.sender,
                    m.kind.as_str(),
                    m.content.chars().take(70).collect::<String>()
                ));
            }
        }
        Err(e) => state.add_status(format!("/inbox: {}", e.cause)),
    }
}

/// The durable collaboration store the TUI reads.
///
/// Opens by path rather than going through the attached agent: `/collab`
/// and `/tasks <other agent>` are inspection commands that must work when
/// no agent is attached, and they are strictly read-only.
fn collaboration_store() -> Result<pantheon_storage::CollaborationStore, String> {
    pantheon_storage::CollaborationStore::open(
        &crate::terminal::data_dir().join("collaboration.db"),
    )
    .map_err(|e| e.cause)
}

/// Every selectable row for the /models browser: one per curated model,
/// plus one per provider with no curated models (empty `model_id`; Enter
/// on those explains the explicit-id path instead of switching).
fn build_model_rows() -> Vec<ModelRow> {
    let mut rows = Vec::new();
    for p in pantheon_providers::catalog::selectable_providers() {
        if p.models.is_empty() {
            rows.push(ModelRow {
                provider_id: p.id.clone(),
                provider_label: p.label.clone(),
                model_id: String::new(),
                ctx: None,
            });
        } else {
            for m in &p.models {
                rows.push(ModelRow {
                    provider_id: p.id.clone(),
                    provider_label: p.label.clone(),
                    model_id: m.model.clone(),
                    ctx: m.context_limit,
                });
            }
        }
    }
    rows
}

/// Persist a model switch to `[model]` in config.toml. Only provider and
/// model are touched: fallbacks, auxiliaries, keys, and agents survive.
/// A missing `[model]` section is created rather than failing, so a
/// switch works on a config that predates the section.
fn persist_model_choice(dd: &std::path::Path, provider: &str, model: &str) -> Result<(), String> {
    let mut cfg = crate::config::Config::load(dd).unwrap_or_default();
    match cfg.model.as_mut() {
        Some(m) => {
            m.provider = provider.to_string();
            m.model = model.to_string();
        }
        None => {
            cfg.model = Some(crate::config::ModelSection {
                reasoning_budget: None,
                provider: provider.to_string(),
                model: model.to_string(),
                api_key_env: None,
                fallbacks: Vec::new(),
                reasoning: None,
            });
        }
    }
    cfg.save(dd).map_err(|e| e.cause.clone())
}

/// Switch the live session to a new default model and say exactly what
/// happened: session-only vs saved, plus a key warning when the catalog
/// names a key env var that is not set in this shell.
fn apply_model_switch(state: &mut TuiState, session: &Arc<Session>, provider: &str, model: &str) {
    if let Err(e) = session.switch_model(provider, model) {
        state.add_status(format!("/model: {e}"));
        return;
    }
    state.model = format!("{provider}/{model}");
    state.tokens_max = pantheon_providers::catalog::model_meta(provider, model)
        .context_limit
        .unwrap_or(0);
    match persist_model_choice(&crate::terminal::data_dir(), provider, model) {
        Ok(()) => state.add_status(format!("model → {provider}/{model} (saved)")),
        Err(e) => state.add_status(format!(
            "model → {provider}/{model} for this session; save failed: {e}"
        )),
    }
    match pantheon_providers::catalog::provider(provider) {
        Some(p) if !p.key_env.trim().is_empty() => {
            let missing = std::env::var(&p.key_env)
                .ok()
                .filter(|v| !v.is_empty())
                .is_none();
            if missing {
                state.add_status(format!(
                    "{} is not set; export it before chatting on {provider}/{model}",
                    p.key_env
                ));
            }
        }
        None if !provider.starts_with("http") => {
            state.add_status(format!(
                "warning: '{provider}' is not a catalog provider; requests may fail"
            ));
        }
        _ => {}
    }
}

/// Render transcript blocks for /export. Markdown keeps readable
/// structure; json is one object per line for scripts. Pure, so tests
/// cover the mapping without touching the filesystem.
fn export_transcript(blocks: &[TranscriptBlock], session_id: &str, format: &str) -> String {
    fn row(b: &TranscriptBlock) -> (&'static str, String) {
        match &b.kind {
            BlockKind::UserMessage(t) => ("user", t.clone()),
            BlockKind::AssistantMessage(t) => ("agent", t.clone()),
            BlockKind::Thinking(t) => ("thinking", t.lines().next().unwrap_or("").to_string()),
            BlockKind::ToolCall { name, args, ok } => {
                let st = match ok {
                    None => "running",
                    Some(true) => "done",
                    Some(false) => "failed",
                };
                ("tool", format!("{name} [{st}] {args}"))
            }
            BlockKind::Swarm { agents, task } => ("swarm", format!("×{agents} {task}")),
            BlockKind::Status(t) => ("status", t.clone()),
        }
    }
    if format == "json" {
        return blocks
            .iter()
            .map(|b| {
                let (kind, text) = row(b);
                serde_json::json!({"session": session_id, "kind": kind, "text": text}).to_string()
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    let mut out = format!("# Pantheon session {session_id}\n");
    for b in blocks {
        out.push_str(&match row(b) {
            ("user", t) => format!("\n## you\n\n{t}\n"),
            ("agent", t) => format!("\n## agent\n\n{t}\n"),
            ("thinking", t) => format!("\n> thinking: {t}\n"),
            ("tool", t) => format!("\n`{t}`\n"),
            ("swarm", t) => format!("\n## swarm\n\n{t}\n"),
            (_, t) => format!("\n*{t}*\n"),
        });
    }
    out
}

/// Persist reasoning state to `[model]`. `Off` removes the level key so
/// the config stays clean; anything else writes the level name. The
/// budget is written alongside (or removed when `None`) so setting a
/// level never silently wipes a budget and vice versa.
fn persist_reasoning(
    dd: &std::path::Path,
    level: pantheon_api::model::ReasoningLevel,
    budget: Option<u32>,
) -> Result<(), String> {
    use pantheon_api::model::ReasoningLevel;
    let mut cfg = crate::config::Config::load(dd).unwrap_or_default();
    let Some(model) = cfg.model.as_mut() else {
        return Err("no [model] section yet; set a model first (/model P M)".into());
    };
    model.reasoning = match level {
        ReasoningLevel::Off => None,
        _ => Some(level.as_str().to_string()),
    };
    model.reasoning_budget = budget;
    cfg.save(dd).map_err(|e| e.cause.clone())
}

fn handle_slash(
    state: &mut TuiState,
    session: &Arc<Session>,
    cmd: &str,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
) {
    let supervisor = &session.supervisor;
    // /help shows the real command surface.
    // --- agent profiles and collaboration -------------------------------
    // One command per question, answering in the transcript. No panels:
    // collaboration state is occasional, and a permanent dashboard for it
    // would be noise in every other conversation.
    if cmd == "/agent" {
        match session.agent() {
            Some(a) => {
                let p = a.profile();
                state.add_status(format!(
                    "agent {} ({}) \u{2022} memory {}",
                    p.display_name.value, p.agent_id, p.memory_namespace.value
                ));
                if let Some(parent) = &p.parent {
                    state.add_status(format!("inherits {parent}"));
                }
                state.add_status("switch with: /agent <profile>".into());
            }
            None => {
                state.add_status("no agent profile attached (config has no [agents] table)".into())
            }
        }
        return;
    }
    if let Some(name) = cmd.strip_prefix("/agent ") {
        let name = name.trim();
        switch_agent(state, session, name);
        return;
    }
    if cmd == "/agents" {
        // List the declared profiles and mark the current one. Names come
        // from the registry, so an agent that exists at runtime is exactly
        // an agent declared in config.
        let profiles = agent_profiles(session);
        if profiles.is_empty() {
            state.add_status("no agent profiles declared (add [agents.<name>])".into());
        } else {
            state.add_status("agent profiles:".into());
            for (n, current) in profiles {
                state.add_status(format!("  {}{n}", if current { " (current)" } else { "" }));
            }
        }
        return;
    }
    if cmd == "/collab" {
        show_collaborations(state);
        return;
    }
    if let Some(agent) = cmd.strip_prefix("/tasks ") {
        show_tasks(state, agent.trim());
        return;
    }
    if cmd == "/inbox" {
        show_inbox(state, session);
        return;
    }
    if cmd == "/help" {
        state.add_status("commands:".into());
        state.add_status("  /help              this list".into());
        state.add_status("  /models [FILTER]   browse providers and models, Enter switches".into());
        state.add_status("  /model [P M]       show the current model, or switch to one".into());
        state.add_status(
            "  /reasoning [LVL]   reasoning effort: off|minimal|low|medium|high|xhigh|max".into(),
        );
        state.add_status("  /remember KEY TEXT remember this (agent memory, user trust)".into());
        state.add_status("  /skills [FILTER]   installed skills".into());
        state.add_status(
            "  /settings          data dir, model, policy, memory, server, agents".into(),
        );
        state.add_status("  /gateway           service state and queued outbound".into());
        state.add_status("  /doctor            diagnose this install".into());
        state.add_status("  /sessions          live sessions holding a lease".into());
        state.add_status("  /new               start a fresh conversation".into());
        state.add_status("  /rewind            roll back the last turn (confirm; ledger kept)".into());
        state.add_status("  /compress         compress this conversation to the window now".into());
        state.add_status("  /export [md|json]  save this conversation to exports/".into());
        state.add_status("  /runs [N]          recent runs (default 10)".into());
        state.add_status(
            "  /history           interactive searchable history (pick + resume)".into(),
        );
        state.add_status("  /resume [ID]       resume a run by id".into());
        state
            .add_status("  /name [TITLE]      show this conversation's title, or rename it".into());
        state.add_status("  /status [run_id]   this run's status, or another by id".into());
        state.add_status("  /agent [name]     current agent profile, or switch to one".into());
        state.add_status("  /agents           declared agent profiles".into());
        state.add_status("  /collab           active collaborations and their tasks".into());
        state.add_status("  /tasks <agent>    that agent's open tasks".into());
        state.add_status("  /inbox            messages sent to this agent".into());
        state.add_status("  /approvals         pending approvals for this run".into());
        state.add_status("  /schedule          scheduled jobs".into());
        state.add_status("  /mcp               MCP server declarations".into());
        state.add_status(
            "  /migrate           import from other harnesses (hermes, openclaw, omp, claude)"
                .into(),
        );
        state.add_status(
            "  /migrate           import from other harnesses (hermes, openclaw, omp, claude)"
                .into(),
        );
        state.add_status(
            "  /migrate           import from other harnesses (hermes, openclaw, omp, claude)"
                .into(),
        );
        state.add_status("  /env               secret names and status (never values)".into());
        state.add_status("  /clear             clear visible transcript".into());
        state.add_status("  PgUp/PgDn          scroll the transcript".into());
        state.add_status("  Ctrl+O / F2        turn timeline: arrows move, Enter jumps".into());
        state.add_status("  Ctrl+E             fullscreen draft editor (Ctrl+Enter sends)".into());
        state.add_status("  Esc Esc (idle)     offer to rewind the last turn".into());
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
        let title = pantheon_api::model::bound_title(rest, pantheon_api::model::TITLE_MAX_CHARS);
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
        let ev = pantheon_api::events::Event::SessionTitled {
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
                                pantheon_api::message::Role::User => {
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
                // Do not force ready here: a turn may still be running for
                // the previous run, and the Enter guard must keep applying
                // to it.
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
                // The current run has no ledger rows until the first message
                // is sent, so on a fresh session /runs claimed there were no
                // runs while the user was plainly sitting in one. Show it,
                // flagged, rather than contradicting what they just saw.
                let listed: std::collections::HashSet<&str> =
                    runs.iter().map(|(id, ..)| id.as_str()).collect();
                if !state.session_id.is_empty() && !listed.contains(state.session_id.as_str()) {
                    state.add_status(format!(
                        "\u{25d0} {}  (this run, not yet saved)",
                        state.session_id
                    ));
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
        match supervisor.render_run_log(id) {
            Ok(s) => state.add_status(s),
            Err(e) => state.add_status(format!("status: {e}")),
        }
        return;
    }
    if cmd == "/status" {
        // Bare `/status` reports the run you are sitting in. The `pantheon
        // status <run_id>` verb is gone, so this is the only way to ask
        // that from inside a session; `/status <id>` still takes any run.
        let id = state.session_id.clone();
        let status = supervisor
            .ledger_status(&id)
            .ok()
            .flatten()
            .unwrap_or_else(|| "new".into());
        match supervisor
            .ledger_title(&id)
            .ok()
            .flatten()
            .filter(|t| !t.is_empty())
        {
            Some(title) => state.add_status(format!("run {id} ({status}) — {title}")),
            None => state.add_status(format!("run {id} ({status})")),
        }
        return;
    }
    if cmd == "/models" || cmd.starts_with("/models ") {
        // Optional filter: `/models anth` opens the browser already
        // filtered, the same as opening it and typing.
        let filter = cmd.strip_prefix("/models").unwrap_or("").trim().to_string();
        let rows = build_model_rows();
        if rows.is_empty() {
            state.add_status("no models in the provider catalog".into());
            return;
        }
        state.models = Some(rows);
        state.models_input = filter;
        state.models_sel = 0;
        return;
    }
    if cmd == "/model" {
        let (p, m) = session.default_model();
        state.add_status(format!("model: {p}/{m}"));
        state.add_status("switch with: /model <provider> <id>  (or /models to browse)".into());
        return;
    }
    if let Some(rest) = cmd.strip_prefix("/model ") {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() != 2 {
            state.add_status("usage: /model <provider> <id>  (or /models to browse)".into());
            return;
        }
        apply_model_switch(state, session, parts[0], parts[1]);
        return;
    }
    if cmd == "/reasoning" {
        let level = session.reasoning();
        match session.reasoning_budget() {
            Some(b) => state.add_status(format!(
                "reasoning: {} + budget {b} (budget wins on budget wires)",
                level.as_str()
            )),
            None => state.add_status(format!("reasoning: {}", level.as_str())),
        }
        state.add_status(
            "set with: /reasoning off|minimal|low|medium|high|xhigh|max  (saved to [model])".into(),
        );
        state.add_status(
            "exact budget with: /reasoning budget <tokens>|off  (Anthropic wire only)".into(),
        );
        state.add_status(
            "maps to reasoning_effort (OpenAI wire) or a thinking budget (Anthropic wire); endpoints without support may ignore or reject it".into(),
        );
        return;
    }
    if let Some(rest) = cmd.strip_prefix("/reasoning budget") {
        let arg = rest.trim();
        let budget = if arg == "off" || arg == "clear" || arg == "none" {
            None
        } else if let Ok(n) = arg.parse::<u32>() {
            if n == 0 {
                None
            } else {
                Some(n)
            }
        } else {
            state.add_status("usage: /reasoning budget <tokens>|off".into());
            return;
        };
        if let Err(e) = session.set_reasoning_budget(budget) {
            state.add_status(format!("/reasoning: {e}"));
            return;
        }
        match persist_reasoning(&crate::terminal::data_dir(), session.reasoning(), budget) {
            Ok(()) => state.add_status(match budget {
                Some(b) => format!("reasoning budget → {b} (saved)"),
                None => "reasoning budget cleared (saved)".into(),
            }),
            Err(e) => state.add_status(format!("budget set for this session; save failed: {e}")),
        }
        return;
    }
    if let Some(rest) = cmd.strip_prefix("/reasoning ") {
        let arg = rest.trim();
        match pantheon_api::model::ReasoningLevel::parse(arg) {
            Some(level) => {
                if let Err(e) = session.set_reasoning(level) {
                    state.add_status(format!("/reasoning: {e}"));
                    return;
                }
                // Carry the live budget through: setting a level must not
                // silently wipe an override set earlier (and vice versa).
                match persist_reasoning(
                    &crate::terminal::data_dir(),
                    level,
                    session.reasoning_budget(),
                ) {
                    Ok(()) => state.add_status(format!("reasoning → {} (saved)", level.as_str())),
                    Err(e) => state.add_status(format!(
                        "reasoning → {} for this session; save failed: {e}",
                        level.as_str()
                    )),
                }
            }
            None => {
                state.add_status("usage: /reasoning off|minimal|low|medium|high|xhigh|max".into());
            }
        }
        return;
    }
    if cmd == "/new" {
        // A fresh conversation is a fresh run id with an empty transcript.
        // No ledger row is created until the first turn, so abandoned
        // /news leave nothing behind.
        state.session_id = pantheon_runtime::new_run_id();
        state.blocks.clear();
        state.title = None;
        state.scroll_offset = 0;
        // Do not force ready here: a turn may still be running for the
        // previous run, and the Enter guard must keep applying to it.
        state.add_status("new conversation (unsaved until the first turn)".into());
        refresh_tabs(state, session);
        return;
    }
    if cmd == "/rewind" {
        // Same confirm flow as double-Esc on an idle session: the ledger
        // is append-only, so rewind hides the last turn from the live view
        // behind a rewind marker instead of rewriting history.
        match state.rewind_candidate() {
            Some(offer) => {
                state.rewind_offer = Some(offer);
                state.status_line = "rewind last turn? [y]es [n]o".into();
            }
            None => state.add_status("/rewind: no finished turn to rewind".into()),
        }
        return;
    }
    if let Some(rest) = cmd.strip_prefix("/remember ") {
        let rest = rest.trim();
        let Some(sp) = rest.find(char::is_whitespace) else {
            state.add_status("usage: /remember KEY TEXT...".into());
            return;
        };
        let (key, value) = (rest[..sp].trim(), rest[sp..].trim());
        if value.is_empty() {
            state.add_status("usage: /remember KEY TEXT...".into());
            return;
        }
        let Some(store) = session.memory.as_ref() else {
            state.add_status("/remember: no memory store in this session".into());
            return;
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let proposal = pantheon_memory::Proposal {
            layer: pantheon_memory::LayerKind::Agent,
            namespace: session.memory_namespace.clone(),
            key: key.to_string(),
            value: value.to_string(),
            provenance: pantheon_memory::Provenance {
                source: "tui".into(),
                origin: "user".into(),
                trust: pantheon_api::provenance::TrustTier::User,
                recorded_at_ms: now_ms,
            },
        };
        match pantheon_memory::write_via(store.as_ref(), &session.policy, proposal, 4096) {
            Ok(rec) => state.add_status(format!("remembered {}", rec.key)),
            Err(e) => state.add_status(format!("/remember: {e}")),
        }
        return;
    }
    if cmd == "/remember" {
        state.add_status("usage: /remember KEY TEXT...".into());
        return;
    }
    if cmd == "/skills" || cmd.starts_with("/skills ") {
        let filter = cmd
            .strip_prefix("/skills")
            .unwrap_or("")
            .trim()
            .to_lowercase();
        let dd = crate::terminal::data_dir();
        let found = crate::skills::extra_roots();
        let skills =
            pantheon_exec::skills::discover_skills_ext(&dd, &crate::skills::project_root(), &found);
        if skills.is_empty() {
            state.add_status("no skills installed".into());
            return;
        }
        let mut shown = 0;
        for s in &skills {
            if !filter.is_empty()
                && !s.meta.name.to_lowercase().contains(&filter)
                && !s.meta.description.to_lowercase().contains(&filter)
            {
                continue;
            }
            let desc: String = s.meta.description.chars().take(70).collect();
            state.add_status(format!("{} [{}] {desc}", s.meta.name, s.meta.origin));
            shown += 1;
        }
        if shown == 0 {
            state.add_status(format!("no skills match '{filter}'"));
        } else {
            state.add_status(format!("{shown} skill(s)"));
        }
        return;
    }
    if cmd == "/settings" {
        let dd = crate::terminal::data_dir();
        match crate::config::Config::load(&dd) {
            Ok(cfg) => {
                let (provider, model) = session.default_model();
                state.add_status(format!("data dir: {}", dd.display()));
                state.add_status(format!("model: {provider}/{model} (live)"));
                state.add_status(format!(
                    "policy: {}",
                    cfg.policy.map(|p| p.as_str()).unwrap_or("coder")
                ));
                state.add_status(format!(
                    "memory: {}",
                    cfg.memory
                        .as_ref()
                        .map(|m| m.backend.as_str())
                        .unwrap_or("native")
                ));
                state.add_status(format!(
                    "server: {}",
                    cfg.server
                        .as_ref()
                        .map(|s| format!("{}:{}", s.host, s.port))
                        .unwrap_or_else(|| "unset".into())
                ));
                state.add_status(format!(
                    "agents: {}",
                    if cfg.agents.is_empty() {
                        "none declared (anonymous runs)".into()
                    } else {
                        let mut names: Vec<&String> = cfg.agents.keys().collect();
                        names.sort();
                        names
                            .iter()
                            .map(|n| n.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                ));
            }
            Err(e) => state.add_status(format!("/settings: {}", e.cause)),
        }
        return;
    }
    if cmd == "/gateway" {
        let st = crate::gateway::gateway_status();
        state.add_status(format!(
            "gateway service: {}",
            if st.active {
                "active"
            } else if st.installed {
                "installed, not active (pantheon gateway start)"
            } else {
                "not installed (pantheon gateway start)"
            }
        ));
        state.add_status(format!("outbox: {} queued", st.outbox_pending));
        return;
    }
    if cmd == "/doctor" {
        let rep = crate::doctor::run_system_doctor(&crate::terminal::data_dir());
        for c in &rep.checks {
            let glyph = match c.status.as_str() {
                "ok" => "\u{2713}",
                "warn" => "\u{26a0}",
                _ => "\u{17d}",
            };
            let mut line = format!("{glyph} {}: {}", c.section, c.detail);
            if !c.fix.is_empty() && c.status != "ok" {
                line.push_str(&format!(" (fix: {})", c.fix));
            }
            state.add_status(line);
        }
        state.add_status(if rep.ok {
            "doctor: healthy".into()
        } else {
            "doctor: issues found (see fixes above)".into()
        });
        return;
    }
    if cmd == "/sessions" {
        match supervisor.ledger_list_runs(50) {
            Ok(runs) => {
                let mut live = 0;
                for (run_id, status, _ts, title) in &runs {
                    let active = supervisor.has_active_lease(run_id).unwrap_or(false);
                    if !active {
                        continue;
                    }
                    live += 1;
                    let label = title
                        .as_deref()
                        .filter(|t| !t.is_empty())
                        .map(|t| t.chars().take(30).collect::<String>())
                        .unwrap_or_else(|| run_id.chars().skip(4).take(8).collect());
                    let here = if *run_id == state.session_id {
                        " (this session)"
                    } else {
                        ""
                    };
                    state.add_status(format!("\u{25cf} {label}  {status}{here}"));
                }
                if live == 0 {
                    state.add_status("no live sessions (this one holds no lease yet)".into());
                }
            }
            Err(e) => state.add_status(format!("sessions: {e}")),
        }
        return;
    }
    if cmd == "/compress" {
        if state.session_id.is_empty() {
            state.add_status("/compress: no conversation yet".into());
            return;
        }
        // Model-assisted compression may spend one compression-model call,
        // exactly as a turn crossing the threshold would. The command is
        // the consent; the report below says what ran.
        state.add_status("compressing (may call the compression model)...".into());
        match session.compress_now(&state.session_id) {
            Ok(rep) if rep.unknown_window => {
                state.add_status("/compress: model has no cataloged window; nothing to fit".into())
            }
            Ok(rep) if !rep.changed => state.add_status(format!(
                "compress: already fits ({} est. tokens)",
                rep.after
            )),
            Ok(rep) => state.add_status(format!(
                "compress: {} → {} est. tokens",
                rep.before, rep.after
            )),
            Err(e) => state.add_status(format!("/compress: {e}")),
        }
        return;
    }
    if cmd == "/export" || cmd.starts_with("/export ") {
        let arg = cmd.strip_prefix("/export").unwrap_or("").trim();
        let format = if arg.is_empty() {
            "markdown"
        } else if arg == "markdown" || arg == "json" {
            arg
        } else {
            state.add_status("usage: /export [markdown|json]".into());
            return;
        };
        if state.session_id.is_empty() {
            state.add_status("/export: no conversation yet".into());
            return;
        }
        let dir = crate::terminal::data_dir().join("exports");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            state.add_status(format!("/export: cannot create {}: {e}", dir.display()));
            return;
        }
        let path = dir.join(format!("{}.{format}", state.session_id));
        let body = export_transcript(&state.blocks, &state.session_id, format);
        match std::fs::write(&path, body) {
            Ok(()) => state.add_status(format!("exported to {}", path.display())),
            Err(e) => state.add_status(format!("/export: write failed: {e}")),
        }
        return;
    }
    // --- approvals ------------------------------------------------------
    if cmd == "/approvals" {
        let pending = supervisor
            .pending_approvals(&state.session_id)
            .unwrap_or_default();
        if pending.is_empty() {
            state.add_status("no pending approvals".into());
        } else {
            state.add_status(format!("{} pending approval(s):", pending.len()));
            for (i, scope) in pending.iter().enumerate() {
                state.add_status(format!("  [{i}] {scope}"));
            }
            state.add_status("approve with: /approve <index>".into());
            state.add_status("deny with: /deny <index>".into());
        }
        return;
    }
    if let Some(idx) = cmd.strip_prefix("/approve ") {
        let idx = idx.trim();
        let Ok(i) = idx.parse::<usize>() else {
            state.add_status(format!("usage: /approve <index> (got {idx:?})"));
            return;
        };
        let pending = supervisor
            .pending_approvals(&state.session_id)
            .unwrap_or_default();
        let Some(scope) = pending.get(i) else {
            state.add_status(format!(
                "no approval at index {i} ({} pending)",
                pending.len()
            ));
            return;
        };
        match supervisor.grant(&state.session_id, scope.as_str()) {
            Ok(()) => {
                state.add_status(format!("approved: {scope}"));
                state.pending_approval = None;
                state.ready = false;
                // Resume the turn: chat_turn with empty message rebuilds
                // from the ledger and continues the loop.
                let tx4 = tx.clone();
                let run4 = state.session_id.clone();
                let sess4 = session.clone();
                state.active_run = Some(run4.clone());
                std::thread::spawn(move || match sess4.chat_turn(&run4, "", "") {
                    Ok(outcome) => {
                        let text = match outcome {
                            pantheon_agent::LoopOutcome::Answered { text, .. } => text,
                            _ => String::new(),
                        };
                        let _ = tx4.send(TuiEvent::Answered(text));
                        let _ = tx4.send(TuiEvent::TurnComplete);
                    }
                    Err(e) => {
                        let _ = tx4.send(TuiEvent::Error(e.to_string()));
                    }
                });
            }
            Err(e) => state.add_status(format!("approve: {e}")),
        }
        return;
    }
    if let Some(idx) = cmd.strip_prefix("/deny ") {
        let idx = idx.trim();
        let Ok(i) = idx.parse::<usize>() else {
            state.add_status(format!("usage: /deny <index> (got {idx:?})"));
            return;
        };
        let pending = supervisor
            .pending_approvals(&state.session_id)
            .unwrap_or_default();
        let Some(scope) = pending.get(i) else {
            state.add_status(format!(
                "no approval at index {i} ({} pending)",
                pending.len()
            ));
            return;
        };
        match supervisor.deny(&state.session_id, scope) {
            Ok(()) => {
                state.add_status(format!("denied: {scope}"));
                state.pending_approval = None;
                state.ready = true;
            }
            Err(e) => state.add_status(format!("deny: {e}")),
        }
        return;
    }
    // --- schedule ---------------------------------------------------------
    if cmd == "/schedule" {
        let dd = crate::terminal::data_dir();
        let jobs = crate::schedule::load_jobs_public(&dd);
        if jobs.is_empty() {
            state.add_status("no scheduled jobs".into());
            state.add_status("create with: pantheon schedule <task> --every 30m".into());
        } else {
            state.add_status(format!("{} scheduled job(s):", jobs.len()));
            for j in &jobs {
                let status = if j.paused { "paused" } else { "active" };
                state.add_status(format!(
                    "  {} [{}] {}",
                    j.id,
                    status,
                    j.task.chars().take(60).collect::<String>()
                ));
            }
        }
        return;
    }
    // --- migrate ----------------------------------------------------------
    if cmd == "/migrate" {
        let home = std::env::var("HOME").unwrap_or_default();
        let sources = [
            ("hermes", format!("{home}/.hermes")),
            ("openclaw", format!("{home}/.openclaw")),
            ("omp", format!("{home}/.omp")),
            ("claude", format!("{home}/.claude")),
        ];
        let mut found = Vec::new();
        for (name, path) in &sources {
            if std::path::Path::new(path).is_dir() {
                found.push(*name);
            }
        }
        if found.is_empty() {
            state.add_status("no migration sources detected".into());
        } else {
            state.add_status(format!("detected sources: {}", found.join(", ")));
        }
        state.add_status("categories: sessions, skills, identity, memory, plugins, config, credentials, mcp, schedules, agents, rules, commands, prompts".into());
        state.add_status("plan:    pantheon migrate plan <source> [--categories <list>]".into());
        state.add_status(
            "apply:   pantheon migrate apply <source> [--categories <list>] [--yes]".into(),
        );
        state.add_status(
            "example: pantheon migrate apply claude --categories sessions,skills,memory --yes"
                .into(),
        );
        return;
    }
    // --- migrate ----------------------------------------------------------
    if cmd == "/migrate" {
        let home = std::env::var("HOME").unwrap_or_default();
        let sources = [
            ("hermes", format!("{home}/.hermes")),
            ("openclaw", format!("{home}/.openclaw")),
            ("omp", format!("{home}/.omp")),
            ("claude", format!("{home}/.claude")),
        ];
        let mut found = Vec::new();
        for (name, path) in &sources {
            if std::path::Path::new(path).is_dir() {
                found.push(*name);
            }
        }
        if found.is_empty() {
            state.add_status("no migration sources detected".into());
        } else {
            state.add_status(format!("detected sources: {}", found.join(", ")));
        }
        state.add_status("categories: sessions, skills, identity, memory, plugins, config, credentials, mcp, schedules, agents, rules, commands, prompts".into());
        state.add_status("plan:    pantheon migrate plan <source> [--categories <list>]".into());
        state.add_status(
            "apply:   pantheon migrate apply <source> [--categories <list>] [--yes]".into(),
        );
        state.add_status(
            "example: pantheon migrate apply claude --categories sessions,skills,memory --yes"
                .into(),
        );
        return;
    }
    // --- mcp -------------------------------------------------------------
    if cmd == "/mcp" {
        let dd = crate::terminal::data_dir();
        let groups = pantheon_migration::read_mcp_declarations(&dd);
        if groups.is_empty() {
            state.add_status("no MCP declarations found".into());
            return;
        }
        let mut total = 0;
        let mut ready = 0;
        for g in &groups {
            for s in &g.servers {
                total += 1;
                let is_ready = match s.transport.as_str() {
                    "stdio" => s
                        .command
                        .as_deref()
                        .map(|c| !c.trim().is_empty())
                        .unwrap_or(false),
                    "http" | "sse" => s
                        .url
                        .as_deref()
                        .map(|u| !u.trim().is_empty())
                        .unwrap_or(false),
                    _ => false,
                } && !s.needs_credentials;
                if is_ready {
                    ready += 1;
                }
                state.add_status(format!(
                    "  {}/{} [{}] {}",
                    g.source,
                    s.name,
                    s.transport,
                    if is_ready { "ready" } else { "not ready" }
                ));
            }
        }
        state.add_status(format!("{total} server(s), {ready} ready"));
        return;
    }
    // --- env --------------------------------------------------------------
    if cmd == "/env" {
        let names = session.secrets.names();
        if names.is_empty() {
            state.add_status("no secrets configured".into());
        } else {
            state.add_status("secrets (names only, values never shown):".to_string());
            for n in &names {
                let present = session.secrets.describe(n);
                state.add_status(format!("  {n}: {present}"));
            }
        }
        state.add_status("set with: /env set <name> <value>".into());
        state.add_status("unset with: /env unset <name>".into());
        return;
    }
    if let Some(rest) = cmd.strip_prefix("/env set ") {
        let rest = rest.trim();
        let Some(sp) = rest.find(char::is_whitespace) else {
            state.add_status("usage: /env set <name> <value>".into());
            return;
        };
        let (name, value) = (rest[..sp].trim(), rest[sp..].trim());
        if name.is_empty() || value.is_empty() {
            state.add_status("usage: /env set <name> <value>".into());
            return;
        }
        // Validate the name: must be a valid env var name.
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || name
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            state.add_status(format!(
                "invalid name {name:?}: use letters, digits, underscores"
            ));
            return;
        }
        match session
            .secrets
            .set(name, pantheon_secrets::SecretValue::new(value))
        {
            Ok(()) => state.add_status(format!("set {name} (value hidden)")),
            Err(e) => state.add_status(format!("env set: {e}")),
        }
        return;
    }
    if let Some(name) = cmd.strip_prefix("/env unset ") {
        let name = name.trim();
        if name.is_empty() {
            state.add_status("usage: /env unset <name>".into());
            return;
        }
        match session.secrets.delete(name) {
            Ok(()) => state.add_status(format!("unset {name}")),
            Err(e) => state.add_status(format!("env unset: {e}")),
        }
        return;
    }
    state.add_status(format!("unknown command: {cmd} (try /help)"));
    // The registry is the command catalog; consult it so a typo names the
    // real command instead of dead-ending at /help.
    let word = cmd.split_whitespace().next().unwrap_or(cmd);
    let suggestions = crate::commands::complete(word);
    if !suggestions.is_empty() {
        state.add_status(format!("did you mean {}?", suggestions.join(", ")));
    }
}
#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "tui_interrupt_tests.rs"]
mod interrupt_tests;
