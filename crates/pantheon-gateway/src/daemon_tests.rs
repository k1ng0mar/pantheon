//! Tests for `pantheon_gateway::daemon::tests` — sibling file so sources stay test-free.
use super::*;
use crate::channel::{ApprovalAnswer, MemoryChannel};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct RecordingSink {
    messages: Mutex<Vec<(String, String)>>,
    approvals: Mutex<Vec<(String, String, bool)>>,
}
impl EventSink for RecordingSink {
    fn on_message(&self, thread: &str, _sender: Option<&str>, text: &str) {
        self.messages
            .lock()
            .unwrap()
            .push((thread.into(), text.into()));
    }
    fn on_approval(&self, thread: &str, _sender: Option<&str>, scope: &str, grant: bool) {
        self.approvals
            .lock()
            .unwrap()
            .push((thread.into(), scope.into(), grant));
    }
}

#[test]
fn cursor_advances_and_persists() {
    let dir = std::env::temp_dir().join(format!("pantheon-cursor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cursor");
    let c = UpdateCursor::new(path.clone());
    assert_eq!(c.get(), 0);
    c.advance(41);
    assert_eq!(c.get(), 41);
    // Reload from disk.
    let c2 = UpdateCursor::new(path);
    assert_eq!(c2.get(), 41);
    // Never goes backwards.
    c2.advance(7);
    assert_eq!(c2.get(), 41);
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
        vec![("42".to_string(), "call_0_0".to_string(), true)]
    );
}

#[test]
fn daemon_stops_when_asked() {
    let dir = std::env::temp_dir().join(format!("pantheon-daemon-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let daemon = ChannelDaemon::new(dir.join("cursor"));
    let sink = RecordingSink {
        messages: Mutex::new(vec![]),
        approvals: Mutex::new(vec![]),
    };
    let outbound = Mutex::new(vec![]);
    let ticks = AtomicUsize::new(0);
    let stop_fn = || {
        let _ = ticks.fetch_add(1, Ordering::SeqCst) > 0;
        true
    };
    daemon.run(vec![], None, &sink, &outbound, &stop_fn);
    assert!(sink.messages.lock().unwrap().is_empty());
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

struct FailChannel {
    name: String,
    sends: AtomicUsize,
}
impl Channel for FailChannel {
    fn name(&self) -> &str {
        &self.name
    }
    fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Err(ChannelError::new("TEST_FAIL", "boom"))
    }
    fn poll(&self) -> Vec<ChannelEvent> {
        Vec::new()
    }
}

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

#[test]
fn failed_sends_dead_letter_through_the_daemon() {
    let dir = std::env::temp_dir().join(format!("pantheon-deadletter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut daemon = ChannelDaemon::new(dir.join("cursor"));
    daemon.poll_interval = Duration::from_millis(1);
    let fail = Arc::new(FailChannel {
        name: "telegram".into(),
        sends: AtomicUsize::new(0),
    });
    let channels: Vec<Arc<dyn Channel>> = vec![fail.clone()];
    let sink = RecordingSink {
        messages: Mutex::new(vec![]),
        approvals: Mutex::new(vec![]),
    };
    // One attempt left before the bound: a single tick must dead-letter it.
    let outbound = Mutex::new(vec![OutboundMessage {
        to_conversation: "t".into(),
        text: "hi".into(),
        gateway: "telegram".into(),
        attempts: MAX_SEND_ATTEMPTS - 1,
    }]);
    let ticks = AtomicUsize::new(0);
    let stop = || ticks.fetch_add(1, Ordering::SeqCst) >= 1;
    daemon.run(channels, None, &sink, &outbound, &stop);
    assert_eq!(fail.sends.load(Ordering::SeqCst), 1);
    assert!(
        outbound.lock().unwrap().is_empty(),
        "dead-lettered, not requeued forever"
    );
}

#[test]
fn daemon_routes_claimed_thread_to_owning_channel() {
    let dir = std::env::temp_dir().join(format!("pantheon-route-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut daemon = ChannelDaemon::new(dir.join("cursor"));
    daemon.poll_interval = Duration::from_millis(1);
    let telegram = named_channel("telegram");
    let discord = named_channel("discord");
    // A Discord inbound event claims thread T9 for discord.
    discord.push_inbound(ChannelEvent {
        thread_id: "T9".into(),
        run_id: None,
        text: "hi".into(),
        approval: None,
        scope: None,
        sender: None,
    });
    let channels: Vec<Arc<dyn Channel>> = vec![telegram.clone(), discord.clone()];
    let sink = RecordingSink {
        messages: Mutex::new(vec![]),
        approvals: Mutex::new(vec![]),
    };
    let outbound = Mutex::new(vec![outbound_msg("T9", "")]);
    let ticks = AtomicUsize::new(0);
    let stop = || ticks.fetch_add(1, Ordering::SeqCst) >= 1;
    daemon.run(channels, None, &sink, &outbound, &stop);
    assert_eq!(discord.drain_outbound().len(), 1, "reply went to discord");
    assert!(
        telegram.drain_outbound().is_empty(),
        "telegram never saw the discord reply"
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 1, "event routed");
}
