//! Modal vim editing for the TUI composer (`/vim`, `[tui] vim`).
//! Run with `cargo test -p pantheon-eval`.
//!
//! v1 scope: Normal + Insert modes only - no Visual, no macros, no
//! `.vimrc`. These tests drive the pure buffer model in
//! `pantheon_tui::session::vim` (the same functions the event loop calls)
//! plus the config persistence behind `/vim`.

use pantheon_tui::session::vim::{
    backspace, begin_insert_undo, handle_normal_key, insert_char, load_vim, move_to_end, save_vim,
    NormalKey, VimMode, VimState,
};
use tempfile::tempdir;

/// A vim-enabled state over `text`, in Normal mode.
fn vstate(text: &str) -> (String, VimState) {
    let mut v = VimState::new();
    v.enabled = true;
    (text.to_string(), v)
}

/// Feed keys through Normal-mode handling, asserting each is consumed as
/// a vim key (not passed through to the app).
fn normal_keys(text: &mut String, vim: &mut VimState, keys: &str) {
    for key in keys.chars() {
        let out = handle_normal_key(text, vim, key);
        assert!(
            matches!(out, NormalKey::Consumed | NormalKey::ToInsert),
            "key {key:?} should be consumed in Normal mode, got {out:?}"
        );
        if out == NormalKey::ToInsert {
            vim.mode = VimMode::Insert;
        }
    }
}

#[test]
fn vim_is_opt_in_and_off_by_default() {
    // Fresh state: disabled, so the legacy key path runs untouched.
    let v = VimState::new();
    assert!(!v.enabled);
    assert_eq!(v.mode, VimMode::Normal);
    // No mode label leaks into the status bar for non-users.
    assert_eq!(v.status_label(), None);
}

#[test]
fn vim_choice_persists_across_loads() {
    let dir = tempdir().unwrap();
    // Missing config = disabled (opt-in).
    assert!(!load_vim(dir.path()));

    save_vim(dir.path(), true).unwrap();
    assert!(load_vim(dir.path()));

    save_vim(dir.path(), false).unwrap();
    assert!(!load_vim(dir.path()));
}

#[test]
fn status_label_names_the_mode() {
    let (_, mut v) = vstate("");
    assert_eq!(v.status_label().as_deref(), Some("-- NORMAL --"));
    v.mode = VimMode::Insert;
    assert_eq!(v.status_label().as_deref(), Some("-- INSERT --"));
}

#[test]
fn esc_leaves_insert_for_normal_without_touching_text() {
    let (mut text, mut v) = vstate("hello");
    v.mode = VimMode::Insert;
    // What the event loop does on Esc in Insert mode.
    v.esc_to_normal();
    assert_eq!(v.mode, VimMode::Normal);
    assert_eq!(text, "hello");
    // A half-typed multi-key sequence is dropped too.
    v.mode = VimMode::Insert;
    handle_normal_key(&mut text, &mut v, 'd');
    v.esc_to_normal();
    assert_eq!(v.mode, VimMode::Normal);
    let out = handle_normal_key(&mut text, &mut v, 'd');
    assert_eq!(out, NormalKey::Consumed); // pending was cleared, not `dd`
    assert_eq!(text, "hello");
}

#[test]
fn i_enters_insert_at_the_cursor() {
    let (mut text, mut v) = vstate("hello");
    // Move onto the second 'l', then `i`: typing goes before it.
    normal_keys(&mut text, &mut v, "ll");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, 'i'),
        NormalKey::ToInsert
    );
    v.mode = VimMode::Insert;
    insert_char(&mut text, &mut v, 'X');
    assert_eq!(text, "heXllo");
}

#[test]
fn insert_entries_position_the_cursor() {
    // `a` appends after the cursor.
    let (mut text, mut v) = vstate("hi");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, 'a'),
        NormalKey::ToInsert
    );
    v.mode = VimMode::Insert;
    insert_char(&mut text, &mut v, '!');
    assert_eq!(text, "h!i");

    // `A` appends at end of line; `I` inserts at first non-blank.
    let (mut text, mut v) = vstate("  hi  ");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, 'A'),
        NormalKey::ToInsert
    );
    v.mode = VimMode::Insert; // the event loop flips the mode on ToInsert
    let (_, c) = v.cursor(&text);
    assert_eq!(c, 6);
    let (mut text, mut v) = vstate("  hi  ");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, 'I'),
        NormalKey::ToInsert
    );
    let (_, c) = v.cursor(&text);
    assert_eq!(c, 2);

    // `o` opens a line below, `O` above; both enter Insert.
    let (mut text, mut v) = vstate("one\ntwo");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, 'o'),
        NormalKey::ToInsert
    );
    assert_eq!(text, "one\n\ntwo");
    let (r, _) = v.cursor(&text);
    assert_eq!(r, 1);
    let (mut text, mut v) = vstate("one\ntwo");
    normal_keys(&mut text, &mut v, "j");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, 'O'),
        NormalKey::ToInsert
    );
    assert_eq!(text, "one\n\ntwo");
    let (r, _) = v.cursor(&text);
    assert_eq!(r, 1);
}

#[test]
fn hjkl_zero_dollar_move_pointwise() {
    let (mut text, mut v) = vstate("abc\nde");
    normal_keys(&mut text, &mut v, "l");
    assert_eq!(v.cursor(&text), (0, 1));
    normal_keys(&mut text, &mut v, "j");
    assert_eq!(v.cursor(&text), (1, 1));
    normal_keys(&mut text, &mut v, "h");
    assert_eq!(v.cursor(&text), (1, 0));
    normal_keys(&mut text, &mut v, "k");
    assert_eq!(v.cursor(&text), (0, 0));
    // `h`/`k` stop at the edges; they never leave the buffer.
    normal_keys(&mut text, &mut v, "hkh");
    assert_eq!(v.cursor(&text), (0, 0));
    normal_keys(&mut text, &mut v, "$");
    assert_eq!(v.cursor(&text), (0, 2)); // on the last char, not past it
    normal_keys(&mut text, &mut v, "0");
    assert_eq!(v.cursor(&text), (0, 0));
    // `l` stops at the last char in Normal mode.
    normal_keys(&mut text, &mut v, "llll");
    assert_eq!(v.cursor(&text), (0, 2));
}

#[test]
fn word_motions_w_b_e() {
    let (mut text, mut v) = vstate("foo bar_baz qux");
    normal_keys(&mut text, &mut v, "w");
    assert_eq!(v.cursor(&text), (0, 4)); // start of bar_baz
    normal_keys(&mut text, &mut v, "w");
    assert_eq!(v.cursor(&text), (0, 12)); // start of qux
    normal_keys(&mut text, &mut v, "b");
    assert_eq!(v.cursor(&text), (0, 4));
    normal_keys(&mut text, &mut v, "e");
    assert_eq!(v.cursor(&text), (0, 10)); // end of bar_baz
                                          // `w` past the last word parks at end of buffer.
    normal_keys(&mut text, &mut v, "ww");
    assert_eq!(v.cursor(&text), (0, 14));
}

#[test]
fn gg_and_big_g_jump_ends() {
    let (mut text, mut v) = vstate("  one\ntwo\n  three  ");
    normal_keys(&mut text, &mut v, "G");
    assert_eq!(v.cursor(&text), (2, 2)); // first non-blank of last line
    normal_keys(&mut text, &mut v, "gg");
    assert_eq!(v.cursor(&text), (0, 2)); // first non-blank of first line
}

#[test]
fn x_deletes_the_char_under_the_cursor() {
    let (mut text, mut v) = vstate("hello");
    normal_keys(&mut text, &mut v, "lx");
    assert_eq!(text, "hllo");
    assert_eq!(v.cursor(&text), (0, 1));
    // `x` on an empty line is a no-op, never an error.
    let (mut text, mut v) = vstate("");
    normal_keys(&mut text, &mut v, "x");
    assert_eq!(text, "");
}

#[test]
fn dd_deletes_a_whole_line_into_the_register() {
    let (mut text, mut v) = vstate("one\ntwo\nthree");
    normal_keys(&mut text, &mut v, "jdd");
    assert_eq!(text, "one\nthree");
    assert_eq!(v.cursor(&text), (1, 0));
    // `p` pastes the linewise register below the current line.
    normal_keys(&mut text, &mut v, "p");
    assert_eq!(text, "one\nthree\ntwo");
    assert_eq!(v.cursor(&text), (2, 0));
}

#[test]
fn dd_on_the_last_line_keeps_a_valid_buffer() {
    let (mut text, mut v) = vstate("only");
    normal_keys(&mut text, &mut v, "dd");
    assert_eq!(text, "");
    assert_eq!(v.cursor(&text), (0, 0));
    normal_keys(&mut text, &mut v, "p");
    assert_eq!(text, "only");
}

#[test]
fn dw_and_db_delete_words() {
    let (mut text, mut v) = vstate("foo bar baz");
    normal_keys(&mut text, &mut v, "dw");
    assert_eq!(text, "bar baz");
    normal_keys(&mut text, &mut v, "wdb");
    assert_eq!(text, "baz");
    // `dw` at the last word deletes to end of line.
    let (mut text, mut v) = vstate("foo bar");
    normal_keys(&mut text, &mut v, "wdw");
    assert_eq!(text, "foo ");
}

#[test]
fn u_undoes_edits_in_reverse() {
    let (mut text, mut v) = vstate("hello");
    normal_keys(&mut text, &mut v, "x");
    assert_eq!(text, "ello");
    normal_keys(&mut text, &mut v, "dd");
    assert_eq!(text, "");
    normal_keys(&mut text, &mut v, "u");
    assert_eq!(text, "ello");
    normal_keys(&mut text, &mut v, "u");
    assert_eq!(text, "hello");
    // Undo past the beginning is a silent no-op.
    normal_keys(&mut text, &mut v, "u");
    assert_eq!(text, "hello");
}

#[test]
fn u_undoes_a_whole_insert_session() {
    let (mut text, mut v) = vstate("");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, 'i'),
        NormalKey::ToInsert
    );
    begin_insert_undo(&text, &mut v); // what the event loop does on ToInsert
    v.mode = VimMode::Insert;
    for c in "hello".chars() {
        insert_char(&mut text, &mut v, c);
    }
    assert_eq!(text, "hello");
    v.esc_to_normal(); // Esc
    normal_keys(&mut text, &mut v, "u");
    assert_eq!(text, "");
}

#[test]
fn p_pastes_the_charwise_register_after_the_cursor() {
    let (mut text, mut v) = vstate("hello");
    normal_keys(&mut text, &mut v, "x"); // register = "h"
    normal_keys(&mut text, &mut v, "$p");
    assert_eq!(text, "elloh");
    // Empty register: `p` is a no-op.
    let (mut text, mut v) = vstate("hi");
    normal_keys(&mut text, &mut v, "p");
    assert_eq!(text, "hi");
}

#[test]
fn unknown_d_sequence_is_dropped_silently() {
    let (mut text, mut v) = vstate("hello");
    normal_keys(&mut text, &mut v, "dq"); // `dq` means nothing
    assert_eq!(text, "hello");
    // ...and the pending `d` does not leak into the next command.
    normal_keys(&mut text, &mut v, "dd");
    assert_eq!(text, "");
}

#[test]
fn insert_backspace_deletes_before_the_cursor() {
    let (mut text, mut v) = vstate("hello");
    move_to_end(&text, &mut v);
    let (r, c) = v.cursor(&text);
    assert_eq!((r, c), (0, 5));
    backspace(&mut text, &mut v);
    assert_eq!(text, "hell");
    // At column 0 it joins with the previous line.
    let (mut text, mut v) = vstate("ab\ncd");
    v.mode = VimMode::Insert;
    backspace(&mut text, &mut v); // col 0, row 0: nothing to join
    assert_eq!(text, "ab\ncd");
}

#[test]
fn multibyte_text_never_splits_a_char() {
    let (mut text, mut v) = vstate("héllo wörld");
    normal_keys(&mut text, &mut v, "w");
    assert_eq!(v.cursor(&text), (0, 6));
    normal_keys(&mut text, &mut v, "x");
    assert_eq!(text, "héllo örld");
    // Insert in the middle of multibyte text.
    let (mut text, mut v) = vstate("héllo");
    v.mode = VimMode::Insert;
    insert_char(&mut text, &mut v, 'X'); // at col 0
    assert_eq!(text, "Xhéllo");
}

#[test]
fn overlay_keys_are_never_consumed_as_vim() {
    // The event loop routes approval/reflection keys before vim; as a
    // second line of defense the Normal handler itself leaves the
    // decision keys unconsumed so a routing regression cannot shadow
    // a permission card.
    let (mut text, mut v) = vstate("draft");
    assert!(matches!(
        handle_normal_key(&mut text, &mut v, 'y'),
        NormalKey::Pass('y')
    ));
    assert!(matches!(
        handle_normal_key(&mut text, &mut v, 'n'),
        NormalKey::Pass('n')
    ));
    assert_eq!(text, "draft");
}

#[test]
fn empty_buffer_app_affordances_survive() {
    // `?` and `q` on an empty prompt keep their app-level jobs in
    // Normal mode; on a non-empty prompt they are plain Pass-through.
    let (mut text, mut v) = vstate("");
    assert_eq!(
        handle_normal_key(&mut text, &mut v, '?'),
        NormalKey::Shortcuts
    );
    assert_eq!(handle_normal_key(&mut text, &mut v, 'q'), NormalKey::Quit);
    let (mut text, mut v) = vstate("draft");
    assert!(matches!(
        handle_normal_key(&mut text, &mut v, 'q'),
        NormalKey::Pass('q')
    ));
}

#[test]
fn move_to_end_parks_for_mention_completion() {
    let (text, mut v) = vstate("hello @");
    v.mode = VimMode::Insert;
    move_to_end(&text, &mut v);
    let (r, c) = v.cursor(&text);
    assert_eq!((r, c), (0, 7));
}
