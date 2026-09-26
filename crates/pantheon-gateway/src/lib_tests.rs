//! Tests for `pantheon_gateway::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn canonical_shape() {
    let m = InboundMessage {
        id: "m1".into(),
        from: Identity {
            gateway: "telegram".into(),
            user: "u1".into(),
        },
        conversation: Conversation::new("telegram", "c1"),
        text: "build this".into(),
        attachments: vec![],
    };
    assert_eq!(m.text, "build this");
    assert_eq!(m.from.gateway, "telegram");
    assert_eq!(m.conversation.key(), "telegram:c1");
}
