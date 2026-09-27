//! Running one component at a time, on the real terminal.
//!
//! The setup wizard is a sequence of these. Each call opens the alternate
//! screen, runs the event loop until the widget submits or cancels, and
//! returns the value. The widgets themselves are in `pantheon-tui` and know
//! nothing about terminals; this is the only place that does.
//!
//! Cancelling returns `None` rather than a default. A wizard that invents an
//! answer the user did not give writes a config they did not ask for, and
//! the failure shows up later as an agent that talks to the wrong provider.

use pantheon_tui::app::{Screen, TuiApp};
use pantheon_tui::widget::{Item, Select, TextInput};

/// Draw and run a `Select` until the user chooses. `None` means cancelled.
pub fn pick_one(title: &str, subtitle: &str, items: Vec<Item>) -> Option<String> {
    let widget = Select::new(title, items).hint(format!(
        "up/down navigate  type to filter  enter select  esc back  ({subtitle})"
    ));
    let mut app = TuiApp::new();
    app.push(Screen::select(subtitle, widget));
    if app.run().is_err() {
        return None;
    }
    app.take_value().and_then(|s| s.one().map(String::from))
}

/// Draw and run a `TextInput`. `None` means cancelled.
pub fn pick_text(title: &str, subtitle: &str, placeholder: &str) -> Option<String> {
    let widget = TextInput::new(title)
        .hint("enter confirm  esc back".to_string())
        .placeholder(placeholder.to_string());
    let mut app = TuiApp::new();
    app.push(Screen::text(subtitle, widget));
    if app.run().is_err() {
        return None;
    }
    app.take_value().and_then(|s| s.text().map(String::from))
}
