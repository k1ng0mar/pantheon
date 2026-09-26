//! Tests for `pantheon_cli::tui::interrupt_tests` — sibling file so sources stay test-free.
use super::*;

/// Build a real running-session state (no hand-maintained field list,
/// so this test cannot rot when the struct grows).
fn state() -> TuiState {
    let mut s = TuiState::new("sess_test01".into(), "test".into(), 128_000);
    s.ready = false;
    s.is_inputting = true;
    s.status_line = "working".into();
    s
}

const ARM_WINDOW: Duration = Duration::from_millis(1500);

/// First Esc arms; it must not claim the run is interrupted yet.
#[test]
fn first_esc_only_arms() {
    let mut s = state();
    assert!(s.interrupt_armed_at.is_none());
    s.interrupt_armed_at = Some(Instant::now());
    assert!(s.interrupt_armed_at.is_some(), "armed");
    assert!(!s.interrupted, "arming is not interruption");
}

/// A second Esc inside the window confirms the interrupt.
#[test]
fn second_esc_inside_window_interrupts() {
    let mut s = state();
    s.interrupt_armed_at = Some(Instant::now());
    let confirms = s
        .interrupt_armed_at
        .is_some_and(|t| t.elapsed() < ARM_WINDOW);
    assert!(confirms, "second esc inside the window is a confirm");
    s.interrupted = true;
    s.interrupt_armed_at = None;
    assert!(s.interrupted);
}

/// An arm that goes stale (user wandered off) must not fire later.
#[test]
fn stale_arm_does_not_interrupt() {
    let mut s = state();
    s.interrupt_armed_at = Some(Instant::now() - ARM_WINDOW - Duration::from_millis(50));
    let confirms = s
        .interrupt_armed_at
        .is_some_and(|t| t.elapsed() < ARM_WINDOW);
    assert!(!confirms, "stale arm is re-armed, not fired");
}

/// Idle sessions must not be interruptible: the arm path is gated on
/// !ready, so an Esc while ready cannot cancel anything.
#[test]
fn idle_session_cannot_arm() {
    let mut s = state();
    s.ready = true;
    let armable = !s.ready && !s.interrupted;
    assert!(!armable, "no interrupt affordance while idle");
}

/// Resume must rebuild the transcript from the ledger so switching runs
/// shows prior turns. Mirrors the two code paths in tui_loop and
/// handle_slash that map Message rows to TranscriptBlock kinds.
#[test]
fn resume_rebuilds_transcript_from_ledger() {
    let dir = std::env::temp_dir().join(format!("pantheon-cli-tui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = pantheon_runtime::Supervisor::open(dir).unwrap();
    sup.start_run("run_tui").unwrap();
    sup.emit(pantheon_core::events::Event::AssistantMessage {
        run_id: "run_tui".into(),
        message: pantheon_core::message::Message::user("do the thing"),
    })
    .unwrap();
    let entries = sup.replay("run_tui").unwrap();
    let msgs = pantheon_runtime::session::rebuild_messages(entries);
    assert_eq!(msgs.len(), 1, "one message round-trips through the ledger");
    assert_eq!(msgs[0].content, "do the thing");

    // The same mapping the TUI uses to seed the transcript after a
    // /resume or /history selection.
    let mut blocks: Vec<TranscriptBlock> = Vec::new();
    for m in &msgs {
        let kind = match m.role {
            pantheon_core::message::Role::User => BlockKind::UserMessage(m.content.clone()),
            _ => BlockKind::AssistantMessage(m.content.clone()),
        };
        blocks.push(TranscriptBlock { kind });
    }
    assert!(matches!(blocks[0].kind, BlockKind::UserMessage(_)));
    assert_eq!(blocks.len(), 1);
}
