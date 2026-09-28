//! Tests for `pantheon_gateway::daemon::tests` — sibling file so sources stay test-free.
//!
//! The behavioral daemon tests (real `ChannelDaemon::run` ticks, cursor
//! persistence, dead-letter routing) live in
//! `eval/tests/gateway_daemon.rs`; a few small helpers are duplicated there
//! because the pure routing tests below use them too.
use super::*;
use crate::channel::{ApprovalAnswer, MemoryChannel};
use serde_json::json;

struct RecordingSink {
    messages: Mutex<Vec<(String, String)>>,
    approvals: Mutex<Vec<(String, Option<String>, String, bool)>>,
}
impl EventSink for RecordingSink {
    fn on_message(&self, thread: &str, _sender: Option<&str>, text: &str) {
        self.messages
            .lock()
            .unwrap()
            .push((thread.into(), text.into()));
    }
    fn on_approval(
        &self,
        thread: &str,
        _sender: Option<&str>,
        run_id: Option<&str>,
        scope: &str,
        grant: bool,
    ) {
        self.approvals.lock().unwrap().push((
            thread.into(),
            run_id.map(|s| s.to_string()),
            scope.into(),
            grant,
        ));
    }
}

fn outbound_msg(to: &str, gateway: &str) -> OutboundMessage {
    OutboundMessage {
        to_conversation: to.into(),
        text: "hello".into(),
        gateway: gateway.into(),
        attempts: 0,
    }
}

fn named_channel(name: &str) -> Arc<MemoryChannel> {
    Arc::new(MemoryChannel::new(name))
}

#[test]
fn events_route_to_messages_and_approvals() {
    let sink = RecordingSink {
        messages: Mutex::new(vec![]),
        approvals: Mutex::new(vec![]),
    };
    route_event(
        &sink,
        &ChannelEvent {
            thread_id: "42".into(),
            run_id: None,
            text: "hello".into(),
            approval: None,
            scope: None,
            sender: None,
        },
    );
    route_event(
        &sink,
        &ChannelEvent {
            thread_id: "42".into(),
            run_id: None,
            text: String::new(),
            approval: Some(ApprovalAnswer::Grant),
            scope: Some("call_0_0".into()),
            sender: None,
        },
    );
    assert_eq!(
        *sink.messages.lock().unwrap(),
        vec![("42".to_string(), "hello".to_string())]
    );
    assert_eq!(
        *sink.approvals.lock().unwrap(),
        vec![("42".to_string(), None, "call_0_0".to_string(), true)]
    );
}

#[test]
fn poll_telegram_parses_updates_through_the_trait() {
    // Pure collector: real update shapes, no transport.
    let updates = vec![
        json!({
            "update_id": 100,
            "message": {"chat": {"id": 42}, "text": "hello"}
        }),
        json!({
            "update_id": 101,
            "callback_query": {
                "data": "grant:call_1_0",
                "message": {"chat": {"id": 42}}
            }
        }),
    ];
    let (events, next) = collect_telegram_events(&updates, 0);
    assert_eq!(next, 102);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].text, "hello");
    assert_eq!(events[0].thread_id, "42");
    assert!(events[1].approval.is_some());
    assert_eq!(events[1].scope.as_deref(), Some("call_1_0"));
}

#[test]
fn poll_telegram_skips_unparseable_updates_but_advances_offset() {
    let updates = vec![
        json!({"update_id": 50, "unknown_blob": true}),
        json!({
            "update_id": 51,
            "message": {"chat": {"id": 7}, "text": "hi"}
        }),
    ];
    let (events, next) = collect_telegram_events(&updates, 0);
    assert_eq!(next, 52);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].thread_id, "7");
}

// ── outbound routing ────────────────────────────────────────────────────

fn two_surfaces() -> Vec<Arc<dyn Channel>> {
    vec![named_channel("telegram"), named_channel("discord")]
}

#[test]
fn route_outbound_honors_claimed_match_over_gateway_tag() {
    let channels = two_surfaces();
    let mut claimed = HashMap::new();
    claimed.insert("t1".to_string(), "discord".to_string());
    // Claimed by discord even though the tag says telegram: the claim wins.
    let msg = outbound_msg("t1", "telegram");
    assert_eq!(
        route_outbound(&channels, &claimed, &msg).unwrap().name(),
        "discord"
    );
}

#[test]
fn route_outbound_uses_gateway_tag_when_thread_unclaimed() {
    let channels = two_surfaces();
    let claimed = HashMap::new();
    let msg = outbound_msg("t9", "discord");
    assert_eq!(
        route_outbound(&channels, &claimed, &msg).unwrap().name(),
        "discord"
    );
}

#[test]
fn route_outbound_lone_channel_takes_untagged_messages() {
    let channels: Vec<Arc<dyn Channel>> = vec![named_channel("telegram")];
    let claimed = HashMap::new();
    let msg = outbound_msg("t9", "");
    assert_eq!(
        route_outbound(&channels, &claimed, &msg).unwrap().name(),
        "telegram"
    );
}

#[test]
fn route_outbound_never_misdelivers_across_surfaces() {
    // Two surfaces, no claim, no tag: dropping beats sending a Telegram
    // reply to Discord (or vice versa). This was the old
    // `channels.first()` fallback bug.
    let channels = two_surfaces();
    let claimed = HashMap::new();
    let msg = outbound_msg("t9", "");
    assert!(route_outbound(&channels, &claimed, &msg).is_none());
}

// ── bounded retries ─────────────────────────────────────────────────────

#[test]
fn retry_or_dead_letter_bounces_until_the_bound() {
    let err = ChannelError::new("TEST_FAIL", "boom");
    let msg = outbound_msg("t", "telegram");
    let msg = retry_or_dead_letter(msg, "telegram", &err).expect("attempt 1 requeues");
    assert_eq!(msg.attempts, 1);
    let msg = retry_or_dead_letter(msg, "telegram", &err).expect("attempt 2 requeues");
    assert_eq!(msg.attempts, 2);
    assert!(
        retry_or_dead_letter(msg, "telegram", &err).is_none(),
        "attempt 3 dead-letters instead of requeueing forever"
    );
}
