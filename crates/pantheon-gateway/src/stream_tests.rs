//! Tests for `pantheon_gateway::stream::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_api::events::Event;
use pantheon_api::provenance::Provenance;
#[test]
fn approval_request_parks_with_interrupt_flag() {
    let f = frame_for_event(
        7,
        "discord:c1",
        &Event::ApprovalRequested {
            run_id: "r1".into(),
            scope: "call_0_0".into(),
        },
        None,
    );
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].kind, UiFrameKind::Approval);
    assert!(f[0].interrupt);
    assert_eq!(f[0].thread_id, "discord:c1");
    assert_eq!(f[0].id, 7);
}
#[test]
fn text_and_tool_events_become_frames() {
    let d = frame_for_event(
        1,
        "t",
        &Event::ModelDelta {
            run_id: "r".into(),
            delta: "hi".into(),
        },
        None,
    );
    assert_eq!(d[0].kind, UiFrameKind::Text);
    assert!(!d[0].interrupt);
    let s = frame_for_event(
        2,
        "t",
        &Event::ToolStarted {
            run_id: "r".into(),
            call_id: "call_0_0".into(),
            tool: "shell".into(),
            args: String::new(),
            provenance: Provenance::system("test"),
        },
        None,
    );
    assert_eq!(s[0].kind, UiFrameKind::Tool);
}
#[test]
fn provider_internals_stay_off_the_wire() {
    let f = frame_for_event(
        3,
        "t",
        &Event::ModelRequested {
            run_id: "r".into(),
            model: "m".into(),
        },
        None,
    );
    assert!(f.is_empty());
}
