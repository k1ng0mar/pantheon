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
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    DefaultTerminal, Frame,
};

use pantheon_api::events::Event as RuntimeErrorEvent;
use pantheon_providers::model_event::ModelEvent;
use pantheon_runtime::session::Session;
use std::path::PathBuf;

/// TUI-B feature modules. Declared here (not in lib.rs) so a sibling
/// worker editing lib.rs cannot conflict with this branch.
pub mod bg;
mod editor;
pub mod git;
pub mod statusbar;
pub mod theme;
mod timeline;
pub mod vim;

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
    /// A finished `/btw` background task, labeled with the originating
    /// prompt. Rendered as its own card — visually distinct from assistant
    /// messages so a result that lands mid-turn never reads as the main
    /// agent speaking.
    BgResult {
        task_id: u64,
        label: String,
        output: String,
        ok: bool,
    },
    /// Inline unified diff of files a write tool changed, computed from
    /// pre-tool snapshots at ToolCompleted. Rendered red/green; never
    /// affects the ledger.
    Diff(Vec<crate::diffview::DiffLine>),
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
/// `TurnRewound` marker to the ledger (history is never rewritten) and
/// truncates the live transcript view back to the pre-turn state. Replay
/// honors the marker, so a resumed session never sees the rewound turns.
#[derive(Debug, Clone)]
pub struct RewindOffer {
    /// 1-based turn number being rewound.
    pub turn_no: usize,
    /// Block index of the turn's user message in `TuiState::blocks`.
    pub block_index: usize,
    /// First line of the discarded user message, for the confirm prompt.
    pub preview: String,
}

/// A `/goal` objective with its iteration budget. Each agent turn
/// started while a goal is active consumes one iteration; at the cap
/// the TUI refuses new turns until the user raises or clears the goal.
#[derive(Debug, Clone, Default)]
pub struct ActiveGoal {
    pub text: String,
    pub iterations_used: u32,
    pub max_iterations: u32,
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
    /// Set when a reflection pass finishes with proposals awaiting
    /// approval. Drives the reflection card; y approves all, n denies all.
    pub pending_reflect: Option<Vec<pantheon_reflect::PendingProposal>>,
    /// A reflection pass is running on its worker thread. Guards against
    /// overlapping passes clobbering the pending-proposals file.
    pub reflect_running: bool,
    /// A consolidation pass is running on its worker thread. Guards
    /// against overlapping passes promoting the same candidates twice.
    pub consolidate_running: bool,
    /// Opt-in phone approval notifications from `[approvals]`:
    /// (channel, chat_id). None = disabled (the default).
    pub approval_notify: Option<(String, String)>,
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
    /// Open `@` file-mention picker: while set, keystrokes filter and
    /// navigate the file list instead of reaching the composer.
    pub mention: Option<crate::mentions::MentionPicker>,
    /// Pre-tool file snapshots keyed by tool call id, for inline diffs.
    /// Snapshots are taken at ToolStarted and consumed at ToolCompleted.
    pub pending_snaps: std::collections::HashMap<String, Vec<crate::diffview::FileSnapshot>>,
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
    /// Active `/goal`: the session objective plus its iteration budget.
    /// Each `start_turn` consumes one iteration; at the cap the TUI
    /// refuses new turns until the user raises (`/goal iterations N`)
    /// or clears (`/goal clear`) the goal. The text is mirrored into
    /// the runtime session so the model keeps pursuing it across turns.
    pub goal: Option<ActiveGoal>,
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
    /// Shortcuts overlay (`?`). Fullscreen; owns the keyboard while open.
    pub show_shortcuts: bool,
    /// Modal vim editing state for the composer (`/vim` to toggle).
    /// Off by default; when off, input behaves exactly as before.
    pub vim: vim::VimState,
    /// Active color theme. Every renderer reads its palette from here;
    /// `/theme` swaps it live.
    pub theme: theme::Theme,
    /// Cached `⎇ branch[*]` label for the status bar. Refreshed at most
    /// every [`GIT_REFRESH`]; `None` outside a git checkout.
    pub git_label: Option<String>,
    /// Last time `git_label` was recomputed.
    pub git_checked_at: Instant,
    /// Background tasks (`/btw`): lifecycle tracked here; each task owns
    /// its run id and cancel token, independent of the main turn.
    pub bg_tasks: Vec<bg::BgTask>,
    /// Monotonic id source for background tasks.
    pub bg_seq: u64,
    /// Inline image state: thumbnail placements recorded during render,
    /// terminal graphics capability, and the fullscreen preview.
    pub img: crate::richtext::ImagePaintState,
}

/// How often the status bar re-reads git metadata. Coarse on purpose:
/// `git status` is a subprocess and must never run per frame.
const GIT_REFRESH: Duration = Duration::from_secs(5);

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
            pending_reflect: None,
            reflect_running: false,
            consolidate_running: false,
            approval_notify: None,
            last_tool: None,
            interrupt_armed_at: None,
            interrupted: false,
            history: None,
            history_input: String::new(),
            history_sel: 0,
            models: None,
            models_input: String::new(),
            models_sel: 0,
            mention: None,
            pending_snaps: std::collections::HashMap::new(),
            title: None,
            queued_message: None,
            active_run: None,
            turn_in: None,
            turn_out: None,
            turn_started_at: None,
            turns_completed: 0,
            goal: None,
            attention: false,
            attention_note: None,
            timeline: None,
            jump_to_block: None,
            editor: None,
            rewind_offer: None,
            tabs: crate::tabs::TabList::default(),
            theme: theme::Theme::pantheon(),
            show_shortcuts: false,
            vim: vim::VimState::new(),
            git_label: None,
            git_checked_at: Instant::now(),
            bg_tasks: Vec::new(),
            bg_seq: 0,
            img: crate::richtext::ImagePaintState::new(),
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
            pending_reflect: None,
            reflect_running: false,
            consolidate_running: false,
            approval_notify: None,
            last_tool: None,
            interrupt_armed_at: None,
            interrupted: false,
            history: None,
            history_input: String::new(),
            history_sel: 0,
            models: None,
            models_input: String::new(),
            models_sel: 0,
            mention: None,
            pending_snaps: std::collections::HashMap::new(),
            title: None,
            queued_message: None,
            active_run: None,
            turn_in: None,
            turn_out: None,
            turn_started_at: None,
            turns_completed: 0,
            goal: None,
            attention: false,
            attention_note: None,
            timeline: None,
            jump_to_block: None,
            editor: None,
            rewind_offer: None,
            tabs: crate::tabs::TabList::default(),
            theme: theme::Theme::pantheon(),
            show_shortcuts: false,
            vim: vim::VimState::new(),
            git_label: None,
            git_checked_at: Instant::now(),
            bg_tasks: Vec::new(),
            bg_seq: 0,
            img: crate::richtext::ImagePaintState::new(),
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
            RuntimeErrorEvent::ToolStarted {
                tool,
                args,
                call_id,
                ..
            } => {
                self.last_tool = Some((tool.clone(), args.clone()));
                // Snapshot edit targets for the inline diff rendered at
                // completion: the tool is about to rewrite these files, so
                // capture the "before" now.
                let targets = crate::diffview::edit_targets(tool, args);
                if !targets.is_empty() {
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                    let snaps: Vec<_> = targets
                        .iter()
                        .take(crate::diffview::MAX_DIFF_FILES)
                        .filter_map(|p| crate::diffview::snapshot_file(&cwd.join(p)))
                        .collect();
                    if !snaps.is_empty() {
                        self.pending_snaps.insert(call_id.clone(), snaps);
                    }
                }
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::ToolCall {
                        name: tool.clone(),
                        args: args.clone(),
                        ok: None,
                    },
                });
            }
            RuntimeErrorEvent::ToolCompleted { tool, call_id, .. } => {
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
                // Inline diff: compare the pre-tool snapshots with the
                // files as they are now. Missing/unchanged files produce
                // no diff and no block.
                if let Some(snaps) = self.pending_snaps.remove(call_id) {
                    let mut lines = Vec::new();
                    for snap in snaps.iter().take(crate::diffview::MAX_DIFF_FILES) {
                        if let Some(mut d) = crate::diffview::diff_snapshot(snap) {
                            lines.append(&mut d);
                        }
                    }
                    if !lines.is_empty() {
                        self.blocks.push(TranscriptBlock {
                            kind: BlockKind::Diff(lines),
                        });
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
                // Opt-in phone notification. Best-effort on a background
                // thread: the TUI must never block on network I/O.
                if let Some((channel, chat_id)) = self.approval_notify.clone() {
                    let run_id = run_id.clone();
                    let scope = scope.clone();
                    std::thread::spawn(move || {
                        let workdir = std::env::current_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                        let Some(notice) =
                            crate::approval_notify::build_notice(&run_id, &scope, &workdir)
                        else {
                            return;
                        };
                        if let Err(e) =
                            crate::approval_notify::send_notice(&channel, &chat_id, &notice)
                        {
                            eprintln!("approval notify: {e}");
                        }
                    });
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
                });
            }
            // Title generation (aux or /name) lands as a durable event;
            // the header follows it with no polling.
            RuntimeErrorEvent::SessionTitled { title, .. } => {
                self.title = Some(title.clone());
            }
            // Mid-turn steering delivered: mark it visibly in the
            // transcript so the redirect is obvious, distinct from an
            // ordinary user message.
            RuntimeErrorEvent::SteeringProvided { text, .. } => {
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::Status(format!("steered: {text}")),
                });
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
    /// The `/goal` iteration gate. Returns the refusal message when the
    /// active goal's iteration budget is exhausted, else `None`.
    /// Extracted so the rule is testable against the shipped code.
    pub fn goal_refusal(&self) -> Option<String> {
        let g = self.goal.as_ref()?;
        if g.iterations_used >= g.max_iterations {
            Some(format!(
                "goal iteration budget exhausted ({}/{}). /goal iterations N to raise it, /goal clear to drop the goal.",
                g.iterations_used, g.max_iterations
            ))
        } else {
            None
        }
    }

    /// Consume one goal iteration. Call when a turn starts under a goal.
    pub fn consume_goal_iteration(&mut self) {
        if let Some(g) = self.goal.as_mut() {
            g.iterations_used = g.iterations_used.saturating_add(1);
        }
    }

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

    /// Reset ephemeral turn state without touching the session identity,
    /// title, or ledger history. Clears the visible transcript (like
    /// `/clear`), drops any queued message, and releases per-turn state
    /// (ready flag, active run, telemetry, interrupt arm).
    ///
    /// Returns true when a turn was in flight: the caller should cancel
    /// it cooperatively (same path as double-Esc).
    ///
    /// Pure state transition, extracted so the reset rules are testable
    /// against the shipped code rather than a copy of it.
    pub fn reset_ephemeral(&mut self) -> bool {
        let was_running = !self.ready;
        self.blocks.clear();
        self.queued_message = None;
        self.ready = true;
        self.status_line = "ready".to_string();
        self.interrupted = false;
        self.interrupt_armed_at = None;
        self.active_run = None;
        self.turn_started_at = None;
        self.turn_in = None;
        self.turn_out = None;
        self.rewind_offer = None;
        was_running
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
        if n == 0 {
            None
        } else {
            Some(n)
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
        // Coarse git refresh: `git status` never runs per frame.
        if self.git_checked_at.elapsed() >= GIT_REFRESH {
            self.refresh_git();
        }
    }

    /// Re-read git metadata for the status bar now. Cheap enough to call
    /// on turn boundaries; silent outside a checkout.
    pub fn refresh_git(&mut self) {
        self.git_label = git::git_label(&std::env::current_dir().unwrap_or_default());
        self.git_checked_at = Instant::now();
    }

    /// Switch the live theme by name. Returns false (and keeps the current
    /// theme) for an unknown name.
    pub fn set_theme(&mut self, name: &str) -> bool {
        match theme::Theme::from_name(name) {
            Some(t) => {
                self.theme = t;
                true
            }
            None => false,
        }
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
/// Enter guard must keep applying to it. `verb` labels the transition
/// block ("resumed", "forked").
fn switch_to_run(state: &mut TuiState, session: &Arc<Session>, id: &str, verb: &str) {
    if id == state.session_id {
        state.attention_clear();
        return;
    }
    let _ = session.supervisor.ledger_reopen_run(id);
    if let Ok(entries) = session.supervisor.replay(id) {
        state.blocks.clear();
        state.session_id = id.to_string();
        state.title = session.supervisor.ledger_title(id).ok().flatten();
        for item in pantheon_runtime::session::rebuild_transcript(entries) {
            let kind = match item {
                pantheon_runtime::session::TranscriptItem::Message(m) => match m.role {
                    pantheon_api::message::Role::User => BlockKind::UserMessage(m.content.clone()),
                    _ => BlockKind::AssistantMessage(m.content.clone()),
                },
                // Imported reasoning traces render as thinking blocks.
                pantheon_runtime::session::TranscriptItem::Reasoning(text) => {
                    BlockKind::Thinking(text)
                }
            };
            state.blocks.push(TranscriptBlock { kind });
        }
        state.scroll_to_bottom();
    }
    state.attention_clear();
    state.blocks.push(TranscriptBlock {
        kind: BlockKind::Status(format!("{verb} {id}")),
    });
}

/// Render one full frame, then place terminal graphics.
///
/// Inline thumbnails and the image preview are Kitty/Sixel graphics, not
/// ratatui cells, so they are emitted after the widget tree is built (but
/// still inside the draw closure — placement is absolute, order with the
/// cell flush does not matter). `render_transcript` records the viewport
/// each frame; when an overlay covers the transcript the view stays `None`
/// and [`crate::richtext::ImagePaintState::paint`] deletes stale images so
/// they never ghost over fullscreen UI.
pub fn render(state: &mut TuiState, f: &mut Frame) {
    // Reset per frame; render_transcript / render_image_preview set these
    // when they actually draw.
    state.img.view = None;
    state.img.preview_area = None;
    render_main(state, f);
    let _ = state.img.paint();
    if let Some(area) = state.img.preview_area {
        let _ = state.img.paint_preview(area);
    }
}

fn render_main(state: &mut TuiState, f: &mut Frame) {
    let outer = Layout::vertical([
        Constraint::Length(1), // session tabs
        Constraint::Length(3), // header
        Constraint::Min(1),    // transcript
        Constraint::Length(3), // input
        Constraint::Length(1), // status bar
    ]);
    let [tab_area, header_area, chat_area, input_area, status_area] = outer.areas(f.area());

    if state.img.preview.is_some() {
        render_image_preview(f, f.area(), state);
        return;
    }
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
    if state.show_shortcuts {
        render_shortcuts(f, f.area(), state);
        return;
    }
    if state.pending_approval.is_some() {
        // Steal the input row: permission card replaces it until resolved.
        render_permission(f, input_area, state);
    } else if state.pending_reflect.is_some() {
        // Reflection card: eval-gated proposals awaiting approval.
        render_reflect_card(f, input_area, state);
    } else {
        render_input(f, input_area, state);
    }
    render_header(f, header_area, state);
    render_transcript(f, chat_area, state);
    render_status(f, status_area, state);
    crate::tabs::render_tab_bar(f, tab_area, &state.tabs, &state.theme);
    if state.timeline.is_some() {
        render_timeline(f, f.area(), state);
    }
    if state.mention.is_some() {
        render_mention_picker(f, f.area(), state);
    }
}

/// `@` file-mention picker popup: type to filter, Up/Down to move,
/// Enter/Tab to insert, Esc to cancel. Drawn over the normal frame so
/// the composer stays visible behind it.
fn render_mention_picker(f: &mut Frame, area: Rect, state: &TuiState) {
    let Some(picker) = state.mention.as_ref() else {
        return;
    };
    let th = &state.theme;
    let w = area.width.clamp(40, 76);
    let h = (picker.files.len().min(10) as u16 + 4).clamp(6, 16);
    let x = area.x + (area.width - w) / 2;
    let y = area.height.saturating_sub(h + 5) + area.y;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "  @{} — type to filter, Enter to insert, Esc to cancel",
            picker.input
        ),
        Style::default().fg(th.primary),
    ))];
    if picker.files.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matching files)",
            Style::default().fg(th.failure),
        )));
    }
    for (i, path) in picker.files.iter().enumerate().take(10) {
        let style = if i == picker.sel {
            Style::default()
                .fg(th.primary)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        let shown: String = path.chars().take((w as usize).saturating_sub(6)).collect();
        lines.push(Line::from(Span::styled(format!("  {shown}"), style)));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.primary))
        .title(" attach file ");
    let para = Paragraph::new(lines).block(block);
    f.render_widget(Clear, area);
    f.render_widget(para, area);
}

/// Searchable scrollable history overlay: /history. Type-to-filter,
/// Up/Down to move, Enter to resume, Esc to close. Mirrors the REPL's
/// /history picker, rendered as a centered list.
fn render_history(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
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
            Style::default().fg(th.primary),
        )),
        Line::from(""),
    ];
    if runs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(th.failure),
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
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD)
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
            .border_style(Style::default().fg(th.primary))
            .title(Span::styled(
                title,
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, area);
}

/// Draw the reflection approval card: eval-gated skill/persona proposals
/// from the last reflection pass, awaiting a decision. Steals the input
/// row like the permission card until resolved.
fn render_reflect_card(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    let mut text = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Reflection produced proposals (eval-gated).",
            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
        )),
    ];
    if let Some(pending) = &state.pending_reflect {
        for pp in pending.iter().take(5) {
            let p = &pp.proposal;
            let first_line = p.body.lines().next().unwrap_or("");
            text.push(Line::from(format!(
                "  [{}] {} ({})",
                p.id,
                p.title,
                p.kind_name()
            )));
            if !first_line.is_empty() {
                let preview: String = first_line.chars().take(60).collect();
                text.push(Line::from(format!("      {preview}")));
            }
        }
        if pending.len() > 5 {
            text.push(Line::from(format!("  …and {} more", pending.len() - 5)));
        }
    }
    text.extend([
        Line::from(""),
        Line::from(Span::styled(
            "  [y] Approve all    [n] Deny all",
            Style::default().fg(th.success),
        )),
    ]);
    let card = Paragraph::new(text).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(th.warning))
            .title(Span::styled(
                " \u{1f4a1} Reflection proposals ",
                Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, area);
}

/// Searchable model browser overlay: /models. Type-to-filter across
/// provider and model names, Up/Down to move, Enter to switch the live
/// session's default model, Esc to close. Same overlay contract as
/// /history, rendered as a centered list.
fn render_models(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
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
            Style::default().fg(th.primary),
        )),
        Line::from(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(th.failure),
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
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD)
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
            .border_style(Style::default().fg(th.primary))
            .title(Span::styled(
                title,
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, area);
}

/// Turn-timeline rail: a right-side panel listing one row per user turn
/// (conversation order). Arrow keys move the selection, Enter jumps the
/// transcript viewport to that turn, Esc/F2 closes. Strictly read-only:
/// navigating never mutates the transcript.
fn render_timeline(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
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
            Style::default().fg(th.primary),
        )),
        Line::from(""),
    ];
    if turns.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no turns yet)",
            Style::default().fg(th.dim),
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
            turn.preview
                .chars()
                .take(inner.saturating_sub(8))
                .collect::<String>(),
        );
        let style = if i == sel {
            Style::default()
                .fg(th.primary)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(row, style)));
    }
    let card = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(th.primary))
            .title(Span::styled(
                " timeline ",
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, panel);
}

/// Fullscreen draft editor. Line numbers, current-line highlight, live
/// line/char stats. `Ctrl+Enter`/`Ctrl+S` sends, `Esc` cancels and keeps
/// the draft in the input box.
fn render_editor(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
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
            Style::default().fg(th.dim),
        )];
        if i == crow {
            // Draw the block cursor inside the current line.
            let chars: Vec<char> = line.chars().collect();
            let c = ccol.min(chars.len());
            let (head, tail) = (
                chars[..c].iter().collect::<String>(),
                chars[c..].iter().collect::<String>(),
            );
            spans.push(Span::raw(head));
            spans.push(Span::styled(
                "▌",
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            ));
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
        Style::default().fg(th.primary),
    )));
    let card = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(th.primary))
            .title(Span::styled(
                " draft editor ",
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
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
/// Fullscreen shortcuts overlay, opened with `?` when the prompt is empty
/// and not editing. Lists only bindings the event loop implements;
/// Esc/?/q dismiss and the overlay swallows everything else.
fn render_shortcuts(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.primary))
        .title(Span::styled(
            " Shortcuts ",
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 3 || inner.width < 20 {
        return;
    }

    let key_style = Style::default().fg(th.primary).add_modifier(Modifier::BOLD);
    let rows: &[(&str, &str)] = &[
        ("?", "this help"),
        ("q", "quit pantheon"),
        ("Enter", "send message"),
        ("Esc", "stop typing"),
        ("Esc x2", "interrupt run / offer rewind"),
        ("PgUp PgDn", "scroll transcript"),
        ("F2 / Ctrl+O", "turn timeline"),
        ("Ctrl+E", "fullscreen editor"),
        ("Ctrl+Tab", "next session"),
        ("Ctrl+Shift+Tab", "previous session"),
        ("Alt+1..9", "jump to session"),
        ("Up / Down", "move in lists"),
        ("y / n", "allow / deny approval"),
        ("Ctrl+Enter", "send from editor"),
        ("Ctrl+S", "send from editor"),
        ("/steer <text>", "steer the running turn mid-flight"),
        (
            "/vim [on|off]",
            "vim editing (Normal/Insert); v1: no Visual",
        ),
    ];
    let footer = Line::from(Span::styled(
        "Esc / ? / q to close",
        Style::default().fg(th.dim),
    ));
    let mut lines: Vec<Line> = Vec::new();
    for (i, (key, desc)) in rows.iter().enumerate() {
        if i as u16 >= inner.height.saturating_sub(2) {
            break;
        }
        lines.push(Line::from(vec![
            Span::styled(format!("  {key:<14}"), key_style),
            Span::raw(*desc),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(footer);
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_permission(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
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
            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("  tool: {tool_name}"),
            Style::default().fg(th.warning),
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
            Style::default().fg(th.success),
        )),
    ]);
    let card = Paragraph::new(text).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(th.warning))
            .title(Span::styled(
                " \u{26a0} Permission required ",
                Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, area);
}

/// Draw the input box at the bottom.
fn render_input(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    let line = if state.vim.enabled && state.is_inputting {
        // Vim: draw the cursor at the modal (row, col) instead of the
        // trailing `_`. Normal shows a bar before the char under the
        // cursor; Insert keeps today's `_` idiom, positioned.
        let (row, col) = state.vim.cursor(&state.input);
        let marker = if state.vim.mode == vim::VimMode::Normal {
            '▌'
        } else {
            '_'
        };
        let mut out = String::from("› ");
        for (i, l) in state.input.split('\n').enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if i == row {
                let mut chars: Vec<char> = l.chars().collect();
                let at = col.min(chars.len());
                chars.insert(at, marker);
                out.extend(chars);
            } else {
                out.push_str(l);
            }
        }
        out
    } else {
        let cursor = if state.is_inputting { "_" } else { " " };
        format!("› {}{}", state.input, cursor)
    };
    let input = Paragraph::new(line).block(
        Block::bordered()
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Input ", Style::default().fg(th.primary))),
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
    let th = &state.theme;
    if let Some(offer) = &state.rewind_offer {
        let text = format!(
            "↩ rewind turn {} ({}…)?  [y] yes   [n] no",
            offer.turn_no,
            offer.preview.chars().take(40).collect::<String>(),
        );
        let bar = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
        )));
        f.render_widget(bar, area);
        return;
    }
    // Interrupt state takes over the status word: an armed interrupt is a
    // call to action, a settled one reports the truth.
    let (icon, bar_color, status_word) = if state.interrupted {
        (icon::WARNING, th.warning, "interrupted")
    } else if !state.ready && state.interrupt_armed_at.is_some() {
        (icon::WARNING, th.warning, "esc to interrupt")
    } else if state.ready
        && state.interrupt_armed_at.is_some()
        && state.rewind_candidate().is_some()
    {
        // Double-Esc is armed while idle: make the rewind discoverable.
        (icon::WARNING, th.warning, "esc again: rewind?")
    } else if state.ready {
        (icon::SUCCESS, th.success, "ready")
    } else {
        (icon::RUNNING, th.running, "working")
    };
    // Vim mode rides along in the status word (`-- NORMAL --` /
    // `-- INSERT --`); absent when vim is off so non-users see no change.
    let status_word = match state.vim.status_label() {
        Some(mode) => format!("{mode} · {status_word}"),
        None => status_word.to_string(),
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
        status_word,
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
        git: state.git_label.clone(),
        // Background tasks alive, e.g. `bg 2 ⠋`; None hides the segment.
        bg: bg::status_segment(&state.bg_tasks),
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
    let th = &state.theme;
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
        Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
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
    // Placements are rebuilt every frame: blocks only ever append, but a
    // rewind can drop them, and scroll changes every row.
    state.img.placements.clear();
    let th = &state.theme;
    if state.blocks.is_empty() {
        let mut welcome = crate::terminal::splash_lines(area.width, th);
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
    {
        // Disjoint field borrows: theme (shared) + image state (mut) +
        // blocks (shared) coexist; the mutable borrows below are dead by
        // the time the scroll math runs.
        let img = &mut state.img;
        let interrupted = state.interrupted;
        for (i, block) in state.blocks.iter().enumerate() {
            block_starts.push(lines.len());
            let is_last = i + 1 == block_count;
            render_block(&mut lines, block, is_last, interrupted, th, img, area.width);
            lines.push(Line::from(""));
        }
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
    // Viewport geometry for the post-draw image painter: `off` lines are
    // scrolled off the top, content starts one row below the border.
    state.img.view = Some(crate::richtext::TranscriptView {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
        scroll_off: off as usize,
    });
    let text = Text::from(lines);
    let para = Paragraph::new(text)
        .block(Block::bordered().title(" Transcript "))
        .scroll((off, 0));
    f.render_widget(para, area);
}

/// Render message text with rich features: fenced mermaid blocks become
/// Unicode diagrams, `$…$` math becomes Unicode approximations (both via
/// [`crate::richtext`]), and image references become a placeholder line
/// plus reserved rows the post-draw painter fills with a thumbnail.
fn render_rich_text(
    lines: &mut Vec<Line>,
    text: &str,
    th: &theme::Theme,
    img: &mut crate::richtext::ImagePaintState,
    term_width: u16,
) {
    let dim = Style::default().fg(th.dim);
    for seg in crate::richtext::segment_message(text) {
        match seg {
            crate::richtext::RichSegment::Text(t) => {
                for line in t.lines() {
                    lines.push(Line::from(format!("│  {line}")));
                }
            }
            crate::richtext::RichSegment::Image(image) => {
                let line_idx = lines.len();
                img.reserve(&image, line_idx, term_width);
                let index = img.placements.len() - 1;
                let rows = img.placements[index].rows;
                let dims = match (image.width, image.height) {
                    (Some(w), Some(h)) => Some((w, h)),
                    _ => None,
                };
                let label = crate::richtext::ImagePaintState::placeholder_text(
                    &img.placements[index],
                    dims,
                    index,
                );
                lines.push(Line::from(Span::styled(format!("│  {label}"), dim)));
                // Spacer rows the thumbnail paints over (label stays visible
                // above it).
                for _ in 1..rows {
                    lines.push(Line::from("│"));
                }
            }
        }
    }
}

/// Fullscreen image preview overlay. Draws the frame and records its area;
/// the pixels are placed post-draw by `ImagePaintState::paint_preview`.
/// Owns the keyboard while open (see the key handler): arrows pan,
/// `+`/`-` zoom, `[`/`]` cycle, `q`/Esc closes.
fn render_image_preview(f: &mut Frame, area: Rect, state: &mut TuiState) {
    f.render_widget(Clear, area);
    let w = (area.width * 80 / 100).clamp(24, area.width.max(24));
    let h = (area.height * 80 / 100).clamp(10, area.height.max(10));
    let box_area = Rect::new(
        area.x + area.width.saturating_sub(w) / 2,
        area.y + area.height.saturating_sub(h) / 2,
        w,
        h,
    );
    let (sel, len, path) = match state.img.preview.as_ref() {
        Some(p) => (p.sel, p.images.len(), p.images.get(p.sel).cloned()),
        None => return,
    };
    let (title, info) = match path {
        Some(path) => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "?".to_string());
            let dims = crate::richtext::image_dimensions(&path)
                .and_then(|(w, h)| match (w, h) {
                    (Some(w), Some(h)) => Some(format!("{w}×{h}")),
                    _ => None,
                })
                .unwrap_or_else(|| "unknown size".to_string());
            (
                format!(" 🖼 {name} [{}/{}] ", sel + 1, len.max(1)),
                format!("{} · {dims}", path.display()),
            )
        }
        None => (" image ".to_string(), "no image".to_string()),
    };
    let graphics_note = match state.img.graphics {
        crate::richtext::GraphicsSupport::None => {
            "this terminal has no inline graphics: showing info only"
        }
        _ => "+/- zoom · arrows pan · [ ] cycle · q/Esc close",
    };
    let th = &state.theme;
    let body = Paragraph::new(vec![
        Line::from(Span::styled(info, Style::default().fg(th.dim))),
        Line::from(""),
        Line::from(Span::styled(graphics_note, Style::default().fg(th.dim))),
    ])
    .block(
        Block::bordered()
            .title(title)
            .border_style(Style::default().fg(th.primary)),
    );
    f.render_widget(body, box_area);
    state.img.preview_area = Some(box_area);
}

/// Cycle the preview to the next/previous transcript image (`dir` ±1).
fn cycle_preview_image(state: &mut TuiState, dir: i32) {
    if let Some(p) = state.img.preview.as_mut() {
        let n = p.images.len() as i32;
        if n > 0 {
            p.sel = (p.sel as i32 + dir).rem_euclid(n) as usize;
            p.pan_x = 0;
            p.pan_y = 0;
        }
    }
}

/// Zoom the preview, clamped to a sane range.
fn zoom_preview(state: &mut TuiState, factor: f32) {
    if let Some(p) = state.img.preview.as_mut() {
        p.zoom = (p.zoom * factor).clamp(0.25, 8.0);
    }
}

/// Render a single transcript block as Lines. `is_last` marks the streaming
/// head: thinking blocks stay expanded while they are the live block and
/// collapse to a summary line once anything else lands after them.
fn render_block(
    lines: &mut Vec<Line>,
    block: &TranscriptBlock,
    is_last: bool,
    interrupted: bool,
    th: &theme::Theme,
    img: &mut crate::richtext::ImagePaintState,
    term_width: u16,
) {
    match &block.kind {
        BlockKind::UserMessage(text) => {
            lines.push(Line::from(Span::styled(
                "╭─ You ──",
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )));
            render_rich_text(lines, text, th, img, term_width);
        }
        BlockKind::AssistantMessage(text) => {
            lines.push(Line::from(Span::styled(
                format!("╭─ {} Pantheon ──", icon::PANTHEON),
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )));
            render_rich_text(lines, text, th, img, term_width);
        }
        BlockKind::BgResult {
            task_id,
            label,
            output,
            ok,
        } => {
            // Own card, never mistaken for the main agent: success is the
            // ◈ glyph in the success color, failure the × glyph in failure
            // color, both labeled with the originating /btw prompt.
            let (glyph, color, verb) = if *ok {
                ("◈", th.success, "background result")
            } else {
                (icon::FAILURE, th.failure, "background task failed")
            };
            lines.push(Line::from(Span::styled(
                format!("╭─ {glyph} {verb} bg-{task_id} · “{label}” ──"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            render_rich_text(lines, output, th, img, term_width);
        }
        BlockKind::Thinking(text) => {
            // Reasoning renders distinctly from final text: dim italic,
            // labeled as thought rather than answer. The live (streaming)
            // block stays expanded; settled ones collapse to a summary so
            // old reasoning never crowds out answers.
            let dim = Style::default().fg(th.dim).add_modifier(Modifier::ITALIC);
            if is_last {
                lines.push(Line::from(Span::styled(
                    format!("┌─ {} reasoning ──", icon::THINKING),
                    Style::default()
                        .fg(th.dim)
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
                None if interrupted => ("■", th.warning),
                None => (icon::RUNNING, th.running),
                Some(true) => (icon::SUCCESS, th.success),
                Some(false) => (icon::FAILURE, th.failure),
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
                Style::default().fg(th.primary),
            )));
            lines.push(Line::from(format!("│  {task}")));
        }
        BlockKind::Status(text) => {
            lines.push(Line::from(Span::styled(
                format!("… {text}"),
                Style::default().fg(th.warning),
            )));
        }
        BlockKind::Diff(diff_lines) => {
            lines.extend(crate::diffview::render_diff_lines(diff_lines, th));
        }
    }
}

/// Internal event types that flow from the worker thread to the TUI loop.
enum TuiEvent {
    Model(pantheon_providers::model_event::ModelEvent),
    Runtime(pantheon_api::events::Event),
    /// A turn finished for `run_id`. The id lets the loop tell a
    /// background-tab completion apart from the visible session's, so only
    /// the former raises a notification.
    TurnComplete {
        run_id: String,
    },
    /// The assistant's final text for a turn, rendered into the transcript.
    Answered(String),
    Error(String),
    /// The run stopped because the user interrupted it (not a failure).
    Canceled,
    /// A `/btw` background task finished on its worker thread. Carries the
    /// task id (not the run id) so the loop can update the right task even
    /// if the visible session changed since it was fired. `Ok` is the
    /// task's final answer text; `Err` is a failure or parked-on-approval
    /// note. Never touches the main turn's state.
    BgDone {
        task_id: u64,
        result: Result<String, String>,
    },
    /// A reflection pass finished on its worker thread (manual `/reflect`,
    /// automatic after N turns, or scheduled). Carries the human summary
    /// and any proposals awaiting approval; the loop sets
    /// `pending_reflect` when the latter is non-empty so the reflection
    /// card appears. Never touches the main turn's state.
    ReflectDone {
        summary: String,
        pending: Vec<pantheon_reflect::PendingProposal>,
    },
    /// A consolidation pass finished on its worker thread (manual
    /// `/consolidate` or scheduled). Carries the human summary; the loop
    /// just renders it. Never touches the main turn's state.
    ConsolidateDone {
        summary: String,
    },
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

    // Run budgets from `[budget]` in config.toml (max turns, tool calls,
    // delegate depth, token cap). Absent = the runtime defaults; `/set`
    // and `/tokens` retune them live for this session.
    session.set_budget(file_cfg.as_ref().map(|c| c.budget()).unwrap_or_default());

    // Tacit temporal awareness (`[temporal]` in config.toml). Absent
    // section = the runtime defaults (enabled, 2h gap, system timezone).
    if let Some(t) = file_cfg.as_ref().and_then(|c| c.temporal.clone()) {
        session.set_temporal_config(pantheon_api::temporal::TemporalConfig::from(&t));
    }

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

    // Phone approval notifications: opt-in `[approvals]` config.
    // Off unless both channel and destination chat are set.
    if let Some(cfg) = file_cfg.as_ref() {
        if let Some(a) = cfg.approvals.as_ref() {
            if let (Some(channel), Some(chat_id)) =
                (a.notify_channel.clone(), a.notify_chat_id.clone())
            {
                state.approval_notify = Some((channel, chat_id));
            }
        }
    }

    // Restore the saved theme before the first frame: load_theme_name
    // falls back to the default on missing config or unknown names.
    let saved_theme = theme::load_theme_name(&crate::terminal::data_dir());
    state.set_theme(&saved_theme);
    // Restore saved vim mode the same way: absent config = off.
    state.vim.enabled = vim::load_vim(&crate::terminal::data_dir());
    // Prime the git segment so the first frame already shows it.
    state.refresh_git();

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

    let result = tui_loop(&mut terminal, &mut state, session, &tx, &rx, &running);

    disable_raw_mode()?;
    let mut stdout = io::stdout();
    // Remove any painted thumbnails/preview before leaving the alternate
    // screen; some terminals persist Kitty images otherwise.
    let _ = state.img.clear_painted();
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
                    let _ = tx2.send(TuiEvent::TurnComplete {
                        run_id: run_id_owned.clone(),
                    });
                }
                pantheon_agent::LoopOutcome::Canceled { .. } => {
                    let _ = tx2.send(TuiEvent::Canceled);
                }
                _ => {
                    let _ = tx2.send(TuiEvent::TurnComplete {
                        run_id: run_id_owned.clone(),
                    });
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
    // An active /goal caps how many turns may pursue it. Background
    // tasks (/btw) and resumes bypass start_turn, so they never consume
    // the goal's iterations.
    let refusal = state.goal_refusal();
    if let Some(refusal) = refusal {
        state.add_status(refusal);
        return false;
    }
    if !state.begin_turn(msg.clone()) {
        return false;
    }
    state.consume_goal_iteration();
    session.reset_cancel();
    let run_id = resolve_send_run_id(state).to_string();
    state.active_run = Some(run_id.clone());
    spawn_turn(tx, session, &run_id, &msg);
    true
}

/// Worker thread for one `/btw` background task. Mirrors `spawn_turn`
/// but drives the turn with the task's own cancel token, so neither the
/// main turn's Esc nor the task can cancel the other. Completion (or
/// failure) returns as `TuiEvent::BgDone` keyed by task id.
fn spawn_bg_task(
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    session: &Arc<Session>,
    task: &bg::BgTask,
    prompt: &str,
) {
    let tx2 = tx.clone();
    let session_owned = session.clone();
    let run_id = task.run_id.clone();
    let task_id = task.id;
    let prompt_owned = prompt.to_string();
    let cancel = task.cancel.clone();
    let turn_id = pantheon_runtime::new_turn_id();
    std::thread::spawn(move || {
        let outcome =
            session_owned.chat_turn_with_cancel(&run_id, &turn_id, &prompt_owned, &cancel);
        let result = match outcome {
            Ok(pantheon_agent::LoopOutcome::Answered { text, .. }) => Ok(text),
            Ok(pantheon_agent::LoopOutcome::Delegated { agent }) => Ok(format!(
                "sub-agent {agent} completed; full transcript in run {run_id}"
            )),
            Ok(pantheon_agent::LoopOutcome::AwaitingApproval { scope, .. }) => Err(format!(
                "parked on approval '{scope}'; grant with \
                 `pantheon run --taskID {run_id} --grant '{scope}'`"
            )),
            Ok(pantheon_agent::LoopOutcome::Canceled { .. }) => Err("canceled".to_string()),
            Ok(pantheon_agent::LoopOutcome::Denied { capability }) => {
                Err(format!("capability denied: {capability:?}"))
            }
            Ok(pantheon_agent::LoopOutcome::BudgetExhausted { cap }) => {
                Err(format!("budget exhausted: {cap}"))
            }
            Err(e) => Err(e.to_string()),
        };
        let _ = tx2.send(TuiEvent::BgDone { task_id, result });
    });
}

/// Fire a background task (`/btw <prompt>`) without touching the running
/// turn: the task gets its own run id, its own cancel token, and a worker
/// thread. The main turn's `active_run`, cancel token, queued message, and
/// transcript are never disturbed — only a labeled result block lands when
/// the task finishes.
fn do_btw(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    prompt: &str,
) {
    if prompt.is_empty() {
        state.add_status("usage: /btw <prompt> — run a task in the background".into());
        return;
    }
    if !bg::can_spawn(&state.bg_tasks) {
        state.add_status(format!(
            "background task cap reached ({} active) — wait for one to finish; see /bg",
            bg::active_count(&state.bg_tasks)
        ));
        return;
    }
    let run_id = pantheon_runtime::new_run_id();
    if let Err(e) = session.supervisor.start_run(&run_id) {
        state.add_status(format!("background task: cannot start run: {e}"));
        return;
    }
    // Provenance: the ledger records which run spawned this one, mirroring
    // the fork marker convention (`forked from <src> at turn <n>`).
    if let Err(e) = session
        .supervisor
        .emit(pantheon_api::events::Event::RunProgress {
            run_id: run_id.clone(),
            detail: format!("spawned via /btw from {}", state.session_id),
        })
    {
        state.add_status(format!("background task: cannot record provenance: {e}"));
        return;
    }
    state.bg_seq += 1;
    let mut task = bg::BgTask::new(
        state.bg_seq,
        prompt,
        run_id.clone(),
        state.session_id.clone(),
    );
    task.mark_running();
    state.add_status(format!(
        "background task bg-{} started: {}",
        task.id, task.label
    ));
    spawn_bg_task(tx, session, &task, prompt);
    state.bg_tasks.push(task);
}

/// `/goal [text]` — set or show the session's objective. Each agent turn
/// started while a goal is active consumes one iteration; at the cap the
/// TUI refuses new turns until `/goal iterations N` raises it or
/// `/goal clear` drops it. The text is mirrored into the runtime session
/// so the model keeps pursuing it across turns.
fn do_goal(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
    let arg = cmd.strip_prefix("/goal").map(str::trim).unwrap_or("");
    if arg.is_empty() {
        match state.goal.clone() {
            Some(g) => {
                state.add_status(format!("goal: {}", g.text));
                state.add_status(format!(
                    "iterations: {}/{}",
                    g.iterations_used, g.max_iterations
                ));
            }
            None => state.add_status("no active goal — /goal <text> to set one".into()),
        }
        return;
    }
    if arg == "clear" {
        state.goal = None;
        session.set_goal(None);
        state.add_status("goal cleared".into());
        return;
    }
    // `/goal iterations <n>` adjusts the cap; anything else — even text
    // starting with "iterations" — is goal text.
    if let Some(rest) = arg
        .strip_prefix("iterations")
        .map(str::trim)
        .filter(|r| !r.is_empty() && r.chars().all(|c| c.is_ascii_digit()))
    {
        match rest.parse::<u32>() {
            Ok(n) if n > 0 => match state.goal.as_mut() {
                Some(g) => {
                    g.max_iterations = n;
                    state.add_status(format!("goal iteration limit: {n}"));
                }
                None => state.add_status("no active goal — /goal <text> first".into()),
            },
            _ => state.add_status("usage: /goal iterations <n> (n >= 1)".into()),
        }
        return;
    }
    let max_iterations = crate::config::Config::load_or_report(&crate::terminal::data_dir())
        .map(|c| c.goal_iterations())
        .unwrap_or(10);
    let text = arg.to_string();
    session.set_goal(Some(text.clone()));
    state.goal = Some(ActiveGoal {
        text: text.clone(),
        iterations_used: 0,
        max_iterations,
    });
    state.add_status(format!("goal set ({max_iterations} iterations): {text}"));
}

/// `/tokens [n|off]` — show or set the per-run token cap. Strictly
/// optional: the default is uncapped (`None`), Pantheon never requires
/// it, and `[budget] max_tokens` only sets the session default.
fn do_tokens(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
    let arg = cmd.strip_prefix("/tokens").map(str::trim).unwrap_or("");
    let mut budget = session.budget_snapshot();
    if arg.is_empty() {
        match budget.max_tokens {
            Some(n) => state.add_status(format!("max tokens: {n}")),
            None => state.add_status("max tokens: uncapped".into()),
        }
        return;
    }
    if matches!(arg, "off" | "clear" | "none" | "uncapped") {
        budget.max_tokens = None;
        session.set_budget(budget);
        state.add_status("max tokens: uncapped".into());
        return;
    }
    match arg.parse::<u32>() {
        Ok(n) if n > 0 => {
            budget.max_tokens = Some(n);
            session.set_budget(budget);
            state.add_status(format!("max tokens: {n}"));
        }
        _ => state.add_status("usage: /tokens [n | off]".into()),
    }
}

/// `/set [key value]` — show or retune the session's run budget live.
/// Session-scoped: config.toml `[budget]` holds the defaults, `/set`
/// changes this session only and the next turn picks it up.
fn do_set(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
    let arg = cmd.strip_prefix("/set").map(str::trim).unwrap_or("");
    let mut budget = session.budget_snapshot();
    if arg.is_empty() {
        state.add_status("session budget:".into());
        state.add_status(format!("  max_turns = {}", budget.max_turns));
        state.add_status(format!("  max_tool_calls = {}", budget.max_tool_calls));
        state.add_status(format!(
            "  max_delegate_depth = {}",
            budget.max_delegate_depth
        ));
        state.add_status(format!(
            "  max_tokens = {}",
            budget
                .max_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| "uncapped".into())
        ));
        state.add_status(
            "usage: /set <key> <value> — this session only; [budget] in config.toml holds the defaults"
                .into(),
        );
        return;
    }
    let (key, val) = match arg.split_once(char::is_whitespace) {
        Some((k, v)) => (k.to_lowercase(), v.trim().to_string()),
        None => {
            state.add_status(format!("usage: /set <key> <value> — unknown: {arg}"));
            return;
        }
    };
    let n: u32 = match val.parse() {
        Ok(n) => n,
        Err(_) => {
            state.add_status(format!("usage: /set {key} <number>"));
            return;
        }
    };
    // max_tokens accepts 0 = uncapped; the loop bounds require >= 1.
    let label = match key.as_str() {
        "max_turns" | "turns" => {
            if n == 0 {
                state.add_status("max_turns must be >= 1".into());
                return;
            }
            budget.max_turns = n;
            "max_turns"
        }
        "max_tool_calls" | "tool_calls" => {
            if n == 0 {
                state.add_status("max_tool_calls must be >= 1".into());
                return;
            }
            budget.max_tool_calls = n;
            "max_tool_calls"
        }
        "max_delegate_depth" | "delegate_depth" | "depth" => {
            if n == 0 {
                state.add_status("max_delegate_depth must be >= 1".into());
                return;
            }
            budget.max_delegate_depth = n;
            "max_delegate_depth"
        }
        "max_tokens" | "tokens" => {
            budget.max_tokens = if n == 0 { None } else { Some(n) };
            "max_tokens"
        }
        _ => {
            state.add_status(format!(
                "unknown key: {key} (max_turns, max_tool_calls, max_delegate_depth, max_tokens)"
            ));
            return;
        }
    };
    session.set_budget(budget);
    let shown = if label == "max_tokens" && n == 0 {
        "uncapped".to_string()
    } else {
        n.to_string()
    };
    state.add_status(format!("{label} = {shown} (this session)"));
}

/// `/reflect [on|off|status]` — the reflection toggle, OpenClaw-`/dreaming`
/// style. Bare `/reflect` runs a manual one-shot pass on a worker thread
/// (eval-gating can take minutes; the TUI never blocks). `on`/`off`
/// persist `[reflect] enabled` to config.toml; `status` shows the toggle
/// state plus the last pass summary.
fn do_reflect(state: &mut TuiState, tx: &std::sync::mpsc::Sender<TuiEvent>, cmd: &str) {
    let data_dir = crate::terminal::data_dir();
    let arg = cmd.strip_prefix("/reflect").map(str::trim).unwrap_or("");
    match arg {
        "on" => match crate::reflect_cli::persist_reflect_enabled(&data_dir, true) {
            Ok(()) => state.add_status("reflection loop: on (persisted to [reflect])".into()),
            Err(e) => state.add_status(format!("reflection: {e}")),
        },
        "off" => match crate::reflect_cli::persist_reflect_enabled(&data_dir, false) {
            Ok(()) => state.add_status("reflection loop: off (persisted to [reflect])".into()),
            Err(e) => state.add_status(format!("reflection: {e}")),
        },
        "status" => {
            for line in crate::reflect_cli::status_line(&data_dir).lines() {
                state.add_status(line.to_string());
            }
        }
        "" => {
            if state.reflect_running {
                state.add_status("reflection pass already running".into());
                return;
            }
            state.reflect_running = true;
            state.add_status("reflection pass started (background)".into());
            let tx2 = tx.clone();
            let dd = data_dir;
            std::thread::spawn(move || match crate::reflect_cli::run_one_pass(&dd, false) {
                Ok(out) => {
                    let summary = crate::reflect_cli::summarize_pass(&out);
                    let _ = tx2.send(TuiEvent::ReflectDone {
                        summary,
                        pending: out.pending,
                    });
                }
                Err(e) => {
                    // Route failures through ReflectDone, not the generic
                    // error event: the ReflectDone arm resets
                    // `reflect_running`, so a failed pass never wedges the
                    // trigger permanently on.
                    let _ = tx2.send(TuiEvent::ReflectDone {
                        summary: format!("reflection pass failed: {e}"),
                        pending: Vec::new(),
                    });
                }
            });
        }
        other => state.add_status(format!(
            "usage: /reflect [on|off|status] — unknown: {other}"
        )),
    }
}

/// `/consolidate [status|--dry-run]` — the nightly consolidation loop,
/// run by hand. Bare `/consolidate` runs a real pass on a worker thread
/// (a pass scans the ledger and can take a while; the TUI never blocks).
/// `--dry-run` stages and weighs but promotes nothing; `status` shows
/// the toggle state plus the last pass summary.
fn do_consolidate(state: &mut TuiState, tx: &std::sync::mpsc::Sender<TuiEvent>, cmd: &str) {
    let data_dir = crate::terminal::data_dir();
    let arg = cmd
        .strip_prefix("/consolidate")
        .map(str::trim)
        .unwrap_or("");
    match arg {
        "status" => {
            for line in crate::consolidate_cli::status_line(&data_dir).lines() {
                state.add_status(line.to_string());
            }
        }
        "" | "--dry-run" | "-n" => {
            let dry_run = !arg.is_empty();
            if state.consolidate_running {
                state.add_status("consolidation pass already running".into());
                return;
            }
            state.consolidate_running = true;
            state.add_status(if dry_run {
                "consolidation dry run started (background)".into()
            } else {
                "consolidation pass started (background)".into()
            });
            let tx2 = tx.clone();
            let dd = data_dir;
            std::thread::spawn(
                move || match crate::consolidate_cli::run_one_pass(&dd, dry_run) {
                    Ok(report) => {
                        let summary = crate::consolidate_cli::summarize_pass(&report, dry_run);
                        let _ = tx2.send(TuiEvent::ConsolidateDone { summary });
                    }
                    Err(e) => {
                        // Send ConsolidateDone (not TuiEvent::Error) so the
                        // consolidate_running guard is always reset.
                        let _ = tx2.send(TuiEvent::ConsolidateDone {
                            summary: format!("consolidation failed: {e}"),
                        });
                    }
                },
            );
        }
        other => state.add_status(format!(
            "usage: /consolidate [status|--dry-run] — unknown: {other}"
        )),
    }
}

/// Trigger rule for automatic reflection: fire on every `auto_turns`-th
/// completed turn, only when the loop is on and no pass is already
/// running. Pure over its inputs so the rule is unit-testable without a
/// TUI (`auto_turns = 0` disables the trigger entirely).
fn auto_reflect_due(turns_completed: u32, enabled: bool, auto_turns: u32, running: bool) -> bool {
    enabled
        && !running
        && auto_turns > 0
        && turns_completed > 0
        && turns_completed.is_multiple_of(auto_turns)
}

/// Maybe fire an automatic reflection pass after a completed turn. Reads
/// fresh config so a mid-session `/reflect on` takes effect without a
/// restart. The pass runs on a worker thread; results arrive as
/// `ReflectDone` and never touch the turn machinery.
fn maybe_auto_reflect(state: &mut TuiState, tx: &std::sync::mpsc::Sender<TuiEvent>) {
    let data_dir = crate::terminal::data_dir();
    let (enabled, auto_turns) = crate::reflect_cli::reflect_state(&data_dir);
    if !auto_reflect_due(
        state.turns_completed,
        enabled,
        auto_turns,
        state.reflect_running,
    ) {
        return;
    }
    state.reflect_running = true;
    state.add_status("automatic reflection pass started (background)".into());
    let tx2 = tx.clone();
    std::thread::spawn(
        move || match crate::reflect_cli::run_one_pass(&data_dir, false) {
            Ok(out) => {
                let summary = format!("automatic {}", crate::reflect_cli::summarize_pass(&out));
                let _ = tx2.send(TuiEvent::ReflectDone {
                    summary,
                    pending: out.pending,
                });
            }
            Err(e) => {
                let _ = tx2.send(TuiEvent::ReflectDone {
                    summary: format!("automatic reflection pass failed: {e}"),
                    pending: Vec::new(),
                });
            }
        },
    );
}

/// `/bg` lists background tasks with their states; `/bg <id>` shows one
/// task's full output.
fn do_bg(state: &mut TuiState, cmd: &str) {
    let arg = cmd.strip_prefix("/bg").map(str::trim).unwrap_or("");
    if arg.is_empty() {
        if state.bg_tasks.is_empty() {
            state.add_status("no background tasks".into());
            return;
        }
        // Collect owned lines first: `add_status` needs `&mut state`
        // while the borrow of `bg_tasks` is still live otherwise.
        let lines: Vec<String> = std::iter::once("background tasks:".to_string())
            .chain(state.bg_tasks.iter().map(|t| {
                let mut line = format!("  bg-{}  {}  {}", t.id, t.status.word(), t.label);
                if t.status.is_active() {
                    line.push_str(&format!("  ({}s)", t.elapsed_ms() / 1000));
                }
                line
            }))
            .collect();
        for line in lines {
            state.add_status(line);
        }
        return;
    }
    let key = arg.strip_prefix("bg-").unwrap_or(arg);
    let found = key.parse::<u64>().ok().and_then(|id| {
        state.bg_tasks.iter().find(|t| t.id == id).map(|t| {
            (
                t.id,
                t.status.word().to_string(),
                t.label.clone(),
                t.run_id.clone(),
                t.parent_run_id.clone(),
                t.output.clone(),
            )
        })
    });
    match found {
        None => state.add_status(format!("unknown background task '{arg}' (see /bg)")),
        Some((id, word, label, run_id, parent, output)) => {
            state.add_status(format!("bg-{id} · {word} · “{label}”"));
            state.add_status(format!("run {run_id} (spawned via /btw from {parent})"));
            match output {
                Some(o) => state.add_status(o),
                None => state.add_status("still running — no output yet".into()),
            }
        }
    }
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
    // A sent message leaves the composer in Normal mode, cursor home.
    state.vim.on_submit();
    if msg.is_empty() {
        return;
    }

    if msg.starts_with('/') {
        handle_slash(state, session, &msg, tx);
        return;
    }

    // `@file` mentions: attach file contents for the model, and annotate
    // the transcript copy with sizes/skip reasons. Resolution failures are
    // per-file and never block the turn.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (display, model) = crate::mentions::resolve_mentions(&msg, &cwd);
    state.add_user_message(display);

    // No second loop on a running turn: begin_turn queues the message
    // instead, and the queue drains when the turn ends. start_turn
    // resolves the run id at send time, so a resume that happened
    // mid-turn sends to the run that is selected now, not the one the
    // turn used.
    if !start_turn(state, session, tx, model) {
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
    // Resolve the ledger turn id of the turn being rewound: the most
    // recent TurnStarted in the effective history is the turn the operator
    // sees as last (replay already honors earlier rewinds).
    let turn_id = session
        .supervisor
        .replay(&state.session_id)
        .ok()
        .and_then(|entries| {
            entries.iter().rev().find_map(|e| match &e.event {
                RuntimeErrorEvent::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
                _ => None,
            })
        });
    let Some(turn_id) = turn_id else {
        state.add_status("rewind: no turn in ledger history, turn kept".to_string());
        return;
    };
    rewind_to_turn(
        state,
        session,
        &turn_id,
        offer.block_index,
        1,
        &format!("turn {}", offer.turn_no),
    );
}

/// Shared rewind core used by double-Esc rewind and `/restore`.
///
/// Emits the `TurnRewound` marker for `turn_id`, then truncates the live
/// view at `drop_block` (the block index of the first dropped user
/// message), restoring that message as the input draft. `turns_dropped`
/// adjusts the completed-turn counter and `label` names the rewind in the
/// status line. The marker write gates the truncation: on failure the
/// view is untouched and the error is reported.
pub(crate) fn rewind_to_turn(
    state: &mut TuiState,
    session: &Arc<Session>,
    turn_id: &str,
    drop_block: usize,
    turns_dropped: u32,
    label: &str,
) {
    if let Err(e) = session.supervisor.emit(RuntimeErrorEvent::TurnRewound {
        run_id: state.session_id.clone(),
        turn_id: turn_id.to_string(),
    }) {
        state.add_status(format!("rewind: ledger marker failed, turn kept: {e}"));
        return;
    }
    let draft = match state.blocks.get(drop_block) {
        Some(TranscriptBlock {
            kind: BlockKind::UserMessage(text),
        }) => text.clone(),
        _ => String::new(),
    };
    state.blocks.truncate(drop_block);
    state.input = draft;
    state.is_inputting = false;
    state.turn_in = None;
    state.turn_out = None;
    state.turn_estimate = 0;
    state.turn_started_at = None;
    state.turns_completed = state.turns_completed.saturating_sub(turns_dropped);
    state.timeline = None;
    state.jump_to_block = None;
    state.scroll_to_bottom();
    state.status_line = "ready".into();
    state.add_status(format!(
        "↩ rewound {label} — hidden from view; ledger keeps full history"
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

/// Key handling while the `@` file picker is open. Returns true when the
/// picker should close.
fn handle_mention_key(state: &mut TuiState, code: KeyCode) -> bool {
    match code {
        KeyCode::Enter | KeyCode::Tab => {
            if let Some(picker) = state.mention.take() {
                state.input = picker.apply_to(&state.input);
                if state.vim.enabled {
                    // The replacement lands at end-of-input; park the
                    // vim cursor there.
                    vim::move_to_end(&state.input, &mut state.vim);
                }
            }
            return true;
        }
        KeyCode::Esc => return true,
        _ => {}
    }
    let Some(picker) = state.mention.as_mut() else {
        return true;
    };
    match code {
        KeyCode::Up => picker.move_sel(-1),
        KeyCode::Down => picker.move_sel(1),
        KeyCode::Char(c) => {
            // Vim: type at the vim cursor (parked at end-of-input when
            // the picker opened) so the cursor tracks the filter text.
            if state.vim.enabled {
                vim::insert_char(&mut state.input, &mut state.vim, c);
            } else {
                state.input.push(c);
            }
            picker.input.push(c);
            picker.requery();
        }
        KeyCode::Backspace => {
            if state.vim.enabled {
                vim::backspace(&mut state.input, &mut state.vim);
            } else {
                state.input.pop();
            }
            picker.input.pop();
            if state.input.len() <= picker.anchor {
                return true; // backspaced over the `@`: cancel the mention
            }
            picker.requery();
        }
        _ => {}
    }
    false
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
                TuiEvent::TurnComplete { run_id } => {
                    // Steering that arrived after the turn's last boundary
                    // was never delivered: fold it into the queued
                    // follow-up so it isn't silently lost. It goes out
                    // with the next turn instead of redirecting this one.
                    for s in session.drain_steers() {
                        let q = state.queued_message.take().unwrap_or_default();
                        state.queued_message =
                            Some(if q.is_empty() { s } else { format!("{q}\n{s}") });
                    }
                    // `session_id` follows the visible tab; a completion
                    // for any other run belongs to a background tab: badge
                    // it and notify instead of touching the visible input.
                    if crate::notify::should_notify(&run_id, &state.session_id) {
                        state.on_turn_complete(false);
                        let summary = crate::notify::summarize_turn(
                            state.blocks.iter().rev().find_map(|b| match &b.kind {
                                BlockKind::AssistantMessage(t) => Some(t.as_str()),
                                _ => None,
                            }),
                            state.turns_completed,
                        );
                        let label = state
                            .title
                            .clone()
                            .unwrap_or_else(|| state.session_id.clone());
                        crate::notify::emit_turn_notification(&label, &summary);
                        refresh_tabs(state, &session);
                    } else {
                        state.on_turn_complete(true);
                        session.reset_cancel();
                        // A message typed while the turn ran goes out now,
                        // to the currently selected run.
                        drain_queued_message(state, &session, tx);
                        // Titles/busy badges may have changed.
                        refresh_tabs(state, &session);
                        // Automatic reflection: every `auto_turns`
                        // completed turns, when the loop is on. Reads
                        // fresh config so a mid-session `/reflect on`
                        // takes effect without a restart. The pass runs
                        // on a worker thread; results arrive as
                        // ReflectDone and never touch the turn machinery.
                        maybe_auto_reflect(state, tx);
                    }
                }
                TuiEvent::ReflectDone { summary, pending } => {
                    // A reflection pass finished on its worker thread. The
                    // main turn — if one is running — is untouched: this arm
                    // never writes active_run, ready, or the input line.
                    state.reflect_running = false;
                    for line in summary.lines() {
                        state.add_status(line.to_string());
                    }
                    if !pending.is_empty() {
                        state.pending_reflect = Some(pending);
                        state.add_status(
                            "reflection proposals await approval: [y] approve all  [n] deny all"
                                .into(),
                        );
                    }
                }
                TuiEvent::ConsolidateDone { summary } => {
                    // A consolidation pass finished on its worker thread.
                    // The main turn — if one is running — is untouched:
                    // this arm never writes active_run, ready, or the
                    // input line.
                    state.consolidate_running = false;
                    for line in summary.lines() {
                        state.add_status(line.to_string());
                    }
                }
                TuiEvent::BgDone { task_id, result } => {
                    // A background task finished: record the terminal state,
                    // hand the result back into the transcript as a labeled
                    // block, and ping the operator (bell + notify-send, the
                    // same mechanism as turn-complete notifications). The
                    // main turn — if one is running — is untouched: this arm
                    // never writes active_run, ready, or the input line.
                    let finished = state
                        .bg_tasks
                        .iter_mut()
                        .find(|t| t.id == task_id)
                        .map(|t| {
                            let (output, ok) = match result {
                                Ok(text) => (text, true),
                                Err(err) => (err, false),
                            };
                            if ok {
                                t.finish(output.clone());
                            } else {
                                t.fail(output.clone());
                            }
                            (t.label.clone(), output, ok)
                        });
                    if let Some((label, output, ok)) = finished {
                        state.blocks.push(TranscriptBlock {
                            kind: BlockKind::BgResult {
                                task_id,
                                label: label.clone(),
                                output: output.clone(),
                                ok,
                            },
                        });
                        state.scroll_to_bottom();
                        let summary = if ok {
                            format!("bg-{task_id} done — {}", bg::summary_line(&output))
                        } else {
                            format!("bg-{task_id} failed — {}", bg::summary_line(&output))
                        };
                        crate::notify::emit_turn_notification(
                            &format!("background task bg-{task_id}"),
                            &summary,
                        );
                    }
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
                                switch_to_run(state, &session, &id, "resumed");
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
                if state.show_shortcuts {
                    // The overlay owns the keyboard while open: Esc/?/q
                    // dismiss, everything else is swallowed so stray
                    // keystrokes never reach the composer or scroll the
                    // transcript behind it.
                    match key.code {
                        KeyCode::Esc
                        | KeyCode::Char('?')
                        | KeyCode::Char('q')
                        | KeyCode::Char('Q') => {
                            state.show_shortcuts = false;
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                if state.mention.is_some() {
                    // The `@` file picker owns the keyboard while open:
                    // type to filter, Up/Down or Tab to move, Enter to
                    // insert, Esc to cancel.
                    let done = handle_mention_key(state, key.code);
                    if done {
                        state.mention = None;
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                if state.img.preview.is_some() {
                    // The image preview owns the keyboard while open:
                    // arrows pan, +/- zoom, [/] cycle images, q/Esc closes.
                    // Everything else is swallowed.
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                            let _ = state.img.close_preview();
                        }
                        KeyCode::Char('[') => cycle_preview_image(state, -1),
                        KeyCode::Char(']') => cycle_preview_image(state, 1),
                        KeyCode::Char('+') | KeyCode::Char('=') => zoom_preview(state, 1.25),
                        KeyCode::Char('-') | KeyCode::Char('_') => zoom_preview(state, 0.8),
                        KeyCode::Char('0') => {
                            if let Some(p) = state.img.preview.as_mut() {
                                p.zoom = 1.0;
                                p.pan_x = 0;
                                p.pan_y = 0;
                            }
                        }
                        KeyCode::Up => {
                            if let Some(p) = state.img.preview.as_mut() {
                                p.pan_y -= 2;
                            }
                        }
                        KeyCode::Down => {
                            if let Some(p) = state.img.preview.as_mut() {
                                p.pan_y += 2;
                            }
                        }
                        KeyCode::Left => {
                            if let Some(p) = state.img.preview.as_mut() {
                                p.pan_x -= 4;
                            }
                        }
                        KeyCode::Right => {
                            if let Some(p) = state.img.preview.as_mut() {
                                p.pan_x += 4;
                            }
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
                        // Yank the last assistant message to the clipboard.
                        // Only when the composer is empty so it never eats
                        // typed text; /yank covers the rest.
                        KeyCode::Char('y') | KeyCode::Char('Y')
                            if state.input.trim().is_empty() => {
                                do_yank(state, "");
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
                        crate::tabs::TabAction::Next => state.tabs.cycle_next().map(str::to_string),
                        crate::tabs::TabAction::Prev => state.tabs.cycle_prev().map(str::to_string),
                        crate::tabs::TabAction::Jump(n) => {
                            if state.tabs.jump(n) {
                                state.tabs.active_run_id().map(str::to_string)
                            } else {
                                None
                            }
                        }
                    };
                    if let Some(id) = target {
                        switch_to_run(state, &session, &id, "resumed");
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
                                                    let _ = tx3.send(TuiEvent::TurnComplete {
                                                        run_id: run3.clone(),
                                                    });
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
                        } else if state.pending_reflect.is_some() {
                            // Reflection card: y approves every pending
                            // proposal (eval-gated already), n denies them
                            // all, Esc dismisses without deciding.
                            match c {
                                'y' | 'Y' => {
                                    if let Some(pending) = state.pending_reflect.take() {
                                        let dd = crate::terminal::data_dir();
                                        let mut approved = 0;
                                        for pp in &pending {
                                            match pantheon_reflect::approve_pending(
                                                &dd,
                                                &pp.proposal.id,
                                                "tui",
                                            ) {
                                                Ok(outcome) => {
                                                    approved += 1;
                                                    state.add_status(format!(
                                                        "approved {}: {}",
                                                        pp.proposal.id,
                                                        outcome.describe()
                                                    ));
                                                }
                                                Err(e) => state.add_status(format!(
                                                    "approve {} failed: {e}",
                                                    pp.proposal.id
                                                )),
                                            }
                                        }
                                        state.status_line =
                                            format!("reflection: {approved} approved");
                                    }
                                }
                                'n' | 'N' => {
                                    if let Some(pending) = state.pending_reflect.take() {
                                        let dd = crate::terminal::data_dir();
                                        let mut denied = 0;
                                        for pp in &pending {
                                            if pantheon_reflect::deny_pending(
                                                &dd,
                                                &pp.proposal.id,
                                                "tui",
                                            )
                                            .is_ok()
                                            {
                                                denied += 1;
                                            }
                                        }
                                        state.status_line = format!("reflection: {denied} denied");
                                    }
                                }
                                _ => {}
                            }
                        } else if state.vim.enabled {
                            // Vim: the composer is a modal buffer. Normal
                            // mode interprets the key as a motion/edit;
                            // Insert mode types at the vim cursor.
                            // Waking the composer always starts in Normal:
                            // the first key is never typed text.
                            if !state.is_inputting && !matches!(c, 'q' | 'Q' | '?') {
                                state.is_inputting = true;
                                state.vim.mode = vim::VimMode::Normal;
                            }
                            if state.vim.mode == vim::VimMode::Normal && state.is_inputting {
                                match vim::handle_normal_key(&mut state.input, &mut state.vim, c) {
                                    vim::NormalKey::ToInsert => {
                                        // Snapshot so `u` undoes the whole
                                        // insert session, not each char.
                                        vim::begin_insert_undo(&state.input, &mut state.vim);
                                        state.vim.mode = vim::VimMode::Insert;
                                    }
                                    // Empty-buffer app affordances, same as
                                    // the non-vim path below.
                                    vim::NormalKey::Quit => break,
                                    vim::NormalKey::Shortcuts => {
                                        state.show_shortcuts = true;
                                    }
                                    // `v` still opens the image preview when
                                    // the transcript has images; with none it
                                    // is an unbound Normal key, not typed.
                                    vim::NormalKey::Pass('v') | vim::NormalKey::Pass('V') => {
                                        if !state.img.placements.is_empty() {
                                            state.img.open_preview();
                                        }
                                    }
                                    vim::NormalKey::Pass(_) | vim::NormalKey::Consumed => {}
                                }
                            } else if state.is_inputting {
                                if c == '@' {
                                    // Open the `@` file picker: the `@` stays
                                    // in the composer and the picker filters
                                    // on the text typed after it. Mentions
                                    // complete at end-of-input, exactly like
                                    // the legacy path below.
                                    vim::move_to_end(&state.input, &mut state.vim);
                                    vim::insert_char(&mut state.input, &mut state.vim, '@');
                                    let anchor = state.input.len() - 1;
                                    let cwd = std::env::current_dir()
                                        .unwrap_or_else(|_| PathBuf::from("."));
                                    state.mention =
                                        Some(crate::mentions::MentionPicker::open(cwd, anchor));
                                } else {
                                    vim::insert_char(&mut state.input, &mut state.vim, c);
                                }
                            } else {
                                // Not inputting and vim on: only q/? reach
                                // here (guarded above); same as non-vim.
                                match c {
                                    'q' | 'Q' => break,
                                    '?' => state.show_shortcuts = true,
                                    _ => {}
                                }
                            }
                        } else if state.is_inputting {
                            if c == '@' {
                                // Open the `@` file picker: the `@` stays in
                                // the composer and the picker filters on the
                                // text typed after it.
                                state.input.push('@');
                                let anchor = state.input.len() - 1;
                                let cwd =
                                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                                state.mention =
                                    Some(crate::mentions::MentionPicker::open(cwd, anchor));
                            } else {
                                state.input.push(c);
                            }
                        } else {
                            match c {
                                'q' | 'Q' => break,
                                // `?` opens the shortcuts overlay instead of
                                // landing in the composer; only when the
                                // prompt is empty and we are not editing.
                                '?' => state.show_shortcuts = true,
                                // `v` opens the image preview when the
                                // transcript has images; with none it types
                                // like any other char.
                                'v' | 'V' => {
                                    if state.img.placements.is_empty() {
                                        state.is_inputting = true;
                                        state.input.push(c);
                                    } else {
                                        state.img.open_preview();
                                    }
                                }
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
                        if state.vim.enabled && state.is_inputting {
                            match state.vim.mode {
                                // Insert: delete before the cursor.
                                vim::VimMode::Insert => {
                                    vim::backspace(&mut state.input, &mut state.vim);
                                }
                                // Normal: Backspace steps left like `h`.
                                vim::VimMode::Normal => {
                                    let _ = vim::handle_normal_key(
                                        &mut state.input,
                                        &mut state.vim,
                                        'h',
                                    );
                                }
                            }
                        } else if state.is_inputting {
                            state.input.pop();
                        }
                    }
                    KeyCode::Enter => {
                        if state.is_inputting {
                            // Submit in both modes: Normal+Enter sends, the
                            // same muscle memory as Insert.
                            let raw = state.input.clone();
                            submit_text(state, &session, tx, &raw);
                        } else {
                            state.is_inputting = true;
                            if state.vim.enabled {
                                state.vim.mode = vim::VimMode::Normal;
                            }
                        }
                    }
                    KeyCode::Esc => {
                        if state.vim.enabled
                            && state.pending_approval.is_none()
                            && state.pending_reflect.is_none()
                        {
                            match state.vim.mode {
                                // Insert → Normal. Never arms the
                                // interrupt/rewind: the legacy path below is
                                // skipped entirely.
                                vim::VimMode::Insert => state.vim.esc_to_normal(),
                                // Normal with a draft: Esc is a no-op, the
                                // text is kept (vim-like; the legacy path
                                // would clear it).
                                vim::VimMode::Normal if !state.input.trim().is_empty() => {}
                                // Normal on an empty prompt: the legacy
                                // double-Esc interrupt/rewind still works.
                                vim::VimMode::Normal => {
                                    state.is_inputting = false;
                                    if state.press_esc() {
                                        let target = state.interrupt_target().to_string();
                                        session
                                            .cancel_current_run(&target, "user pressed esc twice");
                                    }
                                }
                            }
                        } else {
                            state.is_inputting = false;
                            // Double-Esc interrupts the active run. The arm
                            // and confirm rules live in `press_esc` so they
                            // are testable without a terminal.
                            if state.press_esc() {
                                let target = state.interrupt_target().to_string();
                                session.cancel_current_run(&target, "user pressed esc twice");
                            }
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
            BlockKind::BgResult {
                task_id,
                label,
                output,
                ok,
            } => (
                "background",
                format!(
                    "bg-{task_id} [{}] {label}\n{output}",
                    if *ok { "done" } else { "failed" }
                ),
            ),
            BlockKind::Status(t) => ("status", t.clone()),
            BlockKind::Diff(lines) => (
                "diff",
                lines
                    .iter()
                    .map(|l| match l {
                        crate::diffview::DiffLine::FileHeader(p) => format!("diff {p}"),
                        crate::diffview::DiffLine::Hunk(h) => h.clone(),
                        crate::diffview::DiffLine::Context(c) => format!(" {c}"),
                        crate::diffview::DiffLine::Add(a) => format!("+{a}"),
                        crate::diffview::DiffLine::Del(d) => format!("-{d}"),
                        crate::diffview::DiffLine::Truncated(n) => format!("… {n} more lines"),
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
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

/// Copy the last assistant message — or its nth fenced code block — to
/// the system clipboard. `/yank` copies the whole message, `/yank 2` the
/// second code block. Best-effort: reports which backend was used, or
/// that none is installed.
/// `/steer <guidance>`: redirect the running turn without canceling it
/// and without waiting for it to end. The guidance lands in the turn at
/// the next boundary as a marked `SteeringProvided` event the model
/// reads on its next step.
///
/// When no turn is running there is nothing to redirect, so the text
/// is sent as an ordinary message (a fresh turn starts). Steering never
/// queues: a queued message waits for the turn to end, steering
/// deliberately does not.
fn do_steer(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    arg: &str,
) {
    if arg.is_empty() {
        state.add_status("usage: /steer <guidance> — redirect the running turn mid-flight".into());
        return;
    }
    if state.ready {
        // Idle: steer degrades to a normal message.
        let msg = arg.to_string();
        state.add_user_message(msg.clone());
        state.add_status("no turn running — sent as a normal message".into());
        start_turn(state, session, tx, msg);
    } else {
        session.steer(arg);
        state.status_line = "steering the running turn…".into();
    }
}

fn do_yank(state: &mut TuiState, arg: &str) {
    let Some(text) = state.blocks.iter().rev().find_map(|b| match &b.kind {
        BlockKind::AssistantMessage(t) => Some(t.clone()),
        _ => None,
    }) else {
        state.add_status("nothing to yank: no assistant message yet".into());
        return;
    };
    let payload = if arg.is_empty() {
        text
    } else {
        match arg.parse::<usize>() {
            Ok(n) if n >= 1 => {
                let blocks = crate::yank::extract_code_blocks(&text);
                match blocks.get(n - 1) {
                    Some(b) => b.code.clone(),
                    None => {
                        state.add_status(format!(
                            "no code block {n}: last answer has {}",
                            blocks.len()
                        ));
                        return;
                    }
                }
            }
            _ => {
                state.add_status("usage: /yank [N] — copies the Nth code block".into());
                return;
            }
        }
    };
    match crate::yank::copy_to_clipboard(&payload) {
        Some(backend) => state.add_status(format!(
            "yanked {} chars via {backend}",
            payload.chars().count()
        )),
        None => state.add_status("no clipboard tool found (wl-copy, xclip, xsel, pbcopy)".into()),
    }
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
    if cmd == "/yank" || cmd.starts_with("/yank ") {
        let arg = cmd.strip_prefix("/yank").map(str::trim).unwrap_or("");
        do_yank(state, arg);
        return;
    }
    if cmd == "/steer" || cmd.starts_with("/steer ") {
        let arg = cmd.strip_prefix("/steer").map(str::trim).unwrap_or("");
        do_steer(state, session, tx, arg);
        return;
    }
    // Background tasks: /btw fires one without touching the running turn;
    // /bg inspects the task list.
    if cmd == "/btw" || cmd.starts_with("/btw ") {
        let prompt = cmd.strip_prefix("/btw").map(str::trim).unwrap_or("");
        do_btw(state, session, tx, prompt);
        return;
    }
    if cmd == "/bg" || cmd.starts_with("/bg ") {
        do_bg(state, cmd);
        return;
    }
    // Reflection: bare /reflect runs a manual pass; on/off/status manage
    // the loop (persisted to [reflect]).
    if cmd == "/reflect" || cmd.starts_with("/reflect ") {
        do_reflect(state, tx, cmd);
        return;
    }
    // Consolidation: bare /consolidate runs a pass; status/--dry-run are
    // the read-only views (dry run changes nothing, status runs nothing).
    if cmd == "/consolidate" || cmd.starts_with("/consolidate ") {
        do_consolidate(state, tx, cmd);
        return;
    }
    // Session objective with an iteration budget: /goal <text> sets it,
    // /goal shows it, /goal clear drops it, /goal iterations N retunes
    // the cap. Each turn started under a goal consumes one iteration.
    if cmd == "/goal" || cmd.starts_with("/goal ") {
        do_goal(state, session, cmd);
        return;
    }
    // Token cap: strictly optional, session-scoped. Bare /tokens shows it.
    if cmd == "/tokens" || cmd.starts_with("/tokens ") {
        do_tokens(state, session, cmd);
        return;
    }
    // Live budget tuning for this session ([budget] holds the defaults).
    // Placed before /help; "/settings" does not match "/set " so the
    // prefix check cannot swallow it.
    if cmd == "/set" || cmd.starts_with("/set ") {
        do_set(state, session, cmd);
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
        state.add_status(
            "  /goal [TEXT]        set/show the session goal (iteration-limited)".into(),
        );
        state.add_status("  /goal clear         drop the session goal".into());
        state.add_status("  /goal iterations N  retune the goal's iteration budget".into());
        state.add_status(
            "  /tokens [N|off]     show/set the per-run token cap (default: uncapped)".into(),
        );
        state.add_status(
            "  /set [KEY VAL]      show/set session budget (max_turns, max_tool_calls, max_delegate_depth, max_tokens)"
                .into(),
        );
        state
            .add_status("  /learn LESSON      save a behavioral lesson for future sessions".into());
        state.add_status("  /skills [FILTER]   installed skills".into());
        state.add_status("  /<skill> [input]   invoke an installed skill by name".into());
        state.add_status("  /tools [reload]    rebuild the tool registry in place".into());
        state.add_status(
            "  /settings          data dir, model, policy, memory, server, agents".into(),
        );
        state.add_status("  /gateway           service state and queued outbound".into());
        state.add_status("  /doctor            diagnose this install".into());
        state.add_status("  /sessions          live sessions holding a lease".into());
        state.add_status("  /new               start a fresh conversation".into());
        state.add_status(
            "  /rewind            roll back the last turn (confirm; ledger kept)".into(),
        );
        state.add_status("  /checkpoint [NAME] save a named snapshot of the current turn".into());
        state.add_status("  /checkpoints       list saved checkpoints".into());
        state.add_status("  /restore NAME      rewind back to a checkpoint (ledger kept)".into());
        state.add_status("  /swarm             this session's delegation tree".into());
        state.add_status(
            "  /btw PROMPT        run a task in the background (result lands here)".into(),
        );
        state.add_status("  /bg [ID]          list background tasks, or show one's output".into());
        state.add_status(
            "  /fork [TURN]       branch this conversation at a turn into a new run".into(),
        );
        state.add_status("  /theme [name]      switch theme (pantheon, dark, light)".into());
        state.add_status(
            "  /vim [on|off|status] modal vim editing for the composer (v1: Normal/Insert only)"
                .into(),
        );
        state.add_status(
            "  /reflect [on|off|status] run a reflection pass now, or toggle the self-improvement loop".into(),
        );
        state.add_status(
            "  /consolidate [status|--dry-run] run a memory consolidation pass (dry run changes nothing)".into(),
        );
        state.add_status("  /compress         compress this conversation to the window now".into());
        state.add_status("  /export [md|json]  save this conversation to exports/".into());
        state.add_status(
            "  /yank [N]          copy last answer (or its Nth code block) to clipboard".into(),
        );
        state.add_status(
            "  /steer <text>      redirect the running turn mid-flight (normal message when idle)"
                .into(),
        );
        state.add_status("  /runs [N]          recent runs (default 10)".into());
        state.add_status(
            "  /history           interactive searchable history (pick + resume)".into(),
        );
        state.add_status("  /resume [ID]       resume a run by id".into());
        state.add_status(
            "  /title [TITLE]      show this conversation's title, or rename it".into(),
        );
        state.add_status("  /status [run_id]   this run's status, or another by id".into());
        state.add_status("  /agent [name]     current agent profile, or switch to one".into());
        state.add_status("  /agents           declared agent profiles".into());
        state.add_status("  /collab           active collaborations and their tasks".into());
        state.add_status("  /tasks <agent>    that agent's open tasks".into());
        state.add_status("  /inbox            messages sent to this agent".into());
        state.add_status("  /approvals         pending approvals for this run".into());
        state.add_status("  /schedule          scheduled jobs".into());
        state.add_status(
            "  /mcp [reload]        MCP server declarations (reload re-scans them)".into(),
        );
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
        state.add_status(
            "  /reset             reset turn state: clear transcript, cancel turn, drop queue (session, title, ledger kept; /clear is display-only, /new starts a new session)".into(),
        );
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
    if cmd == "/reset" {
        // Capture the interrupt target before the reset clears it.
        let target = state.interrupt_target().to_string();
        let was_running = state.reset_ephemeral();
        if was_running {
            // Same cooperative cancel as double-Esc: the loop stops at its
            // next boundary, in-flight provider requests excepted.
            session.cancel_current_run(&target, "user ran /reset");
            state.add_status("reset: turn canceled, transcript and queue cleared".into());
        } else {
            state.add_status("reset: transcript and queue cleared".into());
        }
        state.add_status("session, title, and ledger history kept".into());
        return;
    }
    // /title is the canonical rename command; /name stays as a silent
    // backward-compatible alias (not shown in /help).
    if cmd == "/title" || cmd.starts_with("/title ") || cmd == "/name" || cmd.starts_with("/name ")
    {
        let rest = cmd
            .strip_prefix("/title")
            .or_else(|| cmd.strip_prefix("/name"))
            .unwrap_or("")
            .trim();
        if rest.is_empty() {
            match state.title.as_deref().filter(|t| !t.is_empty()) {
                Some(t) => state.add_status(format!("title: {t}")),
                None => state.add_status("(untitled)".into()),
            }
            state.add_status("rename with: /title <new title>".into());
            return;
        }
        // Same normalization contract as the aux: one bounded line.
        let title = pantheon_api::model::bound_title(rest, pantheon_api::model::TITLE_MAX_CHARS);
        if title.is_empty() {
            state.add_status("/title: nothing to title with".into());
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
            Err(e) => state.add_status(format!("/title: {e}")),
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
                        for item in pantheon_runtime::session::rebuild_transcript(entries) {
                            let kind = match item {
                                pantheon_runtime::session::TranscriptItem::Message(m) => {
                                    match m.role {
                                        pantheon_api::message::Role::User => {
                                            BlockKind::UserMessage(m.content.clone())
                                        }
                                        _ => BlockKind::AssistantMessage(m.content.clone()),
                                    }
                                }
                                // Imported reasoning traces render as thinking blocks.
                                pantheon_runtime::session::TranscriptItem::Reasoning(text) => {
                                    BlockKind::Thinking(text)
                                }
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
    if cmd == "/stats" {
        // Today's usage from the ledger, plus this session's share.
        // Same aggregation as `pantheon stats`; the ledger is the source
        // of truth for both, so the two never disagree.
        let data_dir = crate::terminal::data_dir();
        match pantheon_storage::Ledger::open(&data_dir.join("ledger.db")) {
            Ok(ledger) => {
                let now = crate::stats::now_ms();
                let from = crate::stats::day_start_ms(now);
                match crate::stats::collect(&ledger, from, from + crate::stats::DAY_MS) {
                    Ok(report) => state.add_status(crate::stats::render_session_summary(
                        &report,
                        state.interrupt_target(),
                    )),
                    Err(e) => state.add_status(format!("stats: {e}")),
                }
            }
            Err(e) => state.add_status(format!("stats: {e}")),
        }
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
        // A new conversation is a new objective: the /goal iteration gate
        // belongs to the old work and must not block the new session.
        state.goal = None;
        session.set_goal(None);
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
    if cmd == "/checkpoint" || cmd.starts_with("/checkpoint ") {
        crate::checkpoint::cmd_checkpoint(state, session, cmd);
        return;
    }
    if cmd == "/checkpoints" {
        crate::checkpoint::cmd_checkpoints(state, session);
        return;
    }
    if cmd == "/restore" || cmd.starts_with("/restore ") {
        crate::checkpoint::cmd_restore(state, session, cmd);
        return;
    }
    if cmd == "/swarm" {
        crate::swarm_view::cmd_swarm(state, session);
        return;
    }
    if cmd == "/theme" || cmd.starts_with("/theme ") {
        // Live theme switching with persistence: the name goes to
        // `[tui] theme` in config.toml so the next launch restores it.
        let arg = cmd.strip_prefix("/theme").unwrap_or("").trim();
        if arg.is_empty() {
            let mut names: Vec<String> = Vec::new();
            for name in theme::Theme::all_names() {
                if name == state.theme.name {
                    names.push(format!("{name} (current)"));
                } else {
                    names.push(name.to_string());
                }
            }
            state.add_status(format!("themes: {}", names.join(", ")));
        } else if state.set_theme(arg) {
            let applied = state.theme.name;
            match theme::save_theme(&crate::terminal::data_dir(), applied) {
                Ok(()) => state.add_status(format!("theme: {applied}")),
                Err(e) => state.add_status(format!("theme applied, not saved: {}", e.cause)),
            }
        } else {
            state.add_status(format!(
                "unknown theme '{arg}'; try: {}",
                theme::Theme::all_names().join(", ")
            ));
        }
        return;
    }
    if cmd == "/vim" || cmd.starts_with("/vim ") {
        // Modal vim editing for the composer: opt-in, persisted to
        // `[tui] vim`. Bare `/vim` toggles; on/off/status are explicit.
        let arg = cmd.strip_prefix("/vim").unwrap_or("").trim().to_lowercase();
        let enabled = match arg.as_str() {
            "" => !state.vim.enabled,
            "on" => true,
            "off" => false,
            "status" => {
                let on_off = if state.vim.enabled { "on" } else { "off" };
                state.add_status(format!("vim mode: {on_off}"));
                return;
            }
            _ => {
                state.add_status("usage: /vim [on|off|status]".into());
                return;
            }
        };
        state.vim.enabled = enabled;
        if enabled {
            // Enabling mid-composer starts in Normal, cursor home.
            state.vim.on_submit();
        }
        let word = if enabled { "on" } else { "off" };
        match vim::save_vim(&crate::terminal::data_dir(), enabled) {
            Ok(()) => state.add_status(if enabled {
                "vim mode: on (-- NORMAL --)".into()
            } else {
                "vim mode: off".into()
            }),
            Err(e) => state.add_status(format!("vim {word} (not saved: {})", e.cause)),
        }
        return;
    }
    if cmd == "/fork" || cmd.starts_with("/fork ") {
        // Branch the durable history into a new run at a chosen turn.
        // The source run is untouched; the fork is a first-class run that
        // resumes, replays, and titles on its own.
        let arg = cmd.strip_prefix("/fork").unwrap_or("").trim();
        let want = if arg.is_empty() {
            None
        } else {
            match arg.parse::<usize>() {
                Ok(n) => Some(n),
                Err(_) => {
                    state.add_status("usage: /fork [TURN]  (turns are 1-based)".into());
                    return;
                }
            }
        };
        if !state.ready {
            state.add_status("/fork: wait for the running turn to finish".into());
            return;
        }
        match supervisor.fork_run(&state.session_id, want) {
            Ok((new_id, n)) => {
                switch_to_run(state, session, &new_id, "forked");
                refresh_tabs(state, session);
                state.add_status(format!("forked at turn {n} → {new_id}"));
            }
            Err(e) => state.add_status(format!("/fork: {}", e.cause)),
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
    if cmd == "/learn" || cmd.starts_with("/learn ") {
        // /remember stores a fact under an explicit key; /learn stores a
        // behavioral lesson ("when editing Rust, run cargo fmt first") as
        // free text, auto-keyed under the `lesson:` marker so it reads as
        // guidance rather than trivia on recall.
        let lesson = cmd.strip_prefix("/learn").unwrap_or("").trim();
        if lesson.is_empty() {
            state.add_status("usage: /learn <lesson>".into());
            return;
        }
        match learn_lesson(session, lesson) {
            Ok(key) => state.add_status(format!("learned [{key}]: {lesson}")),
            Err(e) => state.add_status(format!("/learn: {e}")),
        }
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
                        let _ = tx4.send(TuiEvent::TurnComplete {
                            run_id: run4.clone(),
                        });
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
                let is_ready = crate::mcp::server_readiness(s).is_none();
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
    if cmd == "/mcp reload" {
        // Re-scan the declaration files on disk: picks up servers added
        // or edited since startup without restarting the TUI. There are
        // no live MCP clients to reconnect (no launcher yet, spec
        // section 15), so a reload can never drop the session — the
        // worst case is an unchanged report.
        let dd = crate::terminal::data_dir();
        for line in crate::mcp::reload_report(&dd) {
            state.add_status(line);
        }
        return;
    }
    // --- tools -----------------------------------------------------------
    if cmd == "/tools" || cmd == "/tools reload" {
        // Rebuild through the shared constructor: this is the same
        // registry the next turn will actually use, so the report can
        // never describe a different tool set. Skill discovery re-reads
        // the skill directories, so newly installed skills appear without
        // a restart. MCP contributes zero tools: declarations exist, but
        // there is no launcher attaching them yet (spec section 15).
        let (reg, counts) = session.build_tool_registry();
        debug_assert_eq!(counts.total(), reg.names().len());
        state.add_status(format!(
            "{} tools available ({} built-in, {} skill, {} session-search, 0 MCP — no launcher yet)",
            counts.total(),
            counts.builtin,
            counts.skills,
            counts.session_search
        ));
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
    // Dynamic skill invocation: /<skill-name> [input...]. Every built-in
    // above takes precedence by dispatch order; this only fires for words
    // the command registry doesn't know. Unknown names fall through to
    // the existing unknown-command error below.
    {
        let word = cmd.split_whitespace().next().unwrap_or(cmd);
        if let Some(name) = word.strip_prefix('/') {
            let name = name.trim();
            if !name.is_empty() && !crate::commands::is_builtin(name)
                && try_invoke_skill(state, session, tx, name, cmd) {
                    return;
                }
        }
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

/// Find an installed skill by name (case-insensitive). Pure lookup over
/// an already-discovered skill list, so the matching contract is testable
/// without touching the filesystem.
pub fn find_skill_by_name<'a>(
    skills: &'a [pantheon_exec::skills::Skill],
    name: &str,
) -> Option<&'a pantheon_exec::skills::Skill> {
    skills
        .iter()
        .find(|s| s.meta.name.eq_ignore_ascii_case(name))
}

/// Invoke an installed skill as `/name [input...]`: the skill body becomes
/// the turn's instructions and the remainder of the line its task input —
/// exactly as if the agent had read the skill itself via `skill_read`.
///
/// Returns true when the word matched a skill (the command is consumed
/// even if reading the body failed); false when no installed skill has
/// that name, so the caller falls through to unknown-command handling.
fn try_invoke_skill(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    name: &str,
    full_cmd: &str,
) -> bool {
    let dd = crate::terminal::data_dir();
    let found = crate::skills::extra_roots();
    let skills =
        pantheon_exec::skills::discover_skills_ext(&dd, &crate::skills::project_root(), &found);
    let Some(skill) = find_skill_by_name(&skills, name) else {
        return false;
    };
    let input = full_cmd.split_once(char::is_whitespace).map(|x| x.1)
        .unwrap_or("")
        .trim();
    let body = match pantheon_exec::skills::skill_body(skill) {
        Ok(b) => b,
        Err(e) => {
            state.add_status(format!("/{name}: cannot read skill: {e}"));
            return true;
        }
    };
    // The transcript shows the command as typed (provenance); the model
    // gets the skill body plus the task input.
    state.add_user_message(full_cmd.to_string());
    let model_msg = if input.is_empty() {
        format!("# Skill: {}\n\n{}", skill.meta.name, body)
    } else {
        format!(
            "# Skill: {}\n\n{}\n\n## Task input\n{}",
            skill.meta.name, body, input
        )
    };
    if !start_turn(state, session, tx, model_msg) {
        state.add_status(format!(
            "skill '{}' queued for after this turn",
            skill.meta.name
        ));
    } else {
        state.add_status(format!("invoking skill '{}'", skill.meta.name));
    }
    true
}

/// Turn free-text lesson into a stable memory key: `lesson:<slug>`. The
/// `lesson:` prefix is the marker distinguishing behavioral lessons
/// (from /learn) from plain facts (from /remember KEY TEXT).
pub fn slugify_lesson_key(lesson: &str) -> String {
    let slug: String = lesson
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(6)
        .collect::<Vec<_>>()
        .join("-");
    let slug: String = slug.chars().take(48).collect();
    format!(
        "lesson:{}",
        if slug.is_empty() { "untitled" } else { &slug }
    )
}

/// Build the memory proposal for a `/learn` lesson. Separated from the
/// store write so the keying/provenance contract is testable without a
/// database.
pub fn lesson_proposal(namespace: &str, lesson: &str, now_ms: i64) -> pantheon_memory::Proposal {
    pantheon_memory::Proposal {
        layer: pantheon_memory::LayerKind::Agent,
        namespace: namespace.to_string(),
        key: slugify_lesson_key(lesson),
        value: lesson.to_string(),
        provenance: pantheon_memory::Provenance {
            source: "tui".into(),
            origin: "user".into(),
            trust: pantheon_api::provenance::TrustTier::User,
            recorded_at_ms: now_ms,
        },
    }
}

/// Persist a `/learn` lesson through the session's memory store, gated by
/// the same policy/validation path as `/remember`. Returns the key the
/// lesson was stored under.
pub fn learn_lesson(
    session: &Session,
    lesson: &str,
) -> Result<String, pantheon_api::error::PantheonError> {
    let Some(store) = session.memory.as_ref() else {
        return Err(pantheon_api::error::PantheonError::new(
            "LEARN_NO_STORE",
            pantheon_api::error::Layer::Memory,
            false,
            "no memory store in this session",
            "memory is unavailable here; the lesson was not saved",
            "",
        ));
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let proposal = lesson_proposal(&session.memory_namespace, lesson, now_ms);
    let key = proposal.key.clone();
    pantheon_memory::write_via(store.as_ref(), &session.policy, proposal, 4096)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::auto_reflect_due;

    #[test]
    fn auto_reflect_due_fires_on_configured_multiples() {
        // Default cadence: every 20 completed turns.
        assert!(auto_reflect_due(20, true, 20, false));
        assert!(auto_reflect_due(40, true, 20, false));
        // Not a multiple, or disabled, or already running: no.
        assert!(!auto_reflect_due(19, true, 20, false));
        assert!(!auto_reflect_due(20, false, 20, false));
        assert!(!auto_reflect_due(20, true, 20, true));
        assert!(!auto_reflect_due(0, true, 20, false));
    }
}
