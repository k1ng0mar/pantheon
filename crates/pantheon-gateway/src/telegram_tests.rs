//! Tests for `pantheon_gateway::telegram::tests` — sibling file so sources stay test-free.
use super::*;
use crate::stream::{UiFrame, UiFrameKind};
#[test]
fn parses_telegram_approval_callbacks() {
    // Legacy `grant:{scope}` (no run id): falls back to the thread map.
    let event = parse_event(&json!({
        "callback_query": {
            "data": "grant:call_2_0",
            "message": {"chat": {"id": 42}}
        }
    }))
    .unwrap()
    .unwrap();
    assert_eq!(event.thread_id, "42");
    assert_eq!(event.approval, Some(crate::channel::ApprovalAnswer::Grant));
    assert_eq!(event.scope.as_deref(), Some("call_2_0"));
    assert_eq!(event.run_id, None);
    // New `grant:{run_id}:{scope}` from phone notifications.
    let event = parse_event(&json!({
        "callback_query": {
            "data": "grant:run_9_0007:turn_1-call_0_0:shell:{\"cmd\":\"ls\"}",
            "message": {"chat": {"id": 42}}
        }
    }))
    .unwrap()
    .unwrap();
    assert_eq!(event.run_id.as_deref(), Some("run_9_0007"));
    assert_eq!(
        event.scope.as_deref(),
        Some("turn_1-call_0_0:shell:{\"cmd\":\"ls\"}")
    );
    assert!(parse_event(&json!({"callback_query": {}})).is_err());
    assert!(parse_event(&json!({"callback_query": {"data": "no-scope"}})).is_err());
}

#[test]
fn approval_uses_inline_keyboard() {
    // Pure payload shape through the real channel: no transport involved
    // (payloads() never touches the network; only send() does).
    let c = TelegramChannel::rest("token");
    let payloads = c.payloads(&ChannelEnvelope {
        thread_id: "42".into(),
        frame: UiFrame {
            id: 1,
            kind: UiFrameKind::Approval,
            run_id: "r".into(),
            thread_id: "42".into(),
            name: "requested".into(),
            text: "scope".into(),
            interrupt: true,
            genui: None,
        },
    });
    assert_eq!(payloads.len(), 1);
    assert_eq!(
        payloads[0]["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
        "grant:scope"
    );
}
