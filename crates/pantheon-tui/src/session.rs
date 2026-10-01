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

use chrono::{DateTime, Local};

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
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Clear, Paragraph},
    DefaultTerminal, Frame,
};

use pantheon_api::events::Event as RuntimeErrorEvent;
use pantheon_api::mode::AgentMode;
use pantheon_providers::error_kind::{
    classify_provider_error, display_message, retry_after_secs_from_cause, ProviderErrorKind,
};
use pantheon_providers::model_event::ModelEvent;
use pantheon_runtime::session::Session;
use std::path::PathBuf;

/// TUI-B feature modules. Declared here (not in lib.rs) so a sibling
/// worker editing lib.rs cannot conflict with this branch.
pub mod activity;
pub mod bg;
mod editor;
pub mod git;
pub mod statusbar;
pub mod theme;
mod timeline;
pub mod vim;

/// The slash-command registry: (command, one-line description). This is
/// the single source of truth — `/help`, the `/` command palette, and
/// completion all read from here, so a command can never be listed twice
/// or drift out of sync. Entries with `<...>` placeholders (e.g.
/// `/<skill>`) are documentation-only and are skipped by the palette.
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "this list"),
    ("/models [FILTER]", "providers first, then models — Enter switches"),
    ("/model [P M]", "show the current model, or switch to one"),
    ("/reasoning [LVL]", "reasoning effort: off|minimal|low|medium|high|xhigh|max"),
    ("/remember KEY TEXT", "remember this (agent memory, user trust)"),
    ("/goal [TEXT]", "set/show the session goal (iteration-limited)"),
    ("/goal clear", "drop the session goal"),
    ("/goal iterations N", "retune the goal's iteration budget"),
    ("/tokens [N|off]", "show/set the per-response output token cap (default: uncapped)"),
    ("/set [KEY VAL]", "show/set session budget (max_turns, max_tool_calls, max_delegate_depth, max_tokens)"),
    ("/learn LESSON", "save a behavioral lesson for future sessions"),
    ("/skills [FILTER]", "installed skills"),
    ("/<skill> [input]", "invoke an installed skill by name"),
    ("/tools [reload]", "rebuild the tool registry in place"),
    ("/settings", "data dir, model, policy, memory, server, agents"),
    ("/gateway", "service state and queued outbound"),
    ("/send <target> <message>", "push a message to a surface (telegram|discord|mobile|home)"),
    ("/doctor", "diagnose this install"),
    ("/sessions", "reopen a parked session (searchable)"),
    ("/new", "start a fresh conversation"),
    ("/rewind", "roll back the last turn (confirm; ledger kept)"),
    ("/undo", "undo the last response (same as /rewind)"),
    ("/checkpoint [NAME]", "save a named snapshot of the current turn"),
    ("/checkpoints", "list saved checkpoints"),
    ("/restore NAME", "rewind back to a checkpoint (ledger kept)"),
    ("/swarm", "remote swarms: list, or /swarm new <task> to spawn"),
    ("/swarm <id>", "per-agent statuses + judge verdict for a swarm"),
    ("/swarm retry <id>", "relaunch a swarm ([feedback])"),
    ("/swarm tree", "this session's delegation tree (read-only)"),
    ("/team [<id> [<task>]]", "team of experts: list, roster, or launch on a task"),
    ("/plugin [import|search]", "plugins: import by URL/slug, search registry (also /plugins)"),
    ("/btw PROMPT", "run a task in the background (result lands here)"),
    ("/bg [ID]", "list background tasks, or show one's output"),
    ("/fork [TURN]", "branch this conversation at a turn into a new run"),
    ("/theme [name]", "switch theme (pantheon, dark, light)"),
    ("/vim [on|off|status]", "modal vim editing for the composer (v1: Normal/Insert only)"),
    ("/nightly [on|off|status]", "run the nightly maintenance pass now; on/off/status manage the loop"),
    ("/reflect [on|off|status]", "run a reflection pass now, or toggle the self-improvement loop"),
    ("/consolidate [status|--dry-run]", "run a memory consolidation pass (dry run changes nothing)"),
    ("/todos", "inspect this session's todo list"),
    ("/compress", "compress this conversation to the window now"),
    ("/export [md|json]", "save this conversation to exports/"),
    ("/yank [N]", "copy last answer (or its Nth code block) to clipboard"),
    ("/steer <text>", "redirect the running turn mid-flight (normal message when idle)"),
    ("/runs [N]", "recent runs (default 10)"),
    ("/history", "interactive searchable history (pick + resume)"),
    ("/resume [ID]", "resume a run by id"),
    ("/title [TITLE]", "show this conversation's title, or rename it"),
    ("/project [new|set|clear|list|move]", "named project buckets for sessions"),
    ("/status [run_id]", "this run's status, or another by id"),
    ("/stats", "today's usage from the ledger (same numbers as `pantheon stats`)"),
    ("/agent [name]", "current agent profile, or switch to one"),
    ("/agent new <name>", "create a profile ([--display-name][--inherits][--policy])"),
    ("/agents", "declared agent profiles"),
    ("/agents create <name>", "create a profile (same flags as /agent new)"),
    ("/soul", "print the active profile's SOUL.md (/soul set <text> writes it)"),
    ("/userfile", "print the active profile's USER.md (/userfile set <text> writes it)"),
    ("/agentsfile", "print the active profile's AGENTS.md (/agentsfile set <text> writes it)"),
    ("/collab", "deprecated — multi-profile tasks moved to /swarm"),
    ("/tasks <agent>", "that agent's open tasks"),
    ("/inbox", "messages sent to this agent"),
    ("/approvals", "pending approvals for this run"),
    ("/schedule", "scheduled jobs"),
    ("/mcp [reload]", "MCP server declarations (reload re-scans them)"),
    ("/migrate", "import from other harnesses (hermes, openclaw, omp, claude)"),
    ("/env", "secret names and status (never values)"),
    ("/voice", "speech backends ([stt]/[tts]) status"),
    ("/clear", "clear visible transcript"),
    ("/reset", "reset turn state: clear transcript, cancel turn, drop queue (session, title, ledger kept; /clear is display-only, /new starts a new session)"),
];

/// Key hints shown at the end of `/help`. Not commands: kept separate so
/// the palette never offers them.
const KEY_HINTS: &[(&str, &str)] = &[
    ("PgUp/PgDn", "scroll the transcript"),
    ("Ctrl+O / F2", "turn timeline: arrows move, Enter jumps"),
    ("Ctrl+E", "fullscreen draft editor (Ctrl+Enter sends)"),
    ("Ctrl+B", "toggle sidebar"),
    ("Ctrl+G", "sidebar: activity / todo view"),
    ("Tab", "toggle Build / Plan mode"),
    ("/", "command palette (type to filter, Enter runs)"),
    ("Esc Esc (idle)", "offer to rewind the last turn"),
    ("/exit, /quit", "leave pantheon"),
];

/// One `/help` line per registry entry, in the two-space-separated format
/// `render_help_table` parses.
fn help_lines() -> Vec<String> {
    let mut out = Vec::with_capacity(COMMANDS.len() + KEY_HINTS.len() + 1);
    out.push("commands:".to_string());
    for (name, desc) in COMMANDS {
        out.push(format!("  {name:<22} {desc}"));
    }
    out.push("keys:".to_string());
    for (keys, desc) in KEY_HINTS {
        out.push(format!("  {keys:<22} {desc}"));
    }
    out
}

/// The `/` command palette: type to filter the registry by name or
/// description, Up/Down to move, Enter to run, Esc to dismiss. While open
/// it owns the keyboard — exactly like the `@` mention picker.
#[derive(Debug, Default)]
pub struct PaletteState {
    /// Raw filter text (without the leading `/`).
    pub input: String,
    /// Selected index into the *filtered* list.
    pub sel: usize,
}

impl PaletteState {
    /// Registry entries matching the filter, in registry order. Entries
    /// with `<...>` placeholders are documentation-only and never offered.
    /// Name matches come before description matches: typing `agents`
    /// should offer `/agents`, not `/settings` (whose description happens
    /// to mention "agents").
    pub fn filtered(&self) -> Vec<(&'static str, &'static str)> {
        let f = self.input.to_lowercase();
        let offered: Vec<(&'static str, &'static str)> = COMMANDS
            .iter()
            .copied()
            .filter(|(name, _desc)| !name.contains('<'))
            .collect();
        if f.is_empty() {
            return offered;
        }
        let mut names: Vec<(&'static str, &'static str)> = Vec::new();
        let mut descs: Vec<(&'static str, &'static str)> = Vec::new();
        for entry @ (name, desc) in offered {
            if name.to_lowercase().contains(&f) {
                names.push(entry);
            } else if desc.to_lowercase().contains(&f) {
                descs.push(entry);
            }
        }
        names.extend(descs);
        names
    }

    pub fn move_sel(&mut self, n: isize) {
        let len = self.filtered().len();
        if len == 0 {
            self.sel = 0;
            return;
        }
        let sel = self.sel as isize + n;
        self.sel = sel.clamp(0, len as isize - 1) as usize;
    }
}

/// A single block in the conversation transcript.
#[derive(Debug, Clone)]
pub enum BlockKind {
    UserMessage(String),
    AssistantMessage(String),
    Thinking(String),
    /// Tool card: running until the matching runtime completion event
    /// sets `ok`. `name` is the registry id (`shell`) — the UI shows
    /// the presentable display name. Args render inline;
    /// `started`/`duration` give the timing, `error` carries the
    /// failure detail, and `tokens` freezes the session token count at
    /// completion (the header shows the live count while running).
    ToolCall {
        name: String,
        args: String,
        ok: Option<bool>,
        call_id: String,
        started: Option<std::time::Instant>,
        duration: Option<std::time::Duration>,
        error: Option<String>,
        tokens: Option<u32>,
    },
    Swarm {
        agents: u32,
        task: String,
    },
    Status(String),
    /// Live todo checklist card, rendered inline in the transcript with
    /// the opencode-style card (`todo_card_lines`). Present only while
    /// the sidebar is hidden (narrow terminal or toggled off): in wide
    /// mode the sidebar's todo view owns this. One live block, updated
    /// in place as the plan progresses — never duplicated, removed when
    /// the todo list empties or the sidebar comes back.
    Todos(Vec<crate::todo_card::TodoCardItem>),
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
    /// Approval decision card. The tool intent renders calmly above it;
    /// the card itself is the amber decision point. `decision` collapses
    /// the card to a dim one-line marker once resolved.
    Approval {
        tool: String,
        sentence: String,
        target: String,
        decision: Option<bool>,
    },
    /// Clarify question card (`ask_user`). Cyan `?`, never amber.
    /// `answered` collapses the card once the operator replies.
    Clarify {
        question: String,
        options: Vec<String>,
        answered: Option<String>,
    },
    /// Rendered output of one slash command: the echo plus a titled
    /// result block. `/help` renders as a two-column table.
    Command {
        cmd: String,
        lines: Vec<String>,
        failed: bool,
    },
    /// Destructive slash command awaiting confirmation (`/clear`).
    /// Amber decision card like Approval; `decided` collapses it.
    Confirm {
        label: String,
        sentence: String,
        decided: Option<bool>,
    },
    /// Provider failure card: which provider/model failed, the classified
    /// error kind + message, and what the chain did next. Rate-limit
    /// failures render in the warning tone so quota trouble is visibly
    /// distinct from a dead provider.
    ProviderError {
        provider: String,
        model: String,
        kind: ProviderErrorKind,
        message: String,
        fallback: FallbackOutcome,
    },
}

/// What the fallback chain did after a provider failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackOutcome {
    /// A fallback was available and engaged: this provider/model is now
    /// the active one.
    Engaged { provider: String, model: String },
    /// Retryable failure, but the chain had nothing left to try.
    Exhausted,
    /// The error is not retryable, so no fallback was attempted.
    NotRetryable,
    /// The chain is retrying the same provider/model: retry `attempt` of
    /// `max_attempts`, waiting `wait_secs` before the next try. The card
    /// stays live — each `RetryAttempt` updates this line in place so
    /// the transcript shows "retrying 2/3", never a frozen screen.
    Retrying {
        attempt: u32,
        max_attempts: u32,
        wait_secs: u64,
    },
    /// A retry recovered the turn: retry `attempt` of `max_attempts`
    /// succeeded. The card keeps the honest trail instead of going
    /// silent or freezing on "retrying n/3".
    Recovered { attempt: u32, max_attempts: u32 },
}

/// A retryable provider failure waiting on the chain's verdict. The next
/// chain event resolves it: `RetryAttempt` (a retry is being waited out),
/// `Fallback` (a fallback engaged), `Exhausted` (nothing left to try),
/// or `Attempt` on the same model (a stacked key rotated, not a chain
/// move). Non-retryable failures render immediately too — a TUI that
/// attached mid-chain may never see their verdict, and a failure must
/// never be silent.
struct PendingProviderFailure {
    provider: String,
    model: String,
    kind: ProviderErrorKind,
    message: String,
    code: String,
    /// Block index of the live error card, if one is on screen. Verdicts
    /// update the card in place instead of pushing a second one; the
    /// index is verified before each update (rewind can truncate).
    card_index: Option<usize>,
    /// Set by `RetryAttempt`: the next `Attempt` on this model is a
    /// retry, not a stacked-key rotation (no "trying next key" line).
    retrying: bool,
    /// Last `(attempt, max_attempts)` seen, for the Recovered line.
    last_retry: Option<(u32, u32)>,
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

/// One provider in the `/models` provider picker: the label shown, the
/// id used to drill into its models, and how many curated models it
/// carries (0 = provider with no catalog entries yet).
#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub provider_id: String,
    pub provider_label: String,
    pub model_count: usize,
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

/// An `ask_user` question parked on a run: what the clarify card shows
/// and what the answer keys resolve.
#[derive(Debug, Clone)]
pub struct ClarifyRequest {
    pub call_id: String,
    pub question: String,
    pub options: Vec<String>,
}

/// A parked approval: (scope, tool name, tool args). The scope is
/// `call_id:tool:args`; the name/args are parsed out of it so a
/// re-entered session can render the card without the live `last_tool`.
pub type ApprovalInfo = (String, String, String);

/// A destructive slash command awaiting confirmation (`/clear`).
/// Reuses the approval decision-card pattern: amber card, `[a]`/`[d]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    ClearTranscript,
}

/// Which card the sidebar's bottom half shows. Auto-switches by
/// context (Plan mode / fresh todo updates open the todo view, Build
/// mode the agent view); manual toggle (`^g`) flips it on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SidebarView {
    #[default]
    Agent,
    Todo,
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
    /// A retryable provider failure whose chain verdict has not arrived
    /// yet. Resolved by the next Fallback / Exhausted / Attempt event;
    /// never rendered on its own, so the card can say whether a
    /// fallback engaged.
    pending_provider_failure: Option<PendingProviderFailure>,
    /// Set when a nightly pass finishes with proposals awaiting
    /// approval. Drives the reflection card; y approves all, n denies all.
    pub pending_reflect: Option<Vec<pantheon_nightly::Proposal>>,
    /// A nightly pass is running on its worker thread. Guards against
    /// overlapping passes clobbering the pending-proposals file.
    pub reflect_running: bool,
    /// Last tool that started (name, args) — shown in the permission card
    /// because ApprovalRequested only carries the opaque call id.
    pub last_tool: Option<(String, String)>,
    /// Double-Esc state: None = not interrupting, Some(t) = armed at t.
    /// The second Esc inside the window actually cancels the run.
    pub interrupt_armed_at: Option<Instant>,
    /// True once the user confirmed; shows the canceled card.
    pub interrupted: bool,
    /// Open session-history overlay: Some(runs) while /history is open.
    /// runs: (run_id, status, created_ms, title, project), newest first.
    pub history: Option<Vec<pantheon_storage::RunListing>>,
    /// Cached ledger runs for the session picker: open
    /// tabs first, parked (closed but resumable) runs after. Refreshed
    /// with the tab bar so the sidebar never queries the DB per frame.
    /// (run_id, status, created_ms, title, project), newest first.
    pub parked_runs: Vec<pantheon_storage::RunListing>,
    /// Live filter typed into the history overlay.
    pub history_input: String,
    /// Selected index into the filtered history list.
    pub history_sel: usize,
    /// The project of the session that opened the history picker
    /// (`None` = unassigned). The picker defaults to showing only this
    /// project; ctrl+a toggles the all-projects view.
    pub history_own_project: Option<String>,
    /// History picker: true = list every project, false = only
    /// `history_own_project`. Toggled with ctrl+a.
    pub history_all_projects: bool,
    /// Inline rename in the history picker: Some((run_id, buffer))
    /// while ctrl+r is editing a row's title. Enter commits, Esc cancels.
    pub history_rename: Option<(String, String)>,
    /// Open model-browser overlay: Some(rows) while /models is open.
    pub models: Option<Vec<ModelRow>>,
    /// Live filter typed into the models overlay.
    pub models_input: String,
    /// Selected index into the filtered models list.
    pub models_sel: usize,
    /// Open provider picker: Some(rows) while the /models provider list
    /// is open. Picking a provider drills into `models` scoped to it;
    /// Esc in models returns here, Esc here closes both.
    pub providers: Option<Vec<ProviderRow>>,
    /// Live filter typed into the provider picker.
    pub providers_input: String,
    /// Selected index into the filtered provider list.
    pub providers_sel: usize,
    /// Open `@` file-mention picker: while set, keystrokes filter and
    /// navigate the file list instead of reaching the composer.
    pub mention: Option<crate::mentions::MentionPicker>,
    /// Open `/` command palette: type to filter the command list,
    /// Up/Down to move, Enter to run, Esc to dismiss. While open it owns
    /// the keyboard like the mention picker.
    pub palette: Option<PaletteState>,
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
    /// Per-tab unsent input drafts, keyed by run id. Stashed on every
    /// tab switch and restored on return — switching tabs never eats a
    /// draft.
    pub drafts: std::collections::HashMap<String, String>,
    /// Explicit open-tab list (true open-tab model): run ids in display
    /// order. Opening a tab = opening a session; the ledger is consulted
    /// for titles only, never for membership. Closing a tab parks the
    /// run — it stays reopenable via /sessions or the session picker.
    pub open_tabs: Vec<String>,
    /// Sidebar visibility (`^b`): at ≥100 cols it toggles the 75/25
    /// (80/20) split sidebar; below 100 cols the sidebar is hidden by
    /// default and `^b` opens it as a floating overlay instead. One
    /// unified session view either way.
    pub show_sidebar: bool,
    /// Last observed terminal width in columns, refreshed every frame
    /// before draw. Layout decisions that depend on the real terminal
    /// size (sidebar split vs. single column, inline todo card) read
    /// this instead of a stale or partial area.
    pub term_width: u16,
    /// Narrow-terminal sidebar overlay (`^b` below 100 cols). Always
    /// false unless explicitly toggled on — the single-column view
    /// never opens it by itself.
    pub sidebar_overlay: bool,
    /// Which card the sidebar's bottom half shows. Manual toggle
    /// (`^g`): the operator picks whichever serves the moment.
    pub sidebar_view: SidebarView,
    /// The dynamic footer status string, driving the footer's traveling
    /// highlight. Built from real activity: "Thinking...",
    /// "Reading <path>", "Executing <cmd>", ... The sweep travels
    /// through this string's characters.
    pub activity_status: String,
    /// The traveling highlight sweep for the footer status line.
    /// Advanced in [`tick`] while a turn is live, parked otherwise.
    /// Always renders [`activity_status`] — see [`set_activity_status`].
    pub sweep: activity::TravelHighlight,
    /// Plan mode (Tab toggle). The sidebar mode tag flips between
    /// "Build" and "Plan"; while a turn is live the footer status is
    /// "Planning...". Mirrored into the runtime session via
    /// [`apply_plan_mode_to_session`], which is what actually gates
    /// write tools in the agent loop.
    pub plan_mode: bool,
    /// Every run currently parked on an approval, keyed by run id.
    /// The decision card renders for the active session; other runs get
    /// the amber tab dot.
    pub approvals: std::collections::HashMap<String, ApprovalInfo>,
    /// Every run currently parked on a clarify question, keyed by run id.
    pub clarifies: std::collections::HashMap<String, ClarifyRequest>,
    /// The clarify question the keyboard is answering (the active
    /// session's). Digits pick an option, Enter sends free text.
    pub pending_clarify: Option<ClarifyRequest>,
    /// While `Some`, `add_status` lines are captured into the buffer
    /// instead of the transcript; `handle_slash` flushes them as one
    /// `Command` block when the command finishes.
    pub cmd_capture: Option<(String, Vec<String>)>,
    /// Destructive slash command awaiting confirmation. Renders the
    /// amber decision card; `a` runs it, `d`/Esc dismisses.
    pub pending_confirm: Option<ConfirmAction>,
    /// Tool calls completed this session. Feeds the status bar and the
    /// session sidebar's activity section.
    pub tools_used: u32,
    /// Frame counter for the working spinner. Bumped by `tick()`.
    pub spinner_tick: u64,
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
    /// Subagent activity records fed by `AgentSpawned` / `AgentMessage` /
    /// `AgentCompleted` ledger events. The sidebar activity timeline
    /// renders the ones whose parent run is the visible session, so
    /// background tabs keep their own.
    pub subagents: Vec<activity::SubAgentRecord>,
    /// Inline image state: thumbnail placements recorded during render,
    /// terminal graphics capability, and the fullscreen preview.
    pub img: crate::richtext::ImagePaintState,
    /// Open to-dos, owned by the parallel todo track: this field is the
    /// only todo-related change here. The other track fills it in and
    /// renders the todo UI; the status bar reads the count.
    pub todo_count: usize,
    /// Live todo items for the transcript card, synced from the runtime
    /// session each frame by `refresh_todos`. Rendered opencode-style at
    /// the transcript tail while non-empty.
    pub todo_items: Vec<crate::todo_card::TodoCardItem>,
    /// Running shells for the status bar's `{n} shells` segment. A plain
    /// counter, default 0 — never derived from background tasks, which
    /// are a different thing.
    pub shell_count: usize,
    /// Agent profile display name for the status line's
    /// agent variant segment ("—" when unset).
    pub variant: String,
    /// Reasoning effort level (`/reasoning`), e.g. "xhigh". `None` =
    /// unknown; the status line renders `—` for it, never a guess.
    pub effort: Option<String>,
    /// Cached MCP servers for the sidebar: (name, status word).
    /// Refreshed at startup and by `/mcp`.
    pub mcp_servers: Vec<(String, String)>,
    /// Loaded capability plugins, for the splash's bottom line.
    pub plugin_count: usize,
    /// Newer release tag found by the background update check; the
    /// splash shows `pantheon update to install {tag}` when set.
    pub update_hint: Option<String>,
    /// Cached health alerts for the bottom strip (failing MCP servers,
    /// gateway trouble, overdue scheduled jobs). Refreshed at most
    /// every 30s so the probes never run per frame.
    pub health_alerts: Vec<String>,
    pub health_checked_at: Option<std::time::Instant>,
    /// Swarm ids spawned through the remote swarm backend this session
    /// (`/swarm new`). The API contract has no list-all endpoint yet, so
    /// the TUI remembers what it created to answer bare `/swarm`.
    pub remote_swarms: Vec<String>,
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
            pending_provider_failure: None,
            pending_reflect: None,
            reflect_running: false,
            last_tool: None,
            interrupt_armed_at: None,
            interrupted: false,
            history: None,
            history_input: String::new(),
            history_sel: 0,
            history_own_project: None,
            history_all_projects: false,
            history_rename: None,
            parked_runs: Vec::new(),
            models: None,
            models_input: String::new(),
            models_sel: 0,
            providers: None,
            providers_input: String::new(),
            providers_sel: 0,
            mention: None,
            palette: None,
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
            drafts: std::collections::HashMap::new(),
            open_tabs: Vec::new(),
            show_sidebar: true,
            term_width: 80,
            sidebar_overlay: false,
            sidebar_view: SidebarView::Agent,
            activity_status: String::new(),
            sweep: activity::TravelHighlight::new("Thinking..."),
            plan_mode: false,
            approvals: std::collections::HashMap::new(),
            clarifies: std::collections::HashMap::new(),
            pending_clarify: None,
            cmd_capture: None,
            pending_confirm: None,
            tools_used: 0,
            spinner_tick: 0,
            theme: theme::Theme::pantheon(),
            show_shortcuts: false,
            vim: vim::VimState::new(),
            git_label: None,
            git_checked_at: Instant::now(),
            bg_tasks: Vec::new(),
            bg_seq: 0,
            subagents: Vec::new(),
            img: crate::richtext::ImagePaintState::new(),
            todo_count: 0,
            todo_items: Vec::new(),
            shell_count: 0,
            variant: String::new(),
            effort: None,
            mcp_servers: Vec::new(),
            plugin_count: 0,
            update_hint: None,
            health_alerts: Vec::new(),
            health_checked_at: None,
            remote_swarms: Vec::new(),
        }
    }
}

/// Split an approval scope of the form `call_id:tool:args` into its
/// parts. Returns `None` when the scope doesn't carry all three.
fn parse_scope(scope: &str) -> Option<(&str, &str, &str)> {
    let mut parts = scope.splitn(3, ':');
    let call_id = parts.next()?;
    let tool = parts.next()?;
    let args = parts.next()?;
    Some((call_id, tool, args))
}

/// Plain-language rendering of a pending tool call for the approval
/// card. Returns `(action sentence, concrete target)`:
/// "shell wants to delete:" / "~/notes/old.md". The tool details stay
/// visually calm — the amber card, not the text, carries the urgency.
fn describe_tool_action(tool: &str, args: &str) -> (String, String) {
    // Best-effort JSON field extraction without pulling serde in here;
    // args is a JSON-encoded string.
    fn field(args: &str, key: &str) -> Option<String> {
        let needle = format!("\"{key}\"");
        let start = args.find(&needle)?;
        let rest = &args[start + needle.len()..];
        let rest = rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace());
        let rest = rest.strip_prefix('"')?;
        let end = rest.find('"')?;
        Some(rest[..end].to_string())
    }
    match tool {
        "shell" => {
            let cmd = field(args, "command").unwrap_or_else(|| args.to_string());
            let short: String = cmd.chars().take(72).collect();
            let verb = cmd.trim_start();
            if verb.starts_with("rm ") {
                (
                    "shell wants to delete:".to_string(),
                    verb.trim_start_matches("rm ").trim().to_string(),
                )
            } else if verb.starts_with("mkdir") {
                (
                    "shell wants to create a directory:".to_string(),
                    short
                        .trim_start_matches("mkdir")
                        .trim_start_matches(" -p")
                        .trim()
                        .to_string(),
                )
            } else {
                ("shell wants to run:".to_string(), short)
            }
        }
        "write_file" => (
            "write_file wants to write:".to_string(),
            field(args, "path").unwrap_or_else(|| args.to_string()),
        ),
        "read_file" => (
            "read_file wants to read:".to_string(),
            field(args, "path").unwrap_or_else(|| args.to_string()),
        ),
        _ => {
            let detail: String = args.chars().take(96).collect();
            (format!("{tool} wants to run:"), detail)
        }
    }
}

impl TuiState {
    /// Create a fresh session view-state. Public so embedders (and the
    /// screenshot harness) can drive the renderer directly.
    pub fn new(session_id: String, model: String, tokens_max: u32) -> Self {
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
            pending_provider_failure: None,
            pending_reflect: None,
            reflect_running: false,
            last_tool: None,
            interrupt_armed_at: None,
            interrupted: false,
            history: None,
            history_input: String::new(),
            history_sel: 0,
            history_own_project: None,
            history_all_projects: false,
            history_rename: None,
            parked_runs: Vec::new(),
            models: None,
            models_input: String::new(),
            models_sel: 0,
            providers: None,
            providers_input: String::new(),
            providers_sel: 0,
            mention: None,
            palette: None,
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
            drafts: std::collections::HashMap::new(),
            open_tabs: Vec::new(),
            show_sidebar: true,
            term_width: 80,
            sidebar_overlay: false,
            sidebar_view: SidebarView::Agent,
            activity_status: String::new(),
            sweep: activity::TravelHighlight::new("Thinking..."),
            plan_mode: false,
            approvals: std::collections::HashMap::new(),
            clarifies: std::collections::HashMap::new(),
            pending_clarify: None,
            cmd_capture: None,
            pending_confirm: None,
            tools_used: 0,
            spinner_tick: 0,
            theme: theme::Theme::pantheon(),
            show_shortcuts: false,
            vim: vim::VimState::new(),
            git_label: None,
            git_checked_at: Instant::now(),
            bg_tasks: Vec::new(),
            bg_seq: 0,
            subagents: Vec::new(),
            img: crate::richtext::ImagePaintState::new(),
            todo_count: 0,
            todo_items: Vec::new(),
            shell_count: 0,
            variant: String::new(),
            effort: None,
            mcp_servers: Vec::new(),
            plugin_count: 0,
            update_hint: None,
            health_alerts: Vec::new(),
            health_checked_at: None,
            remote_swarms: Vec::new(),
        }
    }

    /// Runs matching the current filter, in list order. Unless the
    /// all-projects view is on (ctrl+a), only the opener's project is
    /// shown — `None` (unassigned) matches `None`, so a project-less
    /// setup behaves exactly like the old unfiltered list.
    pub fn filtered_history(&self) -> Vec<pantheon_storage::RunListing> {
        match &self.history {
            None => Vec::new(),
            Some(runs) => {
                let f = self.history_input.to_lowercase();
                runs.iter()
                    .filter(|(_, _, _, _, project)| {
                        self.history_all_projects || project == &self.history_own_project
                    })
                    .filter(|(id, status, _, title, project)| {
                        f.is_empty()
                            || id.to_lowercase().contains(&f)
                            || status.to_lowercase().contains(&f)
                            || title
                                .as_deref()
                                .map(|t| t.to_lowercase().contains(&f))
                                .unwrap_or(false)
                            || project
                                .as_deref()
                                .map(|p| p.to_lowercase().contains(&f))
                                .unwrap_or(false)
                    })
                    .cloned()
                    .collect()
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
        self.history_own_project = None;
        self.history_all_projects = false;
        self.history_rename = None;
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

    /// Providers matching the current filter, alphabetical by label.
    pub fn filtered_providers(&self) -> Vec<ProviderRow> {
        match &self.providers {
            None => Vec::new(),
            Some(rows) => {
                let f = self.providers_input.to_lowercase();
                if f.is_empty() {
                    rows.clone()
                } else {
                    rows.iter()
                        .filter(|r| {
                            r.provider_id.to_lowercase().contains(&f)
                                || r.provider_label.to_lowercase().contains(&f)
                        })
                        .cloned()
                        .collect()
                }
            }
        }
    }

    /// Move the provider-picker selection by n, clamped to the filtered list.
    pub fn providers_move(&mut self, n: isize) {
        let len = self.filtered_providers().len();
        if len == 0 {
            self.providers_sel = 0;
            return;
        }
        let sel = self.providers_sel as isize + n;
        self.providers_sel = sel.clamp(0, len as isize - 1) as usize;
    }

    /// Close the provider picker and any drilled-in models view.
    pub fn providers_close(&mut self) {
        self.providers = None;
        self.providers_input.clear();
        self.providers_sel = 0;
        self.models_close();
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
                // A retry's Attempt arrives right after its RetryAttempt
                // (`retrying` set): same model, but a retry, not a stacked
                // key rotating. Only a pending failure with no RetryAttempt
                // in between is a rotation. A dim status line, not an
                // error card; the stash stays for the chain's verdict.
                let is_retry = self
                    .pending_provider_failure
                    .as_ref()
                    .is_some_and(|p| p.retrying);
                if let Some(p) = self.pending_provider_failure.as_mut() {
                    p.retrying = false;
                }
                if !is_retry {
                    if let Some(p) = self.pending_provider_failure.as_ref() {
                        let key_no: String = p
                            .code
                            .rsplit(":key")
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect();
                        let key_bit = if key_no.is_empty() {
                            "a stacked key".to_string()
                        } else {
                            format!("stacked key {key_no}")
                        };
                        self.blocks.push(TranscriptBlock {
                            kind: BlockKind::Status(format!(
                                "{}/{}: {} on {key_bit}, trying next key",
                                p.provider, p.model, p.kind
                            )),
                        });
                    }
                }
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
                id,
                name,
                arguments,
                ..
            } => {
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::ToolCall {
                        name,
                        args: arguments,
                        ok: None,
                        call_id: id,
                        started: Some(std::time::Instant::now()),
                        duration: None,
                        error: None,
                        tokens: None,
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
                // A retry that recovered: flip the live card to
                // "recovered" instead of leaving a stale "retrying n/3".
                self.flip_recovered_card();
                self.pending_provider_failure = None;
            }
            ModelEvent::Completed { .. } => {
                self.flip_recovered_card();
                self.pending_provider_failure = None;
            }
            ModelEvent::AttemptFailed {
                provider,
                model,
                code,
                cause,
                retryable,
                ..
            } => {
                let kind = classify_provider_error(&code, &cause);
                let message = display_message(&cause);
                // Carry the live card across the failure: stacked-key
                // rotation re-stashes per key, and the retry card pushed
                // by RetryAttempt must not be orphaned by the next
                // AttemptFailed — refresh kind/message, keep the index.
                let (card_index, last_retry) = match &self.pending_provider_failure {
                    Some(p) if p.provider == provider && p.model == model => {
                        (p.card_index, p.last_retry)
                    }
                    _ => (None, None),
                };
                let mut card_index = card_index;
                if let Some(i) = card_index {
                    self.refresh_live_card(i, &provider, &model, kind, &message);
                } else if !retryable {
                    // Non-retryable failures render immediately: a TUI
                    // that attached mid-chain may never see the verdict,
                    // and a failure must never be silent. A later
                    // Fallback/Exhausted updates this card in place.
                    card_index = Some(self.push_provider_error_card(
                        provider.clone(),
                        model.clone(),
                        kind,
                        message.clone(),
                        FallbackOutcome::NotRetryable,
                    ));
                }
                // The chain's verdict is still coming (RetryAttempt,
                // Fallback, Exhausted, or a stacked-key Attempt). Hold
                // the failure so the card can say what happened next.
                self.pending_provider_failure = Some(PendingProviderFailure {
                    provider,
                    model,
                    kind,
                    message,
                    code,
                    card_index,
                    retrying: false,
                    last_retry,
                });
            }
            ModelEvent::RetryAttempt {
                provider,
                model,
                attempt,
                max_attempts,
                wait_secs,
                ..
            } => {
                // The chain is waiting `wait_secs` before retry
                // `attempt`/`max_attempts` on the same provider/model.
                // The first retry pushes the live error card; later
                // retries update its line in place — "retrying 2/3",
                // never a frozen screen.
                if self.pending_provider_failure.is_none() {
                    // Defensive: the TUI attached mid-chain, after the
                    // AttemptFailed. The retry is all we know.
                    self.pending_provider_failure = Some(PendingProviderFailure {
                        provider: provider.clone(),
                        model: model.clone(),
                        kind: ProviderErrorKind::Unknown,
                        message: String::new(),
                        code: String::new(),
                        card_index: None,
                        retrying: false,
                        last_retry: None,
                    });
                }
                if let Some(p) = self.pending_provider_failure.as_mut() {
                    p.retrying = true;
                    p.last_retry = Some((attempt, max_attempts));
                }
                self.render_live_card(FallbackOutcome::Retrying {
                    attempt,
                    max_attempts,
                    wait_secs,
                });
            }
            ModelEvent::Fallback {
                from_provider,
                from_model,
                from_code,
                from_cause,
                to_provider,
                to_model,
                ..
            } => {
                // Finalize the live card in place — or push it when the
                // TUI attached mid-chain, after the AttemptFailed.
                if self.pending_provider_failure.is_none() {
                    self.pending_provider_failure = Some(PendingProviderFailure {
                        provider: from_provider.clone(),
                        model: from_model.clone(),
                        kind: classify_provider_error(&from_code, &from_cause),
                        message: display_message(&from_cause),
                        code: from_code.clone(),
                        card_index: None,
                        retrying: false,
                        last_retry: None,
                    });
                }
                // The fallback model is active now; the Attempt that
                // follows must not print the old "routing:" line on top
                // of this card.
                self.model = to_model.clone();
                self.render_live_card(FallbackOutcome::Engaged {
                    provider: to_provider,
                    model: to_model,
                });
                self.pending_provider_failure = None;
            }
            ModelEvent::Exhausted { code } => {
                if self.pending_provider_failure.is_none() {
                    // Defensive: the TUI attached after the failure. The
                    // chain's code is all we have; never invent the model.
                    self.pending_provider_failure = Some(PendingProviderFailure {
                        provider: String::new(),
                        model: String::new(),
                        kind: classify_provider_error(&code, ""),
                        message: code.clone(),
                        code: code.clone(),
                        card_index: None,
                        retrying: false,
                        last_retry: None,
                    });
                }
                self.render_live_card(FallbackOutcome::Exhausted);
                self.pending_provider_failure = None;
            }
        }
    }

    /// Append a provider error card: which provider/model failed, the
    /// error kind + message, and what the chain did next. Returns the
    /// block index so verdicts can update the card in place.
    fn push_provider_error_card(
        &mut self,
        provider: String,
        model: String,
        kind: ProviderErrorKind,
        message: String,
        fallback: FallbackOutcome,
    ) -> usize {
        self.blocks.push(TranscriptBlock {
            kind: BlockKind::ProviderError {
                provider,
                model,
                kind,
                message,
                fallback,
            },
        });
        self.blocks.len() - 1
    }

    /// Refresh the kind/message lines of the live provider-error card at
    /// `index` (a newer AttemptFailed for the same failure); the outcome
    /// line is left alone — the verdict owns it.
    fn refresh_live_card(
        &mut self,
        index: usize,
        provider: &str,
        model: &str,
        kind: ProviderErrorKind,
        message: &str,
    ) {
        if let Some(TranscriptBlock {
            kind:
                BlockKind::ProviderError {
                    provider: p,
                    model: m,
                    kind: k,
                    message: msg,
                    ..
                },
            ..
        }) = self.blocks.get_mut(index)
        {
            if p == provider && m == model {
                *k = kind;
                *msg = message.to_string();
            }
        }
    }

    /// Push the provider-error card for the stashed failure if it isn't
    /// on screen yet, then set its outcome line. The stashed index is
    /// verified before every update: rewind can truncate the blocks, in
    /// which case the card is re-pushed rather than written elsewhere.
    fn render_live_card(&mut self, fallback: FallbackOutcome) {
        let (provider, model, kind, message) = match &self.pending_provider_failure {
            Some(p) => (
                p.provider.clone(),
                p.model.clone(),
                p.kind,
                p.message.clone(),
            ),
            None => return,
        };
        let live = self
            .pending_provider_failure
            .as_ref()
            .and_then(|p| p.card_index)
            .filter(|&i| {
                matches!(
                    self.blocks.get(i),
                    Some(TranscriptBlock {
                        kind: BlockKind::ProviderError {
                            provider: p,
                            model: m,
                            ..
                        },
                        ..
                    }) if p == &provider && m == &model
                )
            });
        let idx = match live {
            Some(i) => i,
            None => {
                let i =
                    self.push_provider_error_card(provider, model, kind, message, fallback.clone());
                if let Some(p) = self.pending_provider_failure.as_mut() {
                    p.card_index = Some(i);
                }
                return;
            }
        };
        if let Some(TranscriptBlock {
            kind: BlockKind::ProviderError { fallback: fb, .. },
            ..
        }) = self.blocks.get_mut(idx)
        {
            *fb = fallback;
        }
    }

    /// A retry that recovered: flip the live card's outcome to
    /// "recovered on retry n/3" instead of leaving a stale "retrying"
    /// line. No-op unless a retry card is on screen.
    fn flip_recovered_card(&mut self) {
        let last_retry = self
            .pending_provider_failure
            .as_ref()
            .filter(|p| p.card_index.is_some())
            .and_then(|p| p.last_retry);
        if let Some((attempt, max_attempts)) = last_retry {
            self.render_live_card(FallbackOutcome::Recovered {
                attempt,
                max_attempts,
            });
        }
    }
}

/// The run an event belongs to. Every ledger event carries one; `None`
/// is defensive only (no such variant exists today).
fn event_run_id(ev: &RuntimeErrorEvent) -> Option<&str> {
    use RuntimeErrorEvent as E;
    match ev {
        E::RunStarted { run_id }
        | E::RunProgress { run_id, .. }
        | E::RunCompleted { run_id }
        | E::RunFailed { run_id, .. }
        | E::RunCanceled { run_id, .. }
        | E::RunRecovered { run_id }
        | E::AgentBound { run_id, .. }
        | E::TurnStarted { run_id, .. }
        | E::UserMessage { run_id, .. }
        | E::TurnParked { run_id, .. }
        | E::TurnCompleted { run_id, .. }
        | E::TurnFailed { run_id, .. }
        | E::TurnRewound { run_id, .. }
        | E::CheckpointCreated { run_id, .. }
        | E::SteeringProvided { run_id, .. }
        | E::ModelRequested { run_id, .. }
        | E::ModelDelta { run_id, .. }
        | E::ModelCompleted { run_id }
        | E::ToolRequested { run_id, .. }
        | E::ToolStarted { run_id, .. }
        | E::ToolOutput { run_id, .. }
        | E::ToolCompleted { run_id, .. }
        | E::AssistantMessage { run_id, .. }
        | E::ToolMessage { run_id, .. }
        | E::ImportedReasoning { run_id, .. }
        | E::AgentSpawned { run_id, .. }
        | E::AgentMessage { run_id, .. }
        | E::AgentCompleted { run_id, .. }
        | E::MemoryProposed { run_id, .. }
        | E::ApprovalRequested { run_id, .. }
        | E::ApprovalGranted { run_id, .. }
        | E::ApprovalDenied { run_id, .. }
        | E::UserInputRequested { run_id, .. }
        | E::UserInputProvided { run_id, .. }
        | E::DecisionRequested { run_id, .. }
        | E::DecisionMade { run_id, .. }
        | E::DecisionRecorded { run_id, .. }
        | E::ContextTrimmed { run_id, .. }
        | E::ContextCompressed { run_id, .. }
        | E::SessionTitled { run_id, .. }
        | E::TodosUpdated { run_id, .. }
        | E::BrowserActivity { run_id, .. }
        | E::ScheduledTaskFailed { run_id, .. }
        | E::ScheduledTaskRecovered { run_id, .. }
        | E::UsageRecorded { run_id, .. } => Some(run_id),
    }
}

impl TuiState {
    /// Toggle the session sidebar (`^b`). At ≥100 cols this flips the
    /// split sidebar; below 100 cols it flips the floating overlay,
    /// which is never open by default.
    pub fn toggle_sidebar(&mut self) {
        self.show_sidebar = !self.show_sidebar;
        self.sidebar_overlay = self.show_sidebar;
    }

    /// Whether the sidebar is actually drawn this frame: ≥100 cols and
    /// not toggled off. Mirrors the split decision in `render_body` —
    /// every layout branch that depends on it must use this, never a
    /// partial area width.
    pub fn sidebar_visible(&self) -> bool {
        self.term_width >= 100 && self.show_sidebar
    }

    /// Flip the sidebar's bottom card: activity timeline | todo list.
    /// `^g`. Manual by design — the operator picks what the moment needs.
    pub fn toggle_sidebar_view(&mut self) {
        self.sidebar_view = match self.sidebar_view {
            SidebarView::Agent => SidebarView::Todo,
            SidebarView::Todo => SidebarView::Agent,
        };
    }

    /// Flip Build/Plan mode (Tab). The sidebar mode tag flips between
    /// "Build" and "Plan", and a live turn's status becomes
    /// "Planning...". The sidebar auto-switches: Plan mode opens the
    /// todo view, Build mode the agent view. Callers must also mirror
    /// the flip into the runtime session
    /// ([`apply_plan_mode_to_session`]) — that is what actually gates
    /// write tools in the agent loop.
    pub fn toggle_plan_mode(&mut self) {
        self.plan_mode = !self.plan_mode;
        self.status_line = if self.plan_mode {
            "plan mode on"
        } else {
            "build mode on"
        }
        .to_string();
        // Sidebar auto-switch by mode; ^g still toggles manually.
        self.sidebar_view = if self.plan_mode {
            SidebarView::Todo
        } else {
            SidebarView::Agent
        };
        if !self.ready {
            self.set_activity_status(if self.plan_mode {
                "Planning...".to_string()
            } else {
                "Thinking...".to_string()
            });
        }
    }

    /// Mirror the TUI's Build/Plan toggle into the runtime session so Tab
    /// actually gates tools, not just labels. The runtime reads the mode
    /// at every tool gate; a flip mid-turn applies from the next tool
    /// call and never kills a running one.
    fn apply_plan_mode_to_session(&self, session: &Session) {
        session.set_mode(if self.plan_mode {
            AgentMode::Plan
        } else {
            AgentMode::Build
        });
    }

    /// Set the dynamic footer status string and keep the sweep on the
    /// same string. The sweep restarts from the left edge only when the
    /// string actually changed — repeated tool starts must not stutter
    /// the animation.
    pub fn set_activity_status(&mut self, status: String) {
        if status != self.activity_status {
            self.activity_status = status.clone();
            self.sweep.set_word(&status);
        }
    }

    /// Park an approval decision for `run_id`: parse the tool identity
    /// out of the scope, record it, and raise the amber tab dot. Shared
    /// by the visible path (which also renders the card) and background
    /// runs (dot only). Returns the card's (tool, sentence, target).
    fn park_approval(&mut self, run_id: &str, scope: &str) -> (String, String, String) {
        // Parse tool identity out of the scope so the card renders even
        // when the live `last_tool` belongs to another run.
        let (tool, args) = parse_scope(scope)
            .map(|(_, t, a)| (t.to_string(), a.to_string()))
            .unwrap_or_else(|| {
                self.last_tool
                    .clone()
                    .unwrap_or_else(|| ("tool".to_string(), String::new()))
            });
        let (sentence, target) = describe_tool_action(&tool, &args);
        self.approvals.insert(
            run_id.to_string(),
            (scope.to_string(), tool.clone(), args.clone()),
        );
        self.tabs.set_approval(run_id, true);
        (tool, sentence, target)
    }

    /// Park a clarify (`ask_user`) question for `run_id`. Same pattern as
    /// [`TuiState::park_approval`]: the map holds the truth, the tab dot
    /// is the background signal.
    fn park_clarify(&mut self, run_id: &str, req: ClarifyRequest) {
        self.clarifies.insert(run_id.to_string(), req);
        self.tabs.set_approval(run_id, true);
    }

    /// Background-run events: the run is not the visible session, so the
    /// visible transcript, input, and status are untouched. Only the
    /// run's tab dot and its parked decision state may change.
    fn handle_background_event(&mut self, ev: &RuntimeErrorEvent, run_id: &str) {
        match ev {
            // Liveness: the tab dot tracks work the user can't see.
            RuntimeErrorEvent::TurnStarted { .. } | RuntimeErrorEvent::RunStarted { .. } => {
                self.tabs.set_busy(run_id, true);
            }
            RuntimeErrorEvent::ToolStarted { tool, call_id, .. } => {
                self.tabs.set_busy(run_id, true);
                // Capture the step for the sidebar activity timeline when
                // this run belongs to a `/btw` background task.
                if let Some(t) = self.bg_tasks.iter_mut().find(|t| t.run_id == run_id) {
                    t.steps.push(activity::BgStep::started(tool, call_id));
                    if t.steps.len() > bg::MAX_BG_STEPS {
                        t.steps.remove(0);
                    }
                }
            }
            RuntimeErrorEvent::ToolCompleted { tool, call_id, .. } => {
                if let Some(t) = self.bg_tasks.iter_mut().find(|t| t.run_id == run_id) {
                    // Exact call id first; fall back to the newest live
                    // step for this tool when the start carried no card.
                    let target =
                        match t.steps.iter_mut().rev().find(|s| {
                            s.call_id == *call_id && s.status == activity::StepStatus::Running
                        }) {
                            Some(s) => Some(s),
                            None => t.steps.iter_mut().rev().find(|s| {
                                s.name == *tool && s.status == activity::StepStatus::Running
                            }),
                        };
                    if let Some(s) = target {
                        s.finish(true);
                    }
                }
            }
            RuntimeErrorEvent::RunCompleted { .. }
            | RuntimeErrorEvent::RunFailed { .. }
            | RuntimeErrorEvent::RunCanceled { .. }
            | RuntimeErrorEvent::TurnCompleted { .. }
            | RuntimeErrorEvent::TurnFailed { .. } => {
                self.tabs.set_busy(run_id, false);
            }
            // Subagent lifecycle on a background tab: recorded against
            // the parent run so the timeline shows it when that tab is
            // visible. The visible-session arm dedupes the same events.
            RuntimeErrorEvent::AgentSpawned { agent, .. }
            | RuntimeErrorEvent::AgentMessage { agent, .. } => {
                activity::record_spawned(&mut self.subagents, run_id, agent);
            }
            RuntimeErrorEvent::AgentCompleted { agent, .. } => {
                activity::record_completed(&mut self.subagents, run_id, agent);
            }
            // Decisions park durably; the card renders on tab switch.
            RuntimeErrorEvent::ApprovalRequested { scope, .. } => {
                self.park_approval(run_id, scope);
            }
            RuntimeErrorEvent::ApprovalGranted { .. }
            | RuntimeErrorEvent::ApprovalDenied { .. } => {
                self.approvals.remove(run_id);
                self.tabs.set_approval(run_id, false);
            }
            RuntimeErrorEvent::UserInputRequested {
                call_id,
                question,
                options,
                ..
            } => {
                self.park_clarify(
                    run_id,
                    ClarifyRequest {
                        call_id: call_id.clone(),
                        question: question.clone(),
                        options: options.clone(),
                    },
                );
            }
            RuntimeErrorEvent::UserInputProvided { .. } => {
                self.clarifies.remove(run_id);
                self.tabs.set_approval(run_id, false);
            }
            // Everything else is visible-session UI; a background run has
            // no business touching it.
            _ => {}
        }
    }

    /// Process a runtime event into a transcript block.
    ///
    /// Background-run safety: events for a run that is not the visible
    /// session only ever update that run's tab dot / parked decision —
    /// the visible transcript, input, and status belong to the active
    /// session alone.
    pub fn handle_runtime_event(&mut self, ev: &RuntimeErrorEvent) {
        if let Some(rid) = event_run_id(ev) {
            if rid != self.session_id {
                self.handle_background_event(ev, rid);
                return;
            }
        }
        match ev {
            RuntimeErrorEvent::ToolStarted {
                tool,
                args,
                call_id,
                ..
            } => {
                self.last_tool = Some((tool.clone(), args.clone()));
                // Drive the footer phase from the real tool identity:
                // search tools → Searching, edits → Editing, plan/todo
                // tools → Planning, everything else → Thinking. In plan
                // mode the phase stays Planning for the whole turn.
                self.set_activity_status(if self.plan_mode {
                    "Planning...".to_string()
                } else {
                    activity::status_for_tool(tool, args)
                });
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
                        call_id: call_id.clone(),
                        started: Some(std::time::Instant::now()),
                        duration: None,
                        error: None,
                        tokens: None,
                    },
                });
            }
            RuntimeErrorEvent::ToolCompleted { tool, call_id, .. } => {
                // Flip the matching still-running card. Prefer the exact
                // call id; fall back to the newest still-running card for
                // this tool when the start event carried no card.
                self.tools_used = self.tools_used.saturating_add(1);
                let by_id = self.blocks.iter_mut().rev().find(|b| {
                    matches!(&b.kind, BlockKind::ToolCall { call_id: c, ok: None, .. } if c == call_id)
                });
                let target = match by_id {
                    Some(b) => Some(b),
                    None => self.blocks.iter_mut().rev().find(|b| {
                        matches!(&b.kind, BlockKind::ToolCall { name: n, ok: None, .. } if n == tool)
                    }),
                };
                if let Some(block) = target {
                    if let BlockKind::ToolCall {
                        ok,
                        started,
                        duration,
                        tokens,
                        ..
                    } = &mut block.kind
                    {
                        *ok = Some(true);
                        if let Some(s) = started.take() {
                            *duration = Some(s.elapsed());
                        }
                        // Freeze the live token count on the card.
                        *tokens = Some(self.tokens_used + self.turn_estimate);
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
                // Park first (map + tab dot), then render the visible card.
                // The background run_id was filtered above, so this is
                // always the visible session here.
                let (tool, sentence, target) = self.park_approval(run_id, scope);
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::Approval {
                        tool,
                        sentence,
                        target,
                        decision: None,
                    },
                });
                self.scroll_to_bottom();
                self.pending_approval = Some((run_id.clone(), scope.clone()));
                self.status_line = "permission required".into();
            }
            // A sibling decision, resolved elsewhere (another client, a
            // re-entered session): collapse the card instead of leaving it
            // pending.
            RuntimeErrorEvent::ApprovalGranted { run_id, .. } => {
                self.approvals.remove(run_id);
                self.tabs.set_approval(run_id, false);
                self.resolve_approval_block(Some(true));
                self.pending_approval = None;
            }
            RuntimeErrorEvent::ApprovalDenied { run_id, .. } => {
                self.approvals.remove(run_id);
                self.tabs.set_approval(run_id, false);
                self.resolve_approval_block(Some(false));
                self.pending_approval = None;
            }
            // ask_user parked the run: same decision pattern as approval,
            // but cyan and with numbered options plus free text.
            RuntimeErrorEvent::UserInputRequested {
                run_id,
                call_id,
                question,
                options,
            } => {
                let req = ClarifyRequest {
                    call_id: call_id.clone(),
                    question: question.clone(),
                    options: options.clone(),
                };
                self.park_clarify(run_id, req.clone());
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::Clarify {
                        question: question.clone(),
                        options: options.clone(),
                        answered: None,
                    },
                });
                self.scroll_to_bottom();
                self.pending_clarify = Some(req);
                self.status_line = "input requested".into();
            }
            RuntimeErrorEvent::UserInputProvided {
                run_id, call_id, ..
            } => {
                let _ = call_id;
                self.clarifies.remove(run_id);
                self.tabs.set_approval(run_id, false);
                self.pending_clarify = None;
            }
            RuntimeErrorEvent::RunProgress { detail, .. } => {
                self.status_line = detail.clone();
            }
            RuntimeErrorEvent::AgentSpawned { agent, run_id } => {
                // Feed the sidebar activity timeline (dedupe collapses
                // the engine + runtime double-announce), then the
                // transcript card as before.
                activity::record_spawned(&mut self.subagents, run_id, agent);
                self.blocks.push(TranscriptBlock {
                    kind: BlockKind::Swarm {
                        agents: 1,
                        task: agent.clone(),
                    },
                });
            }
            // The parent engine announces the delegation before the
            // runtime records it; the timeline dedupes against the
            // AgentSpawned that follows.
            RuntimeErrorEvent::AgentMessage { agent, run_id } => {
                activity::record_spawned(&mut self.subagents, run_id, agent);
            }
            RuntimeErrorEvent::AgentCompleted { agent, run_id } => {
                activity::record_completed(&mut self.subagents, run_id, agent);
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

    /// Collapse the newest still-pending approval card to its resolved
    /// marker (`● approved` / `● denied`).
    fn resolve_approval_block(&mut self, decision: Option<bool>) {
        if let Some(block) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(&b.kind, BlockKind::Approval { decision: None, .. }))
        {
            if let BlockKind::Approval { decision: d, .. } = &mut block.kind {
                *d = decision;
            }
        }
    }

    /// Collapse the newest still-pending clarify card to its answered
    /// marker (`● answered: …`).
    fn resolve_clarify_block(&mut self, answer: &str) {
        if let Some(block) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(&b.kind, BlockKind::Clarify { answered: None, .. }))
        {
            if let BlockKind::Clarify { answered, .. } = &mut block.kind {
                *answered = Some(answer.to_string());
            }
        }
    }

    /// Collapse the newest still-pending confirm card.
    fn resolve_confirm_block(&mut self, decided: bool) {
        if let Some(block) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|b| matches!(&b.kind, BlockKind::Confirm { decided: None, .. }))
        {
            if let BlockKind::Confirm { decided: d, .. } = &mut block.kind {
                *d = Some(decided);
            }
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
        // Fresh working status: Thinking..., or Planning... when plan
        // mode is on. The sweep restarts from the left edge of the new
        // string.
        self.set_activity_status(if self.plan_mode {
            "Planning...".to_string()
        } else {
            "Thinking...".to_string()
        });
        self.sweep.reset();
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

    /// A background run finished: only its tab dot and the notification
    /// badge may change. The visible session's `ready`, status line,
    /// active run, timing, and turn count belong to the active tab and
    /// are never touched here.
    pub fn on_background_turn_complete(&mut self, run_id: &str) {
        self.tabs.set_busy(run_id, false);
        self.attention = true;
        self.attention_note = Some("a background turn finished".to_string());
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
        // While a slash command is running, its output lines are captured
        // into the command buffer instead; `handle_slash` flushes them as
        // one `Command` block when the command finishes.
        if let Some((_, buf)) = self.cmd_capture.as_mut() {
            buf.push(text);
            return;
        }
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
        self.spinner_tick = self.spinner_tick.wrapping_add(1);
        // The traveling highlight only sweeps while a turn is live;
        // parked otherwise it would animate over a settled screen.
        if !self.ready {
            self.sweep.tick();
        }
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

/// Icons for transcript cards and the footer. Every entry is used
/// by `render_block` or `render_footer` below — no speculative glyphs.
mod icon {
    pub const RUNNING: &str = "●";
    pub const SUCCESS: &str = "✓";
    pub const FAILURE: &str = "×";
    pub const TOOL: &str = "⚙";
    pub const AGENT: &str = "→";
    pub const WARN: &str = "⚠";
}

/// Render the full TUI frame: header, transcript, input, status bar.
///
/// Takes `&mut` because a timeline jump pins the viewport to a turn's
/// first block during this render (consuming `jump_to_block`); nothing
/// else about the transcript is mutated.
/// Rebuild the session tab bar from the run ledger. Cheap local query;
/// called on loop start, /new, resume, tab switch and turn completion so
/// titles and busy badges stay current.
/// Rebuild the tab bar from the explicit open-tab list. The ledger is
/// consulted for titles only, never for membership: opening a tab opens
/// a session, closing parks it. Parked runs stay reopenable via
/// /sessions or the session picker.
fn refresh_tabs(state: &mut TuiState, session: &Arc<Session>) {
    let active = state.session_id.clone();
    // A visible run is an open tab by definition: pin it even if the
    // seed hasn't caught up yet (fresh /new has no ledger row).
    if !state.open_tabs.contains(&active) {
        state.open_tabs.push(active.clone());
    }
    let mut titles: Vec<(String, Option<String>)> = Vec::with_capacity(state.open_tabs.len());
    for id in &state.open_tabs {
        let title = session.supervisor.ledger_title(id).ok().flatten();
        titles.push((id.clone(), title));
    }
    state.tabs.refresh_from_explicit(
        &titles,
        |id| session.supervisor.has_active_lease(id).unwrap_or(false),
        &active,
    );
    // Re-apply the parked-decision dots: a refresh rebuilds the list
    // and would otherwise drop them.
    for run in state.approvals.keys().chain(state.clarifies.keys()) {
        state.tabs.set_approval(run, true);
    }
    // Cache the full ledger list for the session picker:
    // open tabs come from the tab bar, parked runs from here.
    state.parked_runs = session.supervisor.ledger_list_runs(50).unwrap_or_default();
}

/// Sync the transcript todo card from the runtime session's todo list.
/// Cheap (one mutex): called every frame before draw. Maps the canonical
/// `pantheon_api::todo` items onto the card's view types; `todo_count`
/// is the not-yet-done count, matching the card header.
fn refresh_todos(state: &mut TuiState, session: &Arc<Session>) {
    use crate::todo_card::{TodoCardItem, TodoStatus as CardStatus};
    use pantheon_api::todo::TodoStatus as ApiStatus;
    let items: Vec<TodoCardItem> = session
        .todo_list()
        .into_iter()
        .map(|i| TodoCardItem {
            content: i.content,
            status: match i.status {
                ApiStatus::Pending => CardStatus::Pending,
                ApiStatus::InProgress => CardStatus::InProgress,
                ApiStatus::Completed => CardStatus::Completed,
            },
        })
        .collect();
    state.apply_todo_items(items);
    sync_inline_todo_card(state);
}

/// Keep the inline transcript todo card in sync. When the sidebar is
/// gone (narrow terminal, or toggled off) the opencode-style card
/// lives in the transcript instead, so the plan stays visible without
/// any keybind: one live block, updated in place as todos progress.
/// Wide mode removes it again — the sidebar owns it there. Idempotent
/// by design: `refresh_todos` calls this every frame, so it only
/// touches `blocks` when the desired state actually changed.
fn sync_inline_todo_card(state: &mut TuiState) {
    let want_card = !state.sidebar_visible() && !state.todo_items.is_empty();
    let pos = state
        .blocks
        .iter()
        .position(|b| matches!(b.kind, BlockKind::Todos(_)));
    match (want_card, pos) {
        (true, Some(i)) => {
            // Refresh in place; a no-op when nothing changed so the
            // transcript never churns.
            if let BlockKind::Todos(items) = &mut state.blocks[i].kind {
                if *items != state.todo_items {
                    *items = state.todo_items.clone();
                }
            }
        }
        (true, None) => {
            state.blocks.push(TranscriptBlock {
                kind: BlockKind::Todos(state.todo_items.clone()),
            });
            state.scroll_to_bottom();
        }
        (false, Some(i)) => {
            state.blocks.remove(i);
        }
        (false, None) => {}
    }
}

impl TuiState {
    /// Apply a fresh todo snapshot: refreshes the count and, when the plan
    /// actually changed, auto-switches the sidebar's bottom card to the
    /// todo view. A manual ^g toggle afterwards still wins.
    pub fn apply_todo_items(&mut self, items: Vec<crate::todo_card::TodoCardItem>) {
        use crate::todo_card::TodoStatus as CardStatus;
        self.todo_count = items
            .iter()
            .filter(|i| i.status != CardStatus::Completed)
            .count();
        if self.todo_items != items {
            self.todo_items = items;
            // Fresh plan activity: the todo view becomes the sidebar's
            // bottom card.
            self.sidebar_view = SidebarView::Todo;
        }
    }
}

/// Seed the open-tab list at startup: the run the loop opens on is the
/// first tab. `/new` adds; closing parks. The ledger is never the source
/// of truth for membership.
fn seed_open_tabs(state: &mut TuiState) {
    state.open_tabs.clear();
    state.open_tabs.push(state.session_id.clone());
}

/// Hydrate a run's pending decisions from the ledger-backed supervisor
/// into the TUI's in-memory maps. Called at startup for the opening
/// session and on every tab switch, so a re-entered session re-renders
/// its approval/clarify cards from durable state rather than from
/// whatever happened to be in memory.
fn hydrate_decisions(state: &mut TuiState, session: &Arc<Session>, run_id: &str) {
    // Approvals: the newest unresolved scope wins (the card shows one
    // decision point; older scopes are superseded by design).
    if let Ok(scopes) = session.supervisor.pending_approvals(run_id) {
        if let Some(scope) = scopes.into_iter().last() {
            let (tool, args) = parse_scope(&scope)
                .map(|(_, t, a)| (t.to_string(), a.to_string()))
                .unwrap_or_else(|| ("tool".to_string(), String::new()));
            state
                .approvals
                .insert(run_id.to_string(), (scope, tool, args));
            state.tabs.set_approval(run_id, true);
        } else {
            state.approvals.remove(run_id);
        }
    }
    // Clarify questions: the newest unresolved one wins.
    if let Ok(pending) = session.supervisor.pending_input(run_id) {
        if let Some((call_id, question, options)) = pending.into_iter().last() {
            state.clarifies.insert(
                run_id.to_string(),
                ClarifyRequest {
                    call_id,
                    question,
                    options,
                },
            );
            state.tabs.set_approval(run_id, true);
        } else {
            state.clarifies.remove(run_id);
        }
    }
}

/// Open a fresh session tab: a new run id with an empty transcript.
/// No ledger row until the first turn, so abandoned tabs leave nothing
/// behind. Stashes the current tab's draft first — switching never eats
/// one. Never forces `ready`: a turn may still run for the previous run.
fn new_tab(state: &mut TuiState, session: &Arc<Session>, status: &str) {
    let old = state.session_id.clone();
    if !state.input.is_empty() {
        state.drafts.insert(old, std::mem::take(&mut state.input));
    }
    state.session_id = pantheon_runtime::new_run_id();
    // Opening a tab is explicit membership: the new run joins the
    // open-tab list immediately, ledger row or not.
    if !state.open_tabs.contains(&state.session_id) {
        state.open_tabs.push(state.session_id.clone());
    }
    state.blocks.clear();
    state.title = None;
    state.scroll_offset = 0;
    state.pending_approval = None;
    state.pending_clarify = None;
    // A new conversation is a new objective: the /goal iteration gate
    // belongs to the old work and must not block the new session.
    state.goal = None;
    session.set_goal(None);
    state.add_status(status.into());
}

/// Close the active tab. Parks the run — never kills it: the draft is
/// stashed, the tab leaves the bar, and the run stays reopenable via
/// /sessions or the session picker. Never strands the session: with no
/// tabs left, a fresh one opens.
fn close_active_tab(state: &mut TuiState, session: &Arc<Session>) {
    let run = state.session_id.clone();
    if !state.input.is_empty() {
        state
            .drafts
            .insert(run.clone(), std::mem::take(&mut state.input));
    }
    // Park: leave the open-tab list, keep the run. It reopens via
    // /sessions or the session picker.
    state.open_tabs.retain(|id| id != &run);
    state.tabs.remove(&run);
    match state.tabs.active_run_id().map(str::to_string) {
        Some(next) => {
            state.add_status(format!(
                "closed tab {run} (run parked, reopen via /sessions)"
            ));
            switch_to_run(state, session, &next, "resumed");
        }
        None => new_tab(
            state,
            session,
            "closed last tab; new tab (unsaved until the first turn)",
        ),
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
    // Stash the draft of the tab we're leaving; the target's restores
    // below. Switching tabs never eats a draft.
    let old = state.session_id.clone();
    if !state.input.is_empty() {
        state.drafts.insert(old, std::mem::take(&mut state.input));
    } else {
        state.input.clear();
    }
    // A visible run is an open tab by definition: reopening a parked
    // run re-adds it. Decisions hydrate from the ledger first so the
    // card renders from durable state, not stale memory.
    if !state.open_tabs.contains(&id.to_string()) {
        state.open_tabs.push(id.to_string());
    }
    let _ = session.supervisor.ledger_reopen_run(id);
    hydrate_decisions(state, session, id);
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
        // Durable decisions: a re-entered session re-renders any still
        // pending approval or clarify card, so the decision point is
        // visible instead of a dead transcript. The keyboard follows the
        // visible session.
        match state.approvals.get(id) {
            Some((scope, tool, args)) => {
                let (sentence, target) = describe_tool_action(tool, args);
                state.blocks.push(TranscriptBlock {
                    kind: BlockKind::Approval {
                        tool: tool.clone(),
                        sentence,
                        target,
                        decision: None,
                    },
                });
                state.pending_approval = Some((id.to_string(), scope.clone()));
            }
            None => state.pending_approval = None,
        }
        match state.clarifies.get(id).cloned() {
            Some(req) => {
                state.blocks.push(TranscriptBlock {
                    kind: BlockKind::Clarify {
                        question: req.question.clone(),
                        options: req.options.clone(),
                        answered: None,
                    },
                });
                state.pending_clarify = Some(req);
            }
            None => state.pending_clarify = None,
        }
        state.scroll_to_bottom();
    }
    state.input = state.drafts.remove(id).unwrap_or_default();
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
    if state.providers.is_some() {
        render_providers(f, f.area(), state);
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

    // Near-black base wash: every widget below paints on it.
    let bg = state.theme.bg;
    f.render_widget(Paragraph::new("").style(Style::default().bg(bg)), f.area());

    // Fresh (or /clear'ed) session: the splash owns the chat area and
    // carries its own centered composer, so the bottom input row is
    // folded away until the first block lands.
    if state.blocks.is_empty() {
        let [tab_area, splash_area, bottom_area] = Layout::vertical([
            Constraint::Length(1), // session tabs
            Constraint::Min(1),    // splash
            Constraint::Length(1), // plugin / version line
        ])
        .areas(f.area());
        crate::tabs::render_tab_bar(f, tab_area, &state.tabs, &state.theme);
        render_splash(f, splash_area, state);
        render_splash_bottom(f, bottom_area, state);
    } else {
        // ONE unified session view: tabs → 75/25 split → footer, with a
        // health alert strip above the footer only when something is
        // actually wrong (failing MCP, gateway trouble, overdue jobs).
        refresh_health(state);
        if !state.health_alerts.is_empty() {
            let outer = Layout::vertical([
                Constraint::Length(1), // session tabs
                Constraint::Min(1),    // body: transcript+input | sidebar
                Constraint::Length(1), // health alerts
                Constraint::Length(1), // footer
            ]);
            let [tab_area, body_area, alerts_area, footer_area] = outer.areas(f.area());
            crate::tabs::render_tab_bar(f, tab_area, &state.tabs, &state.theme);
            render_body(state, f, body_area);
            render_alerts(f, alerts_area, state);
            render_footer(f, footer_area, state);
        } else {
            let outer = Layout::vertical([
                Constraint::Length(1), // session tabs
                Constraint::Min(1),    // body: transcript+input | sidebar
                Constraint::Length(1), // footer
            ]);
            let [tab_area, body_area, footer_area] = outer.areas(f.area());
            crate::tabs::render_tab_bar(f, tab_area, &state.tabs, &state.theme);
            render_body(state, f, body_area);
            render_footer(f, footer_area, state);
        }
    }
    if state.timeline.is_some() {
        render_timeline(f, f.area(), state);
    }
    if state.mention.is_some() {
        render_mention_picker(f, f.area(), state);
    }
    if state.palette.is_some() {
        render_palette(f, f.area(), state);
    }
}

/// The `/` command palette: a centered popup listing the filtered
/// registry entries with their descriptions. The selected row is
/// highlighted; the footer names the keys.
fn render_palette(f: &mut Frame, area: Rect, state: &TuiState) {
    let Some(p) = state.palette.as_ref() else {
        return;
    };
    let th = &state.theme;
    let items = p.filtered();
    let rows = items.len().min(10);
    // Width: longest "name  desc" row, clamped to the terminal.
    let mut inner_w = format!("/{}_", p.input).len();
    for (name, desc) in items.iter().take(rows) {
        inner_w = inner_w.max(format!("  {name:<22} {desc}").len());
    }
    let inner_w = (inner_w as u16 + 4).clamp(30, area.width.saturating_sub(4).max(30));
    // The empty state adds a "no matching command" line below the blank
    // separator: reserve a row for it or the border clips it.
    let body_rows = rows + usize::from(items.is_empty());
    let height = (body_rows as u16 + 4).min(area.height.saturating_sub(4).max(6));
    let x = area.x + (area.width.saturating_sub(inner_w)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 3;
    let popup = Rect::new(x, y, inner_w, height);
    f.render_widget(Clear, popup);
    let mut lines: Vec<Line> = Vec::with_capacity(rows + 2);
    lines.push(Line::from(vec![
        Span::styled(
            "/ ",
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        ),
        Span::styled(p.input.clone(), Style::default().fg(th.primary)),
        Span::styled("▌", Style::default().fg(th.primary)),
    ]));
    lines.push(Line::from(""));
    for (i, (name, desc)) in items.iter().take(rows).enumerate() {
        let row = format!("  {name:<22} {desc}");
        // Middle-ellipsis the description when the popup is narrower
        // than the row: the command name stays readable.
        let row = if row.len() > inner_w as usize - 2 {
            let keep = inner_w as usize - 5;
            format!("{}…", row.chars().take(keep).collect::<String>())
        } else {
            row
        };
        // The name column is 24 chars wide ("  " + 22 padded): split the
        // row there so the command name keeps its primary color. The
        // selected row renders reversed, like every other picker.
        let split = row
            .char_indices()
            .nth(24)
            .map(|(b, _)| b)
            .unwrap_or(row.len());
        let (head, tail) = row.split_at(split);
        let row_style = if i == p.sel {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        let head_style = if i == p.sel {
            row_style.fg(th.primary).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.primary)
        };
        let tail_style = if i == p.sel {
            row_style.fg(th.dim)
        } else {
            Style::default().fg(th.dim)
        };
        lines.push(Line::from(vec![
            Span::styled(head.to_string(), head_style),
            Span::styled(tail.to_string(), tail_style),
        ]));
    }
    if items.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no matching command",
            Style::default().fg(th.dim),
        )));
    }
    let body = Paragraph::new(lines).block(
        Block::bordered()
            .title(format!(
                " commands ({}/{} · Enter run · Esc dismiss) ",
                if items.is_empty() {
                    0
                } else {
                    (p.sel + 1).min(items.len())
                },
                items.len()
            ))
            .border_style(Style::default().fg(th.primary)),
    );
    f.render_widget(body, popup);
}

/// Subtle column divider: a 1-cell column with a slightly lighter
/// background tint than the surrounding #121212/#1E1E1E. No glyphs —
/// the box-drawing ban still stands.
const DIVIDER_BG: Color = Color::Rgb(38, 38, 38);

/// Session body: the unified 75/25 split — transcript + input block on
/// the left, the session sidebar on the right. 100–119 cols narrows the
/// split to 80/20; below 100 cols the body is a single column and the
/// sidebar (while toggled on) floats as a right-side overlay. Column
/// separation is the 1-cell divider's bg tint — zero borders, zero
/// box-drawing.
fn render_body(state: &mut TuiState, f: &mut Frame, area: Rect) {
    let w = area.width;
    if state.sidebar_visible() {
        let pct = if w >= 120 { 25u32 } else { 20u32 };
        let side_w = (area.width as u32 * pct / 100) as u16;
        let left_w = area.width.saturating_sub(side_w).saturating_sub(1);
        let left = Rect::new(area.x, area.y, left_w, area.height);
        let divider = Rect::new(area.x + left_w, area.y, 1, area.height);
        let right = Rect::new(area.x + left_w + 1, area.y, side_w, area.height);
        render_left(state, f, left);
        f.render_widget(
            Paragraph::new("").style(Style::default().bg(DIVIDER_BG)),
            divider,
        );
        render_sidebar(f, right, state);
    } else {
        render_left(state, f, area);
        if state.sidebar_overlay {
            // Narrow terminal: the sidebar floats over the body's right
            // edge (~36 cols) once explicitly toggled on with ^b — it
            // never opens by itself.
            let ow = 36.min(area.width);
            let overlay = Rect::new(
                area.x + area.width.saturating_sub(ow),
                area.y,
                ow,
                area.height,
            );
            f.render_widget(Clear, overlay);
            render_sidebar(f, overlay, state);
        }
    }
}

/// Left column: transcript above, the 2-row input block below. The
/// reflect card owns the input row while it is open.
fn render_left(state: &mut TuiState, f: &mut Frame, area: Rect) {
    let [transcript_area, input_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(area);
    render_transcript(f, transcript_area, state);
    if state.pending_reflect.is_some() {
        render_reflect_card(f, input_area, state);
    } else {
        render_input_block(f, input_area, state);
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
/// /sessions picker: searchable, date-grouped, project-aware. Layout
/// follows the reference: Search header, group headers per calendar day,
/// a full-bleed selection wash in the theme's tab highlight color, and a
/// key-hint footer (delete ctrl+d, rename ctrl+r, all projects ctrl+a).
fn render_history(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    let w = area.width.clamp(44, 84);
    let h = area.height.clamp(9, 24);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    // Inner width (inside the borders): rows are padded to it so the
    // selection wash bleeds edge to edge like the reference.
    let inner = w.saturating_sub(2) as usize;

    let runs = state.filtered_history();
    let renaming = state.history_rename.clone();
    let mut lines = vec![
        Line::from(Span::styled("  Search", Style::default().fg(th.dim))),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(state.history_input.clone(), Style::default().fg(th.body)),
            Span::styled("\u{258f}", Style::default().fg(th.primary)),
        ]),
        Line::from(""),
    ];
    if runs.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(th.failure),
        )));
    }
    let mut last_day = String::new();
    for (i, (id, status, ts, title, project)) in runs.iter().enumerate() {
        // Date group header on day change; purely visual, never selected.
        let day = fmt_day(*ts);
        if day != last_day {
            last_day = day.clone();
            lines.push(Line::from(Span::styled(
                format!("  {day}"),
                Style::default().fg(th.heading).add_modifier(Modifier::BOLD),
            )));
        }
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
            Some(t) => t.chars().take(34).collect::<String>(),
            None => short.clone(),
        };
        // The pinned home session leads the list; mark it so it reads as
        // permanent rather than just another recent run.
        let pin = if id == pantheon_storage::HOME_SESSION_ID {
            "\u{2302} "
        } else {
            "  "
        };
        // In the all-projects view the project tag disambiguates rows;
        // in the single-project view it would just repeat the header.
        let proj_tag = if state.history_all_projects {
            match project.as_deref().filter(|p| !p.is_empty()) {
                Some(p) => format!(" \u{b7} {p}"),
                None => String::new(),
            }
        } else {
            String::new()
        };
        let selected = i == state.history_sel;
        let is_renaming = renaming.as_ref().is_some_and(|(rid, _)| rid == id);
        let row_text = if is_renaming {
            // Inline rename: the row becomes an editor.
            let buf = renaming.as_ref().map(|(_, b)| b.as_str()).unwrap_or("");
            format!("  > {buf}\u{258f}")
        } else {
            format!("  {pin}{glyph} {label:<34} {}{}", fmt_age(*ts), proj_tag)
        };
        let padded = format!("{row_text:<inner$}");
        let style = if selected {
            Style::default()
                .fg(th.body)
                .bg(th.tab_active_bg)
                .add_modifier(Modifier::BOLD)
        } else if is_renaming {
            Style::default().fg(th.warning)
        } else {
            Style::default().fg(th.body)
        };
        lines.push(Line::from(Span::styled(padded, style)));
    }
    lines.push(Line::from(""));
    if renaming.is_some() {
        lines.push(Line::from(Span::styled(
            "  Enter saves   Esc cancels",
            Style::default().fg(th.dim),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  delete ctrl+d    rename ctrl+r    all projects ctrl+a",
            Style::default().fg(th.dim),
        )));
    }
    let scope = if state.history_all_projects {
        "all projects"
    } else {
        match state.history_own_project.as_deref() {
            Some(p) => p,
            None => "sessions",
        }
    };
    let card = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(th.primary))
            .title(Span::styled(
                format!(" {scope} "),
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
        for p in pending.iter().take(5) {
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

/// Provider picker: the first level of `/models`. Search header,
/// alphabetical provider rows, full-bleed selection wash in the theme's
/// tab highlight color. Enter drills into the provider's models.
fn render_providers(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    let w = area.width.clamp(44, 72);
    let h = area.height.clamp(9, 22);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    let inner = w.saturating_sub(2) as usize;

    let rows = state.filtered_providers();
    let mut lines = vec![
        Line::from(Span::styled("  Search", Style::default().fg(th.dim))),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(state.providers_input.clone(), Style::default().fg(th.body)),
            Span::styled("\u{258f}", Style::default().fg(th.primary)),
        ]),
        Line::from(""),
    ];
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no matches)",
            Style::default().fg(th.failure),
        )));
    }
    for (i, row) in rows.iter().enumerate() {
        let selected = i == state.providers_sel;
        let count = if row.model_count == 0 {
            String::new()
        } else {
            format!("  {} models", row.model_count)
        };
        let text = format!("  {}{}", row.provider_label, count);
        let padded = format!("{text:<inner$}");
        let style = if selected {
            Style::default()
                .fg(th.body)
                .bg(th.tab_active_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.body)
        };
        lines.push(Line::from(Span::styled(padded, style)));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter opens models   Esc closes",
        Style::default().fg(th.dim),
    )));
    let card = Paragraph::new(lines).block(
        Block::bordered()
            .border_type(BorderType::Double)
            .border_style(Style::default().fg(th.primary))
            .title(Span::styled(
                " providers ",
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )),
    );
    f.render_widget(card, area);
}

/// Searchable model browser overlay: /models. Type-to-filter across
/// provider and model names, Up/Down to move, Enter to switch the live
/// session's default model, Esc to close. Same overlay contract as
/// /history, rendered as a centered list.
/// Model list: the second level of `/models`, scoped to the provider
/// picked in the provider list. Layout follows the reference: Search
/// header, one row per model with the current model marked `●`, the ctx
/// limit right-aligned as the badge column, a full-bleed selection wash
/// in the theme's tab highlight color. Esc steps back to providers.
fn render_models(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    let w = area.width.clamp(48, 88);
    let h = area.height.clamp(9, 22);
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    let inner = w.saturating_sub(2) as usize;

    let rows = state.filtered_models();
    let mut lines = vec![
        Line::from(Span::styled("  Search", Style::default().fg(th.dim))),
        Line::from(vec![
            Span::styled("  ", Style::default()),
            Span::styled(state.models_input.clone(), Style::default().fg(th.body)),
            Span::styled("\u{258f}", Style::default().fg(th.primary)),
        ]),
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
        let selected = i == state.models_sel;
        if row.model_id.is_empty() {
            let text = "  (no curated models — Enter for how to switch)";
            let padded = format!("{text:<inner$}");
            let style = if selected {
                Style::default()
                    .fg(th.body)
                    .bg(th.tab_active_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.dim)
            };
            lines.push(Line::from(Span::styled(padded, style)));
            continue;
        }
        // Badge column: the ctx limit, right-aligned like the
        // reference's "Free" column.
        let badge = match row.ctx {
            Some(c) if c >= 1_000_000 => format!("{}M ctx", c / 1_000_000),
            Some(c) if c >= 1000 => format!("{}k ctx", c / 1000),
            Some(c) => format!("{c} ctx"),
            None => "ctx ?".to_string(),
        };
        let model: String = row.model_id.chars().take(40).collect();
        let marker = if current { "\u{25cf}" } else { " " };
        // Left part width, then a gap so the badge hugs the right edge
        // and the row fills the card exactly.
        let left_len = 2 + 1 + 1 + model.chars().count();
        let gap = inner.saturating_sub(left_len + badge.chars().count() + 1);
        let base = if selected {
            Style::default()
                .fg(th.body)
                .bg(th.tab_active_bg)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.body)
        };
        let marker_style = if current {
            // The current model glows amber; on a selected row it keeps
            // the selection wash behind it.
            let mut s = Style::default()
                .fg(th.emphasis)
                .add_modifier(Modifier::BOLD);
            if selected {
                s = s.bg(th.tab_active_bg);
            }
            s
        } else {
            base
        };
        let badge_style = if selected {
            Style::default().fg(th.dim).bg(th.tab_active_bg)
        } else {
            Style::default().fg(th.dim)
        };
        lines.push(Line::from(vec![
            Span::styled("  ", base),
            Span::styled(marker, marker_style),
            Span::styled(format!(" {model}"), base),
            Span::styled(format!("{:gap$}", "", gap = gap), base),
            Span::styled(format!(" {badge}"), badge_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Enter switches   Esc back to providers",
        Style::default().fg(th.dim),
    )));
    let scope = state
        .filtered_models()
        .first()
        .map(|r| r.provider_label.clone())
        .unwrap_or_default();
    let title = if scope.is_empty() {
        " models ".to_string()
    } else {
        format!(" models · {scope} ")
    };
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

/// Calendar-day label for session-picker group headers: `Sun Sep 27 2026`.
/// Local timezone, like the rest of the TUI's timestamps.
fn fmt_day(ms: i64) -> String {
    DateTime::from_timestamp(ms / 1000, 0)
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%a %b %e %Y")
                .to_string()
                .replace("  ", " ")
        })
        .unwrap_or_default()
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
    // Note: plain `[`/`]` are deliberately absent here. The main-view key
    // handler gates them out of tab_key_action, so they always type into
    // the composer; tab switching is Ctrl+Tab / Ctrl+Shift+Tab / Alt+1..9.
    let rows: &[(&str, &str)] = &[
        ("?", "this help"),
        ("q", "quit pantheon"),
        ("Enter", "send message"),
        ("Esc", "stop typing"),
        ("Esc x2", "interrupt run / offer rewind"),
        ("PgUp PgDn", "scroll transcript"),
        ("F2 / Ctrl+O", "turn timeline"),
        ("Ctrl+B", "toggle sidebar"),
        ("Ctrl+G", "sidebar activity / todo view"),
        ("Tab", "toggle build / plan mode"),
        ("Ctrl+T / Ctrl+W", "new tab / close tab (parked)"),
        ("Ctrl+Tab", "next session"),
        ("Ctrl+Shift+Tab", "previous session"),
        ("Alt+1..9", "jump to session"),
        ("Up / Down", "move in lists"),
        ("a / d", "approve / deny approval"),
        ("y / n", "allow / deny (alias)"),
        ("digit (clarify)", "answer pending question"),
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

/// Draw the input box at the bottom.
/// The composer text with its cursor marker, no chrome. Shared by the
/// bottom input row and the splash's centered input so both always show
/// the same draft.
fn composer_text(state: &TuiState) -> String {
    if state.vim.enabled && state.is_inputting {
        // Vim: draw the cursor at the modal (row, col) instead of the
        // trailing `_`. Normal shows a bar before the char under the
        // cursor; Insert keeps the `_` idiom, positioned.
        let (row, col) = state.vim.cursor(&state.input);
        let marker = if state.vim.mode == vim::VimMode::Normal {
            '▌'
        } else {
            '_'
        };
        let mut out = String::new();
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
        format!("{}{}", state.input, cursor)
    }
}

/// Spans for one composer row: blue left accent border, then the draft
/// (or the dim placeholder when the draft is empty). `first` selects the
/// accent bar vs. the continuation indent for wrapped rows.
fn composer_line_spans(
    state: &TuiState,
    th: &theme::Theme,
    text: &str,
    first: bool,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    if first {
        spans.push(Span::styled(
            "▌",
            Style::default()
                .fg(th.input_accent)
                .add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
    } else {
        spans.push(Span::raw("  "));
    }
    // Placeholder when the draft is empty: `composer_text` always
    // appends a cursor marker, so it can never be empty itself — check
    // the raw draft instead.
    if state.input.is_empty() && first {
        spans.push(Span::styled(
            "Ask anything, …",
            Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
        ));
        if state.is_inputting {
            spans.push(Span::styled("_", Style::default().fg(th.dim)));
        }
    } else {
        spans.push(Span::styled(text.to_string(), Style::default().fg(th.body)));
    }
    spans
}

/// opencode-style composer: a rounded box with a dim border and a blue
/// left accent, a prompt row, and a `provider · model · effort` row
/// (variant in blue, effort in orange). Exactly 4 rows tall; too-small
/// areas fall back to a single bare prompt row.
fn render_composer_box(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    let dim = Style::default().fg(th.dim);
    let accent = Style::default().fg(th.input_accent);
    let w = area.width as usize;
    if w < 16 || area.height < 4 {
        let text = composer_text(state);
        let first = text.lines().next().unwrap_or("");
        f.render_widget(
            Paragraph::new(Line::from(composer_line_spans(state, th, first, true))),
            Rect::new(area.x, area.y, area.width, area.height.min(1)),
        );
        return;
    }
    let inner = w - 2; // columns between the border glyphs
    let mut rows: Vec<Line> = Vec::with_capacity(4);
    rows.push(Line::from(vec![
        Span::styled("╭", dim),
        Span::styled("─".repeat(inner), dim),
        Span::styled("╮", dim),
    ]));
    rows.push(bordered_row(
        Span::styled("│", accent),
        composer_prompt_spans(state, th, inner - 1),
        inner,
        Span::styled("│", dim),
    ));
    rows.push(bordered_row(
        Span::styled("│", accent),
        composer_variant_spans(state, th, inner - 1),
        inner,
        Span::styled("│", dim),
    ));
    rows.push(Line::from(vec![
        Span::styled("╰", dim),
        Span::styled("─".repeat(inner), dim),
        Span::styled("╯", dim),
    ]));
    f.render_widget(
        Paragraph::new(rows),
        Rect::new(area.x, area.y, area.width, 4),
    );
}

/// One content row inside the composer box: `left | inner…pad | right`.
/// The inner spans are measured (display width) and padded so the right
/// border always lands on the box edge. Over-wide content clips — the
/// Paragraph never wraps, so the box keeps its shape.
fn bordered_row(
    left: Span<'static>,
    inner: Vec<Span<'static>>,
    inner_w: usize,
    right: Span<'static>,
) -> Line<'static> {
    let used = Line::from(inner.clone()).width();
    let mut spans = Vec::with_capacity(inner.len() + 3);
    spans.push(left);
    spans.extend(inner);
    if used < inner_w {
        spans.push(Span::raw(" ".repeat(inner_w - used)));
    }
    spans.push(right);
    Line::from(spans)
}

/// Prompt row spans: the draft's first line (with cursor marker), or the
/// dim placeholder when the draft is empty. Clipped to `max_w` display
/// cells so the box never overflows.
fn composer_prompt_spans(state: &TuiState, th: &theme::Theme, max_w: usize) -> Vec<Span<'static>> {
    if state.input.is_empty() {
        let mut spans = vec![Span::styled(
            "Ask anything, 'Fix broken tests'",
            Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
        )];
        if state.is_inputting {
            spans.push(Span::styled("_", Style::default().fg(th.dim)));
        }
        return spans;
    }
    let text = composer_text(state);
    let first = text.lines().next().unwrap_or("");
    let shown: String = first.chars().take(max_w).collect();
    vec![Span::styled(shown, Style::default().fg(th.body))]
}

/// The composer's `profile · provider · model · effort` row: profile in
/// body bold (when a profile is active), provider in blue bold, model in
/// body, effort in orange — the opencode signature. The model string is
/// `provider/model`; the provider segment replaces the old Build/Plan
/// variant (the mode already shows in the sidebar tag). The profile
/// segment is the active `[agents.<name>]` table name (`state.variant`,
/// set at startup and by `/agent <name>`); with no profiles declared it
/// is empty and the row renders exactly the old `provider · model ·
/// effort` shape (fail-open: no declared profile, no change).
fn composer_variant_spans(state: &TuiState, th: &theme::Theme, max_w: usize) -> Vec<Span<'static>> {
    let (provider, model) = match state.model.split_once('/') {
        Some((p, _)) => (p, state.model.rsplit('/').next().unwrap_or(p)),
        None => ("", state.model.as_str()),
    };
    let effort = state.effort.clone().unwrap_or_else(|| "—".to_string());
    let effort_len = effort.chars().count();
    let profile: String = state.variant.trim().to_string();
    // Fixed cells around the (possibly truncated) model: each present
    // leading segment costs its text plus one ` · ` separator, and the
    // effort costs one more separator.
    let leading = |text: &str| {
        if text.is_empty() {
            0
        } else {
            text.chars().count() + 3
        }
    };
    let fixed = leading(&profile) + leading(provider) + 3 + effort_len;
    // Over-wide: keep profile + provider + effort, middle-ellipsis the
    // model. `fixed + 1` reserves the ellipsis itself.
    let model_shown: String = if fixed + model.chars().count() <= max_w {
        model.to_string()
    } else {
        let keep = max_w.saturating_sub(fixed + 1);
        if keep > 1 {
            let m: String = model.chars().take(keep.saturating_sub(1)).collect();
            format!("{m}…")
        } else {
            "…".to_string()
        }
    };
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !profile.is_empty() {
        spans.push(Span::styled(
            profile,
            Style::default().fg(th.body).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" · ", Style::default().fg(th.dim)));
    }
    if !provider.is_empty() {
        spans.push(Span::styled(
            provider.to_string(),
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(" · ", Style::default().fg(th.dim)));
    }
    spans.push(Span::styled(model_shown, Style::default().fg(th.body)));
    spans.push(Span::styled(" · ", Style::default().fg(th.dim)));
    spans.push(Span::styled(effort, Style::default().fg(th.emphasis)));
    spans
}

/// Input block at the foot of the left column: row 0 is the
/// `profile · provider · model · effort` model status line (the profile
/// segment appears only while an agent profile is active), row 1 is
/// `> ` plus the draft (or the dim italic placeholder when the draft
/// is empty). Exactly 2 rows tall; zero chrome.
fn render_input_block(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    if area.height == 0 || area.width == 0 {
        return;
    }
    let w = area.width as usize;
    f.render_widget(
        Paragraph::new(Line::from(composer_variant_spans(state, th, w))),
        Rect::new(area.x, area.y, area.width, 1),
    );
    if area.height < 2 {
        return;
    }
    let mut prompt = vec![Span::styled(
        "> ",
        Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
    )];
    prompt.extend(composer_prompt_spans(state, th, w.saturating_sub(2)));
    f.render_widget(
        Paragraph::new(Line::from(prompt)),
        Rect::new(area.x, area.y + 1, area.width, 1),
    );
}

/// Health alert strip: one amber line directly above the footer, shown
/// only when something is actually wrong (failing MCP server, dead
/// gateway, stuck outbox, overdue scheduled job). Empty alerts render
/// nothing — the strip never takes space when Pantheon is healthy.
fn render_alerts(f: &mut Frame, area: Rect, state: &TuiState) {
    if area.height == 0 || area.width == 0 || state.health_alerts.is_empty() {
        return;
    }
    let th = &state.theme;
    let text = format!("{} {}", icon::WARN, state.health_alerts.join(" · "));
    let w = area.width as usize;
    let clipped: String = text.chars().take(w.saturating_sub(1)).collect();
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            clipped,
            Style::default().fg(th.warning),
        )))
        .style(Style::default().bg(th.bg)),
        area,
    );
}

/// One-line footer under the body: left is the compact live state word
/// (`✓ ready`, the braille working spinner, `awaiting approval` /
/// `awaiting input`, vim mode prefix); right is
/// `{tokens} ({pct}%) | / commands`. A pending rewind
/// confirmation takes over the whole bar so the question is impossible
/// to miss.
fn render_footer(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    if let Some(offer) = &state.rewind_offer {
        let text = format!(
            "↩ rewind turn {} ({}…)?  [y] yes   [n] no",
            offer.turn_no,
            offer.preview.chars().take(40).collect::<String>(),
        );
        let bar = Paragraph::new(Line::from(Span::styled(
            text,
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        )));
        f.render_widget(bar, area);
        return;
    }
    let mut sweeping = false;
    let (icon, color, word): (String, Color, String) = if state.pending_approval.is_some() {
        ("●".to_string(), th.warning, "awaiting approval".to_string())
    } else if state.pending_clarify.is_some() {
        ("?".to_string(), th.primary, "awaiting input".to_string())
    } else if state.interrupted {
        ("✕".to_string(), th.dim, "interrupted".to_string())
    } else if !state.ready && state.interrupt_armed_at.is_some() {
        ("!".to_string(), th.primary, "esc to interrupt".to_string())
    } else if state.ready
        && state.interrupt_armed_at.is_some()
        && state.rewind_candidate().is_some()
    {
        (
            "↩".to_string(),
            th.primary,
            "esc again: rewind?".to_string(),
        )
    } else if state.ready {
        ("✓".to_string(), th.success, "ready".to_string())
    } else {
        // Traveling highlight through the dynamic status string while the
        // turn is live: a soft purplish light sweeping left→right across
        // its characters, looping until the turn completes. No spinner,
        // no blinking, no layout change.
        sweeping = true;
        (String::new(), th.dim, String::new())
    };
    // Vim mode rides along in the status word; absent when vim is off.
    let vim_prefix: Option<String> = state.vim.status_label().map(|m| format!("{m} · "));
    let left_text = if sweeping {
        match &vim_prefix {
            Some(p) => format!("{p}{}", state.sweep.text()),
            None => state.sweep.text().to_string(),
        }
    } else {
        let word = match &vim_prefix {
            Some(p) => format!("{p}{word}"),
            None => word,
        };
        format!("{icon} {word}")
    };
    // Left-side spans: the sweep renders one span per character with a
    // smooth falloff around the sweep position; every other state is a
    // single bold span.
    let left_spans: Vec<Span> = if sweeping {
        let mut v: Vec<Span> = Vec::new();
        if let Some(p) = &vim_prefix {
            v.push(Span::styled(p.clone(), Style::default().fg(th.dim)));
        }
        v.extend(state.sweep.render(th.dim, activity::SWEEP_PEAK));
        v
    } else {
        vec![Span::styled(
            left_text.clone(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )]
    };
    let live = state.tokens_used + state.turn_estimate;
    let pct = if state.tokens_max > 0 {
        format!("{:.0}%", 100.0 * live as f64 / state.tokens_max as f64)
    } else {
        "—".to_string()
    };
    let left = left_text;
    let right = format!(
        "{} ({}) | / commands",
        statusbar::fmt_count(live as u64),
        pct
    );
    let max = area.width as usize;
    // Shed the right side first on narrow terminals; the state always fits.
    let shed = max > 0 && left.chars().count() + 3 + right.chars().count() > max;
    let line = if shed {
        // Narrow: clip the plain text. The sweep is decorative — static
        // dim text survives the shed.
        let mut text = left.clone();
        if text.chars().count() > max {
            text = text.chars().take(max.saturating_sub(1)).collect();
        }
        Line::from(vec![Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )])
    } else {
        // Right-align the right side: pad between left and right.
        let gap = max.saturating_sub(left.chars().count() + right.chars().count());
        let mut spans = left_spans;
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(right, Style::default().fg(th.dim)));
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line), area);
}

/// Right sidebar: session title + mode tag, and the switchable bottom
/// card (agent view | todo view, `^g`). The whole rect is painted with
/// the panel wash first; column separation is the divider column's bg
/// tint — zero borders, zero box-drawing. The bottom card pins to the
/// bottom (tail-anchored: the latest activity always wins).
fn render_sidebar(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    // Panel wash over the whole rect: this fill IS the column separation.
    f.render_widget(
        Paragraph::new("").style(Style::default().bg(th.panel)),
        area,
    );
    if area.width < 12 || area.height < 6 {
        return;
    }
    let dim = Style::default().fg(th.dim);
    let wash =
        |lines: Vec<Line<'static>>| Paragraph::new(lines).style(Style::default().bg(th.panel));
    let w = area.width as usize;
    let clip = |text: &str| -> String { text.chars().take(w.saturating_sub(2)).collect() };

    // TOP card: the session title, then a subtle dim mode tag
    // ("Build"/"Plan"). The activity status heads the task list in the
    // agent view below instead — it describes the tasks, so it sits
    // with them. No bright boxes — the tag is plain dim text.
    let title = state
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            state
                .session_id
                .rsplit('/')
                .next()
                .unwrap_or(&state.session_id)
                .to_string()
        });
    let mut top: Vec<Line> = Vec::new();
    top.push(Line::from(Span::styled(
        clip(&title),
        Style::default().fg(th.body).add_modifier(Modifier::BOLD),
    )));
    top.push(Line::from(Span::styled(
        if state.plan_mode { "Plan" } else { "Build" },
        dim,
    )));
    // Paint: the top card at the top, the switchable bottom card flowing
    // downward right beneath it (top-anchored: content sits high).
    let h = area.height as usize;
    let top_h = top.len().min(h);
    f.render_widget(
        wash(top.into_iter().take(top_h).collect()),
        Rect::new(area.x, area.y, area.width, top_h as u16),
    );
    let avail = h.saturating_sub(top_h);
    if avail >= 4 {
        let card = Rect::new(area.x, area.y + top_h as u16, area.width, avail as u16);
        match state.sidebar_view {
            SidebarView::Agent => activity::render_agent_card(f, card, state),
            SidebarView::Todo => render_sidebar_todos(f, card, state),
        }
    }
}

/// Sidebar bottom card, Todo view: the same ☒/⊞/☐ card as the
/// transcript, repainted on the panel wash. Top-anchored like the
/// activity card so the list sits high in the column.
fn render_sidebar_todos(f: &mut Frame, area: Rect, state: &TuiState) {
    if area.width < 12 || area.height == 0 {
        return;
    }
    let th = &state.theme;
    let width = area.width as usize;
    let dim = Style::default().fg(th.dim);
    let mut lines: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "Todos",
            Style::default().fg(th.dim).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    if state.todo_items.is_empty() {
        lines.push(Line::from(Span::styled("No to-dos — /todos to plan", dim)));
    } else {
        for line in crate::todo_card::todo_card_lines(&state.todo_items) {
            lines.push(clip_line_spans(line, width));
        }
    }
    let avail = area.height as usize;
    let shown: Vec<Line> = if lines.len() <= avail {
        lines
    } else {
        let mut it = lines.into_iter();
        let h0 = it.next();
        let h1 = it.next();
        let rest: Vec<Line> = it.collect();
        let keep = avail.saturating_sub(2);
        let skip = rest.len().saturating_sub(keep);
        h0.into_iter()
            .chain(h1)
            .chain(rest.into_iter().skip(skip))
            .collect()
    };
    let h = shown.len().min(avail);
    if h == 0 {
        return;
    }
    // Top-anchored like the activity card: the checklist sits high.
    f.render_widget(
        Paragraph::new(shown.into_iter().take(h).collect::<Vec<_>>())
            .style(Style::default().bg(th.panel)),
        Rect::new(area.x, area.y, area.width, h as u16),
    );
}

/// Rebuild a [`Line`] as `'static`, clipping its spans to `max` chars.
fn clip_line_spans(line: Line, max: usize) -> Line<'static> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for sp in line.spans {
        if used >= max {
            break;
        }
        let text: String = sp.content.chars().take(max - used).collect();
        used += text.chars().count();
        out.push(Span::styled(text, sp.style));
        if used >= max {
            break;
        }
    }
    Line::from(out)
}

/// `12.3s`, `850ms` — compact duration for the Activity card.
fn fmt_dur(d: Duration) -> String {
    if d.as_secs() >= 1 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{}ms", d.as_millis())
    }
}

/// Umar's brand purple gradient, top to bottom: #7223FF → #4B00CD → #0C0046.
const WORDMARK_TOP: (u8, u8, u8) = (114, 35, 255);
const WORDMARK_MID: (u8, u8, u8) = (75, 0, 205);
const WORDMARK_BOT: (u8, u8, u8) = (12, 0, 70);

/// Chunky 5-row PANTHEON wordmark, 47 columns wide.
const WORDMARK: [&str; 5] = [
    "█████  ███  █   █ █████ █   █ █████  ███  █   █",
    "█   █ █   █ ██  █   █   █   █ █     █   █ ██  █",
    "█████ █████ █ █ █   █   █████ ████  █   █ █ █ █",
    "█     █   █ █  ██   █   █   █ █     █   █ █  ██",
    "█     █   █ █   █   █   █   █ █████  ███  █   █",
];

/// Wordmark row color: vertical gradient TOP → MID → BOT.
fn wordmark_color(row: usize) -> Color {
    let t = (row as f64 / 4.0).clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8, t: f64| (a as f64 + (b as f64 - a as f64) * t).round() as u8;
    let (r, g, b) = if t <= 0.5 {
        let u = t * 2.0;
        (
            lerp(WORDMARK_TOP.0, WORDMARK_MID.0, u),
            lerp(WORDMARK_TOP.1, WORDMARK_MID.1, u),
            lerp(WORDMARK_TOP.2, WORDMARK_MID.2, u),
        )
    } else {
        let u = (t - 0.5) * 2.0;
        (
            lerp(WORDMARK_MID.0, WORDMARK_BOT.0, u),
            lerp(WORDMARK_MID.1, WORDMARK_BOT.1, u),
            lerp(WORDMARK_MID.2, WORDMARK_BOT.2, u),
        )
    };
    Color::Rgb(r, g, b)
}

/// Center `line` in `width` columns (pads left; over-wide lines pass
/// through unpadded and clip as usual).
fn centered_line(line: Line<'static>, width: usize) -> Line<'static> {
    let w = line.width();
    if w >= width {
        return line;
    }
    let mut spans = vec![Span::raw(" ".repeat((width - w) / 2))];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Refresh the splash/sidebar MCP segment from the live manager:
/// server names with their current status.
fn refresh_mcp_segment(state: &mut TuiState, session: &Session) {
    state.mcp_servers = session
        .mcp_manager
        .health()
        .iter()
        .map(|h| {
            let status = match h.status {
                pantheon_mcp::manager::ServerStatus::Ready => "ready",
                pantheon_mcp::manager::ServerStatus::Connecting => "connecting",
                pantheon_mcp::manager::ServerStatus::Failed => "failed",
                pantheon_mcp::manager::ServerStatus::Backoff => "backoff",
                pantheon_mcp::manager::ServerStatus::Unapproved => "unapproved",
                pantheon_mcp::manager::ServerStatus::Disabled => "disabled",
            };
            (h.name.clone(), status.to_string())
        })
        .collect();
}

/// Pure health-alert builder: failing MCP servers, a dead gateway
/// service, a stuck gateway outbox, and overdue/missed scheduled jobs.
/// Kept pure so tests pin the alert rules without touching the system.
fn health_alerts(
    mcp_servers: &[(String, String)],
    gw_installed: bool,
    gw_active: bool,
    outbox_pending: usize,
    jobs: &[pantheon_scheduler::ScheduledJob],
    now_ms: i64,
) -> Vec<String> {
    let mut alerts = Vec::new();
    for (name, status) in mcp_servers {
        match status.as_str() {
            "failed" => alerts.push(format!("MCP \"{name}\" failed")),
            "backoff" => alerts.push(format!("MCP \"{name}\" backing off")),
            "unapproved" => alerts.push(format!("MCP \"{name}\" needs approval")),
            _ => {}
        }
    }
    if gw_installed && !gw_active {
        alerts.push("gateway installed but not running".to_string());
    }
    if outbox_pending > 0 {
        alerts.push(format!("gateway outbox: {outbox_pending} queued"));
    }
    for sj in jobs {
        let job = &sj.job;
        if job.paused {
            continue;
        }
        let short: String = job.id.chars().take(24).collect();
        match &job.kind {
            pantheon_scheduler::ScheduleKind::Interval { every_ms } => {
                if let Some(last) = sj.last_run {
                    if now_ms - last > (*every_ms as i64) * 2 {
                        alerts.push(format!("schedule \"{short}\" overdue"));
                    }
                }
            }
            pantheon_scheduler::ScheduleKind::OneShot { at_ms } => {
                if sj.last_run.is_none() && *at_ms < now_ms {
                    alerts.push(format!("schedule \"{short}\" missed"));
                }
            }
            pantheon_scheduler::ScheduleKind::Cron { .. } => {}
            // Webhook jobs fire on inbound calls; no clock-based overdue.
            pantheon_scheduler::ScheduleKind::Webhook { .. } => {}
        }
    }
    alerts
}

/// Refresh the cached health alerts (at most every 30s). Called from
/// the render path so the bottom strip stays current without
/// per-frame probes.
fn refresh_health(state: &mut TuiState) {
    let due = state
        .health_checked_at
        .map(|t| t.elapsed().as_secs() >= 30)
        .unwrap_or(true);
    if !due {
        return;
    }
    state.health_checked_at = Some(std::time::Instant::now());
    let gw = crate::gateway::gateway_status();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let jobs = crate::schedule::load_jobs_public(&crate::terminal::data_dir());
    state.health_alerts = health_alerts(
        &state.mcp_servers,
        gw.installed,
        gw.active,
        gw.outbox_pending,
        &jobs,
        now_ms,
    );
}

/// The splash's bottom line: `● N MCP /mcp · M plugins /plugins` left,
/// version right.
fn render_splash_bottom(f: &mut Frame, area: Rect, state: &TuiState) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let th = &state.theme;
    let w = area.width as usize;
    let left = format!(
        "● {} MCP /mcp · {} plugin{} /plugins",
        state.mcp_servers.len(),
        state.plugin_count,
        if state.plugin_count == 1 { "" } else { "s" },
    );
    let right = format!("v{}", env!("CARGO_PKG_VERSION"));
    // The trailing space after the left segment counts as a cell.
    let gap = w.saturating_sub(left.chars().count() + 1 + right.chars().count());
    let brow = Rect::new(area.x, area.y + area.height - 1, area.width, 1);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{left} "), Style::default().fg(th.success)),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, Style::default().fg(th.dim)),
        ])),
        brow,
    );
}

/// New-session splash: big gradient PANTHEON wordmark, update hint,
/// centered composer input, and a bottom line with plugin/MCP counts +
/// version. Rendered while the transcript is empty (fresh or /clear'ed
/// session). The centered input mirrors `state.input` — the same draft
/// the composer edits — so typing works before the first block exists.
fn render_splash(f: &mut Frame, area: Rect, state: &TuiState) {
    let th = &state.theme;
    let w = area.width as usize;
    let h = area.height as usize;

    if w < 30 || h < 10 {
        // Tiny terminal: wordmark text only.
        let mut lines = vec![Line::from(""), Line::from("")];
        lines.push(centered_line(
            Line::from(Span::styled(
                "PANTHEON",
                Style::default().fg(th.heading).add_modifier(Modifier::BOLD),
            )),
            w,
        ));
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    let big = w >= 55;
    let mut head: Vec<Line> = Vec::new();
    if big {
        for (r, row) in WORDMARK.iter().enumerate() {
            head.push(centered_line(
                Line::from(Span::styled(
                    row.to_string(),
                    Style::default()
                        .fg(wordmark_color(r))
                        .add_modifier(Modifier::BOLD),
                )),
                w,
            ));
        }
    } else {
        head.push(centered_line(
            Line::from(Span::styled(
                "PANTHEON",
                Style::default().fg(th.heading).add_modifier(Modifier::BOLD),
            )),
            w,
        ));
    }
    head.push(Line::from(""));
    // Update hint, opencode-style; blank when up to date.
    if let Some(tag) = &state.update_hint {
        head.push(centered_line(
            Line::from(Span::styled(
                format!("pantheon update to install {tag}"),
                Style::default().fg(th.emphasis),
            )),
            w,
        ));
    } else {
        head.push(Line::from(""));
    }
    head.push(Line::from(""));

    // Input zone: centered composer box (or the reflect card while
    // proposals are pending — it owns the keyboard then, like the
    // bottom row does), plus the dim key hint under it, opencode-style.
    let box_w = 64.min(w.saturating_sub(8)).max(20) as u16;
    let zone_rows: usize = if state.pending_reflect.is_some() {
        9
    } else {
        5 // 4-row composer box + hint line
    };
    let top_pad = h.saturating_sub(head.len() + zone_rows + 1) / 2;

    let mut lines: Vec<Line> = vec![Line::from(""); top_pad];
    let head_len = head.len();
    lines.extend(head);
    f.render_widget(Paragraph::new(lines), area);

    // The input zone sits right below the head block.
    let zone_y = area.y + top_pad as u16 + head_len as u16;
    if state.pending_reflect.is_some() {
        let cw = (72usize).min(w.saturating_sub(4)) as u16;
        let ch = (zone_rows as u16).min(area.y + area.height.saturating_sub(zone_y + 1));
        if cw >= 20 && ch >= 5 {
            let card = Rect::new(area.x + area.width.saturating_sub(cw) / 2, zone_y, cw, ch);
            f.render_widget(Clear, card);
            render_reflect_card(f, card, state);
        }
    } else {
        let box_x = area.x + area.width.saturating_sub(box_w) / 2;
        render_composer_box(
            f,
            Rect::new(box_x, zone_y, box_w, 4.min(area.height)),
            state,
        );
        // Dim key hint under the box, right-aligned to its edge.
        let hint = "shift+tab prev tab   / commands";
        let hint_w = (hint.chars().count() as u16).min(box_w);
        let hint_x = box_x + box_w.saturating_sub(hint_w);
        if zone_y + 4 < area.y + area.height {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(th.dim)))),
                Rect::new(hint_x, zone_y + 4, hint_w, 1),
            );
        }
    }
}

/// Draw the conversation transcript with visual blocks per event type.
///
/// No border, no title: the transcript is the screen. Consumes
/// `jump_to_block` (set by the timeline's Enter): the viewport pins so
/// the target turn's first block sits at the top. Read-only otherwise —
/// jumping never mutates the transcript. When the user scrolled up, a
/// dim `Jump to latest ↓` marker anchors to the bottom-right.
fn render_transcript(f: &mut Frame, area: Rect, state: &mut TuiState) {
    // Placements are rebuilt every frame: blocks only ever append, but a
    // rewind can drop them, and scroll changes every row.
    state.img.placements.clear();
    let th = &state.theme;
    // The new-session splash owns the empty state (see render_main); the
    // transcript never renders an empty welcome of its own.
    if state.blocks.is_empty() {
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
        // Live session token count for the tool-card tickers; copied
        // up front so the blocks borrow below stays shared.
        let live_tokens = state.tokens_used + state.turn_estimate;
        for (i, block) in state.blocks.iter().enumerate() {
            block_starts.push(lines.len());
            let is_last = i + 1 == block_count;
            render_block(
                &mut lines,
                block,
                is_last,
                interrupted,
                th,
                img,
                area.width,
                live_tokens,
            );
            // Airy, opencode-style: messages breathe with two blank
            // lines after them; compact cards keep one.
            lines.push(Line::from(""));
            if matches!(
                block.kind,
                BlockKind::UserMessage(_) | BlockKind::AssistantMessage(_)
            ) {
                lines.push(Line::from(""));
            }
        }
    }

    // Todo tail status: a single dim line, not the full checklist — the
    // checklist's primary home is the sidebar's todo view (`^g`), and in
    // narrow mode the live card renders inline in the transcript instead.
    // Synced every frame by `refresh_todos`.
    if !state.todo_items.is_empty() && state.sidebar_visible() {
        use crate::todo_card::TodoStatus as CardStatus;
        let done = state
            .todo_items
            .iter()
            .filter(|i| i.status == CardStatus::Completed)
            .count();
        let total = state.todo_items.len();
        lines.push(Line::from(Span::styled(
            format!("Todos {done}/{total} done · see sidebar (^g)"),
            Style::default().fg(state.theme.dim),
        )));
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
    // Viewport geometry for the post-draw image painter: `off` lines are
    // scrolled off the top, content starts at the area's first row (no
    // border since the transcript went borderless).
    state.img.view = Some(crate::richtext::TranscriptView {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height,
        scroll_off: off as usize,
    });
    let text = Text::from(lines);
    let para = Paragraph::new(text).scroll((off, 0));
    f.render_widget(para, area);
    // Scrolled up: offer the way back. Bottom-right, dim, one row.
    if state.scroll_offset > 0 && area.height > 0 && area.width > 20 {
        let label = "Jump to latest ↓";
        let w = (label.chars().count() as u16 + 2).min(area.width);
        let hint_area = Rect::new(
            area.x + area.width.saturating_sub(w),
            area.y + area.height - 1,
            w,
            1,
        );
        f.render_widget(Clear, hint_area);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {label} "),
                Style::default().fg(th.dim),
            ))),
            hint_area,
        );
    }
}

/// Render message text with rich features: text segments go through
/// [`crate::markdown`] (headings, lists, tables, fences, inline styles),
/// and image references become a placeholder line plus reserved rows the
/// post-draw painter fills with a thumbnail.
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
                // Markdown rendering (headings, lists, tables, fences,
                // inline styles) lives in `crate::markdown`; this only
                // routes each text segment there.
                lines.extend(crate::markdown::render_markdown(&t, th));
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
                lines.push(Line::from(Span::styled(label, dim)));
                // Spacer rows the thumbnail paints over (label stays visible
                // above it).
                for _ in 1..rows {
                    lines.push(Line::from(""));
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
    live_tokens: u32,
) {
    match &block.kind {
        BlockKind::UserMessage(text) => {
            // Colored sender label, no chrome: whitespace + color are
            // the only separators, opencode-style.
            lines.push(Line::from(Span::styled(
                "You",
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )));
            render_rich_text(lines, text, th, img, term_width);
        }
        BlockKind::AssistantMessage(text) => {
            lines.push(Line::from(Span::styled(
                "Pantheon",
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
                format!("{glyph} {verb} · “{label}” · bg-{task_id}"),
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
                    "◇ reasoning",
                    Style::default()
                        .fg(th.dim)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                )));
                for line in text.lines().take(40) {
                    lines.push(Line::from(Span::styled(line.to_string(), dim)));
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
        BlockKind::ToolCall {
            name,
            args,
            ok,
            call_id: _,
            started,
            duration,
            error,
            tokens,
        } => {
            let (glyph, col) = match ok {
                // A tool still marked running after an interrupt was stopped
                // from the outside: show that honestly, in dim, instead of
                // spinning or going amber (amber is reserved for approvals).
                None if interrupted => ("■", th.dim),
                None => (icon::RUNNING, th.success),
                Some(true) => (icon::SUCCESS, th.success),
                Some(false) => (icon::FAILURE, th.failure),
            };
            // Minimal single-line header: `⚙ Execute · 0.4s · 154.2k ✓`.
            // The token count ticks live while the call runs and freezes
            // at completion; cards completed before tracking simply omit
            // it. Tool names render presentable, never raw registry ids.
            let display = activity::tool_display_name(name);
            let mut timing = match duration {
                Some(d) => format!("{:.1}s", d.as_secs_f64()),
                None => match started {
                    Some(_) => "…".to_string(),
                    None => String::new(),
                },
            };
            if !timing.is_empty() {
                timing = format!(" · {timing}");
            }
            let count = match (tokens, ok) {
                (Some(t), _) => Some(statusbar::fmt_count(*t as u64)),
                (None, None) => Some(statusbar::fmt_count(live_tokens as u64)),
                (None, Some(_)) => None,
            };
            let tool_dim = Style::default().fg(th.dim);
            let mut header = vec![
                Span::styled(format!("{} {display}", icon::TOOL), tool_dim),
                Span::styled(timing, tool_dim),
            ];
            if let Some(c) = count {
                header.push(Span::styled(format!(" · {c}"), tool_dim));
            }
            header.push(Span::styled(format!(" {glyph}"), Style::default().fg(col)));
            lines.push(Line::from(header));
            if !args.is_empty() {
                for line in args.lines().take(6) {
                    lines.push(Line::from(Span::styled(format!("  {line}"), tool_dim)));
                }
            }
            // Failure detail: the worker records the error text on the
            // card; the card renders it calmly under the header (tools
            // stay calm — only approvals go amber).
            if let Some(err) = error {
                if !err.is_empty() {
                    for line in err.lines().take(10) {
                        lines.push(Line::from(Span::styled(
                            format!("  ✕ {line}"),
                            Style::default().fg(th.failure),
                        )));
                    }
                }
            }
        }
        BlockKind::Swarm { agents, task } => {
            lines.push(Line::from(Span::styled(
                format!("{} Swarm · {agents} agents", icon::AGENT),
                Style::default().fg(th.primary),
            )));
            lines.push(Line::from(format!("  {task}")));
        }
        BlockKind::Status(text) => {
            lines.push(Line::from(Span::styled(
                format!("… {text}"),
                Style::default().fg(th.dim),
            )));
        }
        BlockKind::Todos(items) => {
            // The opencode-style card, inline: same lines the sidebar's
            // todo view renders, clipped to the transcript width.
            for line in crate::todo_card::todo_card_lines(items) {
                lines.push(clip_line_spans(line, term_width as usize));
            }
        }
        BlockKind::ProviderError {
            provider,
            model,
            kind,
            message,
            fallback,
        } => {
            // Rate limits get their own amber state: quota trouble must
            // never read as a dead provider. Everything else is red.
            let (glyph, color, head) = if kind.is_rate_limited() {
                (icon::WARN, th.warning, "rate limited")
            } else {
                (icon::FAILURE, th.failure, "provider error")
            };
            let mut head_line = format!("{glyph} {head}");
            if !provider.is_empty() || !model.is_empty() {
                head_line.push_str(&format!(" · {provider}/{model}"));
            }
            if kind.is_rate_limited() {
                if let Some(secs) = retry_after_secs_from_cause(message) {
                    head_line.push_str(&format!(" · retry in {secs}s"));
                }
            }
            lines.push(Line::from(Span::styled(
                head_line,
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            )));
            if !message.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {kind} · {message}"),
                    Style::default().fg(th.dim),
                )));
            }
            let outcome = match fallback {
                FallbackOutcome::Engaged {
                    provider: p,
                    model: m,
                } => {
                    format!("fallback engaged → {p}/{m} (now active)")
                }
                FallbackOutcome::Exhausted => "no fallback available — chain exhausted".into(),
                FallbackOutcome::NotRetryable => "not retryable — no fallback attempted".into(),
                FallbackOutcome::Retrying {
                    attempt,
                    max_attempts,
                    wait_secs,
                } => format!("retrying {attempt}/{max_attempts} — waiting {wait_secs}s"),
                FallbackOutcome::Recovered {
                    attempt,
                    max_attempts,
                } => format!("recovered on retry {attempt}/{max_attempts}"),
            };
            lines.push(Line::from(Span::styled(
                format!("  {outcome}"),
                Style::default().fg(th.dim),
            )));
        }
        BlockKind::Diff(diff_lines) => {
            lines.extend(crate::diffview::render_diff_lines(diff_lines, th));
        }
        BlockKind::Approval {
            tool,
            sentence,
            target,
            decision,
        } => {
            // Decision point: resolved cards collapse to a dim one-line
            // marker; the pending card is the amber surface. The tool
            // intent above it stays visually calm.
            match decision {
                Some(true) => lines.push(Line::from(Span::styled(
                    "● approved",
                    Style::default().fg(th.dim),
                ))),
                Some(false) => lines.push(Line::from(Span::styled(
                    "● denied",
                    Style::default().fg(th.dim),
                ))),
                None => {
                    lines.push(Line::from(Span::styled(
                        format!("⚠ Permission required · {tool}"),
                        Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(Span::styled(
                        sentence.clone(),
                        Style::default().fg(th.body),
                    )));
                    if !target.is_empty() {
                        lines.push(Line::from(Span::styled(
                            target.clone(),
                            Style::default().fg(th.primary),
                        )));
                    }
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled(
                            "[a]",
                            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" Approve   ", Style::default().fg(th.dim)),
                        Span::styled(
                            "[d]",
                            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" Deny   ", Style::default().fg(th.dim)),
                        Span::styled("[esc]", Style::default().fg(th.dim)),
                        Span::styled(" cancel", Style::default().fg(th.dim)),
                    ]));
                }
            }
        }
        BlockKind::Clarify {
            question,
            options,
            answered,
        } => {
            // Cyan `?`, never amber. Answered cards collapse; pending
            // ones list numbered options plus free text.
            match answered {
                Some(answer) => {
                    let short: String = answer.chars().take(72).collect();
                    lines.push(Line::from(Span::styled(
                        format!("● answered: {short}"),
                        Style::default().fg(th.dim),
                    )))
                }
                None => {
                    lines.push(Line::from(Span::styled(
                        "? Clarify",
                        Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
                    )));
                    for qline in question.lines().take(6) {
                        lines.push(Line::from(Span::styled(
                            qline.to_string(),
                            Style::default().fg(th.body),
                        )));
                    }
                    for (i, opt) in options.iter().enumerate().take(9) {
                        let short: String = opt.chars().take(64).collect();
                        lines.push(Line::from(vec![
                            Span::styled("  ", Style::default().fg(th.dim)),
                            Span::styled(
                                format!("{}", i + 1),
                                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!("  {short}"), Style::default().fg(th.body)),
                        ]));
                    }
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        "  1-9 answer · type + enter for free text · esc cancel",
                        Style::default().fg(th.dim),
                    )));
                }
            }
        }
        BlockKind::Command {
            cmd,
            lines: cmd_lines,
            failed,
        } => {
            // Every slash command echoes, then renders its result lines.
            // `/help` renders as a two-column table; error lines inside
            // any block render red, and a failed command echoes red.
            let echo_color = if *failed { th.failure } else { th.primary };
            lines.push(Line::from(vec![
                Span::styled("❯ ", Style::default().fg(th.dim)),
                Span::styled(
                    cmd.clone(),
                    Style::default().fg(echo_color).add_modifier(Modifier::BOLD),
                ),
            ]));
            if *cmd == "/help" {
                render_help_table(lines, cmd_lines, th);
            } else {
                for l in cmd_lines.iter().take(60) {
                    let t = l.trim_start();
                    let is_err = t.starts_with('✗')
                        || t.starts_with("error:")
                        || t.starts_with("Error:")
                        || t.starts_with("failed");
                    if is_err {
                        lines.push(Line::from(Span::styled(
                            format!("  {l}"),
                            Style::default().fg(th.failure),
                        )));
                    } else {
                        lines.push(Line::from(Span::styled(
                            format!("  {l}"),
                            Style::default().fg(th.body),
                        )));
                    }
                }
            }
            lines.push(Line::from(""));
        }
        BlockKind::Confirm {
            label,
            sentence,
            decided,
        } => {
            // Same decision pattern as the approval card, for
            // destructive slash commands.
            match decided {
                Some(true) => lines.push(Line::from(Span::styled(
                    "● confirmed",
                    Style::default().fg(th.dim),
                ))),
                Some(false) => lines.push(Line::from(Span::styled(
                    "● dismissed",
                    Style::default().fg(th.dim),
                ))),
                None => {
                    lines.push(Line::from(Span::styled(
                        format!("⚠ {label}"),
                        Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(Span::styled(
                        sentence.clone(),
                        Style::default().fg(th.body),
                    )));
                    lines.push(Line::from(""));
                    lines.push(Line::from(vec![
                        Span::styled(
                            "[a]",
                            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" Confirm   ", Style::default().fg(th.dim)),
                        Span::styled(
                            "[d]",
                            Style::default().fg(th.warning).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(" Dismiss   ", Style::default().fg(th.dim)),
                        Span::styled("[esc]", Style::default().fg(th.dim)),
                        Span::styled(" dismiss", Style::default().fg(th.dim)),
                    ]));
                }
            }
        }
    }
}

/// Two-column rendering of `/help` output: `  /cmd args   description`
/// rows become `cmd` (cyan) + `description` (dim). Non-command lines
/// render dim as-is.
fn render_help_table(lines: &mut Vec<Line>, cmd_lines: &[String], th: &theme::Theme) {
    let dim = Style::default().fg(th.dim);
    for raw in cmd_lines {
        let t = raw.trim_start();
        if !t.starts_with('/') {
            if !t.is_empty() {
                lines.push(Line::from(Span::styled(format!("  {t}"), dim)));
            }
            continue;
        }
        // Split command from description at the first run of 2+ spaces.
        let bytes = t.as_bytes();
        let mut split = t.len();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b' ' {
                let mut j = i;
                while j < bytes.len() && bytes[j] == b' ' {
                    j += 1;
                }
                if j - i >= 2 {
                    split = i;
                    break;
                }
                i = j;
            } else {
                i += 1;
            }
        }
        let (cmd_part, desc_part) = t.split_at(split);
        lines.push(Line::from(vec![
            Span::styled(format!("  {cmd_part:<22}"), Style::default().fg(th.primary)),
            Span::styled(desc_part.trim_start().to_string(), dim),
        ]));
    }
}

// The run id owning the model stream on the current worker thread.
// `spawn_turn` sets it when the thread starts; the `on_event` closure
// stamps every streaming event with it so the loop can route background
// streams away from the visible transcript.
std::thread_local! {
    static STREAM_RUN: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Record the run id owning this worker thread's model stream. Called
/// once at the top of `spawn_turn`'s thread body.
fn set_stream_run(run_id: &str) {
    STREAM_RUN.with(|c| *c.borrow_mut() = Some(run_id.to_string()));
}

/// The run id owning this thread's model stream, if one was recorded.
fn stream_run() -> Option<String> {
    STREAM_RUN.with(|c| c.borrow().clone())
}

/// Internal event types that flow from the worker thread to the TUI loop.
enum TuiEvent {
    /// A model streaming event. `run_id` is the stream's owner, tagged by
    /// the worker thread at send time (via `set_stream_run`), so a model
    /// event from a background tab can never land in the visible session's
    /// transcript. `None` means "no stream owner recorded" — the loop
    /// treats it as visible only when it also started on the visible run.
    Model {
        run_id: Option<String>,
        ev: pantheon_providers::model_event::ModelEvent,
    },
    Runtime(pantheon_api::events::Event),
    /// A turn finished for `run_id`. The id lets the loop tell a
    /// background-tab completion apart from the visible session's, so only
    /// the former raises a notification.
    TurnComplete {
        run_id: String,
    },
    /// The assistant's final text for a turn, rendered into the transcript.
    /// Tagged with the owning run so a background tab's answer never lands
    /// in the visible transcript.
    Answered {
        run_id: String,
        text: String,
    },
    /// A turn failed. Tagged with the owning run so a background failure
    /// badges its tab instead of clobbering the visible session.
    Error {
        run_id: String,
        msg: String,
    },
    /// The run stopped because the user interrupted it (not a failure).
    /// Tagged so an interrupt racing a tab switch lands on the right tab.
    Canceled {
        run_id: String,
    },
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
    /// card appears. `from_command` routes the summary into the
    /// originating command's result block instead of bare status lines,
    /// so late async output is never orphaned. Never touches the main
    /// turn's state.
    ReflectDone {
        summary: String,
        pending: Vec<pantheon_nightly::Proposal>,
        from_command: bool,
    },
    /// The background update probe found a newer release. Sets
    /// `state.update_hint` so the splash offers `pantheon update`.
    UpdateAvailable {
        tag: String,
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
    // Tools screen (`[tools]` in config.toml). Absent section = every
    // group enabled, the pre-Tools-screen default.
    crate::config::apply_tool_enablement(&session, file_cfg.as_ref());

    // Run budgets from `[budget]` in config.toml (max turns, tool calls,
    // delegate depth, token cap). Absent = the runtime defaults; `/set`
    // and `/tokens` retune them live for this session.
    session.set_budget(
        file_cfg
            .as_ref()
            .map(config::config_budget)
            .unwrap_or_default(),
    );
    // The configured token cap, kept apart from the live budget: `/tokens`
    // overwrites `budget.max_tokens` for the session, and `/tokens off`
    // must fall back to this configured value rather than forget it.
    // (0 = unset, same as `resolve_budget_section`.)
    session.set_budget_max_tokens(
        file_cfg
            .as_ref()
            .and_then(|c| c.budget.as_ref())
            .and_then(|b| b.max_tokens)
            .filter(|&v| v > 0),
    );

    // Tacit temporal awareness (`[temporal]` in config.toml). Absent
    // section = the runtime defaults (enabled, 2h gap, system timezone).
    if let Some(t) = file_cfg.as_ref().and_then(|c| c.temporal.clone()) {
        session.set_temporal_config(pantheon_api::temporal::TemporalConfig::from(&t));
    }

    // Web access (`[browser]` / `[websearch]` in config.toml). Absent
    // sections = the runtime defaults (both enabled; web_search only
    // registers when an API key resolves via the secrets broker).
    session.set_browser_config(
        file_cfg
            .as_ref()
            .and_then(|c| c.browser.clone())
            .map(|s| config::resolve_browser_section(&s))
            .unwrap_or_default(),
    );
    session.set_websearch_config(
        file_cfg
            .as_ref()
            .and_then(|c| c.websearch.clone())
            .map(|s| config::resolve_websearch_section(&s))
            .unwrap_or_default(),
    );

    // Speech-to-text (`[stt]` in config.toml) for the video fallback's
    // audio leg. Absent = frames only, noted honestly in the summary.
    session.set_stt_section(file_cfg.as_ref().and_then(|c| c.stt.clone()));

    // MCP servers (`[mcp]` in config.toml + imported declarations under
    // `<data_dir>/mcp/`). The config section is primary; declarations
    // fill in names it does not define. The launcher stays off until
    // either exists, and nothing runs before an explicit `mcp approve`.
    {
        let declarations = pantheon_migration::read_mcp_declarations(&data_dir);
        session.set_mcp_config(config::resolve_mcp_section(
            file_cfg.as_ref().and_then(|c| c.mcp.as_ref()),
            &declarations,
        ));
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
            // Stamp the owning run at send time: model events can only
            // belong to the visible session when the stream does.
            let _ = tx.send(TuiEvent::Model {
                run_id: stream_run(),
                ev,
            });
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

    // Restore the saved theme before the first frame: load_theme_name
    // falls back to the default on missing config or unknown names.
    let saved_theme = theme::load_theme_name(&crate::terminal::data_dir());
    state.set_theme(&saved_theme);
    // Restore saved vim mode the same way: absent config = off.
    state.vim.enabled = vim::load_vim(&crate::terminal::data_dir());
    // Prime the git segment so the first frame already shows it.
    state.refresh_git();

    // Status line / splash metadata: agent variant, reasoning effort, MCP
    // servers, and installed-skill ("plugin") count. Read once here;
    // `/mcp` and `/reasoning` refresh their own slices live.
    state.variant = file_cfg
        .as_ref()
        .and_then(|c| c.resolve_profile(None).ok().flatten())
        .map(|eff| eff.name)
        .unwrap_or_default();
    state.effort = Some(session.reasoning().as_str().to_string());
    refresh_mcp_segment(&mut state, &session);
    {
        let extra_roots: Vec<std::path::PathBuf> = std::env::var("PANTHEON_SKILLS_DIR")
            .map(|v| {
                v.split(':')
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from)
                    .collect()
            })
            .unwrap_or_default();
        let project_root =
            std::env::current_dir().unwrap_or_else(|_| session.supervisor.data_dir().clone());
        state.plugin_count = pantheon_exec::skills::discover_skills_enabled(
            &session.supervisor.data_dir(),
            &project_root,
            &extra_roots,
        )
        .len();
    }
    // Background update probe: sets `update_hint` when a newer release
    // exists, so the splash can offer `/update`. Silent on failure.
    {
        let txu = tx.clone();
        std::thread::spawn(move || {
            if let Some(tag) = crate::update::check_for_update() {
                let _ = txu.send(TuiEvent::UpdateAvailable { tag });
            }
        });
    }

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
    // The opening run is the first open tab, and any decisions it parked
    // (e.g. a previous TUI died mid-approval) hydrate from the ledger so
    // the card renders on the first frame.
    seed_open_tabs(&mut state);
    hydrate_decisions(&mut state, &session, &run_id);
    refresh_tabs(&mut state, &session);

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
        // Tag this thread's model stream with the owning run before any
        // event can flow: the `on_event` closure stamps every streaming
        // event with it so the loop can route background streams away
        // from the visible transcript.
        set_stream_run(&run_id_owned);
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
                    let _ = tx2.send(TuiEvent::Canceled {
                        run_id: run_id_owned.clone(),
                    });
                }
                _ => {
                    let _ = tx2.send(TuiEvent::TurnComplete {
                        run_id: run_id_owned.clone(),
                    });
                }
            },
            Err(e) => {
                let _ = tx2.send(TuiEvent::Error {
                    run_id: run_id_owned.clone(),
                    msg: e.to_string(),
                });
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
        // Tag the stream: bg-task model events belong to the task's run,
        // never to the visible transcript.
        set_stream_run(&run_id);
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
            Ok(pantheon_agent::LoopOutcome::AwaitingInput { question, .. }) => Err(format!(
                "parked on user input: '{question}'; answer with \
                 `pantheon run --taskID {run_id} --input '<answer>'`"
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

/// `/todos` — inspect the session's todo list. Read-only: the agent
/// maintains the list through the `todo` tool (the runtime persists
/// each replacement to the run's ledger); this is the operator's
/// window into it, using the same glyphs as the transcript card.
fn show_todos(state: &mut TuiState, session: &Arc<Session>) {
    use pantheon_api::todo::TodoStatus;
    let items = session.todo_list();
    if items.is_empty() {
        state.add_status("no todos — the agent plans multi-step work with the `todo` tool".into());
        return;
    }
    let remaining = items
        .iter()
        .filter(|i| i.status != TodoStatus::Completed)
        .count();
    state.add_status(format!("working on {remaining} to-dos:"));
    for item in &items {
        let glyph = match item.status {
            TodoStatus::Completed => "☒",
            TodoStatus::InProgress => "⊞",
            TodoStatus::Pending => "☐",
        };
        state.add_status(format!("  {glyph} {}", item.content));
    }
}

/// `/tokens [n|off]` — show or set the per-request output cap: the most
/// tokens the model may emit in one response. Strictly optional: the
/// default is uncapped (`None`), Pantheon never requires it, and
/// `[budget] max_tokens` only sets the session default. The winner is
/// clamped to the session model's known maximum output.
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
            // `/set max_turns` is a missing value, not an unknown key;
            // say which one it is.
            let known = matches!(
                arg.to_lowercase().as_str(),
                "max_turns"
                    | "turns"
                    | "max_tool_calls"
                    | "tool_calls"
                    | "max_delegate_depth"
                    | "delegate_depth"
                    | "depth"
                    | "max_tokens"
                    | "tokens"
            );
            if known {
                state.add_status(format!("usage: /set {arg} <number> — value missing"));
            } else {
                state.add_status(format!(
                    "unknown key: {arg} (max_turns, max_tool_calls, max_delegate_depth, max_tokens)"
                ));
            }
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

/// `/nightly [on|off|status]` — the unified self-improvement pass,
/// OpenClaw-`/dreaming` style. Bare `/nightly` runs a manual one-shot
/// pass on a worker thread (eval-gating and replay can take minutes; the
/// TUI never blocks). `on`/`off` persist the explicit `[nightly] enabled`
/// flag to config.toml (the same flag the dashboard toggle and a manual
/// config edit write); `status` shows the resolved state with the
/// reason, the model-pin presence, and the next scheduled run.
/// `/reflect` and `/consolidate` are kept as aliases: both run the same
/// unified pass.
fn do_nightly(state: &mut TuiState, tx: &std::sync::mpsc::Sender<TuiEvent>, cmd: &str) {
    let data_dir = crate::terminal::data_dir();
    let arg = cmd
        .strip_prefix("/nightly")
        .or_else(|| cmd.strip_prefix("/reflect"))
        .or_else(|| cmd.strip_prefix("/consolidate"))
        .map(str::trim)
        .unwrap_or("");
    match crate::nightly_cli::NightlySub::parse(arg) {
        crate::nightly_cli::NightlySub::On => {
            match crate::nightly_cli::persist_nightly_enabled(&data_dir, true) {
                Ok(()) => {
                    state.add_status("nightly pass: on (persisted to [nightly])".into());
                    // Explicit `true` enables the pass even with no model
                    // pin — say so, and show how to pin one, instead of
                    // pretending a pinned model is in play.
                    if !crate::nightly_cli::nightly_pin_present(&data_dir) {
                        for line in crate::nightly_cli::pin_guidance().lines() {
                            state.add_status(line.to_string());
                        }
                    }
                }
                Err(e) => state.add_status(format!("nightly: {e}")),
            }
        }
        crate::nightly_cli::NightlySub::Off => {
            match crate::nightly_cli::persist_nightly_enabled(&data_dir, false) {
                Ok(()) => state.add_status("nightly pass: off (persisted to [nightly])".into()),
                Err(e) => state.add_status(format!("nightly: {e}")),
            }
        }
        crate::nightly_cli::NightlySub::Status => {
            for line in crate::nightly_cli::status_line(&data_dir).lines() {
                state.add_status(line.to_string());
            }
        }
        crate::nightly_cli::NightlySub::Run { dry_run } => {
            if state.reflect_running {
                state.add_status("nightly pass already running".into());
                return;
            }
            state.reflect_running = true;
            state.add_status(if dry_run {
                "nightly dry run started (background)".into()
            } else {
                "nightly pass started (background)".into()
            });
            let tx2 = tx.clone();
            let dd = data_dir;
            std::thread::spawn(
                move || match crate::nightly_cli::run_one_pass(&dd, dry_run) {
                    Ok(out) => {
                        let summary = crate::nightly_cli::summarize_pass(&out);
                        let pending = pantheon_nightly::load_pending(&dd).unwrap_or_default();
                        let _ = tx2.send(TuiEvent::ReflectDone {
                            summary,
                            pending,
                            from_command: true,
                        });
                    }
                    Err(e) => {
                        // Route failures through ReflectDone, not the generic
                        // error event: the ReflectDone arm resets
                        // `reflect_running`, so a failed pass never wedges the
                        // trigger permanently on.
                        let _ = tx2.send(TuiEvent::ReflectDone {
                            summary: format!("nightly pass failed: {e}"),
                            pending: Vec::new(),
                            from_command: true,
                        });
                    }
                },
            );
        }
        crate::nightly_cli::NightlySub::Unknown(other) => state.add_status(format!(
            "usage: /nightly [on|off|status|--dry-run] — unknown: {other}"
        )),
    }
}

/// Trigger rule for an automatic nightly pass: fire on every
/// `auto_turns`-th completed turn, only when the loop is on and no pass
/// is already running. Pure over its inputs so the rule is unit-testable
/// without a TUI (`auto_turns = 0` disables the trigger entirely).
fn auto_reflect_due(turns_completed: u32, enabled: bool, auto_turns: u32, running: bool) -> bool {
    enabled
        && !running
        && auto_turns > 0
        && turns_completed > 0
        && turns_completed.is_multiple_of(auto_turns)
}

/// Maybe fire an automatic nightly pass after a completed turn. Reads
/// fresh config so a mid-session `/nightly on` takes effect without a
/// restart. The pass runs on a worker thread; results arrive as
/// `ReflectDone` and never touch the turn machinery.
fn maybe_auto_reflect(state: &mut TuiState, tx: &std::sync::mpsc::Sender<TuiEvent>) {
    let data_dir = crate::terminal::data_dir();
    let (enabled, auto_turns) = crate::nightly_cli::nightly_state(&data_dir);
    if !auto_reflect_due(
        state.turns_completed,
        enabled,
        auto_turns,
        state.reflect_running,
    ) {
        return;
    }
    state.reflect_running = true;
    state.add_status("automatic nightly pass started (background)".into());
    let tx2 = tx.clone();
    std::thread::spawn(
        move || match crate::nightly_cli::run_one_pass(&data_dir, false) {
            Ok(out) => {
                let summary = format!("automatic {}", crate::nightly_cli::summarize_pass(&out));
                let pending = pantheon_nightly::load_pending(&data_dir).unwrap_or_default();
                let _ = tx2.send(TuiEvent::ReflectDone {
                    summary,
                    pending,
                    from_command: false,
                });
            }
            Err(e) => {
                let _ = tx2.send(TuiEvent::ReflectDone {
                    summary: format!("automatic nightly pass failed: {e}"),
                    pending: Vec::new(),
                    from_command: false,
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

/// Resume a parked run after a decision (approval granted / clarify
/// answered): `chat_turn` with an empty message rebuilds from the ledger
/// and continues the loop on a worker thread. Shared by the approval and
/// clarify resolve paths so both resume identically.
fn resume_parked_turn(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    run: &str,
) {
    state.ready = false;
    state.active_run = Some(run.to_string());
    let tx3 = tx.clone();
    let run3 = run.to_string();
    let sess3 = session.clone();
    std::thread::spawn(move || {
        set_stream_run(&run3);
        match sess3.chat_turn(&run3, "", "") {
            Ok(outcome) => {
                // Carry the answer through the same event the normal
                // worker path uses, so a resumed turn renders its result
                // instead of going silent once the decision clears.
                let text = match outcome {
                    pantheon_agent::LoopOutcome::Answered { text, .. } => text,
                    _ => String::new(),
                };
                let _ = tx3.send(TuiEvent::Answered {
                    run_id: run3.clone(),
                    text,
                });
                let _ = tx3.send(TuiEvent::TurnComplete {
                    run_id: run3.clone(),
                });
            }
            Err(e) => {
                let _ = tx3.send(TuiEvent::Error {
                    run_id: run3.clone(),
                    msg: e.to_string(),
                });
            }
        }
    });
}

/// Answer the active session's clarify question and resume the parked
/// turn. The answer is recorded durably (supervisor.answer_input) and
/// rides a ToolMessage into the resumed loop.
fn answer_clarify(
    state: &mut TuiState,
    session: &Arc<Session>,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
    answer: &str,
) {
    let Some(req) = state.pending_clarify.take() else {
        return;
    };
    let run = state.session_id.clone();
    match session.supervisor.answer_input(&run, &req.call_id, answer) {
        Ok(()) => {
            state.clarifies.remove(&run);
            state.tabs.set_approval(&run, false);
            state.resolve_clarify_block(answer);
            state.status_line = "answered; resuming".into();
            resume_parked_turn(state, session, tx, &run);
        }
        Err(e) => {
            state.pending_clarify = Some(req);
            state.status_line = format!("answer failed: {e}");
        }
    }
}

/// Run a destructive slash command the user confirmed on the amber card.
fn run_confirmed_action(state: &mut TuiState, action: ConfirmAction) {
    match action {
        ConfirmAction::ClearTranscript => {
            state.blocks.clear();
            state.scroll_offset = 0;
            state.add_status("transcript cleared".into());
        }
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
                TuiEvent::Model { run_id, ev: me } => {
                    // A model stream belongs to exactly one run. Events
                    // from a background tab never touch the visible
                    // transcript: the stream tag proves ownership, and an
                    // untagged event is only accepted when it arrived on
                    // the visible run's own thread.
                    let owned = match run_id {
                        Some(rid) => rid == state.session_id,
                        None => false,
                    };
                    if owned {
                        state.handle_model_event(me);
                    }
                }
                TuiEvent::Runtime(re) => state.handle_runtime_event(&re),
                TuiEvent::Answered { run_id, text } => {
                    if run_id != state.session_id {
                        // A background tab's final answer: badge, don't
                        // render. The turn lands in that tab's transcript
                        // when the user switches to it.
                        state.on_background_turn_complete(&run_id);
                        continue;
                    }
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
                        state.on_background_turn_complete(&run_id);
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
                TuiEvent::ReflectDone {
                    summary,
                    pending,
                    from_command,
                } => {
                    // A reflection pass finished on its worker thread. The
                    // main turn — if one is running — is untouched: this arm
                    // never writes active_run, ready, or the input line.
                    state.reflect_running = false;
                    // Command-originated passes report into the originating
                    // command's result block, so the late async output is
                    // preserved as the command's result instead of bare
                    // status lines. Automatic passes keep the old
                    // status-line behavior.
                    let mut cmd_lines: Vec<String> = Vec::new();
                    for line in summary.lines() {
                        cmd_lines.push(line.to_string());
                    }
                    if !pending.is_empty() {
                        state.pending_reflect = Some(pending);
                        cmd_lines.push(
                            "reflection proposals await approval: [y] approve all  [n] deny all"
                                .to_string(),
                        );
                    }
                    if from_command {
                        append_cmd_output(state, "/reflect", cmd_lines);
                    } else {
                        for line in cmd_lines {
                            state.add_status(line);
                        }
                    }
                }
                TuiEvent::UpdateAvailable { tag } => {
                    // Background update probe: only the splash reads it.
                    state.update_hint = Some(tag);
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
                            // Settle any steps still marked running: the
                            // task is over, so they are too.
                            for s in t.steps.iter_mut() {
                                if s.status == activity::StepStatus::Running {
                                    s.finish(ok);
                                }
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
                TuiEvent::Error { run_id, msg } => {
                    if run_id != state.session_id {
                        // A background run failed: badge its tab and move
                        // on. The visible session's error state is its own.
                        state.on_background_turn_complete(&run_id);
                        state.attention_note = Some(format!("background run failed: {msg}"));
                        continue;
                    }
                    state.blocks.push(TranscriptBlock {
                        kind: BlockKind::Status(format!("error: {msg}")),
                    });
                    // Mark the in-flight tool card failed with the error
                    // text so the failure detail lives on the card, not
                    // just in a status line.
                    if let Some(block) = state
                        .blocks
                        .iter_mut()
                        .rev()
                        .find(|b| matches!(&b.kind, BlockKind::ToolCall { ok: None, .. }))
                    {
                        if let BlockKind::ToolCall {
                            ok, error, tokens, ..
                        } = &mut block.kind
                        {
                            *ok = Some(false);
                            *error = Some(msg.clone());
                            *tokens = Some(state.tokens_used + state.turn_estimate);
                        }
                    }
                    state.ready = true;
                    state.active_run = None;
                    drain_queued_message(state, &session, tx);
                }
                TuiEvent::Canceled { run_id } => {
                    if run_id != state.session_id {
                        // An interrupt racing a tab switch: the cancel
                        // belongs to the background tab, not the visible
                        // one.
                        state.on_background_turn_complete(&run_id);
                        state.attention_note = Some("background run interrupted".to_string());
                        continue;
                    }
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
        // Real terminal size before any layout decision: the sidebar
        // split, the inline todo card, and the transcript tail hint all
        // read `state.term_width`.
        state.term_width = terminal.size().map(|s| s.width).unwrap_or(80);
        refresh_todos(state, &session);
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
                        // Esc steps back to the provider list (which is
                        // still open underneath); Esc there closes all.
                        KeyCode::Esc => {
                            state.models_close();
                        }
                        KeyCode::Enter => {
                            let sel = state.models_sel;
                            let target = state.filtered_models().get(sel).cloned();
                            state.providers_close();
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
                if state.providers.is_some() {
                    match key.code {
                        KeyCode::Char(c) => state.providers_input.push(c),
                        KeyCode::Backspace => {
                            state.providers_input.pop();
                        }
                        KeyCode::Up => state.providers_move(-1),
                        KeyCode::Down => state.providers_move(1),
                        KeyCode::Esc => state.providers_close(),
                        KeyCode::Enter => {
                            let sel = state.providers_sel;
                            let target = state.filtered_providers().get(sel).cloned();
                            if let Some(p) = target {
                                // Drill in: the models view is scoped to
                                // this provider only.
                                let rows: Vec<ModelRow> = build_model_rows()
                                    .into_iter()
                                    .filter(|r| r.provider_id == p.provider_id)
                                    .collect();
                                state.models = Some(rows);
                                state.models_input.clear();
                                state.models_sel = 0;
                            }
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                if state.history.is_some() {
                    // Inline rename captures everything except Esc/Enter.
                    if state.history_rename.is_some() {
                        match key.code {
                            KeyCode::Esc => {
                                state.history_rename = None;
                            }
                            KeyCode::Enter => {
                                if let Some((id, buf)) = state.history_rename.take() {
                                    let title = buf.trim().to_string();
                                    if title.is_empty() {
                                        state.add_status(
                                            "rename: empty title, kept the old one".into(),
                                        );
                                    } else {
                                        let ev = pantheon_api::events::Event::SessionTitled {
                                            run_id: id.clone(),
                                            title: title.clone(),
                                            model: "user".into(),
                                            source: "manual".into(),
                                        };
                                        match session.supervisor.emit(ev) {
                                            Ok(()) => {
                                                if id == state.session_id {
                                                    state.title = Some(title.clone());
                                                }
                                                if let Ok(runs) =
                                                    session.supervisor.ledger_list_runs(50)
                                                {
                                                    state.history = Some(
                                                        pantheon_storage::Ledger::pin_home_first(
                                                            runs,
                                                        ),
                                                    );
                                                }
                                                state.add_status(format!(
                                                    "renamed \u{201c}{title}\u{201d}"
                                                ));
                                            }
                                            Err(e) => state.add_status(format!("rename: {e}")),
                                        }
                                    }
                                }
                            }
                            KeyCode::Char(c) => {
                                if let Some((_, buf)) = state.history_rename.as_mut() {
                                    buf.push(c);
                                }
                            }
                            KeyCode::Backspace => {
                                if let Some((_, buf)) = state.history_rename.as_mut() {
                                    buf.pop();
                                }
                            }
                            _ => {}
                        }
                        state.tick();
                        terminal.draw(|f| render(state, f))?;
                        continue;
                    }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        // Delete the selected session (ctrl+d). The home
                        // session is ledger-protected; deleting the live
                        // session's row keeps the live state going.
                        KeyCode::Char('d') if ctrl => {
                            let sel = state.history_sel;
                            let target = state.filtered_history().get(sel).cloned();
                            if let Some((id, _, _, _, _)) = target {
                                let live = id == state.session_id;
                                match session.supervisor.ledger_delete_run(&id) {
                                    Ok(_) => {
                                        if let Ok(runs) = session.supervisor.ledger_list_runs(50) {
                                            state.history = Some(
                                                pantheon_storage::Ledger::pin_home_first(runs),
                                            );
                                        }
                                        state.history_move(0);
                                        state.add_status(if live {
                                            format!("deleted {id} (live session continues)")
                                        } else {
                                            format!("deleted {id}")
                                        });
                                    }
                                    Err(e) => state.add_status(format!("delete: {e}")),
                                }
                            }
                        }
                        // Rename the selected session inline (ctrl+r).
                        KeyCode::Char('r') if ctrl => {
                            let sel = state.history_sel;
                            let target = state.filtered_history().get(sel).cloned();
                            if let Some((id, _, _, title, _)) = target {
                                state.history_rename = Some((id, title.unwrap_or_default()));
                            }
                        }
                        // Toggle all-projects vs this-project-only (ctrl+a).
                        KeyCode::Char('a') if ctrl => {
                            state.history_all_projects = !state.history_all_projects;
                            state.history_sel = 0;
                        }
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
                if state.palette.is_some() {
                    // The `/` command palette owns the keyboard while
                    // open: type to filter, Up/Down to move, Enter to run
                    // the selected command, Esc to dismiss.
                    let mut run_cmd: Option<String> = None;
                    if let Some(p) = state.palette.as_mut() {
                        match key.code {
                            KeyCode::Esc => state.palette = None,
                            KeyCode::Enter => {
                                let matches = p.filtered();
                                run_cmd = matches
                                    .get(p.sel)
                                    .map(|(name, _)| {
                                        // Strip the `[ARGS]` usage suffix so the
                                        // command runs in its bare form.
                                        name.split_whitespace().next().unwrap_or(name).to_string()
                                    })
                                    .or_else(|| {
                                        // No match: run the typed text as a
                                        // slash command anyway. `/goal fix the
                                        // tests` reaches its handler (with
                                        // args), and a genuine typo lands on
                                        // the unknown-command path with its
                                        // did-you-mean, instead of a dead Enter.
                                        let raw = p.input.trim();
                                        (!raw.is_empty()).then(|| format!("/{raw}"))
                                    });
                                state.palette = None;
                            }
                            KeyCode::Up => p.move_sel(-1),
                            KeyCode::Down => p.move_sel(1),
                            KeyCode::Backspace => {
                                p.input.pop();
                                p.sel = 0;
                            }
                            KeyCode::Char(c) => {
                                p.input.push(c);
                                p.sel = 0;
                            }
                            _ => {}
                        }
                    }
                    if let Some(cmd) = run_cmd {
                        // A palette pick runs exactly like a typed slash
                        // command: same capture, same result block.
                        handle_slash(state, &session, &cmd, tx);
                        refresh_tabs(state, &session);
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
                        KeyCode::Char('b') | KeyCode::Char('B') => {
                            // Toggle the session sidebar: the 75/25
                            // (80/20) split at ≥100 cols, a floating
                            // overlay below that (never open by default).
                            // One unified view — the session underneath
                            // never changes.
                            state.toggle_sidebar();
                        }
                        KeyCode::Char('g') | KeyCode::Char('G') => {
                            // Flip the sidebar's bottom card: activity
                            // timeline | todo list.
                            state.toggle_sidebar_view();
                        }
                        KeyCode::Char('t') | KeyCode::Char('T') => {
                            new_tab(state, &session, "new tab (unsaved until the first turn)");
                            refresh_tabs(state, &session);
                        }
                        KeyCode::Char('w') | KeyCode::Char('W') => {
                            close_active_tab(state, &session);
                            refresh_tabs(state, &session);
                        }
                        KeyCode::Char('e') | KeyCode::Char('E') => {
                            if state.pending_approval.is_none() {
                                state.open_editor();
                            }
                        }
                        // Yank the last assistant message to the clipboard.
                        // Only when the composer is empty so it never eats
                        // typed text; /yank covers the rest.
                        KeyCode::Char('y') | KeyCode::Char('Y')
                            if state.input.trim().is_empty() =>
                        {
                            do_yank(state, "");
                        }
                        _ => {}
                    }
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                // `/` with an empty composer opens the command palette
                // instead of typing. With text present, `/` types
                // normally (paths, regex); the palette is one Backspace
                // away when the composer is empty.
                if key.code == KeyCode::Char('/')
                    && key.modifiers.is_empty()
                    && state.input.is_empty()
                    && !state.is_inputting
                {
                    state.palette = Some(PaletteState::default());
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                // Session tabs: Ctrl+Tab cycle, Alt+1..9 jump. Checked before
                // the Char handler (Alt+1 arrives as Char('1')+ALT).
                // Plain `[`/`]` always type into the chat input; Ctrl+Tab
                // and Alt+digits stay global.
                let bracket_key = matches!(key.code, KeyCode::Char('[') | KeyCode::Char(']'))
                    && key.modifiers.is_empty();
                let tab_action = if bracket_key {
                    None
                } else {
                    crate::tabs::tab_key_action(key.code, key.modifiers)
                };
                if let Some(action) = tab_action {
                    match action {
                        crate::tabs::TabAction::Next => {
                            if let Some(id) = state.tabs.cycle_next().map(str::to_string) {
                                switch_to_run(state, &session, &id, "resumed");
                            }
                        }
                        crate::tabs::TabAction::Prev => {
                            if let Some(id) = state.tabs.cycle_prev().map(str::to_string) {
                                switch_to_run(state, &session, &id, "resumed");
                            }
                        }
                        crate::tabs::TabAction::Jump(n) => {
                            if state.tabs.jump(n) {
                                if let Some(id) = state.tabs.active_run_id().map(str::to_string) {
                                    switch_to_run(state, &session, &id, "resumed");
                                }
                            }
                        }
                        crate::tabs::TabAction::NewTab => {
                            new_tab(state, &session, "new tab (unsaved until the first turn)");
                        }
                        crate::tabs::TabAction::CloseTab => {
                            close_active_tab(state, &session);
                        }
                    }
                    refresh_tabs(state, &session);
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                // Plain Tab toggles Build/Plan mode. The fullscreen
                // editor, @ mention picker, and palette own Tab while
                // open (handled above with `continue`), so reaching here
                // means Tab never lands in the composer as whitespace.
                if key.code == KeyCode::Tab && key.modifiers.is_empty() {
                    state.toggle_plan_mode();
                    state.apply_plan_mode_to_session(&session);
                    state.tick();
                    terminal.draw(|f| render(state, f))?;
                    continue;
                }
                match key.code {
                    KeyCode::Char(c) => {
                        // A lone digit answers the pending clarify question
                        // directly; anything else (or a non-empty input)
                        // types normally into the composer.
                        let clarify_digit: Option<String> = match (&state.pending_clarify, c) {
                            (Some(req), _) if state.input.is_empty() => c
                                .to_digit(10)
                                .filter(|d| *d >= 1 && (*d as usize) <= req.options.len())
                                .map(|d| req.options[d as usize - 1].clone()),
                            _ => None,
                        };
                        if state.pending_approval.is_some() {
                            // Approval card: a approve, d deny, Esc deny.
                            // (y/n kept as aliases.)
                            match c {
                                'a' | 'A' | 'y' | 'Y' => {
                                    if let Some((run, scope)) = state.pending_approval.take() {
                                        let _ = session.supervisor.grant(&run, &scope);
                                        state.approvals.remove(&run);
                                        state.tabs.set_approval(&run, false);
                                        state.resolve_approval_block(Some(true));
                                        state.status_line = "granted; resuming".into();
                                        // Resume: chat_turn with empty
                                        // message rebuilds from the ledger
                                        // and continues the loop.
                                        resume_parked_turn(state, &session, tx, &run);
                                    }
                                }
                                'd' | 'D' | 'n' | 'N' => {
                                    if let Some((run, scope)) = state.pending_approval.take() {
                                        let _ = session.supervisor.deny(&run, &scope);
                                        state.approvals.remove(&run);
                                        state.tabs.set_approval(&run, false);
                                        state.resolve_approval_block(Some(false));
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
                        } else if let Some(answer) = clarify_digit {
                            answer_clarify(state, &session, tx, &answer);
                        } else if state.pending_confirm.is_some() {
                            // Destructive-command confirm card: a runs it,
                            // d dismisses. Same decision pattern as the
                            // approval card.
                            match c {
                                'a' | 'A' | 'y' | 'Y' => {
                                    if let Some(action) = state.pending_confirm.take() {
                                        run_confirmed_action(state, action);
                                    }
                                }
                                'd' | 'D' | 'n' | 'N' => {
                                    state.pending_confirm = None;
                                    state.resolve_confirm_block(false);
                                    state.status_line = "dismissed".into();
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
                                        for p in &pending {
                                            match crate::nightly_cli::decide(&dd, &p.id, true) {
                                                Ok(true) => {
                                                    approved += 1;
                                                    state.add_status(format!("approved {}", p.id));
                                                }
                                                Ok(false) => state.add_status(format!(
                                                    "approve {}: no longer pending",
                                                    p.id
                                                )),
                                                Err(e) => state.add_status(format!(
                                                    "approve {} failed: {e}",
                                                    p.id
                                                )),
                                            }
                                        }
                                        state.status_line = format!("nightly: {approved} approved");
                                    }
                                }
                                'n' | 'N' => {
                                    if let Some(pending) = state.pending_reflect.take() {
                                        let dd = crate::terminal::data_dir();
                                        let mut denied = 0;
                                        for p in &pending {
                                            if crate::nightly_cli::decide(&dd, &p.id, false)
                                                .unwrap_or(false)
                                            {
                                                denied += 1;
                                            }
                                        }
                                        state.status_line = format!("nightly: {denied} denied");
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
                            if state.pending_clarify.is_some() && !raw.trim().is_empty() {
                                // Free-text answer to the clarify card.
                                state.input.clear();
                                answer_clarify(state, &session, tx, raw.trim());
                            } else {
                                submit_text(state, &session, tx, &raw);
                            }
                        } else {
                            state.is_inputting = true;
                            if state.vim.enabled {
                                state.vim.mode = vim::VimMode::Normal;
                            }
                        }
                    }
                    KeyCode::Esc => {
                        // The narrow-terminal sidebar overlay owns Esc
                        // while open: one press dismisses it, nothing
                        // else happens.
                        let overlay_open = state.sidebar_overlay
                            && terminal.size().map(|sz| sz.width < 100).unwrap_or(false);
                        if overlay_open {
                            state.sidebar_overlay = false;
                            state.show_sidebar = false;
                        } else if state.pending_confirm.is_some() {
                            state.pending_confirm = None;
                            state.resolve_confirm_block(false);
                            state.status_line = "dismissed".into();
                        } else if state.pending_approval.is_some() {
                            if let Some((run, scope)) = state.pending_approval.take() {
                                let _ = session.supervisor.deny(&run, &scope);
                                state.approvals.remove(&run);
                                state.tabs.set_approval(&run, false);
                                state.resolve_approval_block(Some(false));
                                session
                                    .cancel_current_run(&run, "user pressed esc during approval");
                                state.status_line = "denied; turn cancelled".into();
                                state.ready = true;
                                state.active_run = None;
                                drain_queued_message(state, &session, tx);
                            }
                        } else if state.pending_clarify.is_some() {
                            let run = state.session_id.clone();
                            state.pending_clarify.take();
                            state.clarifies.remove(&run);
                            state.tabs.set_approval(&run, false);
                            state.resolve_clarify_block("cancelled");
                            session.cancel_current_run(&run, "user pressed esc during clarify");
                            state.status_line = "cancelled".into();
                            state.ready = true;
                            state.active_run = None;
                            drain_queued_message(state, &session, tx);
                        } else if state.vim.enabled
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
        Ok(()) => {
            state.variant = name.to_string();
            state.add_status(format!("now talking to {name}"))
        }
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
/// Opens by path rather than going through the attached agent: `/tasks
/// <other agent>` is an inspection command that must work when no agent
/// is attached, and it is strictly read-only. (`/collab` no longer reads
/// this store — it is deprecated in favor of the swarm system; the stored
/// data is untouched.)
fn collaboration_store() -> Result<pantheon_storage::CollaborationStore, String> {
    pantheon_storage::CollaborationStore::open(
        &crate::terminal::data_dir().join("collaboration.db"),
    )
    .map_err(|e| e.cause)
}

/// Distinct providers from model rows, alphabetical by label, with
/// curated-model counts. Drives the `/models` provider picker; a
/// provider with no catalog entries still lists (count 0) so its
/// models stay reachable via `/model <provider> <id>`.
fn provider_rows(rows: &[ModelRow]) -> Vec<ProviderRow> {
    let mut seen = std::collections::HashSet::new();
    let mut providers: Vec<ProviderRow> = Vec::new();
    for r in rows {
        if seen.insert(r.provider_id.clone()) {
            providers.push(ProviderRow {
                provider_id: r.provider_id.clone(),
                provider_label: r.provider_label.clone(),
                model_count: 0,
            });
        }
    }
    for p in providers.iter_mut() {
        p.model_count = rows
            .iter()
            .filter(|r| r.provider_id == p.provider_id && !r.model_id.is_empty())
            .count();
    }
    providers.sort_by(|a, b| {
        a.provider_label
            .to_lowercase()
            .cmp(&b.provider_label.to_lowercase())
    });
    providers
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
            BlockKind::ToolCall {
                name,
                args,
                ok,
                call_id,
                duration,
                error,
                ..
            } => {
                let st = match ok {
                    None => "running",
                    Some(true) => "done",
                    Some(false) => "failed",
                };
                let mut s = format!("{name} [{st}] #{call_id} {args}");
                if let Some(d) = duration {
                    s.push_str(&format!(" ({:.1}s)", d.as_secs_f64()));
                }
                if let Some(e) = error {
                    if !e.is_empty() {
                        s.push_str(&format!(" — {e}"));
                    }
                }
                ("tool", s)
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
            BlockKind::Todos(items) => {
                use crate::todo_card::TodoStatus as CardStatus;
                let remaining = items
                    .iter()
                    .filter(|i| i.status != CardStatus::Completed)
                    .count();
                let body = items
                    .iter()
                    .map(|i| {
                        let g = match i.status {
                            CardStatus::Completed => "x",
                            CardStatus::InProgress => ">",
                            CardStatus::Pending => " ",
                        };
                        format!("[{g}] {}", i.content)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                ("todos", format!("working on {remaining} to-dos:\n{body}"))
            }
            BlockKind::ProviderError {
                provider,
                model,
                kind,
                message,
                fallback,
            } => {
                let fb = match fallback {
                    FallbackOutcome::Engaged {
                        provider: p,
                        model: m,
                    } => {
                        format!("fallback engaged -> {p}/{m}")
                    }
                    FallbackOutcome::Exhausted => "no fallback available".to_string(),
                    FallbackOutcome::NotRetryable => "not retryable".to_string(),
                    FallbackOutcome::Retrying {
                        attempt,
                        max_attempts,
                        wait_secs,
                    } => {
                        format!("retrying {attempt}/{max_attempts} (waiting {wait_secs}s)")
                    }
                    FallbackOutcome::Recovered {
                        attempt,
                        max_attempts,
                    } => {
                        format!("recovered on retry {attempt}/{max_attempts}")
                    }
                };
                (
                    "provider-error",
                    format!("{provider}/{model}: {kind} - {message} [{fb}]"),
                )
            }
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
            BlockKind::Approval {
                tool,
                sentence,
                decision,
                ..
            } => (
                "approval",
                format!(
                    "{tool}: {sentence} [{}]",
                    match decision {
                        None => "pending",
                        Some(true) => "approved",
                        Some(false) => "denied",
                    }
                ),
            ),
            BlockKind::Clarify {
                question, answered, ..
            } => (
                "clarify",
                match answered {
                    None => format!("{question} [unanswered]"),
                    Some(a) => format!("{question} → {a}"),
                },
            ),
            BlockKind::Command { cmd, lines, failed } => (
                "command",
                format!(
                    "/{cmd} [{}]\n{}",
                    if *failed { "failed" } else { "ok" },
                    lines.join("\n")
                ),
            ),
            BlockKind::Confirm {
                label,
                sentence,
                decided,
            } => (
                "confirm",
                format!(
                    "{label}: {sentence} [{}]",
                    match decided {
                        None => "pending",
                        Some(true) => "confirmed",
                        Some(false) => "dismissed",
                    }
                ),
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

/// Slash command entry: every command echoes, and its status output is
/// captured into one `Command` transcript block (titled result; `/help`
/// as a two-column table). Commands that open an overlay (/history,
/// /models, the editor) or push their own decision card skip the block.
/// Append late async output to a slash command's result block. Finds the
/// most recent `Command` block whose command starts with `cmd_prefix`
/// (`/reflect` covers `/reflect`, `/consolidate`, `/nightly`) and
/// extends its lines; when the block is gone (e.g. `/clear` ran while
/// the worker was out), a fresh result block is pushed so the output is
/// never silently dropped.
fn append_cmd_output(state: &mut TuiState, cmd_prefix: &str, lines: Vec<String>) {
    if lines.is_empty() {
        return;
    }
    let failed = lines.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with('✗') || t.starts_with("error:") || t.starts_with("failed")
    });
    let target =
        state.blocks.iter_mut().rev().find(
            |b| matches!(&b.kind, BlockKind::Command { cmd, .. } if cmd.starts_with(cmd_prefix)),
        );
    match target {
        Some(block) => {
            if let BlockKind::Command {
                lines: existing,
                failed: f,
                ..
            } = &mut block.kind
            {
                existing.extend(lines);
                *f = *f || failed;
            }
        }
        None => {
            state.blocks.push(TranscriptBlock {
                kind: BlockKind::Command {
                    cmd: cmd_prefix.to_string(),
                    lines,
                    failed,
                },
            });
        }
    }
    state.scroll_to_bottom();
}

fn handle_slash(
    state: &mut TuiState,
    session: &Arc<Session>,
    cmd: &str,
    tx: &std::sync::mpsc::Sender<TuiEvent>,
) {
    state.cmd_capture = Some((cmd.to_string(), Vec::new()));
    handle_slash_inner(state, session, cmd, tx);
    let Some((cmd, lines)) = state.cmd_capture.take() else {
        return;
    };
    // Overlays and decision cards own the UI: no transcript block.
    if state.history.is_some()
        || state.models.is_some()
        || state.providers.is_some()
        || state.editor.is_some()
        || state.pending_confirm.is_some()
    {
        return;
    }
    if lines.is_empty() {
        return;
    }
    let failed = lines.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with('✗')
            || t.starts_with("error:")
            || t.starts_with("Error:")
            || t.starts_with("failed")
    });
    // Never persist secret material: `/env set NAME VALUE` keeps the
    // command shape for context but the value is redacted.
    let cmd = redact_command_for_transcript(&cmd);
    state.blocks.push(TranscriptBlock {
        kind: BlockKind::Command { cmd, lines, failed },
    });
    state.scroll_to_bottom();
}

/// Strip secret values from a slash command before it is persisted in the
/// transcript. `/env set NAME VALUE` is stored as `/env set NAME ••••` —
/// the command name and argument count survive for context, the value
/// never reaches the ledger.
fn redact_command_for_transcript(cmd: &str) -> String {
    if let Some(rest) = cmd.strip_prefix("/env set ") {
        let name = rest.split_whitespace().next().unwrap_or("?");
        return format!("/env set {name} ••••");
    }
    cmd.to_string()
}

/// `/voice`: `[stt]`/`[tts]` backend status. Reports backend, provider,
/// and whether the key resolves — never key values. Construction is
/// attempted so a misconfigured backend shows its real config error.
fn voice_status(state: &mut TuiState, session: &Arc<Session>) {
    let dd = crate::terminal::data_dir();
    let cfg = match crate::config::Config::load(&dd) {
        Ok(c) => c,
        Err(e) => {
            state.add_status(format!("/voice: {}", e.cause));
            return;
        }
    };
    // The Tools screen's Voice group gates the whole speech surface:
    // off means no backend is constructed and no key is resolved.
    if !crate::config::tool_group_enabled(Some(&cfg), pantheon_api::config::ToolGroup::Voice) {
        state.add_status("voice: disabled on the Tools screen ([tools] voice = false)".into());
        return;
    }
    state.add_status("voice:".into());
    for (name, section) in [("stt", &cfg.stt), ("tts", &cfg.tts)] {
        let Some(sec) = section else {
            state.add_status(format!("  {name}: unset ([{name}] not configured)"));
            continue;
        };
        let mut line = format!("  {name}: {} backend", sec.backend);
        if sec.backend == "openai" {
            if let Some(p) = sec.options.get("provider") {
                line.push_str(&format!(" via {p}"));
            }
            if let Some(m) = sec.options.get("model") {
                line.push_str(&format!(" ({m})"));
            }
        } else if let Some(c) = sec.options.get("cmd") {
            line.push_str(&format!(" ({c})"));
        }
        state.add_status(line);
        if sec.backend == "openai" {
            let provider = sec
                .options
                .get("provider")
                .map(String::as_str)
                .unwrap_or("");
            match pantheon_providers::voice_api_key(&session.secrets, provider, &sec.options) {
                Ok(Some(_)) => state.add_status(format!("  {name}: key set")),
                Ok(None) => {
                    state.add_status(format!("  {name}: no key found (ok for keyless endpoints)"))
                }
                Err(e) => state.add_status(format!("  {name}: key error: {}", e.cause)),
            }
        }
        let built = if name == "stt" {
            pantheon_providers::stt_from_config(Some(sec), &session.secrets).map(|r| r.map(|_| ()))
        } else {
            pantheon_providers::tts_from_config(Some(sec), &session.secrets).map(|r| r.map(|_| ()))
        };
        match built {
            Some(Ok(())) => state.add_status(format!("  {name}: backend constructs ok")),
            Some(Err(e)) => state.add_status(format!("  {name}: ✗ {}", e.cause)),
            None => {}
        }
    }
    state.add_status(
        "no mic capture or speaker playback in the TUI — backends serve callers that already have audio".into(),
    );
}

/// `/send <target> <message>`: push a message to a gateway surface — the
/// user's surfaces, not this agent's reply path. Runs synchronously so the
/// result lands in the command block: errors are prefixed with `✗`, which
/// marks the block failed. Nothing here fails silently — an unknown target
/// or a missing token shows exactly what is wrong.
fn do_send(state: &mut TuiState, arg: &str) {
    let mut parts = arg.splitn(2, char::is_whitespace);
    let target = parts.next().unwrap_or("").trim();
    let message = parts.next().unwrap_or("").trim();
    if target.is_empty() || message.is_empty() {
        state.add_status("usage: /send <telegram|discord|mobile|home> <message>".into());
        return;
    }
    let deliver = match pantheon_gateway::schedule_delivery::Deliver::parse(target) {
        Ok(d) => d,
        Err(e) => {
            state.add_status(format!("✗ /send: {e}"));
            return;
        }
    };
    match crate::send::send_to_target(&crate::terminal::data_dir(), &deliver, message) {
        Ok(confirmation) => state.add_status(format!("/send: {confirmation}")),
        Err(e) => state.add_status(format!("✗ /send: {e}")),
    }
}

fn handle_slash_inner(
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
    if cmd == "/agent new" || cmd.starts_with("/agent new ") {
        crate::swarm_remote::do_agent_new(state, cmd["/agent new".len()..].trim());
        return;
    }
    if let Some(name) = cmd.strip_prefix("/agent ") {
        let name = name.trim();
        switch_agent(state, session, name);
        return;
    }
    if cmd == "/agents create" || cmd.starts_with("/agents create ") {
        crate::swarm_remote::do_agent_new(state, cmd["/agents create".len()..].trim());
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
    // Persona files of the active profile: `/soul` prints SOUL.md,
    // `/soul set <text>` writes it; same for USER.md (`/userfile`) and
    // AGENTS.md (`/agentsfile`). HTTP when the dashboard server runs,
    // direct declared-path file access otherwise.
    if cmd == "/soul" || cmd.starts_with("/soul ") {
        crate::swarm_remote::do_persona(state, session, cmd, "soul", "soul");
        return;
    }
    if cmd == "/userfile" || cmd.starts_with("/userfile ") {
        crate::swarm_remote::do_persona(state, session, cmd, "user", "userfile");
        return;
    }
    if cmd == "/agentsfile" || cmd.starts_with("/agentsfile ") {
        crate::swarm_remote::do_persona(state, session, cmd, "agents", "agentsfile");
        return;
    }
    if cmd == "/collab" {
        // Deprecated: multi-profile tasks moved to the swarm system.
        // Routes through the swarm backend when it answers; otherwise a
        // one-line pointer. Stored collaboration data is untouched.
        crate::swarm_remote::do_collab(state);
        return;
    }
    if cmd == "/tasks" {
        state.add_status("usage: /tasks <agent> — that agent's open tasks".into());
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
    // Nightly: bare /nightly runs a manual pass; on/off/status manage
    // the loop (persisted to [nightly]). /reflect and /consolidate are
    // kept as aliases for the same unified pass.
    if cmd == "/nightly"
        || cmd.starts_with("/nightly ")
        || cmd == "/reflect"
        || cmd.starts_with("/reflect ")
        || cmd == "/consolidate"
        || cmd.starts_with("/consolidate ")
    {
        do_nightly(state, tx, cmd);
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
    // Agent todo list, maintained through the `todo` tool: bare /todos
    // inspects it.
    if cmd == "/todos" {
        show_todos(state, session);
        return;
    }
    if cmd == "/help" {
        // Generated from the COMMANDS registry: one entry per command,
        // never duplicated, never drifting out of sync with the palette.
        for line in help_lines() {
            state.add_status(line);
        }
        return;
    }
    if cmd == "/exit" || cmd == "/quit" {
        state.shutting_down = true;
        return;
    }
    if cmd == "/clear" {
        // Destructive: reuses the approval decision-card pattern. The
        // card renders in the transcript; `a` clears, `d`/Esc dismisses.
        state.blocks.push(TranscriptBlock {
            kind: BlockKind::Confirm {
                label: "/clear".to_string(),
                sentence: "wants to clear the visible transcript. The ledger keeps everything."
                    .to_string(),
                decided: None,
            },
        });
        state.scroll_to_bottom();
        state.pending_confirm = Some(ConfirmAction::ClearTranscript);
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

    if cmd == "/project" || cmd.starts_with("/project ") {
        // Named projects: user-created buckets sessions are assigned to
        // by hand. A project exists when at least one run carries its
        // name; "new"/"set" just assigns the name, there is no registry.
        let rest = cmd.strip_prefix("/project").unwrap_or("").trim();
        let (sub, arg) = match rest.split_once(char::is_whitespace) {
            Some((s, a)) => (s, a.trim()),
            None => (rest, ""),
        };
        let bound_name = |raw: &str| -> Option<String> {
            let one_line: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            let name = one_line.trim();
            if name.is_empty() || name.len() > 64 {
                return None;
            }
            Some(name.to_string())
        };
        match sub {
            "" => {
                let current = supervisor
                    .ledger_run_project(&state.session_id)
                    .ok()
                    .flatten();
                match current.as_deref() {
                    Some(p) => state.add_status(format!("project: {p}")),
                    None => state.add_status("project: (none)".into()),
                }
                match supervisor.ledger_list_projects() {
                    Ok(ps) if !ps.is_empty() => {
                        state.add_status(format!("projects: {}", ps.join(", ")));
                    }
                    Ok(_) => state.add_status("no projects yet — /project new <name>".into()),
                    Err(e) => state.add_status(format!("/project: {e}")),
                }
                return;
            }
            "new" | "set" => {
                let Some(name) = bound_name(arg) else {
                    state.add_status("usage: /project new <name>".into());
                    return;
                };
                // A never-chatted run has no row yet; create it so the
                // assignment survives (/title does the same).
                if supervisor
                    .ledger_status(&state.session_id)
                    .ok()
                    .flatten()
                    .is_none()
                {
                    let _ = supervisor.start_run(&state.session_id);
                }
                match supervisor.ledger_set_run_project(&state.session_id, Some(&name)) {
                    Ok(()) => state.add_status(format!("session → project \u{201c}{name}\u{201d}")),
                    Err(e) => state.add_status(format!("/project: {e}")),
                }
                return;
            }
            "clear" => {
                match supervisor.ledger_set_run_project(&state.session_id, None) {
                    Ok(()) => state.add_status("session removed from its project".into()),
                    Err(e) => state.add_status(format!("/project: {e}")),
                }
                return;
            }
            "list" => {
                match supervisor.ledger_list_projects() {
                    Ok(ps) if ps.is_empty() => {
                        state.add_status("no projects yet — /project new <name>".into())
                    }
                    Ok(ps) => {
                        for p in ps {
                            state.add_status(format!("  {p}"));
                        }
                    }
                    Err(e) => state.add_status(format!("/project: {e}")),
                }
                return;
            }
            "move" => {
                let Some((run, name_raw)) = arg.split_once(char::is_whitespace) else {
                    state.add_status("usage: /project move <run> <name>".into());
                    return;
                };
                let Some(name) = bound_name(name_raw) else {
                    state.add_status("/project move: bad project name".into());
                    return;
                };
                match supervisor.ledger_status(run) {
                    Ok(Some(_)) => match supervisor.ledger_set_run_project(run, Some(&name)) {
                        Ok(()) => {
                            state.add_status(format!("{run} → project \u{201c}{name}\u{201d}"))
                        }
                        Err(e) => state.add_status(format!("/project: {e}")),
                    },
                    _ => state.add_status(format!("/project move: no such run {run}")),
                }
                return;
            }
            _ => {
                state.add_status(
                    "usage: /project [new <name>|set <name>|clear|list|move <run> <name>]".into(),
                );
                return;
            }
        }
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
        let n = match cmd.strip_prefix("/runs ") {
            None => 10,
            Some(s) => match s.trim().parse::<usize>() {
                Ok(n) => n,
                Err(_) => {
                    state.add_status("usage: /runs [n] — n must be a number".into());
                    return;
                }
            },
        };
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
                for (run_id, status, _ts, title, _project) in runs {
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
        // Two-level flow: /models opens the provider list; picking one
        // drills into that provider's models. Optional filter applies to
        // the provider list, the same as opening it and typing.
        let filter = cmd.strip_prefix("/models").unwrap_or("").trim().to_string();
        let rows = build_model_rows();
        if rows.is_empty() {
            state.add_status("no models in the provider catalog".into());
            return;
        }
        // Distinct providers, alphabetical by label, with curated-model
        // counts. A provider with no catalog entries still lists so its
        // models can be reached via /model <provider> <id>.
        let providers = provider_rows(&rows);
        state.providers = Some(providers);
        state.providers_input = filter;
        state.providers_sel = 0;
        state.models_close();
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
                state.effort = Some(level.as_str().to_string());
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
        new_tab(
            state,
            session,
            "new conversation (unsaved until the first turn)",
        );
        refresh_tabs(state, session);
        return;
    }
    if cmd == "/rewind" || cmd == "/undo" {
        // `/undo` is the friendly alias: undoing the last response is
        // exactly what rewind does. Same confirm flow as double-Esc on an
        // idle session: the ledger is append-only, so rewind hides the
        // last turn from the live view behind a rewind marker instead of
        // rewriting history.
        match state.rewind_candidate() {
            Some(offer) => {
                state.rewind_offer = Some(offer);
                state.status_line = "rewind last turn? [y]es [n]o".into();
            }
            None => state.add_status("/undo: no finished turn to undo".into()),
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
    if cmd == "/swarm" || cmd.starts_with("/swarm ") {
        // Remote swarm controls (`/swarm new`, `/swarm <id>`, `/swarm
        // retry`); `/swarm tree` keeps the old read-only delegation
        // tree render.
        crate::swarm_remote::do_swarm(state, session, cmd);
        return;
    }
    if cmd == "/team" || cmd.starts_with("/team ") {
        // Team of experts: list teams, show a roster, or launch a team on
        // a task (via the dashboard's Teams API).
        crate::team_remote::do_team(state, cmd);
        return;
    }
    if cmd == "/plugins" || cmd.starts_with("/plugins ") {
        // Remote plugin install (`/plugins import <url|clawhub:slug>
        // [--ref <ref>]`) and registry search (`/plugins search
        // <query>`), against the dashboard's plugin API. The local
        // `pantheon plugins list|approve|disable` verbs stay in
        // terminal.rs; there were no TUI slash commands for them.
        crate::plugin_remote::do_plugins(state, cmd);
        return;
    }
    if cmd == "/plugin" || cmd.starts_with("/plugin ") {
        // Singular alias for /plugins: normalize to the plural form the
        // handler parses.
        let normalized = if cmd == "/plugin" {
            "/plugins".to_string()
        } else {
            format!("/plugins {}", cmd["/plugin ".len()..].trim_start())
        };
        crate::plugin_remote::do_plugins(state, &normalized);
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
        let Some(store) = session.memory.as_deref() else {
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
        match pantheon_memory::write_via(store, &session.policy, proposal, 4096) {
            Ok(rec) => state.add_status(format!("remembered {}", rec.key)),
            Err(e) => state.add_status(memory_write_status("/remember", &e)),
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
            Err(e) => state.add_status(memory_write_status("/learn", &e)),
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
    if cmd == "/voice" {
        voice_status(state, session);
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
    if cmd == "/send" || cmd.starts_with("/send ") {
        let arg = cmd.strip_prefix("/send").map(str::trim).unwrap_or("");
        do_send(state, arg);
        return;
    }
    if cmd == "/doctor" {
        let rep = crate::doctor::run_system_doctor(&crate::terminal::data_dir());
        for c in &rep.checks {
            let glyph = match c.status.as_str() {
                "ok" => "\u{2713}",
                "warn" => "\u{26a0}",
                "fail" => "\u{2717}",
                _ => "?",
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
    if cmd == "/sessions" || cmd == "/history" {
        // Interactive searchable picker: Enter reopens the picked run as
        // a tab (parked sessions included). Shared by /sessions (the
        // reopen-UI) and /history (the resume-UI); both route through
        // switch_to_run, which now maintains the explicit open-tab list.
        // The permanent home session is auto-created on first access and
        // pinned first, ahead of every ordinary session.
        match supervisor
            .ensure_home_session()
            .and_then(|_| supervisor.ledger_list_runs(50))
        {
            Ok(runs) => {
                let runs = pantheon_storage::Ledger::pin_home_first(runs);
                if runs.is_empty() {
                    state.add_status("no runs yet".into());
                } else {
                    state.history = Some(runs);
                    state.history_input.clear();
                    state.history_sel = 0;
                    // The picker defaults to the opener's project
                    // (None = unassigned); ctrl+a toggles all projects.
                    state.history_own_project = supervisor
                        .ledger_run_project(&state.session_id)
                        .ok()
                        .flatten();
                    state.history_all_projects = false;
                    state.history_rename = None;
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
        } else if arg == "markdown" || arg == "md" {
            // `md` is the spelling /help advertises; normalize it here so
            // the written filename keeps the canonical extension.
            "markdown"
        } else if arg == "json" {
            arg
        } else {
            state.add_status("usage: /export [md|json]".into());
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
                std::thread::spawn(move || {
                    set_stream_run(&run4);
                    match sess4.chat_turn(&run4, "", "") {
                        Ok(outcome) => {
                            let text = match outcome {
                                pantheon_agent::LoopOutcome::Answered { text, .. } => text,
                                _ => String::new(),
                            };
                            let _ = tx4.send(TuiEvent::Answered {
                                run_id: run4.clone(),
                                text,
                            });
                            let _ = tx4.send(TuiEvent::TurnComplete {
                                run_id: run4.clone(),
                            });
                        }
                        Err(e) => {
                            let _ = tx4.send(TuiEvent::Error {
                                run_id: run4.clone(),
                                msg: e.to_string(),
                            });
                        }
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
                let job = &j.job;
                let status = if job.paused { "paused" } else { "active" };
                state.add_status(format!(
                    "  {} [{}] {}",
                    job.id,
                    status,
                    job.task.chars().take(60).collect::<String>()
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
    // --- mcp -------------------------------------------------------------
    if cmd == "/mcp" {
        // Live view from the session's manager: configured servers with
        // their current status, tool counts, and approval state.
        let health = session.mcp_manager.health();
        refresh_mcp_segment(state, session);
        if health.is_empty() {
            state.add_status(
                "no MCP servers configured — add `[mcp.servers.<name>]` to config.toml or run `pantheon migrate apply`".into(),
            );
            return;
        }
        for h in &health {
            let status = match h.status {
                pantheon_mcp::manager::ServerStatus::Ready => {
                    format!("ready ({} tools)", h.tools)
                }
                pantheon_mcp::manager::ServerStatus::Connecting => "connecting".to_string(),
                pantheon_mcp::manager::ServerStatus::Failed => "failed".to_string(),
                pantheon_mcp::manager::ServerStatus::Backoff => "backoff".to_string(),
                pantheon_mcp::manager::ServerStatus::Unapproved => {
                    "needs `pantheon mcp approve`".to_string()
                }
                pantheon_mcp::manager::ServerStatus::Disabled => "disabled".to_string(),
            };
            let mut line = format!("  {} [{}] {status}", h.name, h.transport);
            if let Some(e) = &h.last_error {
                line.push_str(&format!(": {e}"));
            }
            state.add_status(line);
        }
        state.add_status(format!("{} server(s)", health.len()));
        return;
    }
    if cmd == "/mcp reload" {
        // Re-scan the declaration files on disk: picks up servers added
        // or edited since startup without restarting the TUI. The
        // manager drops live clients for changed specs and reconnects
        // lazily on the next registry build.
        let dd = crate::terminal::data_dir();
        for line in crate::mcp::reload_report(session, &dd) {
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
        // a restart. MCP contributes its approved, connected servers'
        // tools (`mcp_<server>_<tool>`).
        let (reg, counts) = session.build_tool_registry();
        debug_assert_eq!(counts.total(), reg.names().len());
        let pending = session
            .mcp_manager
            .health()
            .iter()
            .filter(|h| h.status == pantheon_mcp::manager::ServerStatus::Unapproved)
            .count();
        let pending_note = if pending > 0 {
            format!(" ({pending} server(s) need `mcp approve`)")
        } else {
            String::new()
        };
        state.add_status(format!(
            "{} tools available ({} built-in, {} skill, {} vault, {} session-search, {} browser, {} web-search, {} MCP{})",
            counts.total(),
            counts.builtin,
            counts.skills,
            counts.vault,
            counts.session_search,
            counts.browser,
            counts.websearch,
            counts.mcp,
            pending_note
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
        // `delete` is a no-op for unknown names by design; the TUI says
        // what actually happened instead of claiming a removal.
        if !session.secrets.names().iter().any(|n| n == name) {
            state.add_status(format!("no secret named {name}"));
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
            if !name.is_empty()
                && !crate::commands::is_builtin(name)
                && try_invoke_skill(state, session, tx, name, cmd)
            {
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
    let input = full_cmd
        .split_once(char::is_whitespace)
        .map(|x| x.1)
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
    let Some(store) = session.memory.as_deref() else {
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
    pantheon_memory::write_via(store, &session.policy, proposal, 4096)?;
    Ok(key)
}

/// Status line for a failed memory write. Memory writes are policy-gated
/// by design (the default `coder` preset leaves them off, `coder_memory`
/// enables them), so a bare `MEM_NO_CAPABILITY` code tells the user
/// nothing about the fix — lead with the one-line remediation instead of
/// burying it past the visible width.
fn memory_write_status(prefix: &str, e: &pantheon_api::error::PantheonError) -> String {
    if e.to_string().contains("MEM_NO_CAPABILITY") {
        return format!(
            "{prefix}: memory writes are gated by the agent policy — set policy = \"coder_memory\" to enable them"
        );
    }
    format!("{prefix}: {e}")
}

#[cfg(test)]
mod inline_todo_card_tests {
    use super::*;
    use crate::todo_card::{TodoCardItem, TodoStatus as CardStatus};

    fn test_state() -> TuiState {
        TuiState::new("test-session".to_string(), "test-model".to_string(), 0)
    }

    fn sample_items() -> Vec<TodoCardItem> {
        vec![
            TodoCardItem {
                content: "Made new things".to_string(),
                status: CardStatus::Completed,
            },
            TodoCardItem {
                content: "Read files".to_string(),
                status: CardStatus::InProgress,
            },
            TodoCardItem {
                content: "Edit files".to_string(),
                status: CardStatus::Pending,
            },
        ]
    }

    fn todo_blocks(state: &TuiState) -> Vec<&Vec<TodoCardItem>> {
        state
            .blocks
            .iter()
            .filter_map(|b| match &b.kind {
                BlockKind::Todos(items) => Some(items),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sidebar_visible_matches_render_body_split() {
        let mut s = test_state();
        s.term_width = 100;
        s.show_sidebar = true;
        assert!(s.sidebar_visible());
        s.term_width = 99;
        assert!(!s.sidebar_visible());
        s.term_width = 140;
        s.show_sidebar = false;
        assert!(!s.sidebar_visible());
    }

    #[test]
    fn inline_card_appears_in_narrow_mode() {
        let mut s = test_state();
        s.term_width = 60;
        s.todo_items = sample_items();
        sync_inline_todo_card(&mut s);
        let cards = todo_blocks(&s);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0], &sample_items());
    }

    #[test]
    fn inline_card_appears_when_sidebar_toggled_off_wide() {
        let mut s = test_state();
        s.term_width = 140;
        s.show_sidebar = false;
        s.todo_items = sample_items();
        sync_inline_todo_card(&mut s);
        assert_eq!(todo_blocks(&s).len(), 1);
    }

    #[test]
    fn no_inline_card_in_wide_mode() {
        let mut s = test_state();
        s.term_width = 140;
        s.todo_items = sample_items();
        sync_inline_todo_card(&mut s);
        assert!(todo_blocks(&s).is_empty());
    }

    #[test]
    fn no_inline_card_without_todos() {
        let mut s = test_state();
        s.term_width = 60;
        sync_inline_todo_card(&mut s);
        assert!(todo_blocks(&s).is_empty());
    }

    #[test]
    fn inline_card_updates_in_place_never_duplicates() {
        let mut s = test_state();
        s.term_width = 60;
        s.todo_items = sample_items();
        sync_inline_todo_card(&mut s);
        sync_inline_todo_card(&mut s);
        assert_eq!(todo_blocks(&s).len(), 1);

        // Progress the plan: the same single block refreshes.
        let mut updated = sample_items();
        updated[1].status = CardStatus::Completed;
        updated[2].status = CardStatus::InProgress;
        s.todo_items = updated.clone();
        sync_inline_todo_card(&mut s);
        let cards = todo_blocks(&s);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0], &updated);
    }

    #[test]
    fn inline_card_removed_when_sidebar_returns() {
        let mut s = test_state();
        s.term_width = 60;
        s.todo_items = sample_items();
        sync_inline_todo_card(&mut s);
        assert_eq!(todo_blocks(&s).len(), 1);
        s.term_width = 140;
        sync_inline_todo_card(&mut s);
        assert!(todo_blocks(&s).is_empty());
    }

    #[test]
    fn inline_card_removed_when_todos_empty() {
        let mut s = test_state();
        s.term_width = 60;
        s.todo_items = sample_items();
        sync_inline_todo_card(&mut s);
        assert_eq!(todo_blocks(&s).len(), 1);
        s.todo_items.clear();
        sync_inline_todo_card(&mut s);
        assert!(todo_blocks(&s).is_empty());
    }

    #[test]
    fn inline_card_survives_other_blocks() {
        // The card is found by kind, not position: surrounding transcript
        // content must not confuse the sync.
        let mut s = test_state();
        s.term_width = 60;
        s.blocks.push(TranscriptBlock {
            kind: BlockKind::Status("hello".to_string()),
        });
        s.todo_items = sample_items();
        sync_inline_todo_card(&mut s);
        s.blocks.push(TranscriptBlock {
            kind: BlockKind::Status("world".to_string()),
        });
        let mut updated = sample_items();
        updated[2].status = CardStatus::Completed;
        s.todo_items = updated.clone();
        sync_inline_todo_card(&mut s);
        let cards = todo_blocks(&s);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0], &updated);
        assert_eq!(s.blocks.len(), 3);
    }
}

#[cfg(test)]
mod picker_tests {
    use super::*;

    fn test_state() -> TuiState {
        TuiState::new("test-session".to_string(), "test-model".to_string(), 0)
    }

    fn run(id: &str, title: &str, project: Option<&str>, ts: i64) -> pantheon_storage::RunListing {
        (
            id.to_string(),
            "completed".to_string(),
            ts,
            Some(title.to_string()),
            project.map(|p| p.to_string()),
        )
    }

    fn model_row(provider_id: &str, label: &str, model_id: &str) -> ModelRow {
        ModelRow {
            provider_id: provider_id.to_string(),
            provider_label: label.to_string(),
            model_id: model_id.to_string(),
            ctx: None,
        }
    }

    #[test]
    fn history_defaults_to_own_project() {
        let mut s = test_state();
        s.history = Some(vec![
            run("a", "Alpha work", Some("alpha"), 1000),
            run("b", "Beta work", Some("beta"), 2000),
            run("c", "Loose work", None, 3000),
        ]);
        s.history_own_project = Some("alpha".to_string());
        s.history_all_projects = false;
        let ids: Vec<_> = s.filtered_history().iter().map(|r| r.0.clone()).collect();
        assert_eq!(ids, vec!["a".to_string()]);
    }

    #[test]
    fn history_all_projects_toggle_shows_everything() {
        let mut s = test_state();
        s.history = Some(vec![
            run("a", "Alpha work", Some("alpha"), 1000),
            run("b", "Beta work", Some("beta"), 2000),
        ]);
        s.history_own_project = Some("alpha".to_string());
        s.history_all_projects = true;
        assert_eq!(s.filtered_history().len(), 2);
    }

    #[test]
    fn history_unassigned_groups_with_unassigned() {
        let mut s = test_state();
        s.history = Some(vec![
            run("a", "Alpha work", Some("alpha"), 1000),
            run("c", "Loose work", None, 3000),
        ]);
        s.history_own_project = None;
        s.history_all_projects = false;
        let ids: Vec<_> = s.filtered_history().iter().map(|r| r.0.clone()).collect();
        assert_eq!(ids, vec!["c".to_string()]);
    }

    #[test]
    fn history_text_filter_composes_with_project_filter() {
        let mut s = test_state();
        s.history = Some(vec![
            run("a", "Alpha work", Some("alpha"), 1000),
            run("a2", "Alpha docs", Some("alpha"), 2000),
        ]);
        s.history_own_project = Some("alpha".to_string());
        s.history_input = "docs".to_string();
        let ids: Vec<_> = s.filtered_history().iter().map(|r| r.0.clone()).collect();
        assert_eq!(ids, vec!["a2".to_string()]);
    }

    #[test]
    fn provider_rows_dedup_sort_and_count() {
        let rows = vec![
            model_row("zeta", "Zeta", "z1"),
            model_row("alpha", "Alpha", "a1"),
            model_row("alpha", "Alpha", "a2"),
            model_row("mid", "Mid", ""),
        ];
        let ps = provider_rows(&rows);
        assert_eq!(ps.len(), 3);
        assert_eq!(ps[0].provider_label, "Alpha");
        assert_eq!(ps[0].model_count, 2);
        assert_eq!(ps[1].provider_label, "Mid");
        assert_eq!(ps[1].model_count, 0);
        assert_eq!(ps[2].provider_label, "Zeta");
        assert_eq!(ps[2].model_count, 1);
    }

    #[test]
    fn filtered_providers_matches_id_and_label() {
        let mut s = test_state();
        s.providers = Some(provider_rows(&[
            model_row("anthropic", "Anthropic", "m"),
            model_row("openai", "OpenAI", "m"),
        ]));
        s.providers_input = "anth".to_string();
        let ps = s.filtered_providers();
        assert_eq!(ps.len(), 1);
        assert_eq!(ps[0].provider_id, "anthropic");
        s.providers_input = "openai".to_string();
        assert_eq!(s.filtered_providers().len(), 1);
        s.providers_input = String::new();
        assert_eq!(s.filtered_providers().len(), 2);
    }

    #[test]
    fn providers_move_clamps_to_filtered_list() {
        let mut s = test_state();
        s.providers = Some(provider_rows(&[
            model_row("a", "A", "m"),
            model_row("b", "B", "m"),
        ]));
        s.providers_move(5);
        assert_eq!(s.providers_sel, 1);
        s.providers_move(-5);
        assert_eq!(s.providers_sel, 0);
    }

    #[test]
    fn providers_close_clears_models_too() {
        let mut s = test_state();
        s.providers = Some(provider_rows(&[model_row("a", "A", "m")]));
        s.models = Some(vec![model_row("a", "A", "m")]);
        s.providers_close();
        assert!(s.providers.is_none());
        assert!(s.models.is_none());
    }

    #[test]
    fn fmt_day_renders_reference_shape() {
        // 2026-09-27 12:00:00 UTC; Local rendering keeps the calendar day
        // for any sane timezone (UTC±14 stays on the 27th at noon).
        let label = fmt_day(1790510400000);
        assert!(
            label.starts_with("Sun Sep 27 2026"),
            "unexpected day label: {label}"
        );
    }
}

#[cfg(test)]
mod picker_render_tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn test_state() -> TuiState {
        TuiState::new("test-session".to_string(), "test-model".to_string(), 0)
    }

    fn run(id: &str, title: &str, project: Option<&str>, ts: i64) -> pantheon_storage::RunListing {
        (
            id.to_string(),
            "completed".to_string(),
            ts,
            Some(title.to_string()),
            project.map(|p| p.to_string()),
        )
    }

    fn model_row(provider_id: &str, label: &str, model_id: &str) -> ModelRow {
        ModelRow {
            provider_id: provider_id.to_string(),
            provider_label: label.to_string(),
            model_id: model_id.to_string(),
            ctx: None,
        }
    }

    fn render_to_string(
        draw: impl FnOnce(&mut Frame, Rect, &TuiState),
        state: &TuiState,
    ) -> Vec<String> {
        let backend = TestBackend::new(84, 24);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, f.area(), state)).unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn history_renders_groups_search_and_footer() {
        let mut s = test_state();
        // Two different calendar days (2026-09-27 and 2026-09-26 noon UTC).
        s.history = Some(vec![
            run("a", "Alpha work", Some("alpha"), 1790510400000),
            run("b", "Beta work", Some("alpha"), 1790424000000),
        ]);
        s.history_own_project = Some("alpha".to_string());
        let lines = render_to_string(render_history, &s);
        let text = lines.join("\n");
        assert!(text.contains("Search"), "search header missing");
        assert!(
            text.contains("Sun Sep 27 2026"),
            "day group missing:\n{text}"
        );
        assert!(
            text.contains("Sat Sep 26 2026"),
            "day group missing:\n{text}"
        );
        assert!(text.contains("Alpha work"), "row missing");
        assert!(text.contains("ctrl+d"), "footer hints missing");
    }

    #[test]
    fn providers_render_alphabetical_with_search() {
        let mut s = test_state();
        // Built through provider_rows, the way production populates the
        // picker: the sort invariant lives there, not in the test data.
        s.providers = Some(provider_rows(&[
            model_row("zeta", "Zeta", "z1"),
            model_row("zeta", "Zeta", "z2"),
            model_row("zeta", "Zeta", "z3"),
            model_row("alpha", "Alpha", "a1"),
        ]));
        let lines = render_to_string(render_providers, &s);
        let text = lines.join("\n");
        assert!(text.contains("Search"), "search header missing");
        let ai = text.find("Alpha").unwrap();
        let zi = text.find("Zeta").unwrap();
        assert!(ai < zi, "providers not alphabetical");
        assert!(text.contains("3 models"), "model count missing");
        assert!(text.contains("Esc closes"), "footer missing");
    }

    #[test]
    fn models_render_ctx_badge_right_aligned() {
        let mut s = test_state();
        s.models = Some(vec![
            ModelRow {
                provider_id: "p".into(),
                provider_label: "Pee".into(),
                model_id: "big-model".into(),
                ctx: Some(1_000_000),
            },
            ModelRow {
                provider_id: "p".into(),
                provider_label: "Pee".into(),
                model_id: "small-model".into(),
                ctx: Some(128_000),
            },
        ]);
        s.model = "p/big-model".to_string();
        let lines = render_to_string(render_models, &s);
        let text = lines.join("\n");
        assert!(text.contains("1M ctx"), "badge missing");
        assert!(text.contains("128k ctx"), "badge missing");
        assert!(text.contains("\u{25cf}"), "current marker missing");
        // Badge hugs the right edge of its row (before the border).
        let badge_line = lines.iter().find(|l| l.contains("1M ctx")).unwrap();
        let pos = badge_line.find("1M ctx").unwrap();
        assert!(
            pos + "1M ctx".len() + 1 >= 82,
            "badge not right-aligned: [{badge_line}]"
        );
    }
}

/// Item 5: redaction test — `/env set NAME VALUE` must reach the
/// transcript with the value masked, never the raw secret (the P1 fix
/// for the `/env set` transcript leak).
#[cfg(test)]
mod transcript_redaction_tests {
    use super::*;

    #[test]
    fn env_set_value_masked_in_transcript() {
        assert_eq!(
            redact_command_for_transcript("/env set FOO bar"),
            "/env set FOO ••••"
        );
        // Secrets with spaces/special chars: only the name survives.
        assert_eq!(
            redact_command_for_transcript("/env set API_KEY sk-live-abc 123"),
            "/env set API_KEY ••••"
        );
    }

    #[test]
    fn other_commands_pass_through_untouched() {
        assert_eq!(redact_command_for_transcript("/help"), "/help");
        assert_eq!(redact_command_for_transcript("/env list"), "/env list");
    }
}
