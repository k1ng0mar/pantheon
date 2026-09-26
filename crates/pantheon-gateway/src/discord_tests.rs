//! Tests for `pantheon_gateway::discord::tests` — sibling file so sources stay test-free.
use super::*;
use crate::stream::{UiFrame, UiFrameKind};
fn frame(kind: UiFrameKind, name: &str, text: &str) -> UiFrame {
    UiFrame {
        id: 1,
        kind,
        run_id: "r".into(),
        thread_id: "t".into(),
        name: name.into(),
        text: text.into(),
        interrupt: false,
        genui: None,
    }
}
#[test]
fn parses_approval_interactions_into_channel_events() {
    let event = parse_event(&json!({
        "type": 2,
        "channel_id": "chan-1",
        "data": {"custom_id": "deny:call_1_0", "channel_id": "chan-1"}
    }))
    .unwrap()
    .unwrap();
    assert_eq!(event.thread_id, "chan-1");
    assert_eq!(event.approval, Some(crate::channel::ApprovalAnswer::Deny));
    assert_eq!(event.scope.as_deref(), Some("call_1_0"));
    assert!(parse_event(&json!({"type": 2, "data": {"custom_id": "unknown:x"}})).is_err());
}

#[test]
fn chunks_and_renders_approval_buttons() {
    // Pure payload shapes through the real channel: payloads() never
    // touches the network; only send() does.
    let c = DiscordChannel::rest("token");
    let long = "x".repeat(DISCORD_CONTENT_LIMIT + 1);
    let rows: Vec<Value> = [
        ChannelEnvelope {
            thread_id: "123".into(),
            frame: frame(UiFrameKind::Text, "delta", &long),
        },
        ChannelEnvelope {
            thread_id: "123".into(),
            frame: frame(UiFrameKind::Approval, "requested", "call_1_0"),
        },
    ]
    .iter()
    .flat_map(|e| c.payloads(e))
    .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[2]["components"][0]["components"][0]["custom_id"],
        "grant:call_1_0"
    );
}
