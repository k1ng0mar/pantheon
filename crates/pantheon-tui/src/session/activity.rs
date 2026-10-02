//! Agent/subagent activity timeline for the right sidebar.
//!
//! The sidebar's bottom card tracks what the agent is doing right now:
//! the current task (latest user turn) with its tool-call steps, one
//! section per spawned subagent, and one per `/btw` background task.
//!
//! Everything shown is real runtime data:
//! - main-task steps come from the transcript's tool-call blocks,
//! - subagents from `AgentSpawned` / `AgentMessage` / `AgentCompleted`
//!   ledger events,
//! - background-task steps from the tool lifecycle events of each task's
//!   own run id, captured in [`BgTask::steps`].
//!
//! Step detail finer than that - e.g. a delegated subagent's inner tool
//! calls - runs in another session and never reaches this process, so
//! subagent sections honestly show spawn / running / done instead of
//! fabricated checklists.

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::{bg, BlockKind, TuiState};

/// Bright purplish peak of the traveling highlight sweep.
pub const SWEEP_PEAK: Color = Color::Rgb(190, 160, 255);

/// Presentable tool names for the UI: `shell`/`exec` → "Execute",
/// `read_file` → "Read file". Unknown registry ids fall back to a
/// title-cased snake_case form so plugin tools still read fine. Pure,
/// so tests pin the mapping.
pub fn tool_display_name(name: &str) -> String {
    match name {
        "shell" | "exec" | "bash" | "command" => "Execute".to_string(),
        "read_file" => "Read file".to_string(),
        "write_file" => "Write file".to_string(),
        "edit" | "edit_file" => "Edit file".to_string(),
        "list_dir" => "List directory".to_string(),
        "ask_user" => "Ask user".to_string(),
        "todo" => "Todo".to_string(),
        "memory_recall" => "Recall memory".to_string(),
        "memory_list" => "List memories".to_string(),
        "memory_propose" => "Propose memory".to_string(),
        "memory_forget" => "Forget memory".to_string(),
        "memory_confirm" => "Confirm memory".to_string(),
        "skills_list" => "List skills".to_string(),
        "skill_read" => "Read skill".to_string(),
        "session_search" => "Search sessions".to_string(),
        "vault_list" => "List vault".to_string(),
        "vault_read" => "Read vault".to_string(),
        "vault_search" => "Search vault".to_string(),
        "vault_archive" => "Archive vault".to_string(),
        "enable_plugin" => "Enable plugin".to_string(),
        "enable_mcp" => "Enable MCP".to_string(),
        other => {
            let mut out = String::new();
            for (i, part) in other.split('_').enumerate() {
                if i > 0 {
                    out.push(' ');
                }
                let mut chars = part.chars();
                match chars.next() {
                    Some(c) => {
                        out.extend(c.to_uppercase());
                        out.extend(chars);
                    }
                    None => {}
                }
            }
            if out.is_empty() {
                other.to_string()
            } else {
                out
            }
        }
    }
}

/// Build the dynamic footer status string from a real tool call: a verb
/// plus the target taken from the actual call arguments.
///
/// - `read_file {"path": "src/main.rs"}` → `Reading src/main.rs`
/// - `shell {"command": "cargo test -p pantheon-tui"}` → `Executing cargo test -p pantheon-tui`
/// - `write_file {"path": "x.rs"}` → `Editing x.rs`
/// - `grep {"pattern": "ripgrep alias"}` → `Searching ripgrep alias`
/// - plan / todo tools → `Planning...`
/// - anything else, or a call with no readable target → `Working...`
///
/// The footer shows exactly one of these strings; the traveling
/// highlight sweeps across its characters.
pub fn status_for_tool(tool: &str, args: &str) -> String {
    let t = tool.to_ascii_lowercase();
    let has = |subs: &[&str]| subs.iter().any(|s| t.contains(s));
    let target = tool_target(args);
    let with_target = |verb: &str| match target {
        Some(ref s) => format!("{verb} {s}"),
        None => format!("{verb}..."),
    };
    if has(&["plan", "todo", "planner"]) {
        "Planning...".to_string()
    } else if has(&["read", "list_dir"]) {
        with_target("Reading")
    } else if has(&["shell", "exec", "bash", "command"]) {
        with_target("Executing")
    } else if has(&[
        "write", "edit", "apply", "patch", "replace", "create", "delete", "mkdir", "move", "rename",
    ]) {
        with_target("Editing")
    } else if has(&["search", "grep", "find", "glob", "lookup", "fetch", "web"]) {
        with_target("Searching")
    } else {
        "Working...".to_string()
    }
}

/// Pull a short human target from tool-call args JSON: the first
/// non-empty `path` / `file` / `command` / `cmd` / `query` / `pattern` /
/// `url` value, first line only, truncated to 56 chars.
fn tool_target(args: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let obj = v.as_object()?;
    for key in ["path", "file", "command", "cmd", "query", "pattern", "url"] {
        if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
            let first: String = s
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .chars()
                .take(56)
                .collect();
            if !first.is_empty() {
                return Some(first);
            }
        }
    }
    None
}

/// Sweep center in char coordinates for `tick` over a `len`-char text.
/// Starts parked off the left edge and wraps seamlessly.
fn sweep_pos(tick: u64, len: usize) -> f64 {
    let margin = 6.0;
    let span = len as f64 + 2.0 * margin;
    -margin + (tick as f64 * 2.0) % span
}

/// Per-character spans for the traveling highlight: near the sweep
/// position → `peak`, far → `base`, smooth Gaussian falloff between.
/// One span per char; the layout never changes.
pub fn sweep_spans(text: &str, tick: u64, base: Color, peak: Color) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let pos = sweep_pos(tick, len);
    let sigma = 4.0;
    chars
        .into_iter()
        .enumerate()
        .map(|(x, ch)| {
            let d = x as f64 - pos;
            let w = (-(d * d) / (2.0 * sigma * sigma)).exp();
            Span::styled(
                ch.to_string(),
                Style::default().fg(lerp_color(base, peak, w)),
            )
        })
        .collect()
}

/// Reusable traveling-highlight sweep over ANY status string: a soft
/// purplish light sweeping left→right through the string's characters,
/// looping while the operation is active. Pure function of `text` +
/// `tick` - no spinners, no cursor characters, no full-line flashing,
/// no layout change. Any status message can use it.
pub fn render_sweep(text: &str, tick: u64, base: Color, peak: Color) -> Line<'static> {
    Line::from(sweep_spans(text, tick, base, peak))
}

/// A soft purplish light that sweeps left→right through the characters
/// of a single status string, looping while the operation is active.
///
/// Reusable: construct with the string, call [`tick`] once per frame
/// while the operation is active, [`render`] the spans each frame - or
/// use the free function [`render_sweep`] for any one-off status
/// message. The sweep never blinks the line, never moves a cursor
/// character, never changes the layout. Swap strings mid-flight with
/// [`set_word`]. When the operation completes, stop ticking and use
/// [`render_plain`]: static dim text, no motion.
pub struct TravelHighlight {
    word: String,
    tick: u64,
}

impl TravelHighlight {
    pub fn new(word: impl Into<String>) -> Self {
        TravelHighlight {
            word: word.into(),
            tick: 0,
        }
    }

    /// Swap the string mid-flight (e.g. the status changed). Resets the
    /// sweep to the left edge so the new string lights up from the
    /// start. No-op when the string is unchanged - the sweep keeps
    /// traveling.
    pub fn set_word(&mut self, word: &str) {
        if self.word != word {
            self.word = word.to_string();
            self.reset();
        }
    }

    /// The plain string, for width calculations. Render output is always
    /// exactly this long - the sweep never changes the layout.
    pub fn text(&self) -> &str {
        &self.word
    }

    /// Advance the sweep one frame. Call only while the operation is
    /// active; the highlight loops seamlessly.
    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    /// Park the sweep off the left edge (turn start).
    pub fn reset(&mut self) {
        self.tick = 0;
    }

    /// Current sweep center, in char coordinates. Exposed for tests.
    pub fn position(&self) -> f64 {
        sweep_pos(self.tick, self.word.chars().count())
    }

    /// Per-character spans: near the sweep → `peak`, far → `base`,
    /// smooth falloff between. One span per char, same total width as
    /// [`text`].
    pub fn render(&self, base: Color, peak: Color) -> Vec<Span<'static>> {
        sweep_spans(&self.word, self.tick, base, peak)
    }

    /// Static dim text for idle/complete: same layout, no motion.
    pub fn render_plain(&self, base: Color) -> Vec<Span<'static>> {
        vec![Span::styled(self.word.clone(), Style::default().fg(base))]
    }
}

fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let l = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
            Color::Rgb(l(ar, br), l(ag, bg), l(ab, bb))
        }
        _ => {
            if t >= 0.5 {
                b
            } else {
                a
            }
        }
    }
}

/// `42.7s` / `850ms` - compact duration for captured background steps.
pub fn fmt_ms(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{ms}ms")
    }
}

/// Lifecycle of one captured step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Running,
    Done,
    Failed,
}

/// One tool call captured from a background task's run.
#[derive(Debug, Clone)]
pub struct BgStep {
    pub name: String,
    pub call_id: String,
    pub status: StepStatus,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
}

impl BgStep {
    pub fn started(name: &str, call_id: &str) -> Self {
        BgStep {
            name: name.to_string(),
            call_id: call_id.to_string(),
            status: StepStatus::Running,
            started_ms: bg::now_ms(),
            finished_ms: None,
        }
    }

    pub fn finish(&mut self, ok: bool) {
        self.status = if ok {
            StepStatus::Done
        } else {
            StepStatus::Failed
        };
        self.finished_ms = Some(bg::now_ms());
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.finished_ms
            .unwrap_or_else(bg::now_ms)
            .saturating_sub(self.started_ms)
    }
}

/// Lifecycle of a spawned subagent, as seen from the parent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubAgentStatus {
    Running,
    Done,
}

/// One delegated subagent. The task text is deliberately absent: the
/// ledger's spawn/completion events don't carry it, and inventing step
/// text would be fabrication.
#[derive(Debug, Clone)]
pub struct SubAgentRecord {
    pub name: String,
    pub status: SubAgentStatus,
    pub parent_run_id: String,
    pub spawned_ms: u64,
    pub finished_ms: Option<u64>,
}

impl SubAgentRecord {
    pub fn elapsed_ms(&self) -> u64 {
        self.finished_ms
            .unwrap_or_else(bg::now_ms)
            .saturating_sub(self.spawned_ms)
    }
}

/// Cap on retained records; the oldest finished ones drop first.
pub const MAX_SUBAGENTS: usize = 8;

/// Feed an `AgentSpawned` / `AgentMessage` event. Duplicate signals for
/// the same live delegation (the engine and the runtime both announce
/// it) collapse into one record.
pub fn record_spawned(records: &mut Vec<SubAgentRecord>, parent_run_id: &str, agent: &str) {
    let name = agent.trim();
    if name.is_empty() {
        return;
    }
    let dup = records.iter().any(|r| {
        r.parent_run_id == parent_run_id && r.name == name && r.status == SubAgentStatus::Running
    });
    if dup {
        return;
    }
    records.push(SubAgentRecord {
        name: name.to_string(),
        status: SubAgentStatus::Running,
        parent_run_id: parent_run_id.to_string(),
        spawned_ms: bg::now_ms(),
        finished_ms: None,
    });
    while records.len() > MAX_SUBAGENTS {
        if let Some(i) = records
            .iter()
            .position(|r| r.status == SubAgentStatus::Done)
        {
            records.remove(i);
        } else {
            records.remove(0);
        }
    }
}

/// Feed an `AgentCompleted` event: the newest live record for this
/// agent under this parent flips to done.
pub fn record_completed(records: &mut Vec<SubAgentRecord>, parent_run_id: &str, agent: &str) {
    if let Some(r) = records.iter_mut().rev().find(|r| {
        r.parent_run_id == parent_run_id
            && r.name == agent.trim()
            && r.status == SubAgentStatus::Running
    }) {
        r.status = SubAgentStatus::Done;
        r.finished_ms = Some(bg::now_ms());
    }
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Activity status for the sidebar agent view: muted text only, no
/// solid badge. `("In progress", amber)` while a turn is live,
/// `("Complete", green)` once a task finished, `("Idle", dim)`
/// otherwise. It heads the task list, sitting under the Context meter.
pub fn activity_status(state: &TuiState) -> (&'static str, Style) {
    let th = &state.theme;
    let dim = Style::default().fg(th.dim);
    let working = !state.ready;
    let has_task = state
        .blocks
        .iter()
        .rev()
        .any(|b| matches!(&b.kind, BlockKind::UserMessage(_)));
    if working {
        ("In progress", Style::default().fg(th.emphasis))
    } else if has_task {
        ("Complete", Style::default().fg(th.success))
    } else {
        ("Idle", dim)
    }
}

/// Build the AGENT VIEW lines for a card `width` wide: the context
/// meter, the activity status, the recent tool steps, and subagent
/// sections. Pure function of state, so tests can assert the layout
/// without a backend.
pub fn build_agent_view_lines(state: &TuiState, width: usize) -> Vec<Line<'static>> {
    let th = &state.theme;
    let dim = Style::default().fg(th.dim);
    let body = Style::default().fg(th.body);
    let inner = width.saturating_sub(2);

    let mut lines: Vec<Line> = Vec::new();

    // Context meter: `Context [████░░░░] 153.0k / 76% used`. The fill
    // blends with the #121212/#1E1E1E theme; the track is a dark solid.
    let live = state.tokens_used + state.turn_estimate;
    let pct = if state.tokens_max > 0 {
        (100.0 * live as f64 / state.tokens_max as f64).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let over = pct >= 80.0;
    lines.push(Line::from(Span::styled("Context", dim)));
    // Adaptive bar: the suffix text is fixed first, the bar takes what
    // fits so the line never overflows narrow cards.
    let suffix = format!(
        "{} / {:.0}% used",
        super::statusbar::fmt_count(live as u64),
        pct
    );
    let cells = inner
        .saturating_sub(suffix.chars().count() + 4)
        .clamp(4, 24);
    let filled = ((pct / 100.0) * cells as f64).round() as usize;
    let mut bar: Vec<Span> = Vec::with_capacity(cells + 2);
    bar.push(Span::styled("[", dim));
    for i in 0..cells {
        let (ch, color) = if i < filled {
            ('█', if over { th.failure } else { th.success })
        } else {
            // Solid dark track, blended with the panel wash.
            ('█', Color::Rgb(52, 52, 62))
        };
        bar.push(Span::styled(ch.to_string(), Style::default().fg(color)));
    }
    bar.push(Span::styled("] ", dim));
    bar.push(Span::styled(
        suffix,
        Style::default().fg(if over { th.failure } else { th.dim }),
    ));
    lines.push(Line::from(bar));
    lines.push(Line::from(""));

    // Activity status heads the task list: it describes the steps
    // below it, so it sits under Context rather than in the top card.
    let (status_text, status_style) = activity_status(state);
    lines.push(Line::from(Span::styled(status_text, status_style)));
    lines.push(Line::from(""));

    // Main-task steps: tool calls since the last user message,
    // chronological. Indented two spaces; no guide characters.
    let mut steps: Vec<(String, Option<std::time::Duration>, StepStatus)> = Vec::new();
    for block in state.blocks.iter().rev() {
        match &block.kind {
            BlockKind::UserMessage(_) => break,
            BlockKind::ToolCall {
                name,
                args,
                ok,
                duration,
                ..
            } => {
                let status = match ok {
                    None => StepStatus::Running,
                    Some(true) => StepStatus::Done,
                    Some(false) => StepStatus::Failed,
                };
                let target = tool_target(args)
                    .unwrap_or_else(|| args.lines().next().unwrap_or("").trim().to_string());
                let display = tool_display_name(name);
                let text = if target.is_empty() {
                    display
                } else {
                    format!("{display} · {target}")
                };
                steps.push((text, *duration, status));
            }
            _ => {}
        }
    }
    steps.reverse();
    for (text, dur, status) in steps {
        let (glyph, gstyle, tstyle) = match status {
            StepStatus::Done => ("✓", Style::default().fg(th.success), dim),
            StepStatus::Running => ("●", Style::default().fg(th.emphasis), body),
            StepStatus::Failed => ("×", Style::default().fg(th.failure), dim),
        };
        let mut label = text;
        if let Some(d) = dur {
            // Clip the tool text first so the duration is never the part
            // that gets cut.
            label = clip(&label, inner.saturating_sub(4).saturating_sub(10))
                .trim_end()
                .to_string();
            label.push_str(" · ");
            label.push_str(&super::fmt_dur(d));
        } else {
            label = clip(&label, inner.saturating_sub(4));
        }
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(glyph, gstyle),
            Span::raw(" "),
            Span::styled(label, tstyle),
        ]));
    }

    // Subagent sections, in spawn order. Only what's real: the agent
    // name plus running/done - never invented step text.
    let subs: Vec<&SubAgentRecord> = state
        .subagents
        .iter()
        .filter(|r| r.parent_run_id == state.session_id)
        .collect();
    for (i, rec) in subs.iter().take(6).enumerate() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("SUBAGENT {:02}", i + 1),
            dim,
        )));
        let (glyph, gstyle, word) = match rec.status {
            SubAgentStatus::Running => ("●", Style::default().fg(th.emphasis), "running"),
            SubAgentStatus::Done => ("✓", Style::default().fg(th.success), "done"),
        };
        let label = format!("{} · {word} · {}", rec.name, fmt_ms(rec.elapsed_ms()));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(glyph, gstyle),
            Span::raw(" "),
            Span::styled(clip(&label, inner.saturating_sub(4)), dim),
        ]));
    }

    lines
}

/// Paint the bottom sidebar card: the agent view. Tail-anchored: the
/// header always stays, and the latest activity wins when the card
/// overflows.
pub fn render_agent_card(f: &mut Frame, area: Rect, state: &TuiState) {
    if area.width < 12 || area.height == 0 {
        return;
    }
    let th = &state.theme;
    let width = area.width as usize;
    let avail = area.height as usize;
    let lines = build_agent_view_lines(state, width);
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
    // Top-anchored: the card fills downward from the top of its area so
    // the context meter and latest activity sit high, not sunk.
    f.render_widget(
        Paragraph::new(shown.into_iter().take(h).collect::<Vec<_>>())
            .style(Style::default().bg(th.panel)),
        Rect::new(area.x, area.y, area.width, h as u16),
    );
}
