//! Theme parsing, switching, persistence, and themed rendering.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_tui::session::theme::{load_theme_name, save_theme, Theme, DEFAULT_THEME};
use pantheon_tui::session::TuiState;
use tempfile::tempdir;

#[test]
fn builtin_themes_parse_case_insensitively() {
    assert_eq!(Theme::from_name("pantheon").unwrap().name, "pantheon");
    assert_eq!(Theme::from_name("DARK").unwrap().name, "dark");
    assert_eq!(Theme::from_name(" Light ").unwrap().name, "light");
    assert!(Theme::from_name("solarized").is_none());
    assert!(Theme::from_name("").is_none());
    assert_eq!(Theme::all_names(), ["pantheon", "dark", "light"]);
    assert_eq!(DEFAULT_THEME, "pantheon");
}

#[test]
fn themes_are_visually_distinct() {
    // Switching must be observable: the three palettes differ.
    let primaries: Vec<_> = Theme::all_names()
        .iter()
        .map(|n| Theme::from_name(n).unwrap().primary)
        .collect();
    assert_eq!(primaries.len(), 3);
    assert_ne!(primaries[0], primaries[1]);
    assert_ne!(primaries[0], primaries[2]);
    // The default theme preserves today's look.
    let p = Theme::pantheon();
    assert_eq!(format!("{:?}", p.primary), "Cyan");
    assert_eq!(format!("{:?}", p.dim), "DarkGray");
}

#[test]
fn set_theme_switches_and_rejects_unknown() {
    let mut state = TuiState::default();
    assert_eq!(state.theme.name, "pantheon");

    assert!(state.set_theme("dark"));
    assert_eq!(state.theme.name, "dark");
    assert_eq!(state.theme.primary, Theme::dark().primary);

    // Unknown names are rejected and keep the current theme.
    assert!(!state.set_theme("neon"));
    assert_eq!(state.theme.name, "dark");
}

#[test]
fn theme_choice_persists_across_loads() {
    let dir = tempdir().unwrap();
    // Missing config = default.
    assert_eq!(load_theme_name(dir.path()), "pantheon");

    save_theme(dir.path(), "light").unwrap();
    assert_eq!(load_theme_name(dir.path()), "light");

    // An unknown persisted name falls back to the default rather than
    // breaking startup over cosmetics.
    save_theme(dir.path(), "neon").unwrap();
    assert_eq!(load_theme_name(dir.path()), "pantheon");
}

#[test]
fn splash_wordmark_uses_the_active_theme_color() {
    // The renderer reads the palette from the theme: the dark wordmark
    // must carry dark's primary, not the default cyan.
    let lines = pantheon_tui::terminal::splash_lines(80, &Theme::dark());
    let last = lines.last().unwrap();
    let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(text.trim(), "PANTHEON");
    assert_eq!(last.spans[0].style.fg, Some(Theme::dark().primary));
    assert_ne!(last.spans[0].style.fg, Some(Theme::pantheon().primary));
}
