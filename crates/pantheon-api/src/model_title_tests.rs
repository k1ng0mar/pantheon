//! Tests for `pantheon_api::model::title_tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn bound_title_is_single_line_and_capped() {
    let raw = "  \n  Fix the login bug  \n second line ignored ";
    assert_eq!(bound_title(raw, TITLE_MAX_CHARS), "Fix the login bug");
    let long = "x".repeat(200);
    assert_eq!(bound_title(&long, 60).chars().count(), 60);
}

#[test]
fn bound_title_strips_quotes_and_labels() {
    assert_eq!(bound_title("\"Refactor auth\"", 60), "Refactor auth");
    assert_eq!(bound_title("Title: Ship the parser", 60), "Ship the parser");
    assert_eq!(bound_title("“curly title”", 60), "curly title");
}

#[test]
fn bound_title_truncates_on_a_char_boundary() {
    let cjk = "语".repeat(100); // 3 bytes each
    let out = bound_title(&cjk, 60);
    assert_eq!(out.chars().count(), 60);
    // Short multibyte input passes through untouched.
    assert_eq!(bound_title("数据管线", 60), "数据管线");
}

#[test]
fn bound_title_collapses_inner_whitespace() {
    assert_eq!(bound_title("multi   word \t title", 60), "multi word title");
    // Only the first line survives: a title is one line, always.
    assert_eq!(bound_title("first line\nsecond line", 60), "first line");
}

#[test]
fn empty_input_bounds_to_empty() {
    assert_eq!(bound_title("  \n \t ", 60), "");
    assert_eq!(fallback_title(""), "");
}

#[test]
fn fallback_title_uses_the_first_prompt() {
    assert_eq!(
        fallback_title("write a cron parser in rust"),
        "write a cron parser in rust"
    );
}
