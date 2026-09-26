//! Tests for `pantheon_gateway::discord_gateway::tests` — sibling file so sources stay test-free.
use super::*;
use crate::channel::ChannelEvent;

#[test]
fn identify_carries_token_intents_and_shape() {
    let g = DiscordGateway::new("tok");
    let v = g.identify();
    assert_eq!(v["op"], 2);
    assert_eq!(v["d"]["token"], "tok");
    assert_eq!(v["d"]["intents"], (1u64 << 9) | (1 << 15) | (1 << 12));
    assert_eq!(v["d"]["properties"]["browser"], "pantheon");
}

#[test]
fn resume_carries_session_and_seq() {
    let g = DiscordGateway::new("tok");
    let state = GatewayState::new();
    *state
        .session_id
        .lock()
        .unwrap()
        .clone()
        .as_mut()
        .unwrap_or(&mut String::new()) = String::new();
    state.session_id.lock().unwrap().replace("sess-1".into());
    state.seq.store(7, Ordering::Release);
    let v = g.resume(&state);
    assert_eq!(v["op"], 6);
    assert_eq!(v["d"]["session_id"], "sess-1");
    assert_eq!(v["d"]["seq"], 7);
}

#[test]
fn dispatch_normalizes_message_create_into_channel_events() {
    // The gateway `d` object is passed straight to parse_event, exactly
    // as run_once does.
    let data = json!({"type": 0, "channel_id": "chan-9", "content": "hi"});
    let event = crate::discord::parse_event(&data).ok().flatten().unwrap();
    assert_eq!(e_thread(&event), "chan-9");
    assert_eq!(event.text, "hi");
}

fn e_thread(e: &ChannelEvent) -> String {
    e.thread_id.clone()
}
