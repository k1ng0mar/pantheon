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
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use super::{bg, BlockKind, TuiState};

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
                if let Some(c) = chars.next() {
                    out.extend(c.to_uppercase());
                    out.extend(chars);
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
/// These strings describe the current step for the last-activity
/// state; they are not rendered as a status line anywhere.
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

/// Live subagent count for the chrome (footer/tab): only Running
/// records count. Callers render nothing at 0 — never a permanent
/// empty indicator (base design layer 2).
pub fn active_subagent_count(records: &[SubAgentRecord]) -> usize {
    records
        .iter()
        .filter(|r| r.status == SubAgentStatus::Running)
        .count()
}

/// Feed an `AgentCompleted` event: the newest live record for this
/// agent under this parent flips to done.
pub fn record_completed(records: &mut [SubAgentRecord], parent_run_id: &str, agent: &str) {
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
/// meter, the activity status, timeline/checkpoint metadata, the
/// selected-tool detail section, and subagent sections. It is
/// deliberately NOT a second transcript: tool steps that merely repeat
/// transcript cards were removed; the tool section shows detail for
/// the one running/expanded tool only. Pure function of state, so
/// tests can assert the layout without a backend.
pub fn build_agent_view_lines(state: &TuiState, width: usize) -> Vec<Line<'static>> {
    let th = &state.theme;
    let dim = Style::default().fg(th.dim);
    let body = Style::default().fg(th.body);
    let inner = width.saturating_sub(2);

    let mut lines: Vec<Line> = Vec::new();

    // Context meter: `Context [████░░░░] 153.0k / 76% used`. The fill
    // blends with the #121212/#1E1E1E theme; the track is a dark solid.
    let live = state.tokens_used + state.turn_estimate;
    let raw_pct = if state.tokens_max > 0 {
        100.0 * live as f64 / state.tokens_max as f64
    } else {
        0.0
    };
    // The bar is clamped to full, but the suffix keeps the raw number
    // past 100% so an over-limit context reads as over, not as 100%.
    // The warn threshold is the one shared threshold
    // (`state.context_warn_pct`, from `[tui] context_warn_pct`, 80 by
    // default) — the same value the composer row's `ctx NN%` uses.
    let pct = raw_pct.clamp(0.0, 100.0);
    let over = pct >= state.context_warn_pct;
    lines.push(Line::from(Span::styled("Context", dim)));
    // Adaptive bar: the suffix text is fixed first, the bar takes what
    // fits so the line never overflows narrow cards.
    let suffix = format!(
        "{} / {:.0}% used",
        super::statusbar::fmt_count(live as u64),
        raw_pct
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
            ('█', super::theme::METER_TRACK)
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

    // Timeline: turn count + latest turn preview. This is metadata
    // (how many turns, where the conversation is), not transcript
    // lines: the transcript itself stays in the left column.
    let turns = super::timeline::build_turns(&state.blocks);
    lines.push(Line::from(Span::styled(
        format!("Turns: {}", turns.len()),
        dim,
    )));
    if let Some(last) = turns.last() {
        lines.push(Line::from(Span::styled(
            format!("  latest · {}", clip(&last.preview, inner.saturating_sub(12))),
            body,
        )));
    }
    if !state.checkpoint_names.is_empty() {
        lines.push(Line::from(Span::styled(
            format!(
                "Checkpoints: {}",
                state.checkpoint_names.join(", ")
            ),
            dim,
        )));
    }
    lines.push(Line::from(""));

    // Selected-tool details (the inspector's core): the running tool
    // call if one is live, else the most recently expanded one.
    // Detail lives here so the transcript card can stay one line;
    // the section is omitted entirely when no tool is running or
    // expanded, rather than showing an empty frame.
    let selected = state.blocks.iter().rev().find_map(|b| match &b.kind {
        BlockKind::ToolCall { ok: None, .. } => Some(b),
        _ => None,
    }).or_else(|| {
        state.blocks.iter().rev().find(|b| {
            matches!(&b.kind, BlockKind::ToolCall { expanded: true, .. })
        })
    });
    if let Some(block) = selected {
        if let BlockKind::ToolCall {
            name,
            args,
            ok,
            duration,
            started,
            error,
            output,
            ..
        } = &block.kind
        {
            let (glyph, gstyle, word) = match ok {
                None => ("●", Style::default().fg(th.emphasis), "running"),
                Some(true) => ("✓", Style::default().fg(th.success), "done"),
                Some(false) => ("×", Style::default().fg(th.failure), "failed"),
            };
            let timing = match duration {
                Some(d) => super::fmt_dur(*d),
                None => started
                    .map(|t| super::fmt_dur(t.elapsed()))
                    .unwrap_or_default(),
            };
            lines.push(Line::from(Span::styled("Tool", dim)));
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(glyph, gstyle),
                Span::raw(" "),
                Span::styled(
                    format!(
                        "{} · {word}{}",
                        tool_display_name(name),
                        if timing.is_empty() {
                            String::new()
                        } else {
                            format!(" · {timing}")
                        }
                    ),
                    body,
                ),
            ]));
            for arg in args.lines().take(4) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", clip(arg.trim(), inner.saturating_sub(4))),
                    dim,
                )));
            }
            if let Some(e) = error.as_deref().filter(|e| !e.is_empty()) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", clip(e.lines().next().unwrap_or(""), inner.saturating_sub(4))),
                    Style::default().fg(th.failure),
                )));
            }
            for out in output.iter().rev().take(4).collect::<Vec<_>>().into_iter().rev() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", clip(out.trim(), inner.saturating_sub(4))),
                    dim,
                )));
            }
            lines.push(Line::from(""));
        }
    }

    // Subagent sections, in spawn order, Running only: the sidebar is
    // the live view, and done agents used to linger here (crowding
    // live ones out). Done agents still render in the transcript's
    // Swarm block, where history belongs. The live count
    // (`active_subagent_count`, Running only, hidden at 0) already
    // feeds the composer row. The sidebar shows the same cap as the
    // record store ([`MAX_SUBAGENTS`]), not a second magic number.
    let subs: Vec<&SubAgentRecord> = state
        .subagents
        .iter()
        .filter(|r| r.parent_run_id == state.session_id && r.status == SubAgentStatus::Running)
        .collect();
    for (i, rec) in subs.iter().take(MAX_SUBAGENTS).enumerate() {
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

    // System metadata, moved here from the splash bottom line: it is
    // available when the sidebar comes out, never in the main view.
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("System", dim)));
    lines.push(Line::from(Span::styled(
        format!("  {} MCP /mcp", state.mcp_servers.len()),
        dim,
    )));
    lines.push(Line::from(Span::styled(
        format!(
            "  {} plugin{} /plugins",
            state.plugin_count,
            if state.plugin_count == 1 { "" } else { "s" },
        ),
        dim,
    )));
    lines.push(Line::from(Span::styled(
        format!("  v{}", env!("CARGO_PKG_VERSION")),
        dim,
    )));

    lines
}

/// Paint the bottom sidebar card: the agent view. Top-anchored: the
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

#[cfg(test)]
mod tests {
    use super::super::TranscriptBlock;
    use super::*;
    use std::time::{Duration, Instant};

    fn state() -> TuiState {
        TuiState::new("test-session".to_string(), "test-model".to_string(), 0)
    }

    fn tool_block(
        name: &str,
        args: &str,
        ok: Option<bool>,
        expanded: bool,
        output: Vec<String>,
    ) -> TranscriptBlock {
        TranscriptBlock {
            kind: BlockKind::ToolCall {
                name: name.to_string(),
                args: args.to_string(),
                ok,
                call_id: format!("c-{name}"),
                started: Some(Instant::now()),
                duration: None,
                error: None,
                tokens: None,
                expanded,
                output,
            },
        }
    }

    fn view_text(state: &TuiState) -> String {
        build_agent_view_lines(state, 40)
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
    fn agent_view_shows_running_tool_detail_not_a_step_list() {
        let mut s = state();
        s.blocks.push(TranscriptBlock {
            kind: BlockKind::UserMessage("do things".to_string()),
        });
        s.blocks.push(tool_block(
            "shell",
            "{\"command\": \"cargo test\"}",
            None,
            false,
            vec![],
        ));
        s.blocks.push(tool_block("read_file", "{\"path\": \"src/main.rs\"}", Some(true), false, vec![]));
        let text = view_text(&s);
        assert!(text.contains("Tool"), "tool section missing: {text}");
        assert!(text.contains("Execute · running"), "got: {text}");
        assert!(
            !text.contains("Read file"),
            "settled tools must not repeat as step lines: {text}"
        );
        assert!(text.contains("Turns: 1"), "got: {text}");
    }

    #[test]
    fn agent_view_shows_expanded_tool_when_nothing_running() {
        let mut s = state();
        s.blocks.push(tool_block(
            "shell",
            "{\"command\": \"ls\"}",
            Some(true),
            true,
            vec!["a.txt".to_string()],
        ));
        let text = view_text(&s);
        assert!(text.contains("Execute · done"), "got: {text}");
        assert!(text.contains("a.txt"), "expanded tool output missing: {text}");
    }

    #[test]
    fn agent_view_shows_running_subagents_only() {
        let mut s = state();
        s.subagents.push(SubAgentRecord {
            name: "live-one".to_string(),
            status: SubAgentStatus::Running,
            parent_run_id: "test-session".to_string(),
            spawned_ms: 0,
            finished_ms: None,
        });
        s.subagents.push(SubAgentRecord {
            name: "done-one".to_string(),
            status: SubAgentStatus::Done,
            parent_run_id: "test-session".to_string(),
            spawned_ms: 0,
            finished_ms: Some(1),
        });
        let text = view_text(&s);
        assert!(text.contains("live-one"), "running agent missing: {text}");
        assert!(
            !text.contains("done-one"),
            "done agents must not crowd the live sidebar (transcript Swarm block keeps them): {text}"
        );
    }

    #[test]
    fn agent_view_shows_checkpoints_and_latest_turn() {
        let mut s = state();
        s.blocks.push(TranscriptBlock {
            kind: BlockKind::UserMessage("first task here".to_string()),
        });
        s.checkpoint_names.push("before-refactor".to_string());
        let text = view_text(&s);
        assert!(text.contains("Turns: 1"), "got: {text}");
        assert!(text.contains("first task here"), "got: {text}");
        assert!(text.contains("Checkpoints: before-refactor"), "got: {text}");
        assert_eq!(Duration::from_secs(0), Duration::ZERO, "sanity");
    }

    #[test]
    fn context_meter_warns_at_the_shared_threshold() {
        // The sidebar meter warns at the same threshold as the
        // composer row: `[tui] context_warn_pct`, 80 by default —
        // not a second, different value.
        let mut s = state();
        s.tokens_max = 1000;
        let suffix_style = |s: &TuiState| {
            build_agent_view_lines(s, 40)[1]
                .spans
                .iter()
                .find(|sp| sp.content.contains("% used"))
                .map(|sp| sp.style)
        };
        s.tokens_used = 790;
        assert_ne!(
            suffix_style(&s).unwrap().fg,
            Some(s.theme.failure),
            "meter must not warn below 80%"
        );
        s.tokens_used = 800;
        assert_eq!(
            suffix_style(&s).unwrap().fg,
            Some(s.theme.failure),
            "meter must warn at 80%"
        );
        // A configured threshold moves the meter with it.
        s.context_warn_pct = 50.0;
        s.tokens_used = 500;
        assert_eq!(
            suffix_style(&s).unwrap().fg,
            Some(s.theme.failure),
            "meter must follow context_warn_pct"
        );
    }

    #[test]
    fn agent_view_shows_system_metadata_moved_from_splash() {
        let mut s = state();
        s.mcp_servers = vec![("srv".to_string(), "ok".to_string())];
        s.plugin_count = 75;
        let text = view_text(&s);
        assert!(text.contains("System"), "got: {text}");
        assert!(text.contains("1 MCP /mcp"), "got: {text}");
        assert!(text.contains("75 plugins /plugins"), "got: {text}");
        assert!(
            text.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))),
            "got: {text}"
        );
    }
}
