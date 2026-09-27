//! Tests for the session tab bar. All pure: the keybinding map, the tab
//! list model, and rendering via ratatui's TestBackend — no TTY needed.

use super::*;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn runs() -> Vec<RunListing> {
    vec![
        (
            "run_aaaaaaaaaaaaaaaa".to_string(),
            "running".to_string(),
            1_700_000_000_000,
            Some("fix the login bug".to_string()),
        ),
        (
            "run_bbbbbbbbbbbbbbbb".to_string(),
            "completed".to_string(),
            1_699_999_900_000,
            None,
        ),
        (
            "run_cccccccccccccccc".to_string(),
            "running".to_string(),
            1_699_999_800_000,
            Some("".to_string()),
        ),
    ]
}

fn busy_map() -> impl Fn(&str) -> bool {
    |id| id == "run_aaaaaaaaaaaaaaaa"
}

#[test]
fn tab_labels_prefer_title_fall_back_to_id_stem() {
    let list = TabList::from_runs(&runs(), busy_map(), "run_bbbbbbbbbbbbbbbb");
    assert_eq!(list.len(), 3);
    assert_eq!(list.tabs()[0].label(), "fix the login bug");
    // No title: id stem with the same skip(4).take(8) the overlays use.
    assert_eq!(list.tabs()[1].label(), "bbbbbbbb");
    // Empty title counts as untitled.
    assert_eq!(list.tabs()[2].label(), "cccccccc");
}

#[test]
fn busy_indicator_comes_from_the_closure() {
    let list = TabList::from_runs(&runs(), busy_map(), "run_aaaaaaaaaaaaaaaa");
    assert!(list.tabs()[0].busy);
    assert!(!list.tabs()[1].busy);
    assert!(!list.tabs()[2].busy);
}

#[test]
fn active_session_is_included_even_without_a_lease() {
    // A fresh /new has no ledger row; the bar must still show the session
    // the user is looking at.
    let list = TabList::from_runs(&runs(), busy_map(), "run_zzz_fresh_session");
    assert_eq!(list.len(), 4);
    assert_eq!(list.active_run_id(), Some("run_zzz_fresh_session"));
    assert_eq!(list.tabs()[0].run_id, "run_zzz_fresh_session");
}

#[test]
fn cycle_wraps_around() {
    let mut list = TabList::from_runs(&runs(), busy_map(), "run_aaaaaaaaaaaaaaaa");
    assert_eq!(list.active_run_id(), Some("run_aaaaaaaaaaaaaaaa"));
    assert_eq!(list.cycle_next(), Some("run_bbbbbbbbbbbbbbbb"));
    assert_eq!(list.cycle_next(), Some("run_cccccccccccccccc"));
    assert_eq!(list.cycle_next(), Some("run_aaaaaaaaaaaaaaaa"));
    assert_eq!(list.cycle_prev(), Some("run_cccccccccccccccc"));
    assert_eq!(list.cycle_prev(), Some("run_bbbbbbbbbbbbbbbb"));
    assert_eq!(list.cycle_prev(), Some("run_aaaaaaaaaaaaaaaa"));
}

#[test]
fn jump_is_one_based_and_rejects_out_of_range() {
    let mut list = TabList::from_runs(&runs(), busy_map(), "run_aaaaaaaaaaaaaaaa");
    assert!(list.jump(2));
    assert_eq!(list.active_run_id(), Some("run_bbbbbbbbbbbbbbbb"));
    // Jumping to the already-active tab is a no-op.
    assert!(!list.jump(2));
    // 0 and past-the-end are ignored, never panic, never move.
    assert!(!list.jump(0));
    assert!(!list.jump(4));
    assert!(!list.jump(99));
    assert_eq!(list.active_run_id(), Some("run_bbbbbbbbbbbbbbbb"));
}

#[test]
fn set_active_run_id_selects_and_reports_unknown() {
    let mut list = TabList::from_runs(&runs(), busy_map(), "run_aaaaaaaaaaaaaaaa");
    assert!(list.set_active_run_id("run_cccccccccccccccc"));
    assert_eq!(list.active_index(), 2);
    assert!(!list.set_active_run_id("run_nope"));
    assert_eq!(list.active_index(), 2);
}

#[test]
fn refresh_preserves_active_tab_by_run_id() {
    let mut list = TabList::from_runs(&runs(), busy_map(), "run_bbbbbbbbbbbbbbbb");
    list.jump(3);
    // Titles arrive later; the selection must survive the rebuild. The
    // driver passes the currently selected session id on each refresh.
    let mut updated = runs();
    updated[2].3 = Some("new title".to_string());
    let current = list.active_run_id().unwrap().to_string();
    list.refresh_from_runs(&updated, busy_map(), &current);
    assert_eq!(list.active_run_id(), Some("run_cccccccccccccccc"));
    assert_eq!(list.tabs()[2].label(), "new title");
}

#[test]
fn set_busy_updates_one_tab() {
    let mut list = TabList::from_runs(&runs(), busy_map(), "run_aaaaaaaaaaaaaaaa");
    list.set_busy("run_bbbbbbbbbbbbbbbb", true);
    assert!(list.tabs()[1].busy);
    list.set_busy("run_bbbbbbbbbbbbbbbb", false);
    assert!(!list.tabs()[1].busy);
    // Unknown id: no-op, no panic.
    list.set_busy("run_nope", true);
}

#[test]
fn keybindings_map() {
    use KeyCode::*;
    use TabAction::*;
    let ctrl = KeyModifiers::CONTROL;
    let alt = KeyModifiers::ALT;
    let shift = KeyModifiers::SHIFT;

    assert_eq!(tab_key_action(Tab, ctrl), Some(Next));
    assert_eq!(tab_key_action(BackTab, ctrl), Some(Prev));
    // Ctrl+Shift+Tab arrives with both modifiers on most terminals.
    assert_eq!(tab_key_action(BackTab, ctrl | shift), Some(Prev));
    // Plain Shift+Tab fallback.
    assert_eq!(tab_key_action(BackTab, shift), Some(Prev));

    assert_eq!(tab_key_action(Char('1'), alt), Some(Jump(1)));
    assert_eq!(tab_key_action(Char('5'), alt), Some(Jump(5)));
    assert_eq!(tab_key_action(Char('9'), alt), Some(Jump(9)));
    assert_eq!(tab_key_action(Char('0'), alt), None);

    // Everything else is untouched: plain Tab, typing, navigation.
    assert_eq!(tab_key_action(Tab, KeyModifiers::empty()), None);
    assert_eq!(tab_key_action(Char('1'), KeyModifiers::empty()), None);
    assert_eq!(tab_key_action(Char('a'), alt), None);
    assert_eq!(tab_key_action(KeyCode::Up, KeyModifiers::empty()), None);
    assert_eq!(tab_key_action(KeyCode::Esc, KeyModifiers::empty()), None);
    assert_eq!(tab_key_action(Enter, ctrl), None);
}

/// Render the bar into a string via TestBackend for assertions.
fn render_to_string(tabs: &TabList, width: u16) -> String {
    let backend = TestBackend::new(width, 1);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| render_tab_bar(f, f.area(), tabs))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut out = String::new();
    for x in 0..width {
        out.push_str(buffer[(x, 0)].symbol());
    }
    out
}

#[test]
fn renders_tabs_with_numbers_busy_glyph_and_active() {
    let list = TabList::from_runs(&runs(), busy_map(), "run_aaaaaaaaaaaaaaaa");
    let text = render_to_string(&list, 80);
    assert!(text.contains("1 fix the login bug"), "got: {text:?}");
    assert!(text.contains("●"), "busy glyph missing: {text:?}");
    assert!(text.contains("2 bbbbbbbb"), "got: {text:?}");
    assert!(text.contains("│"), "separator missing: {text:?}");
}

#[test]
fn narrow_terminal_keeps_active_tab_visible() {
    let mut list = TabList::from_runs(&runs(), busy_map(), "run_aaaaaaaaaaaaaaaa");
    list.jump(3);
    // 20 cols cannot fit all three tabs; the active (3rd) tab must survive.
    let text = render_to_string(&list, 20);
    assert!(text.contains("3 cccccccc"), "active tab clipped: {text:?}");
}

#[test]
fn render_empty_list_draws_nothing() {
    let list = TabList::default();
    let text = render_to_string(&list, 80);
    assert!(text.trim().is_empty(), "got: {text:?}");
}
