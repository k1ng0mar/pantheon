//! Hermes-inspired transcript blocks, adapted to Pantheon's look.
//!
//! Pure renderers: every block maps to `Vec<Line<'static>>` given the
//! theme. No terminal types, no session state — the same split as
//! `widget.rs`/`render.rs`, so tests assert on spans instead of ANSI.
//!
//! What this adds over the current transcript:
//! - [`ThoughtView`]: reasoning as `+ Thought · 4.0s`, collapsed by
//!   default, expandable to the full text with its duration.
//! - [`ToolView`]: tool calls as `› server.tool_name` rows with a live
//!   marker while running, ✓/× on settle, expandable args/error detail.
//! - [`ShellView`]: shell commands as `$ command` cards whose long output
//!   collapses behind an `(N earlier lines)` expander.
//! - [`background_jobs_line`]: the `↓ N` footer indicator for background
//!   work, and [`jump_to_latest_line`] for the scrolled-up affordance.
//!
//! Expansion state (`expanded`) is owned here as plain data. Wiring it to
//! keys (Enter to expand/collapse) and feeding live durations belongs in
//! `session.rs`, which the concurrent fix pass is editing — that
//! integration is intentionally left for after it settles.

use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::diffview::DiffLine;
use crate::session::theme::Theme;

/// How many output lines a collapsed shell card keeps visible.
pub const SHELL_VISIBLE_LINES: usize = 8;

/// `4.0s`, `0.4s` — the Hermes-style duration suffix.
pub fn fmt_duration(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

fn dim_span(th: &Theme, text: String) -> Span<'static> {
    Span::styled(text, Style::default().fg(th.dim))
}

/// A reasoning block: `+ Thought · 4.0s` collapsed, full text expanded.
///
/// The `+`/`−` marker is the amber highlight; everything else stays dim so
/// old reasoning never shouts over answers. Collapsed is the default —
/// settled thought should read as one quiet line, not a wall.
#[derive(Debug, Clone)]
pub struct ThoughtView {
    pub text: String,
    pub duration: Option<Duration>,
    pub expanded: bool,
}

impl ThoughtView {
    pub fn collapsed(text: impl Into<String>, duration: Option<Duration>) -> Self {
        Self {
            text: text.into(),
            duration,
            expanded: false,
        }
    }

    /// Header text without styling, for tests and screen readers.
    pub fn header_text(&self) -> String {
        let marker = if self.expanded { "−" } else { "+" };
        match self.duration {
            Some(d) => format!("{marker} Thought · {}", fmt_duration(d)),
            None => format!("{marker} Thought"),
        }
    }

    pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
        let marker = if self.expanded { "−" } else { "+" };
        let mut header = vec![Span::styled(
            format!("{marker} "),
            Style::default()
                .fg(th.emphasis)
                .add_modifier(Modifier::BOLD),
        )];
        header.push(dim_span(th, "Thought".to_string()));
        if let Some(d) = self.duration {
            header.push(dim_span(th, format!(" · {}", fmt_duration(d))));
        }
        let mut out = vec![Line::from(header)];
        if self.expanded {
            let body = Style::default().fg(th.dim).add_modifier(Modifier::ITALIC);
            for line in self.text.lines() {
                out.push(Line::from(Span::styled(format!("  {line}"), body)));
            }
        }
        out
    }
}

/// Tool-call lifecycle for the row marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Still running: `›` in the success color, the "live" signal.
    Running,
    Succeeded,
    Failed,
    /// Stopped from the outside: honest dim `■`, never a spinner.
    Interrupted,
}

/// One tool-call row: `› server.tool_name · 0.4s ✓`.
///
/// The row is a single quiet line; Enter expands it to args and the
/// error/result detail. Names arrive pre-resolved to display form (the
/// registry id never reaches the transcript).
#[derive(Debug, Clone)]
pub struct ToolView {
    pub display_name: String,
    pub status: ToolStatus,
    pub duration: Option<Duration>,
    /// Pre-formatted token count (`154.2k`), if known.
    pub tokens: Option<String>,
    pub expanded: bool,
    /// Args lines shown when expanded.
    pub args: Vec<String>,
    /// Error text (failure) or result summary, shown when expanded.
    pub detail: Vec<String>,
}

impl ToolView {
    /// Marker glyph and its color per status.
    fn marker(&self, th: &Theme) -> (String, Style) {
        match self.status {
            ToolStatus::Running => (
                "›".to_string(),
                Style::default().fg(th.success).add_modifier(Modifier::BOLD),
            ),
            ToolStatus::Succeeded => ("✓".to_string(), Style::default().fg(th.success)),
            ToolStatus::Failed => ("×".to_string(), Style::default().fg(th.failure)),
            ToolStatus::Interrupted => ("■".to_string(), Style::default().fg(th.dim)),
        }
    }

    pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
        let (glyph, glyph_style) = self.marker(th);
        let mut header = vec![
            Span::styled(format!("{glyph} "), glyph_style),
            dim_span(th, self.display_name.clone()),
        ];
        if let Some(d) = self.duration {
            header.push(dim_span(th, format!(" · {}", fmt_duration(d))));
        }
        if let Some(t) = &self.tokens {
            header.push(dim_span(th, format!(" · {t}")));
        }
        let mut out = vec![Line::from(header)];
        if self.expanded {
            let arg_style = Style::default().fg(th.dim);
            for arg in &self.args {
                out.push(Line::from(Span::styled(format!("  {arg}"), arg_style)));
            }
            for line in &self.detail {
                let style = match self.status {
                    ToolStatus::Failed => Style::default().fg(th.failure),
                    _ => arg_style,
                };
                let prefix = if self.status == ToolStatus::Failed {
                    "  ✕ "
                } else {
                    "  "
                };
                out.push(Line::from(Span::styled(format!("{prefix}{line}"), style)));
            }
        }
        out
    }
}

/// A shell command card: `$ find . -name '*.rs'` with its output.
///
/// Long output collapses to the last [`SHELL_VISIBLE_LINES`] lines behind
/// an `(N earlier lines)` expander, so a chatty command never floods the
/// transcript. The header carries the same status marker as [`ToolView`].
#[derive(Debug, Clone)]
pub struct ShellView {
    pub command: String,
    pub output: Vec<String>,
    pub status: ToolStatus,
    pub duration: Option<Duration>,
    pub expanded: bool,
}

impl ShellView {
    /// `(3 earlier lines)` when collapsed and output overflows, else `None`.
    pub fn hidden_count(&self) -> Option<usize> {
        if self.expanded {
            return None;
        }
        self.output
            .len()
            .checked_sub(SHELL_VISIBLE_LINES)
            .filter(|&n| n > 0)
    }

    pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
        let (glyph, glyph_style) = match self.status {
            ToolStatus::Running => (
                "›",
                Style::default().fg(th.success).add_modifier(Modifier::BOLD),
            ),
            ToolStatus::Succeeded => ("✓", Style::default().fg(th.success)),
            ToolStatus::Failed => ("×", Style::default().fg(th.failure)),
            ToolStatus::Interrupted => ("■", Style::default().fg(th.dim)),
        };
        let mut header = vec![
            dim_span(th, "$ ".to_string()),
            Span::styled(self.command.clone(), Style::default().fg(th.body)),
        ];
        if let Some(d) = self.duration {
            header.push(dim_span(th, format!(" · {}", fmt_duration(d))));
        }
        header.push(Span::styled(format!(" {glyph}"), glyph_style));
        let mut out = vec![Line::from(header)];

        let shown: &[String] = match self.hidden_count() {
            Some(hidden) => &self.output[hidden..],
            None => &self.output,
        };
        if let Some(hidden) = self.hidden_count() {
            out.push(Line::from(dim_span(
                th,
                format!("  ({hidden} earlier lines)"),
            )));
        }
        let out_style = Style::default().fg(th.dim);
        for line in shown {
            out.push(Line::from(Span::styled(format!("  {line}"), out_style)));
        }
        out
    }
}

/// Footer indicator for background work: `↓ 2 shells`, dim.
///
/// Renders in the footer's right cluster next to the context meter. Zero
/// jobs renders nothing — the footer stays quiet when there is nothing to
/// say. Callers skip the line when `jobs == 0`.
pub fn background_jobs_line(jobs: usize, th: &Theme) -> Option<Line<'static>> {
    if jobs == 0 {
        return None;
    }
    let noun = if jobs == 1 { "shell" } else { "shells" };
    Some(Line::from(dim_span(th, format!("↓ {jobs} {noun}"))))
}

/// Appears when the transcript is scrolled up: `Jump to latest ↓`.
///
/// Muted blue (primary), not amber — it is navigation, not a decision.
pub fn jump_to_latest_line(th: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "Jump to latest ↓",
        Style::default().fg(th.primary),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn theme() -> Theme {
        Theme::pantheon()
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
    fn thought_collapsed_shows_duration() {
        let v = ThoughtView::collapsed("some long reasoning", Some(Duration::from_millis(4000)));
        let t = text(&v.lines(&theme()));
        assert!(t.contains("+ Thought · 4.0s"), "got: {t}");
        assert!(!t.contains("some long reasoning"), "body leaked: {t}");
    }

    #[test]
    fn thought_expanded_shows_body() {
        let mut v = ThoughtView::collapsed("line one\nline two", Some(Duration::from_millis(400)));
        v.expanded = true;
        let t = text(&v.lines(&theme()));
        assert!(t.contains("− Thought · 0.4s"), "got: {t}");
        assert!(t.contains("line one") && t.contains("line two"), "got: {t}");
    }

    #[test]
    fn thought_without_duration_omits_suffix() {
        let v = ThoughtView::collapsed("x", None);
        assert_eq!(v.header_text(), "+ Thought");
    }

    #[test]
    fn tool_row_markers_per_status() {
        let base = ToolView {
            display_name: "memory.list_projects".to_string(),
            status: ToolStatus::Running,
            duration: None,
            tokens: None,
            expanded: false,
            args: vec![],
            detail: vec![],
        };
        let t = text(&base.lines(&theme()));
        assert!(t.contains("› memory.list_projects"), "got: {t}");

        for (status, glyph) in [
            (ToolStatus::Succeeded, "✓"),
            (ToolStatus::Failed, "×"),
            (ToolStatus::Interrupted, "■"),
        ] {
            let v = ToolView {
                status,
                ..base.clone()
            };
            let t = text(&v.lines(&theme()));
            assert!(t.contains(glyph), "status {status:?}: {t}");
            assert!(t.contains("memory.list_projects"), "status {status:?}: {t}");
        }
    }

    #[test]
    fn tool_row_expanded_shows_args_and_error() {
        let v = ToolView {
            display_name: "shell".to_string(),
            status: ToolStatus::Failed,
            duration: Some(Duration::from_millis(1200)),
            tokens: Some("1.2k".to_string()),
            expanded: true,
            args: vec!["cmd: rm -rf /".to_string()],
            detail: vec!["permission denied".to_string()],
        };
        let t = text(&v.lines(&theme()));
        assert!(t.contains("· 1.2s"), "got: {t}");
        assert!(t.contains("cmd: rm -rf /"), "got: {t}");
        assert!(t.contains("permission denied"), "got: {t}");
    }

    #[test]
    fn tool_row_collapsed_hides_detail() {
        let v = ToolView {
            display_name: "shell".to_string(),
            status: ToolStatus::Succeeded,
            duration: None,
            tokens: None,
            expanded: false,
            args: vec!["secret arg".to_string()],
            detail: vec![],
        };
        let t = text(&v.lines(&theme()));
        assert!(!t.contains("secret arg"), "leaked: {t}");
    }

    #[test]
    fn shell_card_collapses_long_output() {
        let output: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let v = ShellView {
            command: "find . -name '*.rs'".to_string(),
            output,
            status: ToolStatus::Succeeded,
            duration: Some(Duration::from_secs(2)),
            expanded: false,
        };
        assert_eq!(v.hidden_count(), Some(12));
        let t = text(&v.lines(&theme()));
        assert!(t.contains("$ find . -name '*.rs'"), "got: {t}");
        assert!(t.contains("(12 earlier lines)"), "got: {t}");
        assert!(t.contains("line 20"), "got: {t}");
        assert!(!t.contains("\n  line 1\n"), "early line leaked: {t}");
    }

    #[test]
    fn shell_card_expanded_shows_everything() {
        let output: Vec<String> = (1..=20).map(|i| format!("line {i}")).collect();
        let v = ShellView {
            command: "ls".to_string(),
            output,
            status: ToolStatus::Succeeded,
            duration: None,
            expanded: true,
        };
        assert_eq!(v.hidden_count(), None);
        let t = text(&v.lines(&theme()));
        assert!(!t.contains("earlier lines"), "got: {t}");
        assert!(t.contains("line 1") && t.contains("line 20"), "got: {t}");
    }

    #[test]
    fn shell_card_short_output_has_no_expander() {
        let v = ShellView {
            command: "pwd".to_string(),
            output: vec!["/home/hatch".to_string()],
            status: ToolStatus::Succeeded,
            duration: None,
            expanded: false,
        };
        assert_eq!(v.hidden_count(), None);
        let t = text(&v.lines(&theme()));
        assert!(!t.contains("earlier lines"), "got: {t}");
    }

    #[test]
    fn background_indicator_counts_and_pluralizes() {
        assert!(background_jobs_line(0, &theme()).is_none());
        let one = text(&[background_jobs_line(1, &theme()).unwrap()]);
        assert!(one.contains("↓ 1 shell"), "got: {one}");
        assert!(!one.contains("shells"), "got: {one}");
        let two = text(&[background_jobs_line(2, &theme()).unwrap()]);
        assert!(two.contains("↓ 2 shells"), "got: {two}");
    }

    #[test]
    fn jump_to_latest_renders() {
        let t = text(&[jump_to_latest_line(&theme())]);
        assert!(t.contains("Jump to latest ↓"), "got: {t}");
    }

    #[test]
    fn fmt_duration_shapes() {
        assert_eq!(fmt_duration(Duration::from_millis(4000)), "4.0s");
        assert_eq!(fmt_duration(Duration::from_millis(400)), "0.4s");
    }

    #[test]
    fn numbered_diff_tracks_real_line_numbers() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nB\nc\nd\nE\n";
        let lines = numbered_diff(old, new);
        let dels: Vec<_> = lines
            .iter()
            .filter(|l| matches!(l.kind, NumberedKind::Del(_)))
            .collect();
        let adds: Vec<_> = lines
            .iter()
            .filter(|l| matches!(l.kind, NumberedKind::Add(_)))
            .collect();
        assert_eq!(dels.len(), 2);
        assert_eq!(dels[0].old_no, Some(2));
        assert_eq!(dels[1].old_no, Some(5));
        assert_eq!(adds.len(), 2);
        assert_eq!(adds[0].new_no, Some(2));
        assert_eq!(adds[1].new_no, Some(5));
    }

    #[test]
    fn edit_view_renders_header_and_numbered_lines() {
        let v = EditView::from_texts("src/main.rs", "a\nb\n", "a\nB\n", true);
        let t = text(&v.lines(&theme()));
        assert!(t.contains("← Edit src/main.rs"), "got: {t}");
        assert!(t.contains("-b"), "got: {t}");
        assert!(t.contains("+B"), "got: {t}");
        // Real line numbers show in the gutter.
        assert!(t.contains("2"), "got: {t}");
    }

    #[test]
    fn edit_view_collapses_long_diffs() {
        let old: String = (1..=40).map(|i| format!("line {i}\n")).collect();
        let new: String = (1..=40)
            .map(|i| {
                if i % 2 == 0 {
                    format!("LINE {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect();
        let v = EditView::from_texts("big.rs", &old, &new, false);
        assert!(v.hidden_count() > 0);
        let t = text(&v.lines(&theme()));
        assert!(t.contains("more lines"), "got: {t}");

        let v2 = EditView::from_texts("big.rs", &old, &new, true);
        assert_eq!(v2.hidden_count(), 0);
        let t2 = text(&v2.lines(&theme()));
        assert!(!t2.contains("more lines"), "got: {t2}");
    }

    #[test]
    fn activity_summary_rows() {
        let one = text(&[activity_summary_line(1, "search", &theme())]);
        assert!(one.contains("→ Explored: 1 search"), "got: {one}");
        let many = text(&[activity_summary_line(3, "read", &theme())]);
        assert!(many.contains("→ Explored: 3 reads"), "got: {many}");
        let searches = text(&[activity_summary_line(2, "search", &theme())]);
        assert!(searches.contains("2 searches"), "got: {searches}");
    }

    #[test]
    fn burst_object_classifies() {
        assert_eq!(burst_object("web_search"), "search");
        assert_eq!(burst_object("read_file"), "read");
        assert_eq!(burst_object("shell"), "tool");
    }

    #[test]
    fn run_stats_formatting() {
        assert_eq!(fmt_elapsed(Duration::from_secs(270)), "4m 30s");
        assert_eq!(fmt_elapsed(Duration::from_millis(4400)), "4.4s");
        assert_eq!(
            run_stats_segment(Duration::from_secs(270), 7317),
            "· 4m 30s · 27.1 tok/s"
        );
    }
}

/// How many diff lines a collapsed edit card keeps visible.
pub const EDIT_VISIBLE_LINES: usize = 12;

/// A diff line annotated with real file line numbers, parsed out of the
/// `@@ -a,b +c,d @@` hunk headers [`crate::diffview::unified_diff`]
/// emits. Removed lines carry the old-file number, added lines the
/// new-file number, context lines both.
#[derive(Debug, Clone)]
pub struct NumberedDiffLine {
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub kind: NumberedKind,
}

#[derive(Debug, Clone)]
pub enum NumberedKind {
    Hunk(String),
    Context(String),
    Add(String),
    Del(String),
    Truncated(usize),
}

/// Parse `@@ -573,4 +578,6 @@` into the 1-based starting lines.
fn parse_hunk_header(h: &str) -> Option<(u32, u32)> {
    let h = h.strip_prefix("@@ -")?;
    let (old, rest) = h.split_once(" +")?;
    let new = rest.split_whitespace().next()?;
    let a: u32 = old.split(',').next()?.parse().ok()?;
    let b: u32 = new.split(',').next()?.parse().ok()?;
    Some((a, b))
}

/// [`crate::diffview::unified_diff`] plus file line numbers. `FileHeader`
/// lines are dropped — [`EditView`] renders the path itself.
pub fn numbered_diff(old: &str, new: &str) -> Vec<NumberedDiffLine> {
    let mut out = Vec::new();
    let (mut ao, mut bo) = (0u32, 0u32);
    for dl in crate::diffview::unified_diff(old, new) {
        match dl {
            DiffLine::FileHeader(_) => {}
            DiffLine::Hunk(h) => {
                if let Some((a, b)) = parse_hunk_header(&h) {
                    ao = a;
                    bo = b;
                }
                out.push(NumberedDiffLine {
                    old_no: None,
                    new_no: None,
                    kind: NumberedKind::Hunk(h),
                });
            }
            DiffLine::Context(c) => {
                out.push(NumberedDiffLine {
                    old_no: Some(ao),
                    new_no: Some(bo),
                    kind: NumberedKind::Context(c),
                });
                ao += 1;
                bo += 1;
            }
            DiffLine::Del(d) => {
                out.push(NumberedDiffLine {
                    old_no: Some(ao),
                    new_no: None,
                    kind: NumberedKind::Del(d),
                });
                ao += 1;
            }
            DiffLine::Add(a) => {
                out.push(NumberedDiffLine {
                    old_no: None,
                    new_no: Some(bo),
                    kind: NumberedKind::Add(a),
                });
                bo += 1;
            }
            DiffLine::Truncated(n) => out.push(NumberedDiffLine {
                old_no: None,
                new_no: None,
                kind: NumberedKind::Truncated(n),
            }),
        }
    }
    out
}

/// A file edit card: `← Edit <path>` over a line-numbered diff.
///
/// Red strikethrough for removals, green for additions, dim line numbers
/// and hunk headers. Long diffs collapse behind `(N more lines)`; the
/// collapsed card keeps the first [`EDIT_VISIBLE_LINES`] diff lines.
#[derive(Debug, Clone)]
pub struct EditView {
    pub path: String,
    pub lines: Vec<NumberedDiffLine>,
    pub expanded: bool,
}

impl EditView {
    pub fn from_texts(path: impl Into<String>, old: &str, new: &str, expanded: bool) -> Self {
        Self {
            path: path.into(),
            lines: numbered_diff(old, new),
            expanded,
        }
    }

    /// Diff body lines hidden while collapsed.
    pub fn hidden_count(&self) -> usize {
        if self.expanded {
            0
        } else {
            self.lines.len().saturating_sub(EDIT_VISIBLE_LINES)
        }
    }

    fn render_diff_line(&self, l: &NumberedDiffLine, th: &Theme) -> Line<'static> {
        // Line number gutter: the side that exists for this line kind.
        let no = l.old_no.or(l.new_no).map(|n| format!("{n:>4} "));
        let gutter = Span::styled(
            no.unwrap_or_else(|| "     ".to_string()),
            Style::default().fg(th.dim),
        );
        match &l.kind {
            NumberedKind::Hunk(h) => Line::from(vec![
                gutter,
                Span::styled(h.clone(), Style::default().fg(th.dim)),
            ]),
            NumberedKind::Context(c) => Line::from(vec![
                gutter,
                Span::styled(format!(" {c}"), Style::default().fg(th.dim)),
            ]),
            NumberedKind::Add(a) => Line::from(vec![
                gutter,
                Span::styled(format!("+{a}"), Style::default().fg(th.success)),
            ]),
            NumberedKind::Del(d) => Line::from(vec![
                gutter,
                Span::styled(
                    format!("-{d}"),
                    Style::default()
                        .fg(th.failure)
                        .add_modifier(Modifier::CROSSED_OUT),
                ),
            ]),
            NumberedKind::Truncated(n) => Line::from(Span::styled(
                format!("  … {n} more lines"),
                Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
            )),
        }
    }

    pub fn lines(&self, th: &Theme) -> Vec<Line<'static>> {
        let mut out = vec![Line::from(vec![
            Span::styled(
                "← ",
                Style::default()
                    .fg(th.emphasis)
                    .add_modifier(Modifier::BOLD),
            ),
            dim_span(th, "Edit ".to_string()),
            Span::styled(self.path.clone(), Style::default().fg(th.body)),
        ])];
        let shown = if self.expanded {
            &self.lines[..]
        } else {
            &self.lines[..self.lines.len().min(EDIT_VISIBLE_LINES)]
        };
        for l in shown {
            out.push(self.render_diff_line(l, th));
        }
        let hidden = self.hidden_count();
        if hidden > 0 {
            out.push(Line::from(Span::styled(
                format!("  ({hidden} more lines)"),
                Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
            )));
        }
        out
    }
}

/// Map a tool's display name to the object word for burst summaries:
/// search-like tools → "search", read-like → "read", anything else → "tool".
pub fn burst_object(tool_display_name: &str) -> &'static str {
    let n = tool_display_name.to_lowercase();
    if n.contains("search") || n.contains("grep") || n.contains("find") {
        "search"
    } else if n.contains("read") || n.contains("cat") || n.contains("list") || n.contains("glob") {
        "read"
    } else {
        "tool"
    }
}

fn plural_object(object: &str, count: usize) -> String {
    if count == 1 {
        return object.to_string();
    }
    match object {
        "search" => "searches".to_string(),
        _ => format!("{object}s"),
    }
}

/// Condensed activity row for a burst of read-only tool calls:
/// `→ Explored: 1 search`, `→ Explored: 3 reads`.
///
/// The `→` is dim; the row is deliberately quieter than a tool card —
/// it summarizes, it does not announce.
pub fn activity_summary_line(count: usize, object: &str, th: &Theme) -> Line<'static> {
    Line::from(vec![
        dim_span(th, "→ ".to_string()),
        dim_span(
            th,
            format!("Explored: {count} {}", plural_object(object, count)),
        ),
    ])
}

/// `4m 30s`; under a minute, `12.4s`.
pub fn fmt_elapsed(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

/// `27.1 tok/s`; `—` when the turn was instant (no division by zero).
pub fn fmt_tok_per_sec(tokens: u64, elapsed: Duration) -> String {
    let s = elapsed.as_secs_f64();
    if s <= 0.0 {
        return "—".to_string();
    }
    format!("{:.1} tok/s", tokens as f64 / s)
}

/// Footer suffix for a completed turn: `· 4m 30s · 27.1 tok/s`.
///
/// Pure formatting — splicing this into `render_footer`'s right cluster
/// (next to the context meter) needs `session.rs`, which the concurrent
/// fix pass is editing. Left as a documented integration point.
pub fn run_stats_segment(elapsed: Duration, tokens: u64) -> String {
    format!(
        "· {} · {}",
        fmt_elapsed(elapsed),
        fmt_tok_per_sec(tokens, elapsed)
    )
}
