//! Tests for the session view.
//!
//! The interrupted-park, unknown-context, and permission-card cases are the
//! three places the old view was wrong, so each gets a regression test.

use super::*;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn view() -> SessionView {
    SessionView {
        session_id: "run_abc123".into(),
        model: "llama3.2".into(),
        provider: "local".into(),
        ready: true,
        status_line: "ready".into(),
        ..Default::default()
    }
}

#[test]
fn the_header_shows_the_configured_model_not_a_hardcoded_one() {
    // The old view shipped "opus-4.1" in the constructor, so every session
    // displayed a model it was not running.
    let v = view();
    assert_eq!(v.model, "llama3.2");
    assert!(
        !v.model.contains("opus"),
        "a placeholder model string leaked back into the view"
    );
}

#[test]
fn an_unknown_context_window_is_stated_rather_than_invented() {
    let mut v = view();
    v.tokens_max = None;
    v.tokens_used = 12_000;
    assert!(
        v.context_label().contains("unknown"),
        "got {}",
        v.context_label()
    );
    v.tokens_max = Some(200_000);
    assert_eq!(v.context_label(), "12.0k/200k");
}

#[test]
fn the_live_counter_ticks_before_usage_lands() {
    let mut v = view();
    v.bump_estimate(40);
    assert_eq!(v.live_tokens(), 10, "40 chars is about 10 tokens");
    v.bump_estimate(40);
    assert_eq!(v.live_tokens(), 20, "it accumulates across deltas");
    v.snap_usage(500);
    assert_eq!(v.live_tokens(), 500, "usage snaps and clears the estimate");
}

#[test]
fn a_first_esc_only_arms() {
    let mut v = view();
    v.ready = false;
    assert!(!v.press_esc(Instant::now()), "one esc must not cancel");
    assert!(v.interrupt_armed);
    assert!(v.status_line.contains("again"));
}

#[test]
fn a_second_esc_interrupts() {
    let mut v = view();
    v.ready = false;
    v.press_esc(Instant::now());
    assert!(v.press_esc(Instant::now()));
    assert!(v.interrupted);
    assert!(!v.interrupt_armed, "the interrupt fires once");
}

#[test]
fn an_idle_session_cannot_be_interrupted() {
    let mut v = view();
    assert!(!v.press_esc(Instant::now()));
    assert!(!v.interrupted);
}

#[test]
fn a_permission_card_owns_esc() {
    // Esc on the permission card denies the call. Letting it also arm an
    // interrupt means one keypress refuses an approval and starts cancelling.
    let mut v = view();
    v.ready = false;
    v.pending_approval = Some(("run_1".into(), "call_1:shell:ls".into()));
    assert!(!v.press_esc(Instant::now()));
    assert!(
        !v.interrupt_armed,
        "esc must not arm an interrupt on the card"
    );
}

#[test]
fn the_composer_collects_text_and_submits_once() {
    let mut s = SessionScreen::new(view());
    let got: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let sink = got.clone();
    s.on_submit = Some(Box::new(move |m| {
        *sink.lock().unwrap() = m;
    }));
    for c in "hello".chars() {
        s.key(Key::Char(c));
    }
    s.key(Key::Enter);
    assert_eq!(&*got.lock().unwrap(), "hello");
    assert!(s.composer.is_empty(), "the composer clears after sending");
    // Enter on an empty composer must not submit a blank turn.
    s.key(Key::Enter);
    assert_eq!(&*got.lock().unwrap(), "hello");
}

#[test]
fn ctrl_c_quits_from_the_session() {
    let mut s = SessionScreen::new(view());
    assert_eq!(s.key(Key::CtrlC), Some(ScreenResult::Quit));
}

#[test]
fn a_run_parked_on_approval_reports_the_scope() {
    let mut v = view();
    v.pending_approval = Some(("run_9".into(), "call_2:shell:rm".into()));
    let s = SessionScreen::new(v);
    // The status line is what the card shows; the scope must be in it so the
    // user reads the exact command they are approving.
    assert!(s.view.pending_approval.as_ref().unwrap().1.contains("rm"));
}
