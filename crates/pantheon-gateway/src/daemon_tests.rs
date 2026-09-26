//! Tests for `pantheon_gateway::daemon::tests` — sibling file so sources stay test-free.
use super::*;
use crate::channel::ApprovalAnswer;
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
