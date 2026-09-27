//! Turning widgets into pixels.
//!
//! `widget.rs` holds state and key handling and imports no terminal types,
//! which is what makes it testable without a tty. This file is the other
//! half: the same widgets, rendered. Keeping the two apart is why a widget
//! test can assert on `rows[0].selected` instead of scraping ANSI.
//!
//! Every list widget renders through [`rows_for`], so the selected row looks
//! the same whether it is a provider, a tool group, or a session.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};

use crate::widget::{Confirm, MultiSelect, Row, SearchList, Select, TextInput};

/// How much vertical space a list window gets. Set from the frame height each
/// draw, so a short terminal shows fewer rows instead of overflowing.
fn window(height: u16) -> usize {
    // 4 = the box's two borders plus the title and the hint line.
    (height as usize).saturating_sub(4).max(1)
}

fn frame_box(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
}

/// Render one row: marker, label, description, right-aligned tag.
fn row_line(r: &Row) -> Line<'static> {
    let mut spans = Vec::new();
    if !r.marker.is_empty() {
        spans.push(Span::raw(format!("{} ", r.marker)));
    }
    let bullet = if r.selected { "● " } else { "  " };
    spans.push(Span::raw(bullet.to_string()));
    spans.push(Span::styled(
        r.label.clone(),
        if r.selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        },
    ));
    if !r.desc.is_empty() {
        spans.push(Span::styled(
            format!("  {}", r.desc),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if !r.tag.is_empty() {
        spans.push(Span::styled(
            format!("  [{}]", r.tag),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !r.enabled {
        // Grey the whole row and say why, rather than letting the user pick
        // it and hit a silent refusal.
        spans = spans
            .into_iter()
            .map(|s| Span::styled(s.content, Style::default().fg(Color::DarkGray)))
            .collect();
        spans.push(Span::styled(
            "  (unavailable)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    Line::from(spans)
}

/// The shared list frame: a bordered box, the rows, and a hint footer.
fn draw_list(
    f: &mut ratatui::Frame,
    area: Rect,
    title: &str,
    hint: &str,
    filter: &str,
    empty_reason: &str,
    rows: &[Row],
) {
    let [main, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);
    f.render_widget(Clear, area);
    f.render_widget(frame_box(title), main);

    let inner = main.inner(ratatui::layout::Margin::new(1, 1));
    if rows.is_empty() {
        // An empty list must say which of the two empty cases it is: nothing
        // exists, or the filter excluded everything. They need different
        // reactions from the user.
        let why = if filter.is_empty() {
            empty_reason
        } else {
            "no matches for that filter"
        };
        f.render_widget(
            Paragraph::new(Span::styled(why, Style::default().fg(Color::DarkGray))),
            inner,
        );
    } else {
        let lines: Vec<Line> = rows.iter().map(row_line).collect();
        f.render_widget(Paragraph::new(lines), inner);
    }

    let filter_note = if filter.is_empty() {
        String::new()
    } else {
        format!("  filter: {filter}")
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("{hint}{filter_note}"),
            Style::default().fg(Color::DarkGray),
        )),
        foot,
    );
}

pub fn draw_select(f: &mut ratatui::Frame, area: Rect, s: &mut Select) {
    s.list.set_visible_rows(window(area.height));
    let rows = crate::widget::select_rows(s);
    draw_list(
        f,
        area,
        &s.title,
        &s.hint,
        s.list.filter(),
        &s.empty_reason,
        &rows,
    );
}

pub fn draw_multi(f: &mut ratatui::Frame, area: Rect, m: &mut MultiSelect) {
    m.list.set_visible_rows(window(area.height));
    let rows = crate::widget::multi_rows(m);
    draw_list(
        f,
        area,
        &m.title,
        &m.hint,
        m.list.filter(),
        &m.empty_reason,
        &rows,
    );
}

pub fn draw_search(f: &mut ratatui::Frame, area: Rect, s: &mut SearchList) {
    s.list.set_visible_rows(window(area.height));
    let rows = crate::widget::search_rows(s);
    let mut reason = s.empty_reason.clone();
    if s.list.visible_len() > rows.len() {
        reason = format!("{} matches", s.list.visible_len());
    }
    draw_list(f, area, &s.title, &s.hint, s.list.filter(), &reason, &rows);
}

pub fn draw_text(f: &mut ratatui::Frame, area: Rect, t: &mut TextInput) {
    let [main, foot] = Layout::vertical([Constraint::Length(3), Constraint::Length(1)]).areas(area);
    f.render_widget(Clear, area);
    f.render_widget(frame_box(&t.title), main);

    let shown = t.display();
    let mut lines = vec![Line::from(Span::styled(
        shown,
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    if t.is_empty() && !t.placeholder.is_empty() {
        lines.push(Line::from(Span::styled(
            t.placeholder.clone(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    let inner = main.inner(ratatui::layout::Margin::new(1, 1));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
    f.render_widget(
        Paragraph::new(Span::styled(
            t.hint.clone(),
            Style::default().fg(Color::DarkGray),
        )),
        foot,
    );
}

pub fn draw_confirm(f: &mut ratatui::Frame, area: Rect, c: &mut Confirm) {
    let [main, foot] = Layout::vertical([Constraint::Length(4), Constraint::Length(1)]).areas(area);
    f.render_widget(Clear, area);
    f.render_widget(frame_box(&c.title), main);
    let inner = main.inner(ratatui::layout::Margin::new(1, 1));
    let y = if c.default_yes { "yes" } else { "no" };
    let n = if c.default_yes { "no" } else { "yes" };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(format!("{y} / {n}")),
            Line::from(""),
            Line::from("left/right choose  enter confirm  esc cancels"),
        ]),
        inner,
    );
    f.render_widget(Paragraph::new(Span::raw("")), foot);
}
