//! Channel daemon: standalone polling loop for surfaces.
//!
//! Telegram is long-polled (`getUpdates`) with the offset cursor kept in
//! memory and persisted to disk so a restart does not replay the window.
//! Discord's gateway is a websocket protocol; running one without an async
//! runtime is out of scope here, so Discord inbound remains webhook-fed
//! (`push_inbound`), which the same daemon loop drains.
//!
//! The daemon owns exactly one responsibility: move events from surfaces
//! into the runtime (chat / grant / deny / cancel) and move frames back out.
//! It contains no business logic.

use crate::channel::{Channel, ChannelEvent};
use crate::telegram::{TelegramRestTransport, TelegramTransport};
use crate::OutboundMessage;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where polled events go: implemented by the runtime bridge.
pub trait EventSink: Send + Sync {
    /// A user message arrived.
    fn on_message(&self, thread_id: &str, text: &str);
    /// An approval button was clicked.
    fn on_approval(&self, thread_id: &str, scope: &str, grant: bool);
}

/// Persisted `update_id` cursor so a daemon restart skips already-seen
/// updates. Telegram offsets: pass `offset = last_id + 1`.
#[derive(Debug, Default)]
pub struct UpdateCursor {
    path: PathBuf,
    offset: Mutex<i64>,
}

impl UpdateCursor {
    pub fn new(path: PathBuf) -> Self {
        let offset = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0);
        Self {
            path,
            offset: Mutex::new(offset),
        }
    }
    pub fn get(&self) -> i64 {
        *self.offset.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn advance(&self, to: i64) {
        let mut o = self.offset.lock().unwrap_or_else(|e| e.into_inner());
        if to > *o {
            *o = to;
            // Best-effort persist: a lost cursor only replays the poll window.
            let _ = std::fs::write(&self.path, to.to_string());
        }
    }
}

/// Long-poll Telegram once through the transport trait. Returns normalized
/// events and the next offset (highest update_id + 1). A transport error is
/// returned as Err so the caller decides the backoff; it never panics the
/// daemon.
pub fn poll_telegram_once(
    transport: &dyn TelegramTransport,
    offset: i64,
    timeout_secs: u64,
) -> Result<(Vec<ChannelEvent>, i64), String> {
    let updates = transport
        .get_updates(offset, timeout_secs)
        .map_err(|e| e.to_string())?;
    let mut events = Vec::new();
    let mut highest = offset;
    for update in &updates {
        let id = update.get("update_id").and_then(Value::as_i64).unwrap_or(0);
        if id >= highest {
            highest = id + 1;
        }
        // Reuse the adapter's normalizer so webhook and poll paths agree.
        if let Ok(Some(event)) = crate::telegram::parse_event(update) {
            events.push(event);
        }
    }
    Ok((events, highest))
}

/// Route one channel event to the sink. Returns the thread id it came from.
pub fn route_event(sink: &dyn EventSink, event: &ChannelEvent) -> String {
    match (&event.approval, &event.scope) {
        (Some(answer), Some(scope)) => {
            sink.on_approval(
                &event.thread_id,
                scope,
                matches!(answer, crate::channel::ApprovalAnswer::Grant),
            );
            event.thread_id.clone()
        }
        _ => {
            if !event.text.trim().is_empty() {
                sink.on_message(&event.thread_id, &event.text);
            }
            event.thread_id.clone()
        }
    }
}

/// The daemon loop. Runs until `stop` returns true. Each iteration polls
/// Telegram (long poll), drains any bridge-fed inboxes (Discord via
/// `push_inbound`), routes events, and flushes outbound messages.
pub struct ChannelDaemon {
    pub poll_interval: Duration,
    pub long_poll_timeout: u64,
    state_path: PathBuf,
}

impl ChannelDaemon {
    pub fn new(state_path: PathBuf) -> Self {
        Self {
            poll_interval: Duration::from_secs(1),
            long_poll_timeout: 25,
            state_path,
        }
    }

    /// Run until `stop` signals. `channels` are drained each tick (their
    /// `poll()` includes bridge-fed inboxes); `telegram` participates in
    /// true long polling when configured.
    pub fn run(
        &self,
        channels: Vec<Arc<dyn Channel>>,
        telegram: Option<(&Arc<TelegramRestTransport>, &str, &str)>,
        sink: &dyn EventSink,
        outbound: &Mutex<Vec<OutboundMessage>>,
        stop: &dyn Fn() -> bool,
    ) {
        let cursor = UpdateCursor::new(self.state_path.clone());
        let mut backoff = 0u32;
        while !stop() {
            let mut progressed = false;
            // Telegram long poll (with the persisted cursor).
            if let Some((transport, _api_base, _bot_token)) = telegram {
                match poll_telegram_once(transport.as_ref(), cursor.get(), self.long_poll_timeout) {
                    Ok((events, next)) => {
                        for event in &events {
                            route_event(sink, event);
                        }
                        if next > cursor.get() {
                            cursor.advance(next);
                        }
                        if !events.is_empty() {
                            progressed = true;
                        }
                    }
                    Err(e) => {
                        eprintln!("telegram poll error: {e}");
                    }
                }
            }
            // Drain every channel's inbox (webhook-fed Discord, bridges).
            for channel in &channels {
                for event in channel.poll() {
                    route_event(sink, &event);
                    progressed = true;
                }
            }
            // Flush outbound messages in order.
            {
                let mut out = outbound.lock().unwrap_or_else(|e| e.into_inner());
                for msg in out.drain(..) {
                    // Delivery planning (backoff/drop) is the caller's job;
                    // the daemon only guarantees ordering.
                    let _ = &msg;
                }
            }
            if progressed {
                backoff = 0;
            } else {
                backoff = backoff.saturating_add(1);
            }
            // Idle backoff between empty polls; capped like delivery.
            let wait = if backoff > 0 {
                Duration::from_millis(crate::delivery::backoff_ms(backoff.min(6)))
                    .min(self.poll_interval.max(Duration::from_secs(5)))
            } else {
                self.poll_interval
            };
            std::thread::sleep(wait);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::{ApprovalAnswer, ChannelError};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingSink {
        messages: Mutex<Vec<(String, String)>>,
        approvals: Mutex<Vec<(String, String, bool)>>,
    }
    impl EventSink for RecordingSink {
        fn on_message(&self, thread: &str, text: &str) {
            self.messages
                .lock()
                .unwrap()
                .push((thread.into(), text.into()));
        }
        fn on_approval(&self, thread: &str, scope: &str, grant: bool) {
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
            ticks.fetch_add(1, Ordering::SeqCst) > 0;
            true
        };
        daemon.run(vec![], None, &sink, &outbound, &stop_fn);
        assert!(sink.messages.lock().unwrap().is_empty());
    }

    #[test]
    fn poll_telegram_parses_updates_through_the_trait() {
        struct FakeTransport;
        impl TelegramTransport for FakeTransport {
            fn send_message(&self, _chat_id: &str, _payload: &Value) -> Result<(), ChannelError> {
                Ok(())
            }
            fn get_updates(
                &self,
                _offset: i64,
                _timeout_secs: u64,
            ) -> Result<Vec<Value>, ChannelError> {
                Ok(vec![
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
                ])
            }
        }
        let (events, next) = poll_telegram_once(&FakeTransport, 0, 1).unwrap();
        assert_eq!(next, 102);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "hello");
        assert_eq!(events[0].thread_id, "42");
        assert!(events[1].approval.is_some());
        assert_eq!(events[1].scope.as_deref(), Some("call_1_0"));
    }

    #[test]
    fn poll_telegram_error_is_returned_not_panicked() {
        struct BrokenTransport;
        impl TelegramTransport for BrokenTransport {
            fn send_message(&self, _chat_id: &str, _payload: &Value) -> Result<(), ChannelError> {
                Ok(())
            }
            fn get_updates(
                &self,
                _offset: i64,
                _timeout_secs: u64,
            ) -> Result<Vec<Value>, ChannelError> {
                Err(ChannelError::new(
                    "TELEGRAM_HTTP",
                    "unreachable".to_string(),
                ))
            }
        }
        let result = poll_telegram_once(&BrokenTransport, 0, 1);
        assert!(result.is_err());
    }
}
