//! Tests for `crate::args::tests` — sibling file so sources stay test-free.
use super::*;

fn raw(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn both_flag_forms_parse() {
    let a = Args::parse(&raw(&[
        "--model",
        "m1",
        "--provider=openai",
        "pos1",
        "--dry-run",
    ]));
    assert_eq!(a.flag("model").as_deref(), Some("m1"));
    assert_eq!(a.flag("provider").as_deref(), Some("openai"));
    assert_eq!(a.positional(0).as_deref(), Some("pos1"));
    assert!(a.has("dry-run"));
    assert_eq!(a.flag("dry-run"), None, "no value = not a value flag");
}

#[test]
fn last_occurrence_wins() {
    let a = Args::parse(&raw(&["--model", "a", "--model", "b"]));
    assert_eq!(a.flag("model").as_deref(), Some("b"));
}

#[test]
fn boolean_flag_does_not_eat_the_next_flag() {
    // `setup --yes --provider openai` used to record `yes = "--provider"`
    // and drop the provider entirely. A bare flag followed by `--x` is a
    // switch, not a value.
    let a = Args::parse(&raw(&["--yes", "--provider", "openai"]));
    assert!(a.has("yes"));
    assert_eq!(a.flag("provider").as_deref(), Some("openai"));
    assert_eq!(a.flag("yes"), None);
}

#[test]
fn value_still_wins_over_positional() {
    let a = Args::parse(&raw(&["--model", "m1", "hello"]));
    assert_eq!(a.flag("model").as_deref(), Some("m1"));
    assert_eq!(a.positional(0).as_deref(), Some("hello"));
}
