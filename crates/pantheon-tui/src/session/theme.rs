//! TUI themes: named semantic color palettes.
//!
//! Every color the session renderer uses flows through [`Theme`]; nothing
//! outside this module names a raw [`Color`] for the conversation view.
//! Three built-ins ship; `/theme [name]` switches live and persists the
//! choice in `[tui]` config.

use ratatui::style::Color;

/// Semantic colors for the session view. Field names are roles, not
/// hues: a theme redefines what "primary" means, renderers never branch
/// on theme names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Registry name (`pantheon`, `dark`, `light`).
    pub name: &'static str,
    /// Headers, message labels, accents, links.
    pub primary: Color,
    /// "Working" spinner and in-progress glyphs.
    pub running: Color,
    /// Ready state and success glyphs.
    pub success: Color,
    /// Warnings, status text, rewind prompts.
    pub warning: Color,
    /// Errors and failure glyphs.
    pub failure: Color,
    /// Reasoning text, hints, placeholders, secondary text.
    pub dim: Color,
    /// The active session tab.
    pub tab_active: Color,
    /// Inactive session tabs.
    pub tab_idle: Color,
}

/// The theme a fresh install renders: today's look, unchanged.
pub const DEFAULT_THEME: &str = "pantheon";

impl Theme {
    /// The default theme (today's look).
    pub fn pantheon() -> Self {
        Self {
            name: "pantheon",
            primary: Color::Cyan,
            running: Color::Yellow,
            success: Color::Green,
            warning: Color::Yellow,
            failure: Color::Red,
            dim: Color::DarkGray,
            tab_active: Color::Yellow,
            tab_idle: Color::DarkGray,
        }
    }

    /// High-contrast on dark backgrounds: brighter primaries and text.
    pub fn dark() -> Self {
        Self {
            name: "dark",
            primary: Color::White,
            running: Color::Yellow,
            success: Color::LightGreen,
            warning: Color::LightYellow,
            failure: Color::LightRed,
            dim: Color::Gray,
            tab_active: Color::White,
            tab_idle: Color::DarkGray,
        }
    }

    /// For light terminal backgrounds: dark-on-light hues only.
    pub fn light() -> Self {
        Self {
            name: "light",
            primary: Color::Blue,
            running: Color::Magenta,
            success: Color::Green,
            warning: Color::Magenta,
            failure: Color::Red,
            dim: Color::DarkGray,
            tab_active: Color::Blue,
            tab_idle: Color::Gray,
        }
    }

    /// Look up a built-in by name (case-insensitive). `None` = unknown.
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_lowercase().as_str() {
            "pantheon" => Some(Self::pantheon()),
            "dark" => Some(Self::dark()),
            "light" => Some(Self::light()),
            _ => None,
        }
    }

    /// Registry names, in display order.
    pub fn all_names() -> [&'static str; 3] {
        ["pantheon", "dark", "light"]
    }
}

/// Read the configured theme name from `[tui] theme`. Missing config or
/// missing key = the default; an unknown name falls back to the default
/// rather than failing startup over cosmetics.
pub fn load_theme_name(data_dir: &std::path::Path) -> String {
    let name = crate::config::Config::load(data_dir)
        .ok()
        .and_then(|c| c.tui)
        .and_then(|t| t.theme)
        .unwrap_or_else(|| DEFAULT_THEME.to_string());
    if Theme::from_name(&name).is_some() {
        name
    } else {
        DEFAULT_THEME.to_string()
    }
}

/// Persist the theme choice to `[tui] theme` in config.toml.
pub fn save_theme(
    data_dir: &std::path::Path,
    name: &str,
) -> Result<(), pantheon_api::error::PantheonError> {
    let mut cfg = crate::config::Config::load(data_dir).unwrap_or_default();
    let mut tui = cfg.tui.unwrap_or_default();
    tui.theme = Some(name.to_string());
    cfg.tui = Some(tui);
    cfg.save(data_dir)
}
