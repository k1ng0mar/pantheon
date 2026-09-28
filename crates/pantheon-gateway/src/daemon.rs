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

use crate::channel::{Channel, ChannelEnvelope, ChannelError, ChannelEvent};
use crate::telegram::{TelegramRestTransport, TelegramTransport};
use crate::OutboundMessage;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Send attempts per message before it is dead-lettered. A reply that can
/// never be delivered (wrong chat id, revoked token, persistent 429) must
/// not wedge the queue behind it forever: three strikes, then a log line.
pub const MAX_SEND_ATTEMPTS: u32 = 3;

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

/// Choose the channel that should deliver `msg`. Pure: unit-tested.
///
/// 1. The channel that claimed the thread (polled an event from it) owns
///    the reply — first claim wins, so a thread is never stolen.
/// 2. Otherwise the message's gateway tag names a channel
///    ("telegram"/"discord").
/// 3. Otherwise a lone channel takes it (single-surface daemons).
/// 4. Otherwise `None`: delivering a reply to the wrong surface is worse
///    than dropping it, so the caller dead-letters with a log.
pub fn route_outbound(
    channels: &[Arc<dyn Channel>],
    claimed: &HashMap<String, String>,
    msg: &OutboundMessage,
) -> Option<Arc<dyn Channel>> {
    if let Some(owner) = claimed.get(&msg.to_conversation) {
        if let Some(c) = channels.iter().find(|c| c.name() == owner) {
            return Some(c.clone());
        }
    }
    if !msg.gateway.is_empty() {
        if let Some(c) = channels.iter().find(|c| c.name() == msg.gateway) {
            return Some(c.clone());
        }
    }
    if channels.len() == 1 {
        return Some(channels[0].clone());
    }
    None
}

/// What to do with a message whose send just failed: bump the attempt
/// counter and requeue, or dead-letter once the bound is hit. Pure apart
/// from the log line, so the retry bound is unit-testable without sleeping
/// through daemon ticks.
fn retry_or_dead_letter(
    mut msg: OutboundMessage,
    channel_name: &str,
    err: &ChannelError,
) -> Option<OutboundMessage> {
    msg.attempts += 1;
    if msg.attempts >= MAX_SEND_ATTEMPTS {
        eprintln!(
            "daemon: dead-lettering message to '{}' after {} attempts ({}): {err}",
            msg.to_conversation, msg.attempts, channel_name,
        );
        None
    } else {
        eprintln!(
            "daemon: deliver to {channel_name} failed (attempt {}/{MAX_SEND_ATTEMPTS}): {err}",
            msg.attempts,
        );
        Some(msg)
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
    /// thread-first (the channel that claimed the thread, else the
    /// message's gateway tag, else a lone channel — never fan-out, never
    /// a wrong surface) and failed sends are retried at most
    /// [`MAX_SEND_ATTEMPTS`] times before the message is dead-lettered
    /// with a log. Rate-limited sends stay queued for the next tick and
    /// honor the platform's `Retry-After` hint.
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
        // thread_id -> owning channel name. First claim wins.
        let mut claimed: HashMap<String, String> = HashMap::new();
        // The long-poll path has no channel object of its own; attribute
        // its events to the bound telegram channel (else the first one).
        let telegram_owner: Option<String> = channels
            .iter()
            .find(|c| c.name() == "telegram")
            .or(channels.first())
            .map(|c| c.name().to_string());
        while !stop() {
            let mut progressed = false;
            // Telegram long poll (with the persisted cursor).
            if let Some((transport, _api_base, _bot_token)) = telegram {
                match poll_telegram_once(transport.as_ref(), cursor.get(), self.long_poll_timeout) {
                    Ok((events, next)) => {
                        for event in &events {
                            route_event(sink, event);
                            if let Some(owner) = &telegram_owner {
                                claimed
                                    .entry(event.thread_id.clone())
                                    .or_insert_with(|| owner.clone());
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
                    claimed
                        .entry(event.thread_id.clone())
                        .or_insert_with(|| channel.name().to_string());
                    progressed = true;
                }
            }
            // Deliver outbound messages. Never fan-out: one reply goes to
            // one surface, chosen by `route_outbound`. Failed sends are
            // requeued at the front in order so conversations keep
            // sequence; rate-limited ones also force the idle backoff so we
            // don't hot-loop the API.
            {
                let mut out = outbound.lock().unwrap_or_else(|e| e.into_inner());
                let msgs: Vec<OutboundMessage> = std::mem::take(&mut *out);
                drop(out);
                let mut pending: Vec<OutboundMessage> = Vec::new();
                let mut rate_limited = false;
                for msg in msgs {
                    let target = route_outbound(&channels, &claimed, &msg);
                    let Some(channel) = target else {
                        // No owning surface: misdelivering is worse than
                        // dropping, so this is a dead letter with a log.
                        eprintln!(
                            "daemon: no channel for thread '{}' (gateway '{}'); dead-lettering",
                            msg.to_conversation, msg.gateway,
                        );
                        continue;
                    };
                    let envelope = ChannelEnvelope {
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
                    match channel.send(envelope) {
                        Ok(()) => {
                            progressed = true;
                        }
                        Err(e) => {
                            if e.is_rate_limited() {
                                rate_limited = true;
                                // Honor the platform's own wait hint, not
                                // just our backoff guess.
                                rate_wait_ms =
                                    rate_wait_ms.max(crate::delivery::retry_delay_ms(&e, backoff));
                            }
                            if let Some(retry) = retry_or_dead_letter(msg, channel.name(), &e) {
                                pending.push(retry);
                            }
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
