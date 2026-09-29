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
///
/// Responsive: at 100+ columns the classic three-column mission control
/// (nav | live transcript | detail). Below that the panes stack
/// vertically — compact nav strip, live transcript, detail — so nothing
/// squeezes to zero width and the transcript stays scrollable.
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

    // Top bar: status left, totals right. The right side yields when the
    // bar is narrower than both halves.
    let top = if top_area.width as usize > model.top_left.len() + 4 {
        Line::from(vec![
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
        ])
    } else {
        // The leading space is part of the budget: ellipsize to
        // width-1 so the line never exceeds the bar.
        Line::from(Span::styled(
            format!(
                " {}",
                middle_ellipsis(&model.top_left, (top_area.width as usize).saturating_sub(1))
            ),
            Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
        ))
    };
    f.render_widget(Paragraph::new(top), top_area);

    if panes_area.width < 100 {
        return render_stacked(f, panes_area, model, th);
    }

    let cols = Layout::horizontal([
        Constraint::Length(22), // nav
        Constraint::Min(1),     // live transcript
        Constraint::Length(30), // detail
    ]);
    let [nav_area, center_area, detail_area] = cols.areas(panes_area);

    // Nav.
    let mut nav_lines: Vec<Line> = build_nav_lines(model, th, nav_area.width as usize);
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
    let detail = Paragraph::new(build_detail_lines(model, th)).block(
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

/// Nav rows for `width`: label left, count right, selected row reversed.
/// Long labels are middle-ellipsized so the count always survives.
fn build_nav_lines(model: &OverviewModel, th: &Theme, width: usize) -> Vec<Line<'static>> {
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
        let width = width.saturating_sub(4);
        let label_w = width.saturating_sub(count.len() + 1);
        let label = middle_ellipsis(label, label_w);
        let row = format!("{marker} {label:<label_w$}");
        nav_lines.push(Line::from(vec![
            Span::styled(format!("{row} "), style),
            Span::styled(count.clone(), Style::default().fg(th.dim)),
        ]));
    }
    nav_lines
}

fn build_detail_lines(model: &OverviewModel, th: &Theme) -> Vec<Line<'static>> {
    let mut detail_lines: Vec<Line> = Vec::new();
    detail_lines.push(Line::from(Span::styled(
        format!(" {} ", model.detail_title),
        Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
    )));
    detail_lines.push(Line::from(""));
    detail_lines.extend(model.detail.iter().cloned());
    detail_lines
}

/// Narrow overview: the three panes stack vertically — compact nav,
/// live transcript, detail — instead of squeezing side by side.
/// Returns the inner rect of the transcript pane.
fn render_stacked(f: &mut Frame, area: Rect, model: &OverviewModel, th: &Theme) -> Rect {
    // Nav is compact: one line per row plus borders. Detail gets a fixed
    // slice; the transcript takes the rest and stays scrollable.
    let nav_h = (model.nav.len() as u16 + 4).min(area.height / 3).max(4);
    let detail_h = 10u16.min(area.height.saturating_sub(nav_h + 3)).max(5);
    let rows = Layout::vertical([
        Constraint::Length(nav_h),
        Constraint::Min(1),
        Constraint::Length(detail_h),
    ]);
    let [nav_area, center_area, detail_area] = rows.areas(area);

    let nav = Paragraph::new(build_nav_lines(model, th, nav_area.width as usize)).block(
        Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(th.dim))
            .title(Span::styled(" nav ", Style::default().fg(th.dim))),
    );
    f.render_widget(nav, nav_area);

    let detail = Paragraph::new(build_detail_lines(model, th)).block(
        Block::bordered()
            .border_type(BorderType::Plain)
            .border_style(Style::default().fg(th.dim))
            .title(Span::styled(" detail ", Style::default().fg(th.dim))),
    );
    f.render_widget(detail, detail_area);

    let center_frame = Block::bordered()
        .border_type(BorderType::Plain)
        .border_style(Style::default().fg(th.dim))
        .title(Span::styled(" Live ", Style::default().fg(th.dim)));
    let center_inner = center_frame.inner(center_area);
    f.render_widget(center_frame, center_area);
    center_inner
}

/// Shorten `s` to at most `max` chars with a middle ellipsis: the head
/// and tail survive, which is what identifies a session/model/path.
/// Never panics on tiny widths.
pub fn middle_ellipsis(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max || max <= 1 {
        return chars.into_iter().take(max).collect();
    }
    let tail = (max - 1) / 2;
    let head = max - 1 - tail;
    let h: String = chars.iter().take(head).collect();
    let t: String = chars
        .iter()
        .rev()
        .take(tail)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{h}…{t}")
}
