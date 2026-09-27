//! Tests for `crate::session::interrupt_tests` — sibling file so sources stay test-free.
//!
//! These drive `TuiState::press_esc`, the same method the event loop calls.
//! An earlier version re-implemented the arm/confirm rules inline and
//! asserted on its own copies, so the suite passed even if the shipped
//! logic were deleted.
use super::*;

/// A running session: not ready (a turn is in flight) and accepting input.
fn running() -> TuiState {
    let mut s = TuiState::new("sess_test01".into(), "test".into(), 128_000);
    s.ready = false;
    s.is_inputting = true;
    s.status_line = "working".into();
    s
}

#[test]
fn first_esc_arms_without_interrupting() {
    let mut s = running();
    assert!(!s.press_esc(), "arming is not interruption");
    assert!(s.interrupt_armed_at.is_some());
    assert!(!s.interrupted);
}

#[test]
fn second_esc_inside_the_window_interrupts() {
    let mut s = running();
    s.press_esc();
    assert!(s.press_esc(), "second esc inside the window confirms");
    assert!(s.interrupted);
    assert!(s.interrupt_armed_at.is_none(), "confirming clears the arm");
}

#[test]
fn a_stale_arm_does_not_interrupt() {
    let mut s = running();
    s.interrupt_armed_at = Some(Instant::now() - TuiState::ARM_WINDOW - Duration::from_millis(50));
    assert!(!s.press_esc(), "a stale arm re-arms instead of firing");
    assert!(!s.interrupted);
    assert!(s.interrupt_armed_at.is_some(), "it re-arms");
}

#[test]
fn an_idle_session_cannot_be_interrupted() {
    let mut s = running();
    s.ready = true;
    assert!(!s.press_esc());
    assert!(s.interrupt_armed_at.is_none(), "no arm while idle");
    assert!(!s.interrupted);
}

#[test]
fn a_finished_run_disarms_rather_than_firing() {
    // The user arms, the run completes, then they press Esc again. That must
    // clear the arm and leave the session ready, not interrupt anything.
    let mut s = running();
    s.press_esc();
    assert!(s.interrupt_armed_at.is_some());
    s.ready = true;
    assert!(!s.press_esc());
    assert!(s.interrupt_armed_at.is_none());
    assert_eq!(s.status_line, "ready");
    assert!(!s.interrupted);
}

#[test]
fn a_confirmed_interrupt_does_not_fire_twice() {
    let mut s = running();
    s.press_esc();
    assert!(s.press_esc());
    assert!(
        !s.press_esc(),
        "already interrupted, nothing left to cancel"
    );
}

/// Resume must rebuild the transcript from the ledger so switching runs
/// shows prior turns.
#[test]
fn resume_rebuilds_transcript_from_ledger() {
    let dir = std::env::temp_dir().join(format!("pantheon-tui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = pantheon_runtime::Supervisor::open(dir).unwrap();
    sup.start_run("run_tui").unwrap();
    sup.emit(pantheon_api::events::Event::AssistantMessage {
        run_id: "run_tui".into(),
        message: pantheon_api::message::Message::user("do the thing"),
    })
    .unwrap();
    sup.emit(pantheon_api::events::Event::AssistantMessage {
        run_id: "run_tui".into(),
        message: pantheon_api::message::Message::assistant("done"),
    })
    .unwrap();
    let entries = sup.replay("run_tui").unwrap();
    let msgs = pantheon_runtime::session::rebuild_messages(entries);
    assert_eq!(msgs.len(), 2, "both turns round-trip through the ledger");
    assert_eq!(msgs[0].content, "do the thing");
    assert_eq!(msgs[1].content, "done");
}
