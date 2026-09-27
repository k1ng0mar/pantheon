//! Tests for the TUI component layer.
//!
//! Every test here drives the shipped widget through `handle_key` with no
//! terminal, no alternate screen, and no escape-sequence timing. That is the
//! property the whole design exists for: a component that can only be tested
//! by launching a terminal is a component nobody tests.

use super::*;

fn providers() -> Vec<Item> {
    vec![
        Item::new("OpenRouter", "openrouter")
            .desc("aggregator")
            .tag("paid"),
        Item::new("OpenAI", "openai")
            .desc("frontier lab")
            .tag("paid"),
        Item::new("LM Studio", "lmstudio")
            .desc("local, no key needed")
            .tag("free"),
        Item::new("Nous", "nous").desc("open models").tag("free"),
    ]
}

// ---------------------------------------------------------------------------
// ListState: filtering and the cursor invariant
// ---------------------------------------------------------------------------

#[test]
fn filtering_is_case_insensitive_across_every_column() {
    let mut s = Select::new("model", providers());
    for c in "LMSTUDIO".chars() {
        s.handle_key(Key::Char(c));
    }
    assert_eq!(s.list.visible_len(), 1, "one match for a full-label filter");
    assert_eq!(s.selected_value().as_deref(), Some("lmstudio"));
}

#[test]
fn filter_matches_the_value_not_just_the_label() {
    // A provider row's value is its catalog id, which the user never sees.
    // Filtering on the value keeps the two spellings interchangeable.
    let mut s = Select::new("model", providers());
    s.handle_key(Key::Char('o'));
    s.handle_key(Key::Char('p'));
    s.handle_key(Key::Char('e'));
    s.handle_key(Key::Char('n'));
    assert!(s.list.visible_len() >= 1);
    assert!(s
        .list
        .visible()
        .iter()
        .all(|i| providers()[*i].matches("open")));
}

#[test]
fn a_narrowing_filter_resets_the_cursor_to_the_top() {
    let mut s = Select::new("model", providers());
    s.handle_key(Key::Down);
    s.handle_key(Key::Down);
    assert_eq!(s.list.cursor(), 2, "cursor moved down twice");
    s.handle_key(Key::Char('o'));
    assert_eq!(
        s.list.cursor(),
        0,
        "filtering must not leave the highlight on an unrelated row"
    );
}

#[test]
fn backspace_widens_the_filter_back() {
    let mut s = Select::new("model", providers());
    s.handle_key(Key::Char('n'));
    s.handle_key(Key::Char('o'));
    let narrowed = s.list.visible_len();
    s.handle_key(Key::Backspace);
    assert!(narrowed < providers().len(), "the filter really narrowed");
    assert_eq!(s.list.visible_len(), providers().len());
}

#[test]
fn the_cursor_cannot_point_past_the_end_of_a_shortened_list() {
    let mut s = Select::new("model", providers());
    s.handle_key(Key::End);
    assert_eq!(s.list.cursor(), providers().len() - 1);
    // Now shrink the list out from under the cursor.
    s.handle_key(Key::Char('l'));
    s.handle_key(Key::Char('m'));
    assert!(
        s.list.cursor() < s.list.visible_len(),
        "cursor {} out of range for {} rows",
        s.list.cursor(),
        s.list.visible_len()
    );
}

#[test]
fn home_and_end_cover_the_filtered_list_not_the_whole_list() {
    let mut s = Select::new("model", providers());
    for c in "o".chars() {
        s.handle_key(Key::Char(c));
    }
    s.handle_key(Key::End);
    assert_eq!(s.list.cursor(), s.list.visible_len() - 1);
    s.handle_key(Key::Home);
    assert_eq!(s.list.cursor(), 0);
}

#[test]
fn paging_clamps_at_both_ends() {
    let mut s = Select::new("model", providers());
    s.list.set_visible_rows(2);
    s.handle_key(Key::PageUp);
    assert_eq!(s.list.cursor(), 0, "page up at the top stays at the top");
    s.handle_key(Key::PageDown);
    s.handle_key(Key::PageDown);
    assert!(
        s.list.cursor() < s.list.visible_len(),
        "page down past the end must not run off the list"
    );
}

#[test]
fn the_scroll_window_follows_the_cursor() {
    let mut s = Select::new("model", providers());
    s.list.set_visible_rows(2);
    s.handle_key(Key::End);
    // The window must have moved down to keep the last row visible.
    assert!(s.list.scroll() > 0, "window did not follow the cursor");
    let rows = select_rows(&s);
    assert!(
        rows.iter().any(|r| r.selected),
        "the selected row is inside the drawn window"
    );
}

// ---------------------------------------------------------------------------
// Select
// ---------------------------------------------------------------------------

#[test]
fn enter_submits_the_value_under_the_cursor() {
    let mut s = Select::new("model", providers());
    s.handle_key(Key::Down);
    match s.handle_key(Key::Enter) {
        KeyResult::Submit(Selection::One(v)) => assert_eq!(v, "openai"),
        other => panic!("expected a single submit, got {other:?}"),
    }
}

#[test]
fn esc_cancels_rather_than_selecting() {
    let mut s = Select::new("model", providers());
    assert_eq!(s.handle_key(Key::Esc), KeyResult::Cancel);
}

#[test]
fn a_disabled_row_is_visible_but_cannot_be_submitted() {
    // The spec's rule: a provider that exists but has no integration is
    // shown, and choosing it is refused. Silently hiding it is how a user
    // concludes the option does not exist at all.
    let items = vec![
        Item::new("Working", "ok"),
        Item::new("Planned", "planned")
            .desc("no integration yet")
            .disabled(),
    ];
    let mut s = Select::new("provider", items);
    s.handle_key(Key::Down);
    assert_eq!(
        s.current().map(|i| i.value.as_str()),
        Some("planned"),
        "the row is still reachable"
    );
    assert_eq!(
        s.handle_key(Key::Enter),
        KeyResult::Redraw,
        "submitting a disabled row must be refused, not accepted"
    );
}

#[test]
fn an_empty_filter_result_submits_none_not_an_empty_string() {
    let mut s = Select::new("model", providers());
    s.handle_key(Key::Char('z'));
    s.handle_key(Key::Char('z'));
    s.handle_key(Key::Char('z'));
    assert_eq!(s.list.visible_len(), 0);
    match s.handle_key(Key::Enter) {
        KeyResult::Submit(Selection::None) => {}
        other => panic!("expected Selection::None, got {other:?}"),
    }
}

#[test]
fn unhandled_keys_are_reported_as_ignored() {
    let mut s = Select::new("model", providers());
    assert_eq!(s.handle_key(Key::CtrlK), KeyResult::Ignored);
    assert_eq!(s.handle_key(Key::Other), KeyResult::Ignored);
}

#[test]
fn space_types_into_the_filter() {
    let mut s = Select::new("model", providers());
    s.handle_key(Key::Space);
    assert_eq!(s.list.filter(), " ");
}

// ---------------------------------------------------------------------------
// MultiSelect
// ---------------------------------------------------------------------------

fn gates() -> Vec<Item> {
    vec![
        Item::new("Web", "web"),
        Item::new("Terminal", "terminal"),
        Item::new("Files", "files"),
        Item::new("Browser", "browser").disabled(),
    ]
}

#[test]
fn space_toggles_and_enter_returns_every_checked_row() {
    let mut m = MultiSelect::new("tools", gates(), &[]);
    m.handle_key(Key::Down);
    m.handle_key(Key::Space);
    match m.handle_key(Key::Enter) {
        KeyResult::Submit(Selection::Many(v)) => assert_eq!(v, vec!["terminal"]),
        other => panic!("expected a multi submit, got {other:?}"),
    }
}

#[test]
fn an_existing_selection_is_shown_rather_than_reset() {
    // Reopening a checklist must reflect the config it edits, or the user
    // re-configures from a blank slate and loses what they had.
    let m = MultiSelect::new("tools", gates(), &["web", "files"]);
    assert_eq!(m.checked_values(), vec!["web", "files"]);
}

#[test]
fn a_disabled_row_cannot_be_toggled() {
    let mut m = MultiSelect::new("tools", gates(), &[]);
    m.handle_key(Key::End);
    assert_eq!(m.current().map(|i| i.value.as_str()), Some("browser"));
    assert!(!m.toggle_current(), "a disabled row refuses the toggle");
    assert!(m.checked_values().is_empty());
}

#[test]
fn space_is_the_toggle_and_never_a_filter_character() {
    let mut m = MultiSelect::new("tools", gates(), &[]);
    m.handle_key(Key::Space);
    assert_eq!(m.list.filter(), "", "space must not enter the filter");
    assert_eq!(m.checked_values(), vec!["web"]);
}

#[test]
fn checkboxes_render_only_for_the_multi_select() {
    let m = MultiSelect::new("tools", gates(), &["web"]);
    let rows = multi_rows(&m);
    assert_eq!(rows[0].marker, "[x]");
    assert_eq!(rows[1].marker, "[ ]");
    // The single-select path has no checkbox column at all.
    let s = Select::new("provider", providers());
    assert_eq!(select_rows(&s)[0].marker, "");
}

#[test]
fn exactly_one_row_is_ever_marked_selected() {
    let m = MultiSelect::new("tools", gates(), &[]);
    let rows = multi_rows(&m);
    assert_eq!(rows.iter().filter(|r| r.selected).count(), 1);
}

// ---------------------------------------------------------------------------
// TextInput
// ---------------------------------------------------------------------------

#[test]
fn typing_and_backspace_edit_the_value() {
    let mut t = TextInput::new("profile name");
    for c in "Zeus".chars() {
        t.handle_key(Key::Char(c));
    }
    assert_eq!(t.value(), "Zeus");
    t.handle_key(Key::Backspace);
    assert_eq!(t.value(), "Zeu");
}

#[test]
fn an_untouched_input_returns_the_default_and_an_empty_one_does_not() {
    let t = TextInput::new("name").default("default");
    assert_eq!(t.value_or_default(), "default");
    let mut typed = TextInput::new("name").default("default");
    typed.handle_key(Key::Char('x'));
    assert_eq!(typed.value_or_default(), "x");
}

#[test]
fn a_secret_input_never_renders_its_value() {
    let mut t = TextInput::new("api key").secret();
    for c in "sk-abcdefghij".chars() {
        t.handle_key(Key::Char(c));
    }
    // The real value is retained for the call that needs it...
    assert_eq!(t.value(), "sk-abcdefghij");
    // ...but what is drawn is a mask, and the mask leaks no characters.
    let drawn = t.display();
    assert!(
        !drawn.contains("abcdefghij"),
        "mask leaked the value: {drawn}"
    );
    assert_eq!(drawn.chars().count(), t.value().chars().count());
}

#[test]
fn arrows_move_the_cursor_and_delete_respects_it() {
    let mut t = TextInput::new("x").prefilled("abcd");
    t.handle_key(Key::Left);
    t.handle_key(Key::Delete);
    assert_eq!(t.value(), "abc", "delete removes the char at the cursor");
    assert_eq!(t.cursor(), 3, "the cursor does not jump on delete");
    t.handle_key(Key::Home);
    t.handle_key(Key::Backspace);
    assert_eq!(t.value(), "abc", "backspace at position 0 is a no-op");
    assert_eq!(t.cursor(), 0, "and it does not drag the cursor");
}

#[test]
fn cursor_arithmetic_survives_non_ascii_text() {
    // Char-indexed cursor into a multi-byte string: a byte-indexed cursor
    // would panic here rather than corrupt quietly.
    let mut t = TextInput::new("x").prefilled("héllo→wörld");
    t.handle_key(Key::Home);
    for _ in 0..2 {
        t.handle_key(Key::Right);
    }
    t.handle_key(Key::Backspace);
    assert_eq!(
        t.value(),
        "hllo→wörld",
        "backspace removed a 2-byte char without corrupting the string"
    );
    // And typing multi-byte text at a mid-string cursor must not split a char.
    t.handle_key(Key::End);
    t.handle_key(Key::Home);
    t.handle_key(Key::Right);
    t.handle_key(Key::Char('→'));
    assert!(
        t.value().starts_with('h'),
        "cursor math stayed on a boundary"
    );
}

// ---------------------------------------------------------------------------
// Confirm
// ---------------------------------------------------------------------------

#[test]
fn confirm_defaults_to_the_safe_answer() {
    let mut c = Confirm::new("danger", "delete the ledger?");
    match c.handle_key(Key::Enter) {
        KeyResult::Submit(Selection::Bool(false)) => {}
        other => panic!("a reflexive enter must not confirm, got {other:?}"),
    }
    let mut yes = Confirm::new("ok", "continue?").default_yes(true);
    match yes.handle_key(Key::Enter) {
        KeyResult::Submit(Selection::Bool(true)) => {}
        other => panic!("got {other:?}"),
    }
}

#[test]
fn confirm_answers_y_and_n_explicitly() {
    let mut c = Confirm::new("q", "really?");
    assert_eq!(
        c.handle_key(Key::Char('y')),
        KeyResult::Submit(Selection::Bool(true))
    );
    assert_eq!(
        c.handle_key(Key::Char('n')),
        KeyResult::Submit(Selection::Bool(false))
    );
}

// ---------------------------------------------------------------------------
// SearchList
// ---------------------------------------------------------------------------

#[test]
fn an_empty_search_list_states_why_it_is_empty() {
    let mut s = SearchList::new("sessions", vec![], "no runs yet");
    assert_eq!(s.empty_reason, "no runs yet");
    // Distinguishing "nothing exists" from "your filter matched nothing" is
    // the whole reason this is not just a Select.
    s.list.set_filter("zzz");
    assert!(!s.empty_reason.is_empty());
}

#[test]
fn search_list_filters_and_submits_like_select() {
    let mut s = SearchList::new(
        "sessions",
        vec![
            Item::new("Banking mission", "run_a").desc("2h ago"),
            Item::new("Pantheon development", "run_b").desc("yesterday"),
        ],
        "no runs yet",
    );
    s.handle_key(Key::Char('b'));
    assert_eq!(s.selected_value().as_deref(), Some("run_a"));
}
