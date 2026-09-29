//! Mission-control overview (`^b`): three panes inside one frame.
//!
//! Same session underneath chat — a view toggle, never a state split.
//! Chat and overview share transcript/input/status rendering; the
//! overview swaps the transcript row for nav + live transcript + detail.
//!
//! `OverviewModel` is built by the driver (it owns the state); this
//! module only lays out and paints. `render_frame` returns the inner
//! center rect so the driver can paint the shared transcript into it.

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph},
    Frame,
};

use super::theme::Theme;

/// Nav rows, in order. The driver moves `overview_sel` over these.
pub const NAV_ROWS: usize = 7;
pub const NAV_CURRENT: usize = 0;
pub const NAV_SESSIONS: usize = 1;
pub const NAV_AGENTS: usize = 2;
pub const NAV_SCHEDULE: usize = 3;
pub const NAV_APPROVALS: usize = 4;
pub const NAV_MCP: usize = 5;
pub const NAV_MEMORY: usize = 6;

/// Everything the overview paints, built by the driver from live state.
pub struct OverviewModel {
    /// Left of the top bar, e.g. `RUNNING 003 · openai/gpt-4o-mini`.
    pub top_left: String,
    /// Right of the top bar, e.g. `3 sessions · 1 approval · 2 jobs`.
    pub top_right: String,
    /// Nav labels with their right-aligned counts.
    pub nav: Vec<(String, String)>,
    /// Selected nav row.
    pub sel: usize,
    /// Detail pane title + lines for the selected row.
    pub detail_title: String,
    pub detail: Vec<Line<'static>>,
}

/// Paint the frame, top bar, nav, and detail panes. Returns the inner
/// rect of the center pane for the shared transcript.
pub fn render_frame(f: &mut Frame, area: Rect, model: &OverviewModel, th: &Theme) -> Rect {
    let frame = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.dim))
        .title(Span::styled(
            " Mission control ",
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        ));
    let inner = frame.inner(area);
    f.render_widget(frame, area);

    let rows = Layout::vertical([
        Constraint::Length(1), // top bar
        Constraint::Min(1),    // panes
    ]);
    let [top_area, panes_area] = rows.areas(inner);

    // Top bar: status left, totals right.
    let top = Line::from(vec![
        Span::styled(
            format!(" {} ", model.top_left),
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "{:>width$}",
                model.top_right,
                width = (top_area.width as usize).saturating_sub(model.top_left.len() + 3)
            ),
            Style::default().fg(th.dim),
        ),
    ]);
    f.render_widget(Paragraph::new(top), top_area);

    let cols = Layout::horizontal([
        Constraint::Length(22), // nav
        Constraint::Min(1),     // live transcript
        Constraint::Length(30), // detail
    ]);
    let [nav_area, center_area, detail_area] = cols.areas(panes_area);

    // Nav.
    let mut nav_lines: Vec<Line> = Vec::new();
    nav_lines.push(Line::from(Span::styled(
        " WORKSPACE ",
        Style::default().fg(th.dim),
    )));
    for (i, (label, count)) in model.nav.iter().enumerate() {
        let selected = i == model.sel;
        let marker = if selected { "▸" } else { " " };
        let style = if selected {
            Style::default()
                .fg(th.primary)
                .add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default().fg(th.dim)
        };
        let width = (nav_area.width as usize).saturating_sub(4);
        let row = format!(
            "{marker} {label:<width$}",
            width = width.saturating_sub(count.len() + 1)
        );
        nav_lines.push(Line::from(vec![
            Span::styled(format!("{row} "), style),
            Span::styled(count.clone(), Style::default().fg(th.dim)),
        ]));
    }
    nav_lines.push(Line::from(""));
    nav_lines.push(Line::from(Span::styled(
        " ↑↓ nav · enter open ",
        Style::default().fg(th.dim),
    )));
    let nav = Paragraph::new(nav_lines).block(
        Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(th.dim)),
    );
    f.render_widget(nav, nav_area);

    // Detail.
    let mut detail_lines: Vec<Line> = Vec::new();
    detail_lines.push(Line::from(Span::styled(
        format!(" {} ", model.detail_title),
        Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
    )));
    detail_lines.push(Line::from(""));
    detail_lines.extend(model.detail.iter().cloned());
    let detail = Paragraph::new(detail_lines).block(
        Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(th.dim)),
    );
    f.render_widget(detail, detail_area);

    // Center: single-line frame; the driver paints the transcript inside.
    let center_frame = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.dim))
        .title(Span::styled(" Live ", Style::default().fg(th.dim)));
    let center_inner = center_frame.inner(center_area);
    f.render_widget(center_frame, center_area);
    center_inner
}
