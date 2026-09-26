//! Tests for `pantheon_gateway::sse::tests` — sibling file so sources stay test-free.
use super::*;
use crate::stream::{UiFrame, UiFrameKind};
fn f() -> UiFrame {
    UiFrame {
        id: 9,
        kind: UiFrameKind::Text,
        run_id: "r".into(),
        thread_id: "t".into(),
        name: "delta".into(),
        text: "hi".into(),
        interrupt: false,
        genui: None,
    }
}
#[test]
fn frame_encodes_with_id_and_kind() {
    let s = encode_frame(&f());
    assert!(s.starts_with("id: 9\n"), "{s}");
    assert!(s.contains("event: text\n"), "{s}");
    assert!(s.contains("\"run_id\":\"r\""), "{s}");
    assert!(s.ends_with("\n\n"));
}
#[test]
fn last_event_id_parses() {
    assert_eq!(parse_last_event_id("42"), Some(42));
    assert_eq!(parse_last_event_id("nope"), None);
}
