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

/// Where polled events go: implemented by the runtime bridge. `sender`
/// is the platform sender identity when the surface exposes one; the
/// gateway allowlist keys on it.
pub trait EventSink: Send + Sync {
    /// A user message arrived.
    fn on_message(&self, thread_id: &str, sender: Option<&str>, text: &str);
    /// An approval button was clicked.
    fn on_approval(&self, thread_id: &str, sender: Option<&str>, scope: &str, grant: bool);
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
    Ok(collect_telegram_events(&updates, offset))
}

/// Fold polled updates into channel events + the next offset. Pure:
/// unit-tested directly with inline update JSON, no transport involved.
pub fn collect_telegram_events(updates: &[Value], offset: i64) -> (Vec<ChannelEvent>, i64) {
    let mut events = Vec::new();
    let mut highest = offset;
    for update in updates {
        let id = update.get("update_id").and_then(Value::as_i64).unwrap_or(0);
        if id >= highest {
            highest = id + 1;
        }
        // Reuse the adapter's normalizer so webhook and poll paths agree.
        if let Ok(Some(event)) = crate::telegram::parse_event(update) {
            events.push(event);
        }
    }
    (events, highest)
}

/// True when the outbound queue is empty (used for backoff decisions).
fn pending_none(outbound: &Mutex<Vec<OutboundMessage>>) -> bool {
    outbound.lock().map(|q| q.is_empty()).unwrap_or(true)
}

/// Route one channel event to the sink. Returns the thread id it came from.
pub fn route_event(sink: &dyn EventSink, event: &ChannelEvent) -> String {
    match (&event.approval, &event.scope) {
        (Some(answer), Some(scope)) => {
            sink.on_approval(
                &event.thread_id,
                event.sender.as_deref(),
                scope,
                matches!(answer, crate::channel::ApprovalAnswer::Grant),
            );
            event.thread_id.clone()
        }
        _ => {
            if !event.text.trim().is_empty() {
                sink.on_message(&event.thread_id, event.sender.as_deref(), &event.text);
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
    /// true long polling when configured. Outbound delivery is
    /// thread-first (exact claimed-thread owner, else first channel;
    /// never fan-out) and failed sends are requeued at the front in
    /// order. Rate-limited sends stay queued for the next tick.
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
        // Longest server-asked wait outstanding (from `Retry-After` /
        // `parameters.retry_after` on a 429). The platform knows its window;
        // our exponential guess does not override it.
        let mut rate_wait_ms = 0u64;
        let mut claimed: Vec<String> = Vec::new();
        while !stop() {
            let mut progressed = false;
            // Telegram long poll (with the persisted cursor).
            if let Some((transport, _api_base, _bot_token)) = telegram {
                match poll_telegram_once(transport.as_ref(), cursor.get(), self.long_poll_timeout) {
                    Ok((events, next)) => {
                        for event in &events {
                            route_event(sink, event);
                            if !claimed.contains(&event.thread_id) {
                                claimed.push(event.thread_id.clone());
                            }
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
                    if !claimed.contains(&event.thread_id) {
                        claimed.push(event.thread_id.clone());
                    }
                    progressed = true;
                }
            }
            // Deliver outbound messages. Thread-first routing: the channel
            // that claimed the thread (polled an event from it) owns the
            // reply; otherwise the first bound channel is the fallback.
            // Never fan-out: one reply goes to one surface. Failed sends
            // (rate-limited / transport errors) are requeued at the front
            // in order so conversations keep sequence; rate-limited ones
            // also force the idle backoff so we don't hot-loop the API.
            {
                let mut out = outbound.lock().unwrap_or_else(|e| e.into_inner());
                let msgs: Vec<OutboundMessage> = std::mem::take(&mut *out);
                drop(out);
                let mut pending: Vec<OutboundMessage> = Vec::new();
                let mut rate_limited = false;
                for msg in msgs {
                    let target = claimed
                        .iter()
                        .find(|t| **t == msg.to_conversation)
                        .and_then(|_| channels.first().cloned())
                        .or_else(|| channels.first().cloned());
                    match target {
                        Some(channel) => {
                            let envelope = crate::channel::ChannelEnvelope {
                                thread_id: msg.to_conversation.clone(),
                                frame: crate::stream::UiFrame {
                                    id: 0,
                                    kind: crate::stream::UiFrameKind::Text,
                                    run_id: String::new(),
                                    thread_id: msg.to_conversation.clone(),
                                    name: "delta".into(),
                                    text: msg.text.clone(),
                                    interrupt: false,
                                    genui: None,
                                },
                            };
                            if let Err(e) = channel.send(envelope) {
                                eprintln!("daemon: deliver to {} failed: {e}", channel.name());
                                if e.is_rate_limited() {
                                    rate_limited = true;
                                    // Honor the platform's own wait hint, not
                                    // just our backoff guess.
                                    rate_wait_ms = rate_wait_ms
                                        .max(crate::delivery::retry_delay_ms(&e, backoff));
                                }
                                pending.push(msg);
                            } else {
                                progressed = true;
                            }
                        }
                        None => {
                            eprintln!(
                                "daemon: no channel bound for thread {}; message dropped",
                                msg.to_conversation
                            );
                        }
                    }
                }
                if !pending.is_empty() {
                    // Requeue at the front in original order.
                    let mut out = outbound.lock().unwrap_or_else(|e| e.into_inner());
                    pending.extend(std::mem::take(&mut *out));
                    *out = pending;
                    if rate_limited {
                        // Don't count a 429 as idle-but-healthy: force the
                        // backoff path below so we sleep before retrying.
                        backoff = backoff.max(1);
                    }
                }
            }
            if progressed && pending_none(outbound) {
                backoff = 0;
                rate_wait_ms = 0;
            } else if !progressed {
                backoff = backoff.saturating_add(1);
            }
            // Idle backoff between empty polls; capped like delivery. A
            // server-asked wait overrides the guess when it is longer.
            let wait = if backoff > 0 || rate_wait_ms > 0 {
                Duration::from_millis(crate::delivery::backoff_ms(backoff.min(6)).max(rate_wait_ms))
                    .min(
                        self.poll_interval
                            .max(Duration::from_secs(5))
                            .max(Duration::from_millis(rate_wait_ms)),
                    )
            } else {
                self.poll_interval
            };
            std::thread::sleep(wait);
        }
    }
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
