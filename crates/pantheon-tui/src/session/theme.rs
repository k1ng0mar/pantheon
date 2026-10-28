//! TUI themes: named semantic color palettes.
//!
//! Every color the conversation view (transcript, composer, sidebar,
//! tab bar) uses flows through this module: [`Theme`] fields for the
//! per-theme palette, and the fixed chrome constants below for values
//! that are the same in every theme (divider tint, meter track,
//! todo-card palette). Nothing outside this module names a raw
//! [`Color`] for the conversation view. Two deliberate exceptions:
//! modal widget chrome in `render.rs` (pickers/overlays, not the
//! conversation view) and the splash wordmark gradient, whose colors
//! are computed by interpolation, not picked from a palette.
//! Three built-ins ship; `/theme [name]` switches live and persists the
//! choice in `[tui]` config.

use ratatui::style::Color;

/// Subtle column divider tint (sidebar divider column background).
pub const DIVIDER_BG: Color = Color::Rgb(38, 38, 38);

/// Unfilled meter track color for the sidebar context meter.
pub const METER_TRACK: Color = Color::Rgb(52, 52, 62);

/// Todo-card palette (previously literals in `todo_card.rs`).
pub const TODO_PURPLE: Color = Color::Rgb(0xA7, 0x8B, 0xFA);
pub const TODO_GREEN: Color = Color::Rgb(0x7E, 0xE7, 0x87);
pub const TODO_ORANGE: Color = Color::Rgb(0xF0, 0xA3, 0x5E);
pub const TODO_DIM_GRAY: Color = Color::Rgb(0x6B, 0x72, 0x80);
pub const TODO_BLUE: Color = Color::Rgb(0x5B, 0x8C, 0xFF);
pub const TODO_NEAR_BLACK: Color = Color::Rgb(0x0B, 0x0B, 0x0F);

/// Semantic colors for the session view. Field names are roles, not
/// hues: a theme redefines what "primary" means, renderers never branch
/// on theme names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    /// Registry name (`pantheon`, `dark`, `light`).
    pub name: &'static str,
    /// Headers, message labels, accents, links.
    pub primary: Color,
    /// Ready state and success glyphs.
    pub success: Color,
    /// Approval states only: pending approvals, decision cards, rewind prompts.
    pub warning: Color,
    /// Errors and failure glyphs.
    pub failure: Color,
    /// Reasoning text, hints, placeholders, secondary text.
    pub dim: Color,
    /// The active session tab.
    pub tab_active: Color,
    /// Inactive session tabs.
    pub tab_idle: Color,
    /// Subtle wash behind the active tab (browser-style highlight).
    pub tab_active_bg: Color,
    /// Markdown headings in the transcript.
    pub heading: Color,
    /// Inline code, fenced code, and file paths in the transcript.
    pub code: Color,
    /// `**emphasis**` in the transcript.
    pub emphasis: Color,
    /// Default transcript body text (off-white on dark themes).
    pub body: Color,
    /// The input box's left accent border.
    pub input_accent: Color,
    /// Near-black app background.
    pub bg: Color,
    /// Sidebar / panel wash: the only thing separating the right column
    /// from the transcript (no borders, no box-drawing).
    pub panel: Color,
}

/// The theme a fresh install renders: today's look, unchanged.
pub const DEFAULT_THEME: &str = "pantheon";

impl Theme {
    /// The default theme: flat transcript on near-black.
    pub fn pantheon() -> Self {
        Self {
            name: "pantheon",
            primary: Color::Rgb(147, 95, 255),
            success: Color::Rgb(74, 222, 128),
            warning: Color::Rgb(251, 191, 36),
            failure: Color::Rgb(248, 113, 113),
            dim: Color::Rgb(107, 114, 128),
            // Brand purple #7223FF: the active tab accent. Inactive tabs
            // stay dim; nothing else in the palette changes for this.
            tab_active: Color::Rgb(114, 35, 255),
            tab_idle: Color::Rgb(107, 114, 128),
            tab_active_bg: Color::Rgb(0, 0, 0),
            heading: Color::White,
            code: Color::Rgb(237, 237, 237),
            emphasis: Color::Rgb(240, 163, 94),
            body: Color::Rgb(237, 237, 237),
            input_accent: Color::Rgb(147, 95, 255),
            bg: Color::Rgb(0, 0, 0),
            // Distinct from bg: with panel == bg the sidebar wash
            // was invisible and only the divider tint separated the
            // columns. #121212 keeps the wash subtle but real.
            panel: Color::Rgb(18, 18, 18),
        }
    }

    /// High-contrast on dark backgrounds: brighter primaries and text.
    pub fn dark() -> Self {
        Self {
            name: "dark",
            primary: Color::White,
            success: Color::LightGreen,
            warning: Color::LightYellow,
            failure: Color::LightRed,
            dim: Color::Gray,
            // Same brand accent as the pantheon theme; the light theme
            // keeps its own blue accent (its palette is blue-based).
            tab_active: Color::Rgb(114, 35, 255),
            tab_idle: Color::DarkGray,
            tab_active_bg: Color::Rgb(30, 30, 40),
            heading: Color::Rgb(167, 139, 250),
            code: Color::Rgb(126, 231, 135),
            emphasis: Color::Rgb(240, 163, 94),
            body: Color::Rgb(237, 237, 237),
            input_accent: Color::Rgb(91, 140, 255),
            bg: Color::Rgb(10, 10, 14),
            panel: Color::Rgb(24, 24, 30),
        }
    }

    /// For light terminal backgrounds: dark-on-light hues only.
    pub fn light() -> Self {
        Self {
            name: "light",
            primary: Color::Blue,
            success: Color::Green,
            warning: Color::Magenta,
            failure: Color::Red,
            dim: Color::DarkGray,
            tab_active: Color::Blue,
            tab_idle: Color::Gray,
            tab_active_bg: Color::Rgb(229, 231, 235),
            heading: Color::Rgb(109, 40, 217),
            code: Color::Rgb(22, 101, 52),
            emphasis: Color::Rgb(154, 66, 14),
            body: Color::Rgb(17, 24, 39),
            input_accent: Color::Blue,
            bg: Color::Rgb(250, 250, 250),
            panel: Color::Rgb(235, 235, 238),
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
    let mut cfg = crate::config::Config::load_for_save(data_dir)?;
    let mut tui = cfg.tui.unwrap_or_default();
    tui.theme = Some(name.to_string());
    cfg.tui = Some(tui);
    cfg.save(data_dir)
}
