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
    assert_eq!(event.run_id, None);
    // New `deny:{run_id}:{scope}` from phone notifications.
    let event = parse_event(&json!({
        "type": 2,
        "channel_id": "chan-1",
        "data": {"custom_id": "deny:run_3_0002:turn_1-call_0_0:shell:{}", "channel_id": "chan-1"}
    }))
    .unwrap()
    .unwrap();
    assert_eq!(event.run_id.as_deref(), Some("run_3_0002"));
    assert_eq!(event.scope.as_deref(), Some("turn_1-call_0_0:shell:{}"));
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

// ── REST request building (no live token; asserts the wire shape) ────────

#[test]
fn rest_transport_builds_correct_request() {
    // build_send_request is the exact (url, auth header, body) that
    // send_message POSTs; asserting it here covers the wire shape
    // without touching the network.
    let t = DiscordRestTransport::new("tok123");
    let (url, auth, body) = t.build_send_request("987", &json!({"content": "hi"}));
    assert_eq!(url, "https://discord.com/api/v10/channels/987/messages");
    assert_eq!(auth, "Bot tok123");
    assert_eq!(body, json!({"content": "hi"}));
}

#[test]
fn rest_transport_honors_custom_api_base() {
    let t = DiscordRestTransport::new("tok").with_api_base("http://localhost:9/");
    let (url, _, _) = t.build_send_request("1", &json!({"content": "x"}));
    assert_eq!(url, "http://localhost:9/channels/1/messages");
}

#[test]
fn long_messages_chunk_at_content_limit_and_round_trip() {
    let c = DiscordChannel::rest("token");
    let text = "a".repeat(DISCORD_CONTENT_LIMIT * 2 + 7);
    let payloads = c.payloads(&ChannelEnvelope {
        thread_id: "123".into(),
        frame: frame(UiFrameKind::Text, "delta", &text),
    });
    assert_eq!(payloads.len(), 3);
    for p in &payloads {
        assert!(
            p["content"].as_str().unwrap().len() <= DISCORD_CONTENT_LIMIT,
            "no chunk exceeds the Discord limit"
        );
    }
    let joined: String = payloads
        .iter()
        .map(|p| p["content"].as_str().unwrap())
        .collect();
    assert_eq!(joined, text);
}

#[test]
fn chunking_never_splits_utf8() {
    let c = DiscordChannel::rest("token");
    let text = "é".repeat(1500);
    let payloads = c.payloads(&ChannelEnvelope {
        thread_id: "123".into(),
        frame: frame(UiFrameKind::Text, "delta", &text),
    });
    assert!(payloads.len() >= 2);
    let joined: String = payloads
        .iter()
        .map(|p| p["content"].as_str().unwrap())
        .collect();
    assert_eq!(joined, text);
}

// ── send() through a recording transport ─────────────────────────────────

struct RecordingTransport {
    sent: Mutex<Vec<(String, Value)>>,
}
impl DiscordTransport for RecordingTransport {
    fn send_message(&self, channel_id: &str, payload: &Value) -> Result<(), ChannelError> {
        self.sent
            .lock()
            .unwrap()
            .push((channel_id.to_string(), payload.clone()));
        Ok(())
    }
}

#[test]
fn send_posts_each_chunk_to_the_thread_channel() {
    let transport = Arc::new(RecordingTransport {
        sent: Mutex::new(vec![]),
    });
    let c = DiscordChannel::new("tok", transport.clone());
    let text = "b".repeat(DISCORD_CONTENT_LIMIT + 5);
    c.send(ChannelEnvelope {
        thread_id: "chan-7".into(),
        frame: frame(UiFrameKind::Text, "delta", &text),
    })
    .unwrap();
    let sent = transport.sent.lock().unwrap();
    assert_eq!(sent.len(), 2, "one POST per chunk");
    // The envelope's thread id is the Discord channel id on the wire.
    assert!(sent.iter().all(|(id, _)| id == "chan-7"));
    let joined: String = sent
        .iter()
        .map(|(_, p)| p["content"].as_str().unwrap())
        .collect();
    assert_eq!(joined, text);
}

#[test]
fn send_includes_approval_buttons() {
    let transport = Arc::new(RecordingTransport {
        sent: Mutex::new(vec![]),
    });
    let c = DiscordChannel::new("tok", transport.clone());
    c.send(ChannelEnvelope {
        thread_id: "chan-7".into(),
        frame: frame(UiFrameKind::Approval, "requested", "call_9"),
    })
    .unwrap();
    let sent = transport.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].1["components"][0]["components"][0]["custom_id"],
        "grant:call_9"
    );
    assert_eq!(
        sent[0].1["components"][0]["components"][1]["custom_id"],
        "deny:call_9"
    );
}
