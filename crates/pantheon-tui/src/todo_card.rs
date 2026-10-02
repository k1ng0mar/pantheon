//! opencode-style todo card for the transcript.
//!
//! Standalone by design: it renders [`TodoCardItem`]s with ratatui only,
//! no pantheon types, so it compiles against ratatui alone. The backend
//! maps `pantheon_api::todo::TodoItem` onto [`TodoCardItem`] at the
//! integration point.
//!
//! ```text
//! ⬢ Working on 2 to-dos
//!   ☒ Made new things
//!   ⊞ Read files
//!   ☐ Edit files
//!   ☐ Give summary
//! ```

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Exact palette for the card.
pub const PURPLE: Color = Color::Rgb(0xA7, 0x8B, 0xFA);
pub const GREEN: Color = Color::Rgb(0x7E, 0xE7, 0x87);
pub const ORANGE: Color = Color::Rgb(0xF0, 0xA3, 0x5E);
pub const DIM_GRAY: Color = Color::Rgb(0x6B, 0x72, 0x80);
pub const BLUE: Color = Color::Rgb(0x5B, 0x8C, 0xFF);
pub const NEAR_BLACK: Color = Color::Rgb(0x0B, 0x0B, 0x0F);

/// Lifecycle state of one card row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// One row of the card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoCardItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Build the card as styled lines (for embedding in the line-based
/// transcript renderer). [`render_todo_card`] draws these same lines as
/// a widget.
pub fn todo_card_lines(items: &[TodoCardItem]) -> Vec<Line<'_>> {
    let remaining = items
        .iter()
        .filter(|i| i.status != TodoStatus::Completed)
        .count();
    let header_style = Style::default().add_modifier(Modifier::BOLD);
    let mut lines = Vec::with_capacity(items.len() + 1);
    lines.push(Line::from(vec![
        Span::styled(
            "⬢ ",
            Style::default().fg(PURPLE).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("Working on {remaining} to-dos"), header_style),
    ]));
    for item in items {
        let (glyph, glyph_style, text_style) = match item.status {
            TodoStatus::Completed => (
                "☒",
                Style::default().fg(DIM_GRAY),
                Style::default().fg(DIM_GRAY),
            ),
            TodoStatus::InProgress => (
                "⊞",
                Style::default().fg(BLUE).add_modifier(Modifier::BOLD),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            TodoStatus::Pending => (
                "☐",
                Style::default().fg(DIM_GRAY),
                Style::default().fg(DIM_GRAY),
            ),
        };
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(glyph, glyph_style),
            Span::raw(" "),
            Span::styled(item.content.clone(), text_style),
        ]));
    }
    lines
}

/// Render the card into `area`: a bold "⬢ Working on N to-dos" header
/// (N = not-yet-done), then one row per item - ☒ dimmed for done, ⊞
/// highlighted for in-progress, ☐ dimmed for pending.
///
/// The card takes `1 + items.len()` rows; anything taller is left blank,
/// anything shorter clips from the bottom.
pub fn render_todo_card(frame: &mut Frame, area: Rect, items: &[TodoCardItem]) {
    let lines = todo_card_lines(items);
    let card = Paragraph::new(lines).style(Style::default().bg(NEAR_BLACK));
    frame.render_widget(card, area);
}
