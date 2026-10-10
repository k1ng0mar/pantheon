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

use crate::widget::{Confirm, MultiSelect, Row, Select, TextInput};

/// How much vertical space a list window gets. Set from the frame height each
/// draw, so a short terminal shows fewer rows instead of overflowing.
fn window(height: u16) -> usize {
    // 4 = the box's two borders plus the title and the hint line.
    (height as usize).saturating_sub(4).max(1)
}

/// Rows that fit, accounting for rows that draw a second `meta` line: a
/// two-line row costs double, so a list full of them shows half as many
/// before the box overflows. `uses_meta` is read off the list's items
/// (homogeneous: a list either carries `meta` on every row or none).
fn visible_rows_for(height: u16, uses_meta: bool) -> usize {
    let base = window(height);
    if uses_meta {
        (base / 2).max(1)
    } else {
        base
    }
}

fn frame_box(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .title(format!(" {title} "))
}

/// Render one row to one or two lines: the label line, then a dim
/// second line when the row carries `meta`. The second line is what
/// keeps a long context-plus-price string from clipping against the
/// label on a narrow terminal.
fn row_lines(r: &Row) -> Vec<Line<'static>> {
    let mut head = Vec::new();
    if !r.marker.is_empty() {
        head.push(Span::raw(format!("{} ", r.marker)));
    }
    let bullet = if r.selected { "● " } else { "  " };
    head.push(Span::raw(bullet.to_string()));
    head.push(Span::styled(
        r.label.clone(),
        if r.selected {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        },
    ));
    if !r.desc.is_empty() {
        head.push(Span::styled(
            format!("  {}", r.desc),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if !r.tag.is_empty() {
        head.push(Span::styled(
            format!("  [{}]", r.tag),
            Style::default().fg(Color::Cyan),
        ));
    }
    if !r.enabled {
        // Grey the whole row and say why, rather than letting the user pick
        // it and hit a silent refusal.
        head = head
            .into_iter()
            .map(|s| Span::styled(s.content, Style::default().fg(Color::DarkGray)))
            .collect();
        head.push(Span::styled(
            "  (unavailable)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let mut lines = vec![Line::from(head)];
    if !r.meta.is_empty() {
        // Indented under the bullet so it reads as belonging to the row
        // above, not a new top-level entry.
        lines.push(Line::from(Span::styled(
            format!("    {}", r.meta),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
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
        let lines: Vec<Line> = rows.iter().flat_map(row_lines).collect();
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
    let uses_meta = s.list.items().first().is_some_and(|i| !i.meta.is_empty());
    s.list
        .set_visible_rows(visible_rows_for(area.height, uses_meta));
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
    let uses_meta = m.list.items().first().is_some_and(|i| !i.meta.is_empty());
    m.list
        .set_visible_rows(visible_rows_for(area.height, uses_meta));
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
    let [main, foot] = Layout::vertical([Constraint::Length(6), Constraint::Length(1)]).areas(area);
    f.render_widget(Clear, area);
    f.render_widget(frame_box(&c.title), main);
    let inner = main.inner(ratatui::layout::Margin::new(1, 1));
    let y = if c.default_yes { "yes" } else { "no" };
    let n = if c.default_yes { "no" } else { "yes" };
    f.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                c.question.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(format!("{y} / {n}")),
            Line::from(""),
            // The widget's own hint, not a hardcoded one: it names the
            // keys `handle_key` actually honors (y/n/enter/esc).
            Line::from(Span::styled(c.hint(), Style::default().fg(Color::DarkGray))),
        ]),
        inner,
    );
    f.render_widget(Paragraph::new(Span::raw("")), foot);
}
