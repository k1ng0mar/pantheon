//! Discord gateway websocket client (std-only, blocking).
//!
//! Speaks Discord's gateway protocol over tungstenite's blocking client:
//! IDENTIFY → READY → dispatch loop, with heartbeat/reconnect per Discord's
//! rules (heartbeat every `heartbeat_interval`, reconnect op 7, invalid
//! session op 9, resume op 6). Messages and interactions arrive as
//! MESSAGE_CREATE / INTERACTION_CREATE dispatches and are normalized by
//! `parse_event` into the shared channel seam, exactly like the webhook
//! bridge path.
//!
//! Why blocking and not tokio: the rest of Pantheon is std-only. One
//! daemon thread per gateway connection is the same cost model as the
//! AG-UI server's thread-per-connection.

use serde_json::{json, Value};
use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tungstenite::client::IntoClientRequest;
use tungstenite::Message;

use crate::discord::DiscordChannel;

/// Discord gateway URL (v10, JSON encoding - no compression, keeps the
/// client dependency-free beyond the websocket itself).
pub const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Opcodes we must handle.
const OP_DISPATCH: u8 = 0;
const OP_HEARTBEAT: u8 = 1;
const OP_RECONNECT: u8 = 7;
const OP_INVALID_SESSION: u8 = 9;
const OP_HELLO: u8 = 10;
const OP_HEARTBEAT_ACK: u8 = 11;

/// Events the channel layer consumes.
const EV_READY: &str = "READY";
const EV_MESSAGE_CREATE: &str = "MESSAGE_CREATE";
const EV_INTERACTION_CREATE: &str = "INTERACTION_CREATE";
const EV_RESUMED: &str = "RESUMED";

/// Result of driving the socket until an error or a reconnect signal.
#[derive(Debug, PartialEq, Eq)]
pub enum LoopExit {
    /// Socket died; reconnect (with resume data if any).
    Reconnect,
    /// Invalid session: drop resume data, back off, re-IDENTIFY.
    Reseed,
    /// Caller asked to stop.
    Stopped,
}

/// One gateway connection's state. `seq` is the last dispatched sequence
/// number, needed for RESUME after a reconnect.
pub struct GatewayState {
    seq: AtomicI64,
    session_id: Mutex2<Option<String>>,
    resume_gateway_url: Mutex2<Option<String>>,
}

// std-only mutex wrapper (parking_lot not needed; poisoned locks are
// recoverable here since the value is advisory).
type Mutex2<T> = std::sync::Mutex<T>;

impl GatewayState {
    fn new() -> Self {
        Self {
            seq: AtomicI64::new(-1),
            session_id: Mutex2::new(None),
            resume_gateway_url: Mutex2::new(None),
        }
    }
    fn last_seq(&self) -> i64 {
        self.seq.load(Ordering::Acquire)
    }
    fn can_resume(&self) -> bool {
        self.session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
            && self.last_seq() >= 0
    }
}

/// The Discord gateway adapter. Feed it a bot token and an inbox; dispatch
/// events land in the inbox as `ChannelEvent`s.
pub struct DiscordGateway {
    pub token: String,
    /// Intents to request: GUIL_MESSAGES (1<<9) | GUIL_MESSAGES (1<<15)
    /// | DIRECT_MESSAGES (1<<12). Minimal set for a chat/app surface.
    pub intents: u64,
}

/// Hand-written so the bot token never reaches a log line. A `#[derive(Debug)]`
/// here would print a live credential, and the token is a `pub` field, so any
/// future derive is a leak. Same intent as `SecretValue`'s Debug, kept local
/// to avoid a gateway -> secrets dependency for three fields.
impl fmt::Debug for DiscordGateway {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DiscordGateway {{ token: *** }}")
    }
}

impl DiscordGateway {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            intents: (1 << 9) | (1 << 15) | (1 << 12),
        }
    }

    fn identify(&self) -> Value {
        json!({
            "op": 2,
            "d": {
                "token": self.token,
                "intents": self.intents,
                "properties": {
                    "os": std::env::consts::OS,
                    "browser": "pantheon",
                    "device": "pantheon",
                }
            }
        })
    }

    fn resume(&self, state: &GatewayState) -> Value {
        let session = state
            .session_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_default();
        json!({
            "op": 6,
            "d": {
                "token": self.token,
                "session_id": session,
                "seq": state.last_seq(),
            }
        })
    }

    /// Connect, IDENTIFY (or RESUME), and dispatch until the socket dies or
    /// the caller stops. Every dispatch is normalized through
    /// `crate::discord::parse_event` and pushed to `inbox`.
    pub fn run_once(
        &self,
        state: &GatewayState,
        inbox: &DiscordChannel,
        stop: &dyn Fn() -> bool,
    ) -> LoopExit {
        let request = match GATEWAY_URL.into_client_request() {
            Ok(r) => r,
            Err(_) => return LoopExit::Reconnect,
        };
        let (mut socket, _resp) = match tungstenite::connect(request) {
            Ok(x) => x,
            Err(_) => return LoopExit::Reconnect,
        };
        let mut heartbeat_interval = Duration::from_secs(41); // default; HELLO overrides
        let mut last_heartbeat = std::time::Instant::now();
        let mut acked = true;

        loop {
            if stop() {
                return LoopExit::Stopped;
            }
            // Heartbeat when due (only after HELLO set the interval). A
            // missed ACK escalates to reconnect: Discord tears down
            // zombied connections after ~2 missed acks; we reconnect on
            // the first one rather than trust a half-dead socket.
            if last_heartbeat.elapsed() >= heartbeat_interval {
                if !acked {
                    return LoopExit::Reconnect;
                }
                let payload = json!({"op": OP_HEARTBEAT, "d": state.last_seq()});
                if socket
                    .send(Message::Text(payload.to_string().into()))
                    .is_err()
                {
                    return LoopExit::Reconnect;
                }
                last_heartbeat = std::time::Instant::now();
                acked = false;
            }
            // Read with a short timeout so heartbeat cadence and stop are
            // honored even on silence. MaybeTlsStream exposes the TCP socket
            // only through its variants; both are handled.
            match socket.get_mut() {
                tungstenite::stream::MaybeTlsStream::Plain(s) => {
                    s.set_read_timeout(Some(Duration::from_millis(500))).ok();
                }
                tungstenite::stream::MaybeTlsStream::Rustls(s) => {
                    s.sock
                        .set_read_timeout(Some(Duration::from_millis(500)))
                        .ok();
                }
                #[cfg(any())] // native-tls feature not enabled in this build
                tungstenite::stream::MaybeTlsStream::NativeTls(_) => {}
                #[allow(unreachable_patterns)]
                _ => {}
            }
            let msg = match socket.read() {
                Ok(m) => m,
                Err(tungstenite::Error::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => return LoopExit::Reconnect,
            };
            let payload: Value = match msg {
                Message::Text(t) => match serde_json::from_str(t.as_str()) {
                    Ok(v) => v,
                    Err(_) => continue,
                },
                Message::Ping(p) => {
                    let _ = socket.send(Message::Pong(p));
                    continue;
                }
                Message::Close(_) => return LoopExit::Reconnect,
                _ => continue,
            };
            match payload
                .get("op")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX) as u8
            {
                OP_HELLO => {
                    if let Some(ms) = payload
                        .pointer("/d/heartbeat_interval")
                        .and_then(Value::as_u64)
                    {
                        heartbeat_interval = Duration::from_millis(ms);
                    }
                    // Resume when we can, identify otherwise.
                    let hello = if state.can_resume() {
                        self.resume(state)
                    } else {
                        self.identify()
                    };
                    if socket
                        .send(Message::Text(hello.to_string().into()))
                        .is_err()
                    {
                        return LoopExit::Reconnect;
                    }
                }
                OP_HEARTBEAT => {
                    let payload = json!({"op": OP_HEARTBEAT, "d": state.last_seq()});
                    if socket
                        .send(Message::Text(payload.to_string().into()))
                        .is_err()
                    {
                        return LoopExit::Reconnect;
                    }
                }
                OP_HEARTBEAT_ACK => {
                    acked = true;
                }
                OP_RECONNECT => {
                    return LoopExit::Reconnect;
                }
                OP_INVALID_SESSION => {
                    // d == false means the session is unrecoverable: drop it.
                    return LoopExit::Reseed;
                }
                OP_DISPATCH => {
                    let seq = payload.get("s").and_then(Value::as_i64).unwrap_or(-1);
                    if seq >= 0 {
                        state.seq.store(seq, Ordering::Release);
                    }
                    let ev_name = payload.get("t").and_then(Value::as_str).unwrap_or("");
                    let data = payload.get("d").cloned().unwrap_or(Value::Null);
                    match ev_name {
                        EV_READY => {
                            *state.session_id.lock().unwrap_or_else(|e| e.into_inner()) = data
                                .get("session_id")
                                .and_then(Value::as_str)
                                .map(String::from);
                            *state
                                .resume_gateway_url
                                .lock()
                                .unwrap_or_else(|e| e.into_inner()) = data
                                .get("resume_gateway_url")
                                .and_then(Value::as_str)
                                .map(String::from);
                        }
                        EV_RESUMED => {
                            // Replay done; nothing to do.
                        }
                        EV_MESSAGE_CREATE | EV_INTERACTION_CREATE => {
                            // The gateway dispatch's `d` object already has
                            // the shape parse_event expects: `type` and
                            // `channel_id` at top level, interaction payload
                            // nested in `data`. For MESSAGE_CREATE, `data`
                            // is absent and parse_event falls back to the
                            // payload itself, which holds channel_id/content.
                            //
                            // Voice-aware: messages with audio attachments
                            // are transcribed through the channel's voice
                            // pipes; declines go straight back as text (the
                            // bridge owns no outbound queue, so this is
                            // best-effort rather than daemon-retried).
                            for outcome in inbox.ingest(&data) {
                                match outcome {
                                    crate::channel_voice::VoiceOutcome::Event(event) => {
                                        inbox.push_inbound(event);
                                    }
                                    crate::channel_voice::VoiceOutcome::Reply {
                                        thread_id,
                                        text,
                                    } => {
                                        if let Err(e) = inbox.send_text(&thread_id, &text) {
                                            eprintln!(
                                                "discord: voice decline send failed [{}]",
                                                e.code
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    /// The daemon-level loop: run_once until stopped, honoring reconnect
    /// rules and a backoff on repeated failures. Blocks its caller.
    /// `inbox` is the `DiscordChannel` whose outbox the channel daemon
    /// drains: one object owns both directions, so replies go out over
    /// Discord REST instead of into a memory buffer nobody reads.
    pub fn run(&self, inbox: &DiscordChannel, stop: &dyn Fn() -> bool) {
        let state = GatewayState::new();
        let mut failures: u32 = 0;
        while !stop() {
            match self.run_once(&state, inbox, stop) {
                LoopExit::Stopped => return,
                LoopExit::Reconnect | LoopExit::Reseed => {
                    if self.run_once_reseed(&state, &LoopExit::Reconnect) {
                        failures = 0;
                    } else {
                        failures = failures.saturating_add(1);
                    }
                    // 1s, 2s, 4s ... capped 60s (Discord asks for jitter; a
                    // small deterministic cap is acceptable for v1).
                    let wait = crate::delivery::backoff_ms(failures.min(6));
                    let deadline = std::time::Instant::now() + Duration::from_millis(wait);
                    while !stop() && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
            }
        }
    }

    fn run_once_reseed(&self, state: &GatewayState, exit: &LoopExit) -> bool {
        if matches!(exit, LoopExit::Reseed) {
            *state.session_id.lock().unwrap_or_else(|e| e.into_inner()) = None;
            state.seq.store(-1, Ordering::Release);
        }
        true
    }
}
