//! Todo card for the transcript.
//!
//! Standalone by design: it renders [`TodoCardItem`]s with ratatui only.
//! The backend maps `pantheon_api::todo::TodoItem` onto [`TodoCardItem`]
//! at the integration point. Its palette comes from the theme module
//! (see `session::theme`), not from literals here.
//!
//! ```text
//! ⬢ Working on 2 to-dos
//!   ☒ Made new things
//!   ⊞ Read files
//!   ☐ Edit files
//!   ☐ Give summary
//! ```

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

// The card's palette lives in the theme module, not here: no renderer
// outside `session::theme` names a raw color for the conversation view.
use crate::session::theme::{
    TODO_BLUE as BLUE, TODO_DIM_GRAY as DIM_GRAY, TODO_NEAR_BLACK as NEAR_BLACK,
    TODO_PURPLE as PURPLE,
};

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
        Span::styled(
            if items.is_empty() {
                "No to-dos".to_string()
            } else if remaining == 0 {
                "All to-dos done".to_string()
            } else {
                format!("Working on {remaining} to-dos")
            },
            header_style,
        ),
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
///
/// Widget form of [`todo_card_lines`]. The session sidebar embeds
/// `todo_card_lines` directly (see `render_sidebar_todos`) and does
/// not call this; it exists for standalone embedding.
pub fn render_todo_card(frame: &mut Frame, area: Rect, items: &[TodoCardItem]) {
    let lines = todo_card_lines(items);
    let card = Paragraph::new(lines).style(Style::default().bg(NEAR_BLACK));
    frame.render_widget(card, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(items: &[TodoCardItem]) -> String {
        todo_card_lines(items)
            .first()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .unwrap_or_default()
    }

    fn item(status: TodoStatus) -> TodoCardItem {
        TodoCardItem {
            content: "x".to_string(),
            status,
        }
    }

    #[test]
    fn todo_header_states() {
        assert_eq!(header(&[]), "⬢ No to-dos");
        assert_eq!(header(&[item(TodoStatus::Completed)]), "⬢ All to-dos done");
        assert_eq!(
            header(&[item(TodoStatus::Completed), item(TodoStatus::Pending)]),
            "⬢ Working on 1 to-dos"
        );
    }
}
