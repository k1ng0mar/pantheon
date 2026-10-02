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
use crate::channel_voice::VoiceOutcome;
use crate::telegram::{TelegramChannel, TelegramRestTransport, TelegramTransport};
use crate::OutboundMessage;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Send attempts per message before it is dead-lettered. A reply that can
/// never be delivered (wrong chat id, revoked token, persistent 429) must
/// not wedge the queue behind it forever: three strikes, then a log line.
pub const MAX_SEND_ATTEMPTS: u32 = 3;

/// Consecutive poll/send failures before a channel is marked dead.
const HEALTH_DEAD_AFTER: u32 = 5;
/// Health files are rewritten on state transition and at most this often.
const HEALTH_WRITE_INTERVAL_MS: u64 = 30_000;
/// A health file older than this means the daemon stopped reporting.
const HEALTH_STALE_AFTER_MS: u64 = 120_000;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Channel health state for `GET /api/health/channels` (F-7). The daemon
/// reports; the dashboard reads via [`channel_health_json`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelState {
    /// No poll/send outcome observed yet.
    Unknown,
    /// Last interaction succeeded.
    Connected,
    /// Recent failures, still retrying.
    Degraded,
    /// Auth failure, failure budget exhausted, or dead-lettered: needs an
    /// operator (fix the token / config, then the daemon recovers on the
    /// next success).
    Dead,
}

impl ChannelState {
    fn as_str(self) -> &'static str {
        match self {
            ChannelState::Unknown => "unknown",
            ChannelState::Connected => "connected",
            ChannelState::Degraded => "degraded",
            ChannelState::Dead => "dead",
        }
    }
}

/// One channel's health snapshot. Serialized to
/// `channel-health-<name>.json` next to the cursor file.
#[derive(Debug, Clone)]
pub struct ChannelHealth {
    pub name: String,
    pub state: ChannelState,
    pub last_error: Option<String>,
    pub last_success_ms: Option<u64>,
    pub consecutive_failures: u32,
    pub updated_ms: u64,
}

/// True for errors that mean the credential is gone (401 from the
/// platform), not that the network hiccuped: those go straight to dead,
/// no failure-budget grace.
fn is_auth_failure(detail: &str) -> bool {
    detail.contains("401") || detail.to_lowercase().contains("unauthorized")
}

/// Per-daemon health tracker (F-7). Call the `record_*` methods from the
/// daemon loop; snapshots are written on state transition and at most
/// every 30s so a crash mid-degradation still leaves evidence.
pub struct ChannelHealthTracker {
    dir: PathBuf,
    states: HashMap<String, ChannelHealth>,
    last_write_ms: u64,
}

impl ChannelHealthTracker {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            dir,
            states: HashMap::new(),
            last_write_ms: 0,
        }
    }

    fn entry(&mut self, name: &str) -> &mut ChannelHealth {
        let now = now_ms();
        self.states
            .entry(name.to_string())
            .or_insert(ChannelHealth {
                name: name.to_string(),
                state: ChannelState::Unknown,
                last_error: None,
                last_success_ms: None,
                consecutive_failures: 0,
                updated_ms: now,
            })
    }

    fn record_ok(&mut self, name: &str) {
        let now = now_ms();
        let h = self.entry(name);
        let transitioned = h.state != ChannelState::Connected;
        h.state = ChannelState::Connected;
        h.consecutive_failures = 0;
        h.last_error = None;
        h.last_success_ms = Some(now);
        h.updated_ms = now;
        self.maybe_write(transitioned);
    }

    fn record_err(&mut self, name: &str, detail: String) {
        let now = now_ms();
        let h = self.entry(name);
        let prev = h.state;
        if is_auth_failure(&detail) {
            h.state = ChannelState::Dead;
            h.consecutive_failures = HEALTH_DEAD_AFTER;
        } else {
            h.consecutive_failures += 1;
            h.state = if h.consecutive_failures >= HEALTH_DEAD_AFTER {
                ChannelState::Dead
            } else {
                ChannelState::Degraded
            };
        }
        let transitioned = prev != h.state;
        h.last_error = Some(detail);
        h.updated_ms = now;
        // The 30s throttle in maybe_write keeps the failure budget
        // visible even between transitions.
        self.maybe_write(transitioned);
    }

    /// Telegram long-poll batch succeeded (even an empty batch proves the
    /// connection works).
    pub fn record_poll_ok(&mut self, name: &str) {
        self.record_ok(name);
    }

    /// Telegram long-poll batch failed.
    pub fn record_poll_err(&mut self, name: &str, err: &str) {
        self.record_err(name, err.to_string());
    }

    /// A channel send succeeded.
    pub fn record_send_ok(&mut self, name: &str) {
        self.record_ok(name);
    }

    /// A channel send failed (non-rate-limit).
    pub fn record_send_err(&mut self, name: &str, err: &ChannelError) {
        if is_auth_failure(&err.to_string()) {
            self.record_err(name, format!("{} (credential rejected)", err));
        } else {
            self.record_err(name, err.to_string());
        }
    }

    /// A 429 is the platform asking us to slow down, not a channel fault:
    /// mark degraded without spending the failure budget.
    pub fn record_send_rate_limited(&mut self, name: &str, err: &ChannelError) {
        let now = now_ms();
        let h = self.entry(name);
        let transitioned = h.state != ChannelState::Degraded && h.state != ChannelState::Dead;
        if h.state != ChannelState::Dead {
            h.state = ChannelState::Degraded;
        }
        h.last_error = Some(err.to_string());
        h.updated_ms = now;
        self.maybe_write(transitioned);
    }

    /// A message exhausted [`MAX_SEND_ATTEMPTS`] and was dead-lettered.
    pub fn record_dead_letter(&mut self, name: &str, detail: &str) {
        let now = now_ms();
        let h = self.entry(name);
        let transitioned = h.state != ChannelState::Dead;
        h.state = ChannelState::Dead;
        h.consecutive_failures = HEALTH_DEAD_AFTER;
        h.last_error = Some(format!(
            "dead-lettered after {MAX_SEND_ATTEMPTS} attempts: {detail}"
        ));
        h.updated_ms = now;
        self.maybe_write(transitioned);
    }

    fn maybe_write(&mut self, transitioned: bool) {
        let now = now_ms();
        if transitioned || now.saturating_sub(self.last_write_ms) >= HEALTH_WRITE_INTERVAL_MS {
            self.write();
        }
    }

    fn write(&mut self) {
        for h in self.states.values() {
            let safe: String = h
                .name
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            let path = self.dir.join(format!("channel-health-{safe}.json"));
            let v = serde_json::json!({
                "name": h.name,
                "state": h.state.as_str(),
                "last_error": h.last_error,
                "last_success_ms": h.last_success_ms,
                "consecutive_failures": h.consecutive_failures,
                "updated_ms": h.updated_ms,
            });
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&v).unwrap_or_default());
        }
        self.last_write_ms = now_ms();
    }
}

/// Merge the daemon's per-channel health files for
/// `GET /api/health/channels`. Files older than ~120s are marked stale
/// (the daemon stopped reporting); a missing file means the daemon never
/// reported that channel.
pub fn channel_health_json(data_dir: &Path) -> serde_json::Value {
    let dir = data_dir.join("gateway");
    let mut channels = Vec::new();
    let now = now_ms();
    let entries: Vec<_> = std::fs::read_dir(&dir)
        .map(|r| r.filter_map(|e| e.ok()).collect())
        .unwrap_or_default();
    for entry in entries {
        let fname = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = fname.strip_prefix("channel-health-") else {
            continue;
        };
        if !rest.ends_with(".json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let updated = v.get("updated_ms").and_then(|u| u.as_u64()).unwrap_or(0);
        if now.saturating_sub(updated) > HEALTH_STALE_AFTER_MS {
            v["stale"] = serde_json::Value::Bool(true);
        }
        channels.push(v);
    }
    channels.sort_by(|a, b| {
        a.get("name")
            .and_then(|n| n.as_str())
            .cmp(&b.get("name").and_then(|n| n.as_str()))
    });
    serde_json::json!({ "channels": channels })
}

/// Where polled events go: implemented by the runtime bridge. `sender`
/// is the platform sender identity when the surface exposes one; the
/// gateway allowlist keys on it.
pub trait EventSink: Send + Sync {
    /// A user message arrived.
    fn on_message(&self, thread_id: &str, sender: Option<&str>, text: &str);
    /// An approval button was clicked. `run_id` is `Some` when the
    /// callback carries one (`grant:{run_id}:{scope}`); `None` for the
    /// plain `grant:{scope}` format, where the sink falls back to its
    /// thread→run map.
    fn on_approval(
        &self,
        thread_id: &str,
        sender: Option<&str>,
        run_id: Option<&str>,
        scope: &str,
        grant: bool,
    );
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
                event.run_id.as_deref(),
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
///    the reply - first claim wins, so a thread is never stolen.
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
    /// message's gateway tag, else a lone channel - never fan-out, never
    /// a wrong surface) and failed sends are retried at most
    /// [`MAX_SEND_ATTEMPTS`] times before the message is dead-lettered
    /// with a log. Rate-limited sends stay queued for the next tick and
    /// honor the platform's `Retry-After` hint.
    pub fn run(
        &self,
        channels: Vec<Arc<dyn Channel>>,
        telegram: Option<(&Arc<TelegramChannel>, &Arc<TelegramRestTransport>)>,
        sink: &dyn EventSink,
        outbound: &Mutex<Vec<OutboundMessage>>,
        stop: &dyn Fn() -> bool,
    ) {
        let cursor = UpdateCursor::new(self.state_path.clone());
        // Channel health (F-7): snapshots live next to the cursor file, in
        // `<data_dir>/gateway/`.
        let state_dir = self
            .state_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        // Retention prunes under the data dir; state_dir is
        // `<data_dir>/gateway/`, so its parent is the data dir.
        let data_dir = state_dir.parent().map(|p| p.to_path_buf());
        let mut health = ChannelHealthTracker::new(state_dir);
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
        let telegram_name = telegram_owner
            .clone()
            .unwrap_or_else(|| "telegram".to_string());
        while !stop() {
            // Retention: prune old events/search chunks/claims past the
            // configured window. The 24h gate lives inside
            // `maybe_run_retention`, so this is a cheap check per tick.
            if let Some(data_dir) = data_dir.as_deref() {
                pantheon_scheduler::retention::maybe_run_retention(data_dir);
            }
            let mut progressed = false;
            // Telegram long poll (with the persisted cursor). Voice-aware:
            // voice/audio messages are downloaded and transcribed with
            // the telegram channel's voice pipes; declines ride the
            // normal outbound queue below.
            if let Some((channel, transport)) = telegram {
                match channel.poll_updates(transport.as_ref(), cursor.get(), self.long_poll_timeout)
                {
                    Ok((outcomes, next)) => {
                        health.record_poll_ok(&telegram_name);
                        for outcome in &outcomes {
                            match outcome {
                                VoiceOutcome::Event(event) => {
                                    route_event(sink, event);
                                    if let Some(owner) = &telegram_owner {
                                        claimed
                                            .entry(event.thread_id.clone())
                                            .or_insert_with(|| owner.clone());
                                    }
                                }
                                VoiceOutcome::Reply { thread_id, text } => {
                                    let mut out =
                                        outbound.lock().unwrap_or_else(|e| e.into_inner());
                                    out.push(OutboundMessage::new(thread_id, text, "telegram"));
                                }
                            }
                        }
                        if next > cursor.get() {
                            cursor.advance(next);
                        }
                        if !outcomes.is_empty() {
                            progressed = true;
                        }
                    }
                    Err(e) => {
                        health.record_poll_err(&telegram_name, &e);
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
                            health.record_send_ok(channel.name());
                            progressed = true;
                        }
                        Err(e) => {
                            if e.is_rate_limited() {
                                rate_limited = true;
                                // Honor the platform's own wait hint, not
                                // just our backoff guess.
                                rate_wait_ms =
                                    rate_wait_ms.max(crate::delivery::retry_delay_ms(&e, backoff));
                                health.record_send_rate_limited(channel.name(), &e);
                            } else {
                                health.record_send_err(channel.name(), &e);
                            }
                            if let Some(retry) = retry_or_dead_letter(msg, channel.name(), &e) {
                                pending.push(retry);
                            } else {
                                health.record_dead_letter(channel.name(), &e.to_string());
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
mod health_tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pantheon-health-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn poll_ok_marks_connected() {
        let d = tmpdir("ok");
        let mut t = ChannelHealthTracker::new(d.clone());
        t.record_poll_ok("telegram");
        let h = t.states.get("telegram").unwrap();
        assert_eq!(h.state, ChannelState::Connected);
        assert_eq!(h.consecutive_failures, 0);
        // Snapshot file written on the unknown->connected transition.
        assert!(d.join("channel-health-telegram.json").is_file());
    }

    #[test]
    fn failures_degrade_then_die() {
        let d = tmpdir("degrade");
        let mut t = ChannelHealthTracker::new(d);
        for i in 1..HEALTH_DEAD_AFTER {
            t.record_poll_err("telegram", "getUpdates failed [TELEGRAM_HTTP]");
            let h = t.states.get("telegram").unwrap();
            assert_eq!(h.state, ChannelState::Degraded);
            assert_eq!(h.consecutive_failures, i);
        }
        t.record_poll_err("telegram", "getUpdates failed [TELEGRAM_HTTP]");
        assert_eq!(t.states.get("telegram").unwrap().state, ChannelState::Dead);
    }

    #[test]
    fn revoked_token_is_dead_immediately() {
        let d = tmpdir("revoked");
        let mut t = ChannelHealthTracker::new(d);
        t.record_poll_err(
            "telegram",
            "getUpdates failed [TELEGRAM_HTTP]: 401 unauthorized (token revoked?)",
        );
        let h = t.states.get("telegram").unwrap();
        assert_eq!(h.state, ChannelState::Dead);
    }

    #[test]
    fn success_resets_failures() {
        let d = tmpdir("reset");
        let mut t = ChannelHealthTracker::new(d);
        t.record_poll_err("telegram", "boom");
        t.record_poll_err("telegram", "boom");
        t.record_poll_ok("telegram");
        let h = t.states.get("telegram").unwrap();
        assert_eq!(h.state, ChannelState::Connected);
        assert_eq!(h.consecutive_failures, 0);
        assert!(h.last_error.is_none());
    }

    #[test]
    fn rate_limit_does_not_spend_failure_budget() {
        let d = tmpdir("ratelimit");
        let mut t = ChannelHealthTracker::new(d);
        let err = ChannelError::rate_limited("TELEGRAM_HTTP", "slow down", Some(5));
        for _ in 0..10 {
            t.record_send_rate_limited("telegram", &err);
        }
        let h = t.states.get("telegram").unwrap();
        assert_eq!(h.state, ChannelState::Degraded);
        assert_eq!(h.consecutive_failures, 0);
    }

    #[test]
    fn dead_letter_marks_dead() {
        let d = tmpdir("deadletter");
        let mut t = ChannelHealthTracker::new(d);
        t.record_send_ok("discord");
        t.record_dead_letter("discord", "[DISCORD_HTTP] 404");
        let h = t.states.get("discord").unwrap();
        assert_eq!(h.state, ChannelState::Dead);
        assert!(h.last_error.as_deref().unwrap().contains("dead-lettered"));
    }

    #[test]
    fn reader_merges_and_marks_stale() {
        let data_dir = tmpdir("reader");
        let gw = data_dir.join("gateway");
        std::fs::create_dir_all(&gw).unwrap();
        // Fresh file: written by a tracker just now.
        let mut t = ChannelHealthTracker::new(gw.clone());
        t.record_poll_ok("telegram");
        // Stale file: hand-written with an ancient timestamp.
        std::fs::write(
            gw.join("channel-health-discord.json"),
            r#"{"name":"discord","state":"connected","last_error":null,"last_success_ms":1,"consecutive_failures":0,"updated_ms":1}"#,
        )
        .unwrap();
        let v = channel_health_json(&data_dir);
        let channels = v.get("channels").unwrap().as_array().unwrap();
        assert_eq!(channels.len(), 2);
        // Sorted by name: discord first.
        assert_eq!(channels[0].get("name").unwrap(), "discord");
        assert_eq!(
            channels[0].get("stale"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(channels[1].get("name").unwrap(), "telegram");
        assert!(channels[1].get("stale").is_none());
    }

    #[test]
    fn reader_empty_without_gateway_dir() {
        let data_dir = tmpdir("reader-empty");
        let v = channel_health_json(&data_dir);
        assert_eq!(v.get("channels").unwrap().as_array().unwrap().len(), 0);
    }
}
