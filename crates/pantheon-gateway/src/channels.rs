//! Gateway channels merged group.

pub mod channel {
    //! Channel transport seam (interactive path).
    //! One trait every surface implements: discord/slack/web/CLI consume the
    //! same `UiFrame` stream and answer approvals through the same call.
    //! Discord formatting (code fences, chunking) lives in the discord adapter,
    //! not here: this seam only moves frames and approval decisions.
    use crate::server::stream::UiFrame;
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;

    /// Lock a mutex without letting a poisoned lock take the process down.
    ///
    /// A panic while the lock is held poisons it, and every later `poll`/`send`
    /// then panics too. For an inbox/outbox that turns one bad message into a
    /// permanently dead channel: the bot stops responding and a restart is the
    /// only way out. The data in here is plain queued envelopes, so recovering
    /// the guard is safe and strictly better than propagating the poison. The
    /// panic that caused it is already reported by its own thread.
    pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Approval answer from a user/surface.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ApprovalAnswer {
        Grant,
        Deny,
    }
    /// What a surface hands the runtime (normalized inbound).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ChannelEvent {
        pub thread_id: String,
        pub run_id: Option<String>,
        pub text: String,
        pub approval: Option<ApprovalAnswer>,
        pub scope: Option<String>,
        /// Platform sender identity when the surface exposes one (Telegram
        /// user id, Discord author id). None for surfaces that cannot know
        /// (bridges). The gateway allowlist keys on this.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub sender: Option<String>,
    }

    /// Parse an approval button callback (`callback_data` / `custom_id`) into
    /// `(answer, run_id, scope)`.
    ///
    /// Two formats are accepted:
    /// - `grant:{run_id}:{scope}` / `deny:{run_id}:{scope}` - sent by phone
    ///   approval notifications for locally-started runs. The run id lets the
    ///   daemon resolve the pending approval directly.
    /// - legacy `grant:{scope}` / `deny:{scope}` - no run id; the daemon
    ///   falls back to its thread→run map.
    ///
    /// The first segment after the verb is unambiguous: run ids always start
    /// with `run_`, call ids with `turn_`.
    pub fn parse_approval_callback(data: &str) -> Option<(ApprovalAnswer, Option<String>, String)> {
        let (verb, rest) = data.split_once(':')?;
        let answer = match verb {
            "grant" => ApprovalAnswer::Grant,
            "deny" => ApprovalAnswer::Deny,
            _ => return None,
        };
        if rest.is_empty() {
            return None;
        }
        match rest.split_once(':') {
            // `grant:{run_id}:{scope}`: the run id lets the daemon resolve the
            // pending approval directly. Run ids always start with `run_`;
            // legacy scopes (`call_id:tool:args`) never do.
            Some((first, scope)) if first.starts_with("run_") && !scope.is_empty() => {
                Some((answer, Some(first.to_string()), scope.to_string()))
            }
            // Legacy `grant:{scope}` (bare call id or full scope, no run id).
            _ => Some((answer, None, rest.to_string())),
        }
    }
    /// What the runtime hands a surface (one frame + routing).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ChannelEnvelope {
        pub thread_id: String,
        pub frame: UiFrame,
    }
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ChannelError {
        pub code: String,
        pub message: String,
        /// The platform's own "wait this long" hint, when it gave one. Discord
        /// sends `Retry-After` (seconds); Telegram sends `parameters.retry_after`
        /// in the 429 JSON body. Honoring it is the difference between backing
        /// off and hammering; equivalent reference data ships in a bundled
        /// rate-limits file.
        pub retry_after_secs: Option<u64>,
    }
    impl ChannelError {
        pub fn new(code: &str, message: impl Into<String>) -> Self {
            Self {
                code: code.into(),
                message: message.into(),
                retry_after_secs: None,
            }
        }
        /// A 429 with the platform's wait hint attached (already capped by the
        /// caller to something sane; see [`parse_retry_after`]).
        pub fn rate_limited(
            prefix: &str,
            detail: impl Into<String>,
            retry_after_secs: Option<u64>,
        ) -> Self {
            Self {
                code: format!("{prefix}_RATE_LIMITED"),
                message: format!("rate limited (429): {}", detail.into()),
                retry_after_secs,
            }
        }
        /// True when the surface is rate-limiting us (HTTP 429). Callers
        /// should back off via `delivery::backoff_ms` and retry; the planner
        /// treats this as retryable, everything else permanent unless the
        /// transport says otherwise.
        /// Transports detect this by mapping HTTP 429 to code `RATE_LIMITED`
        /// (or `*_RATE_LIMITED`); this predicate keeps that convention in
        /// one place so send paths don't string-match ad hoc.
        pub fn is_rate_limited(&self) -> bool {
            self.code == "RATE_LIMITED" || self.code.ends_with("_RATE_LIMITED")
        }
        /// Map an HTTP error response (the channel agents run with
        /// statuses-as-responses, so 4xx/5xx arrive here instead of as
        /// errors), mapping HTTP 429 to `RATE_LIMITED` so `is_rate_limited()`
        /// works. `prefix` is the transport code (e.g. `TELEGRAM_HTTP`); the
        /// rate-limit code becomes `{prefix}_RATE_LIMITED`. Transport-level
        /// failures are mapped separately at the call site
        /// (`ChannelError::new(prefix, err.to_string())`). The message carries
        /// only the numeric status - never the URL (which may embed a bot
        /// token); the response is borrowed, so the `Retry-After` header is
        /// still readable here.
        pub fn from_ureq(prefix: &str, resp: &ureq::http::Response<ureq::Body>) -> Self {
            let status = resp.status().as_u16();
            if status == 429 {
                Self::rate_limited(
                    prefix,
                    format!("status code {status}"),
                    parse_retry_after(
                        resp.headers()
                            .get("retry-after")
                            .and_then(|v| v.to_str().ok()),
                    ),
                )
            } else {
                Self::new(prefix, format!("status code {status}"))
            }
        }
        /// Best-effort extraction of the HTTP status from a status-mapped
        /// message (`"status code {n}"`, optionally behind a URL prefix).
        /// `None` for non-HTTP errors. Lets health classification see a 401
        /// without re-exposing the URL (which may embed a bot token).
        pub fn http_status(&self) -> Option<u16> {
            if !self.message.contains("status code ") {
                return None;
            }
            self.message
                .rsplit("status code ")
                .next()?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        }
    }
    /// Parse a `Retry-After` header value (delta-seconds). HTTP-date form is
    /// not parsed - a platform clock we cannot verify is worse than our own
    /// backoff, so it falls back to `None` and the caller uses exponential
    /// backoff instead. Capped at 10 minutes: a larger hint is honored as 10
    /// minutes rather than sleeping the daemon into irrelevance.
    pub fn parse_retry_after(value: Option<&str>) -> Option<u64> {
        let v = value?.trim();
        let secs: u64 = v.parse().ok()?;
        if secs == 0 {
            return None;
        }
        Some(secs.min(600))
    }

    /// Shared HTTP agent for the channel REST transports (Telegram, Discord):
    /// platform TLS roots and statuses-as-responses, so 4xx/5xx come back as
    /// responses and call sites can read rate-limit headers and error bodies
    /// off them (see [`ChannelError::from_ureq`]). No global timeout: the
    /// long-poll calls attach their own per-request budget (clearing the
    /// per-call bound below), and ordinary sends/downloads must not inherit
    /// one. The per-call bound reproduces ureq 2's implicit 30s connect
    /// timeout, which ureq 3 dropped: a wedged API host must fail the send,
    /// not the channel thread.
    pub(crate) fn channel_agent() -> ureq::Agent {
        channel_agent_with(std::time::Duration::from_secs(30))
    }

    /// [`channel_agent`] with an explicit per-call budget, so tests can pin
    /// the timeout contract with a short one: an API host that accepts and
    /// never answers must fail the send, not wedge the channel thread
    /// (ureq 3 ships no default timeout; ureq 2's implicit 30s connect
    /// bound is what this restores).
    pub(crate) fn channel_agent_with(per_call: std::time::Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_per_call(Some(per_call))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    }

    impl std::fmt::Display for ChannelError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "[{}] {}", self.code, self.message)
        }
    }
    impl std::error::Error for ChannelError {}
    /// Transport seam. Blocking + object-safe so web/discord/slack/CLI share it.
    pub trait Channel: Send + Sync {
        fn name(&self) -> &str;
        fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError>;
        fn poll(&self) -> Vec<ChannelEvent>;
    }

    /// Fan-out: one frame to every attached surface. A failing surface does not
    /// break the others; errors come back per channel for the caller to log.
    pub fn fanout(
        channels: &[&dyn Channel],
        envelope: &ChannelEnvelope,
    ) -> Vec<(String, Result<(), ChannelError>)> {
        channels
            .iter()
            .map(|c| (c.name().to_string(), c.send(envelope.clone())))
            .collect()
    }

    /// Split text at character boundaries without breaking UTF-8. Adapters use
    /// this for their platform limits; it also prevents a large model response
    /// from becoming one un-sendable message.
    pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
        if max_chars == 0 {
            return vec![text.to_string()];
        }
        let mut out = Vec::new();
        let mut start = 0;
        while start < text.len() {
            let mut end = (start + max_chars).min(text.len());
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                end = (start + 1).min(text.len());
            }
            out.push(text[start..end].to_string());
            start = end;
        }
        if out.is_empty() {
            out.push(String::new());
        }
        out
    }

    /// Buttons carried by approval frames. The custom IDs are stable protocol
    /// values so a transport can map a click back to the original scope.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ApprovalButtons {
        pub scope: String,
    }

    /// Render one frame as plain text for surfaces without rich UI
    /// (discord/slack code fences and chunking live in the adapters; this is
    /// the shared fallback shape).
    pub fn format_text(frame: &UiFrame) -> String {
        match frame.kind {
            crate::server::stream::UiFrameKind::Run => {
                if frame.name == "progress" && !frame.text.is_empty() {
                    frame.text.clone()
                } else {
                    format!("[run {}]", frame.name)
                }
            }
            crate::server::stream::UiFrameKind::Text => frame.text.clone(),
            crate::server::stream::UiFrameKind::Tool => format!("`{}` {}", frame.name, frame.text),
            crate::server::stream::UiFrameKind::State => frame.text.clone(),
            crate::server::stream::UiFrameKind::Approval => {
                if frame.name == "requested" {
                    format!("approval needed: `{}` - reply grant/deny", frame.text)
                } else if frame.name == "denied" {
                    format!("approval denied: `{}`", frame.text)
                } else {
                    format!("approval granted: `{}`", frame.text)
                }
            }
            crate::server::stream::UiFrameKind::GenUi => frame
                .genui
                .as_ref()
                .and_then(|g| g.get("url"))
                .and_then(|u| u.as_str())
                .map(|u| u.to_string())
                .unwrap_or_default(),
            crate::server::stream::UiFrameKind::Component => {
                // Text channels get the summary line; full document rides
                // the frame JSON for capable clients.
                format!("[ui component] {}", frame.text)
            }
        }
    }
    /// Thread -> run affinity so one SSE stream multiplexes many runs.
    #[derive(Debug, Default)]
    pub struct ThreadRunMap {
        map: HashMap<String, String>,
    }
    impl ThreadRunMap {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn bind(&mut self, thread_id: &str, run_id: &str) {
            self.map.insert(thread_id.to_string(), run_id.to_string());
        }
        pub fn run_for(&self, thread_id: &str) -> Option<&str> {
            self.map.get(thread_id).map(|s| s.as_str())
        }
        pub fn thread_for_run(&self, run_id: &str) -> Option<&str> {
            self.map
                .iter()
                .find(|(_, r)| r.as_str() == run_id)
                .map(|(t, _)| t.as_str())
        }
        pub fn len(&self) -> usize {
            self.map.len()
        }
        pub fn is_empty(&self) -> bool {
            self.map.is_empty()
        }
    }
    /// In-memory channel: tests, CLI streaming, and the web shim's outbox.
    #[derive(Debug, Default)]
    pub struct MemoryChannel {
        pub name: String,
        pub outbox: std::sync::Mutex<Vec<ChannelEnvelope>>,
        pub inbox: std::sync::Mutex<Vec<ChannelEvent>>,
    }
    impl MemoryChannel {
        pub fn new(name: &str) -> Self {
            Self {
                name: name.into(),
                outbox: Default::default(),
                inbox: Default::default(),
            }
        }
        pub fn push_inbound(&self, ev: ChannelEvent) {
            lock(&self.inbox).push(ev);
        }
        pub fn drain_outbound(&self) -> Vec<ChannelEnvelope> {
            std::mem::take(&mut *lock(&self.outbox))
        }
    }
    impl Channel for MemoryChannel {
        fn name(&self) -> &str {
            &self.name
        }
        fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
            lock(&self.outbox).push(envelope);
            Ok(())
        }
        fn poll(&self) -> Vec<ChannelEvent> {
            std::mem::take(&mut *lock(&self.inbox))
        }
    }

    #[cfg(test)]
    mod error_tests {
        use super::*;

        #[test]
        fn http_status_parses_status_message() {
            // URL-prefixed form (legacy ureq 2 display); the parser must
            // tolerate any prefix before the marker.
            let e = ChannelError::new(
                "TELEGRAM_HTTP",
                "https://api.telegram.org/bottoken/getUpdates: status code 401",
            );
            assert_eq!(e.http_status(), Some(401));
            // URL-free form produced by `from_ureq`.
            let e4 = ChannelError::new("TELEGRAM_HTTP", "status code 401");
            assert_eq!(e4.http_status(), Some(401));
            let e2 = ChannelError::new("TELEGRAM_HTTP", "Dns Failed");
            assert_eq!(e2.http_status(), None);
            let e3 = ChannelError::new("X", "no marker here");
            assert_eq!(e3.http_status(), None);
        }

        #[test]
        fn rate_limited_predicate() {
            let e = ChannelError::rate_limited("TELEGRAM_HTTP", "slow", Some(5));
            assert!(e.is_rate_limited());
            assert!(!ChannelError::new("TELEGRAM_HTTP", "x").is_rate_limited());
        }

        #[test]
        fn channel_send_is_bounded_by_the_agent_timeout() {
            // A host that accepts the connection but never answers: the
            // per-call budget must fail the request instead of hanging.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                // Accept and HOLD the streams: dropping an accepted socket
                // closes it, which reads as an instant "peer disconnected"
                // instead of the wedged-server shape under test.
                let mut held = Vec::new();
                for _ in 0..4 {
                    match listener.accept() {
                        Ok((stream, _)) => held.push(stream),
                        Err(_) => break,
                    }
                }
            });
            let agent = channel_agent_with(std::time::Duration::from_millis(300));
            let start = std::time::Instant::now();
            let err = agent
                .post(format!("http://127.0.0.1:{port}/send"))
                .send_empty()
                .unwrap_err();
            assert!(
                start.elapsed() >= std::time::Duration::from_millis(250),
                "error arrived before the budget expired; something other than \
             the deadline failed (took {:?}, err: {err})",
                start.elapsed()
            );
            assert!(
                start.elapsed() < std::time::Duration::from_secs(5),
                "wedged host must fail within the budget, took {:?} (err: {err})",
                start.elapsed()
            );
        }
    }
}
pub mod telegram {
    //! Telegram Channel adapter.

    use crate::channels::channel::{
        chunk_text, format_text, Channel, ChannelEnvelope, ChannelError, ChannelEvent,
    };
    use crate::server::stream::{UiFrame, UiFrameKind};
    use crate::voice_group::channel_voice::{
        download_capped, ext_for_mime, multipart, ureq_kind, warn_voice_unsupported_once,
        VoiceOutcome, VoicePipes, MAX_VOICE_BYTES, STT_FAILED,
    };
    use pantheon_providers::voice::AudioFormat;
    use serde_json::{json, Value};
    use std::fmt;
    use std::sync::{Arc, Mutex};

    pub const TELEGRAM_MESSAGE_LIMIT: usize = 4_096;

    /// Map a send failure response (4xx/5xx arrive as responses; transport
    /// failures are mapped at the call site). Telegram puts the 429 wait in
    /// the JSON body (`parameters.retry_after`, seconds) rather than a
    /// `Retry-After` header, so the generic header parse in `from_ureq`
    /// would miss it; read the body first (falling back to the header), and
    /// cap like the shared parser.
    fn send_err(resp: &mut ureq::http::Response<ureq::Body>) -> ChannelError {
        if resp.status().as_u16() == 429 {
            let header = crate::channels::channel::parse_retry_after(
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
            );
            let body_secs = resp
                .body_mut()
                .read_to_string()
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .and_then(|v| {
                    v.pointer("/parameters/retry_after")
                        .and_then(|n| n.as_u64())
                })
                .map(|n| n.min(600));
            // A zero-second body hint means "no hint", not "send now".
            let body_secs = body_secs.filter(|n| *n > 0);
            ChannelError::rate_limited("TELEGRAM_HTTP", "sendMessage", body_secs.or(header))
        } else {
            telegram_http_err(resp)
        }
    }

    /// Telegram HTTP failure with an actionable credential diagnosis
    /// appended when the status maps to one. The message stays
    /// URL-free (from_ureq reports only the status code) and the
    /// diagnosis is static text, so the bot token in the request URL
    /// can never pass through it.
    fn telegram_http_err(resp: &ureq::http::Response<ureq::Body>) -> ChannelError {
        let status = resp.status().as_u16();
        let mut err = ChannelError::from_ureq("TELEGRAM_HTTP", resp);
        if let Some(d) = pantheon_providers::credential_error::classify_telegram(status, "") {
            err.message
                .push_str(&format!(" [credential: {} {}]", d.summary, d.remediation));
        }
        err
    }

    /// Bot API boundary. A live implementation can use any HTTP client; the
    /// adapter only owns Telegram's inline keyboard and message limit semantics.
    pub trait TelegramTransport: Send + Sync {
        fn send_message(&self, chat_id: &str, payload: &Value) -> Result<(), ChannelError>;
        /// Download a file by its `file_id` (`getFile` + the file download
        /// URL). Default: unsupported - test recorders only implement
        /// `send_message`.
        fn download_file(&self, file_id: &str) -> Result<Vec<u8>, ChannelError> {
            let _ = file_id;
            Err(ChannelError::new(
                "TELEGRAM_TRANSPORT",
                "file download not supported by this transport",
            ))
        }
        /// Send synthesized audio back (`sendVoice` for ogg, `sendAudio`
        /// otherwise). Default: unsupported - the channel falls back to text.
        fn send_voice(
            &self,
            chat_id: &str,
            audio: &[u8],
            format: AudioFormat,
        ) -> Result<(), ChannelError> {
            let _ = (chat_id, audio, format);
            Err(ChannelError::new(
                "TELEGRAM_TRANSPORT",
                "voice upload not supported by this transport",
            ))
        }
        /// Long-poll `getUpdates`. Returns raw update objects; normalization is
        /// shared with the webhook path via `parse_event`.
        fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, ChannelError> {
            let _ = (offset, timeout_secs);
            Ok(Vec::new())
        }
        fn poll_events(&self) -> Vec<ChannelEvent> {
            Vec::new()
        }
    }

    /// Live Telegram Bot API transport. Updates are normalized by a webhook or
    /// long-poll bridge and fed to `TelegramChannel::push_inbound`.
    pub struct TelegramRestTransport {
        agent: ureq::Agent,
        api_base: String,
        bot_token: String,
    }

    impl TelegramRestTransport {
        pub fn new(bot_token: impl Into<String>) -> Self {
            Self {
                agent: crate::channels::channel::channel_agent(),
                api_base: "https://api.telegram.org".into(),
                bot_token: bot_token.into(),
            }
        }
        pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
            self.api_base = base.into().trim_end_matches('/').to_string();
            self
        }
    }

    impl TelegramTransport for TelegramRestTransport {
        fn send_message(&self, chat_id: &str, payload: &Value) -> Result<(), ChannelError> {
            let url = format!("{}/bot{}/sendMessage", self.api_base, self.bot_token);
            let mut body = payload.clone();
            if let Some(object) = body.as_object_mut() {
                object.insert("chat_id".into(), json!(chat_id));
            }
            let mut resp = self
                .agent
                .post(&url)
                .send_json(body)
                .map_err(|e| ChannelError::new("TELEGRAM_HTTP", e.to_string()))?;
            if resp.status().as_u16() >= 400 {
                return Err(send_err(&mut resp));
            }
            Ok(())
        }
        fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, ChannelError> {
            let url = format!("{}/bot{}/getUpdates", self.api_base, self.bot_token);
            let resp = self
                .agent
                .post(&url)
                // Per-request budget: the long-poll window plus slack for the
                // round trip. per_call is cleared because the shared agent's
                // 30s connect bound would cut a long poll that legitimately
                // waits past it; ordinary sends never inherit the poll budget.
                .config()
                .timeout_per_call(None)
                .timeout_global(Some(std::time::Duration::from_secs(timeout_secs + 5)))
                .build()
                .send_json(json!({
                    "offset": offset,
                    "timeout": timeout_secs,
                    "allowed_updates": ["message", "callback_query"],
                }))
                .map_err(|e| ChannelError::new("TELEGRAM_HTTP", e.to_string()))?;
            if resp.status().as_u16() >= 400 {
                return Err(telegram_http_err(&resp));
            }
            let body: Value = resp
                .into_body()
                .read_json()
                .map_err(|e| ChannelError::new("TELEGRAM_HTTP", e.to_string()))?;
            if body.get("ok").and_then(Value::as_bool) != Some(true) {
                return Err(ChannelError::new(
                    "TELEGRAM_HTTP",
                    format!("getUpdates not ok: {body}"),
                ));
            }
            Ok(body
                .get("result")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default())
        }
        fn download_file(&self, file_id: &str) -> Result<Vec<u8>, ChannelError> {
            // getFile answers with a path, not bytes; the follow-up download
            // URL embeds the bot token, so every failure here maps to a
            // static, token-free message (see channel_voice::ureq_kind).
            let url = format!("{}/bot{}/getFile", self.api_base, self.bot_token);
            let resp = self
                .agent
                .post(&url)
                .send_json(json!({ "file_id": file_id }))
                .map_err(|_| ChannelError::new("TELEGRAM_HTTP", "getFile failed"))?;
            // Status failures map to the same static, token-free message as
            // transport ones: the getFile URL embeds the bot token.
            if resp.status().as_u16() >= 400 {
                return Err(ChannelError::new("TELEGRAM_HTTP", "getFile failed"));
            }
            let body: Value = resp
                .into_body()
                .read_json()
                .map_err(|_| ChannelError::new("TELEGRAM_HTTP", "getFile: bad response body"))?;
            let path = body
                .pointer("/result/file_path")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| ChannelError::new("TELEGRAM_HTTP", "getFile: no file_path"))?;
            let file_url = format!("{}/file/bot{}/{}", self.api_base, self.bot_token, path);
            download_capped(&self.agent, &file_url, MAX_VOICE_BYTES, "TELEGRAM_HTTP")
        }
        fn send_voice(
            &self,
            chat_id: &str,
            audio: &[u8],
            format: AudioFormat,
        ) -> Result<(), ChannelError> {
            // sendVoice only accepts ogg/opus; anything else goes out as a
            // regular audio attachment instead of failing the reply.
            let as_voice = format == AudioFormat::Ogg;
            let (method, field) = if as_voice {
                ("sendVoice", "voice")
            } else {
                ("sendAudio", "audio")
            };
            let url = format!("{}/bot{}/{method}", self.api_base, self.bot_token);
            let filename = format!("reply.{}", format.as_str());
            let (content_type, body) = multipart(
                &[("chat_id", chat_id)],
                field,
                &filename,
                crate::voice_group::voice::content_type_for(format),
                audio,
            );
            // The URL carries the bot token: the raw ureq error (whose text
            // is not ours to audit) must never reach a log line.
            let resp = self
                .agent
                .post(&url)
                .header("Content-Type", content_type.as_str())
                .send(&body)
                .map_err(|e| {
                    ChannelError::new(
                        "TELEGRAM_HTTP",
                        format!("{method} failed: {}", ureq_kind(&e)),
                    )
                })?;
            let code = resp.status().as_u16();
            if code >= 400 {
                return Err(ChannelError::new(
                    "TELEGRAM_HTTP",
                    format!("{method} failed: http {code}"),
                ));
            }
            Ok(())
        }
    }

    /// A Telegram `voice`/`audio` message reduced to its download handle.
    /// Pure: fixture-tested.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct TelegramVoiceRef {
        pub file_id: String,
        /// Extension for the staged temp file (`ogg`, `mp3`, ...).
        pub ext: String,
    }

    /// Extract the voice/audio file from a Telegram `message` object, if it
    /// carries one. `voice` notes are always ogg/opus; `audio` messages carry
    /// a `mime_type` hint.
    pub fn voice_ref(message: &Value) -> Option<TelegramVoiceRef> {
        if let Some(voice) = message.get("voice") {
            let file_id = voice.get("file_id")?.as_str().filter(|s| !s.is_empty())?;
            return Some(TelegramVoiceRef {
                file_id: file_id.to_string(),
                ext: "ogg".to_string(),
            });
        }
        if let Some(audio) = message.get("audio") {
            let file_id = audio.get("file_id")?.as_str().filter(|s| !s.is_empty())?;
            let ext = audio
                .get("mime_type")
                .and_then(Value::as_str)
                .map(ext_for_mime)
                .unwrap_or("bin")
                .to_string();
            return Some(TelegramVoiceRef {
                file_id: file_id.to_string(),
                ext,
            });
        }
        None
    }

    /// Thread id + sender for a Telegram `message` object. `None` when the
    /// chat id is missing - the same condition `parse_event` rejects with.
    fn message_thread_sender(message: &Value) -> Option<(String, Option<String>)> {
        let chat_id = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .map(chat_id_string)
            .filter(|id| !id.is_empty())?;
        let sender = message
            .get("from")
            .and_then(|from| from.get("id"))
            .and_then(Value::as_i64)
            .map(|id| id.to_string());
        Some((chat_id, sender))
    }

    /// Normalize a Telegram Bot API update (message or callback query) into the
    /// shared channel event shape.
    pub fn parse_event(payload: &Value) -> Result<Option<ChannelEvent>, ChannelError> {
        if let Some(message) = payload.get("message") {
            let (chat_id, sender) = message_thread_sender(message)
                .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "message has no chat id"))?;
            let text = message.get("text").and_then(Value::as_str).unwrap_or("");
            return Ok(Some(ChannelEvent {
                thread_id: chat_id,
                run_id: None,
                text: text.to_string(),
                approval: None,
                scope: None,
                sender,
            }));
        }
        if let Some(callback) = payload.get("callback_query") {
            let data = callback
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "callback has no data"))?;
            let (approval, run_id, scope) = crate::channels::channel::parse_approval_callback(data)
                .ok_or_else(|| {
                    ChannelError::new("TELEGRAM_EVENT", "invalid approval callback_data")
                })?;
            let approval = Some(approval);
            let chat_id = callback
                .get("message")
                .and_then(|message| message.get("chat"))
                .and_then(|chat| chat.get("id"))
                .map(chat_id_string)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "callback has no chat id"))?;
            let sender = callback
                .get("from")
                .and_then(|from| from.get("id"))
                .and_then(Value::as_i64)
                .map(|id| id.to_string());
            return Ok(Some(ChannelEvent {
                thread_id: chat_id,
                run_id,
                text: String::new(),
                approval,
                scope: Some(scope),
                sender,
            }));
        }
        Ok(None)
    }

    fn chat_id_string(value: &Value) -> String {
        value
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| value.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// Fold polled updates into voice outcomes plus the next offset.
    /// `voice`/`audio` messages are downloaded through `download` and
    /// transcribed with the channel's voice pipes; everything else flows
    /// through [`parse_event`], so webhook and poll paths agree. Pure apart
    /// from the `download` closure: tested with fixtures and a fake
    /// downloader, no network.
    pub fn collect_voice_outcomes(
        updates: &[Value],
        offset: i64,
        voice: &VoicePipes,
        download: &dyn Fn(&str) -> Result<Vec<u8>, ChannelError>,
    ) -> (Vec<VoiceOutcome>, i64) {
        let mut outcomes = Vec::new();
        let mut highest = offset;
        for update in updates {
            let id = update.get("update_id").and_then(Value::as_i64).unwrap_or(0);
            if id >= highest {
                highest = id + 1;
            }
            let Some(message) = update.get("message") else {
                // Callback queries and anything else: plain normalization.
                if let Ok(Some(event)) = parse_event(update) {
                    outcomes.push(VoiceOutcome::Event(event));
                }
                continue;
            };
            let Some((chat_id, sender)) = message_thread_sender(message) else {
                continue;
            };
            match voice_ref(message) {
                Some(vr) => {
                    let outcome = match download(&vr.file_id) {
                        Ok(bytes) => voice.handle_voice_bytes(&chat_id, sender, &bytes, &vr.ext),
                        Err(e) => {
                            // Token-free: download errors never carry the
                            // file URL (see channel_voice::ureq_kind).
                            eprintln!("telegram: voice download failed [{}]", e.code);
                            VoiceOutcome::Reply {
                                thread_id: chat_id.clone(),
                                text: STT_FAILED.to_string(),
                            }
                        }
                    };
                    outcomes.push(outcome);
                }
                None => {
                    if let Ok(Some(event)) = parse_event(update) {
                        outcomes.push(VoiceOutcome::Event(event));
                    }
                }
            }
        }
        (outcomes, highest)
    }

    pub struct TelegramChannel {
        pub bot_token: String,
        transport: Arc<dyn TelegramTransport>,
        inbox: Mutex<Vec<ChannelEvent>>,
        voice: VoicePipes,
    }

    /// Hand-written so the bot token never reaches a log line. A `#[derive(Debug)]`
    /// here would print a live credential, and the token is a `pub` field, so any
    /// future derive is a leak. Same intent as `SecretValue`'s Debug, kept local
    /// to avoid a gateway -> secrets dependency for three fields.
    impl fmt::Debug for TelegramChannel {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "TelegramChannel {{ bot_token: *** }}")
        }
    }

    impl TelegramChannel {
        pub fn new(bot_token: impl Into<String>, transport: Arc<dyn TelegramTransport>) -> Self {
            Self {
                bot_token: bot_token.into(),
                transport,
                inbox: Mutex::new(Vec::new()),
                voice: VoicePipes::disabled(),
            }
        }
        pub fn rest(bot_token: impl Into<String>) -> Self {
            let bot_token = bot_token.into();
            let transport = Arc::new(TelegramRestTransport::new(bot_token.clone()));
            Self::new(bot_token, transport)
        }
        /// Attach the channel's voice capability (STT/TTS backends plus the
        /// `voice_replies` preference). Default is disabled: inbound voice is
        /// declined, outbound stays text.
        pub fn with_voice(mut self, voice: VoicePipes) -> Self {
            self.voice = voice;
            self
        }
        pub fn push_inbound(&self, event: ChannelEvent) {
            crate::channels::channel::lock(&self.inbox).push(event);
        }

        /// Long-poll one batch through `transport` and voice-process it:
        /// `voice`/`audio` messages are downloaded and transcribed with the
        /// channel's voice pipes; everything else flows through
        /// [`parse_event`]. Returns voice outcomes (events for the sink,
        /// text replies the caller must send back) plus the next offset.
        pub fn poll_updates(
            &self,
            transport: &dyn TelegramTransport,
            offset: i64,
            timeout_secs: u64,
        ) -> Result<(Vec<VoiceOutcome>, i64), String> {
            // Token-safe: the getUpdates URL embeds the bot token, so only
            // the error code and (for HTTP failures) the numeric status are
            // surfaced - never the error message, whose text is not ours to
            // audit. The daemon's health tracker needs the 401 to spot a
            // revoked token.
            let updates =
                transport
                    .get_updates(offset, timeout_secs)
                    .map_err(|e| match e.http_status() {
                        Some(401) => {
                            "getUpdates failed [TELEGRAM_HTTP]: 401 unauthorized (token revoked?)"
                                .to_string()
                        }
                        Some(s) => format!("getUpdates failed [{}]: http {s}", e.code),
                        None => format!("getUpdates failed [{}]", e.code),
                    })?;
            Ok(collect_voice_outcomes(
                &updates,
                offset,
                &self.voice,
                &|file_id| transport.download_file(file_id),
            ))
        }

        fn payload(frame: &UiFrame, text: String) -> Value {
            if frame.kind == UiFrameKind::Approval && frame.name == "requested" {
                json!({
                    "text": text,
                    "reply_markup": {"inline_keyboard": [[
                        {"text": "Grant", "callback_data": format!("grant:{}", frame.text)},
                        {"text": "Deny", "callback_data": format!("deny:{}", frame.text)}
                    ]]}
                })
            } else {
                json!({ "text": text })
            }
        }

        pub fn payloads(&self, envelope: &ChannelEnvelope) -> Vec<Value> {
            let mut payloads = chunk_text(&format_text(&envelope.frame), TELEGRAM_MESSAGE_LIMIT)
                .into_iter()
                .map(|part| Self::payload(&envelope.frame, part))
                .collect::<Vec<_>>();
            if payloads.is_empty() {
                payloads.push(json!({"text": ""}));
            }
            payloads
        }
    }

    impl Channel for TelegramChannel {
        fn name(&self) -> &str {
            "telegram"
        }
        fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
            // Voice replies: agent text goes through the [tts] backend and out
            // as sendVoice/sendAudio - but only for plain Text frames.
            // Approval requests keep their inline keyboard: buttons cannot be
            // spoken.
            if self.voice.speak_replies() && envelope.frame.kind == UiFrameKind::Text {
                let text = format_text(&envelope.frame);
                if !text.trim().is_empty() {
                    match self.voice.synthesize(&text, AudioFormat::Ogg) {
                        Ok(audio) => {
                            match self.transport.send_voice(
                                &envelope.thread_id,
                                &audio.bytes,
                                audio.format,
                            ) {
                                Ok(()) => return Ok(()),
                                Err(e) if e.code == "TELEGRAM_TRANSPORT" => {
                                    warn_voice_unsupported_once("telegram");
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        Err(code) => {
                            eprintln!("telegram: tts failed [{code}]; falling back to text");
                        }
                    }
                }
            }
            for payload in self.payloads(&envelope) {
                self.transport.send_message(&envelope.thread_id, &payload)?;
            }
            Ok(())
        }
        fn poll(&self) -> Vec<ChannelEvent> {
            let mut events = std::mem::take(&mut *crate::channels::channel::lock(&self.inbox));
            events.extend(self.transport.poll_events());
            events
        }
    }
}
pub mod discord {
    //! Discord Channel adapter.
    //!
    //! The adapter owns Discord's wire formatting and limit handling, while the
    //! transport seam owns the actual HTTP client. This keeps the channel usable
    //! with the std-only server, a gateway daemon, or a test recorder without
    //! coupling the runtime to a Discord SDK.

    use crate::channels::channel::{
        chunk_text, format_text, Channel, ChannelEnvelope, ChannelError, ChannelEvent,
    };
    use crate::server::stream::{UiFrame, UiFrameKind};
    use crate::voice_group::channel_voice::{
        download_capped, ext_for_filename, ext_for_mime, multipart, ureq_kind,
        warn_voice_unsupported_once, VoiceOutcome, VoicePipes, MAX_VOICE_BYTES, STT_FAILED,
    };
    use pantheon_providers::voice::AudioFormat;
    use serde_json::{json, Value};
    use std::fmt;
    use std::sync::{Arc, Mutex};

    pub const DISCORD_CONTENT_LIMIT: usize = 2_000;

    /// Minimal API boundary implemented by a live Discord REST client or a test
    /// recorder. `send_message` receives a complete channel message payload.
    pub trait DiscordTransport: Send + Sync {
        fn send_message(&self, channel_id: &str, payload: &Value) -> Result<(), ChannelError>;
        /// Download an attachment URL (Discord CDN links are pre-signed).
        /// Default: unsupported - test recorders only implement
        /// `send_message`.
        fn download(&self, url: &str) -> Result<Vec<u8>, ChannelError> {
            let _ = url;
            Err(ChannelError::new(
                "DISCORD_TRANSPORT",
                "attachment download not supported by this transport",
            ))
        }
        /// Send synthesized audio as a message attachment. Default:
        /// unsupported - the channel falls back to text.
        fn send_audio(
            &self,
            channel_id: &str,
            audio: &[u8],
            format: AudioFormat,
        ) -> Result<(), ChannelError> {
            let _ = (channel_id, audio, format);
            Err(ChannelError::new(
                "DISCORD_TRANSPORT",
                "audio upload not supported by this transport",
            ))
        }
        fn poll_events(&self) -> Vec<ChannelEvent> {
            Vec::new()
        }
    }

    /// Live Discord REST transport. Gateway updates can be fed to
    /// `DiscordChannel::push_inbound`; REST is used for the outbound path and
    /// retry policy remains the caller's concern.
    pub struct DiscordRestTransport {
        agent: ureq::Agent,
        api_base: String,
        token: String,
    }

    impl DiscordRestTransport {
        pub fn new(token: impl Into<String>) -> Self {
            Self {
                agent: crate::channels::channel::channel_agent(),
                api_base: "https://discord.com/api/v10".into(),
                token: token.into(),
            }
        }
        pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
            self.api_base = base.into().trim_end_matches('/').to_string();
            self
        }

        /// Build the POST request for one message: `(url, authorization header
        /// value, body)`. Pure: unit tests assert the wire shape
        /// (`POST {api_base}/channels/{id}/messages`, `Authorization: Bot
        /// <token>`, `{"content": ...}`) without touching the network.
        pub fn build_send_request(
            &self,
            channel_id: &str,
            payload: &Value,
        ) -> (String, String, Value) {
            let url = format!("{}/channels/{}/messages", self.api_base, channel_id);
            let auth = format!("Bot {}", self.token);
            (url, auth, payload.clone())
        }
    }

    impl DiscordTransport for DiscordRestTransport {
        fn send_message(&self, channel_id: &str, payload: &Value) -> Result<(), ChannelError> {
            let (url, auth, body) = self.build_send_request(channel_id, payload);
            let resp = self
                .agent
                .post(&url)
                .header("Authorization", auth.as_str())
                .send_json(body)
                .map_err(|e| ChannelError::new("DISCORD_HTTP", e.to_string()))?;
            if resp.status().as_u16() >= 400 {
                return Err(ChannelError::from_ureq("DISCORD_HTTP", &resp));
            }
            Ok(())
        }
        fn download(&self, url: &str) -> Result<Vec<u8>, ChannelError> {
            // Attachment URLs are pre-signed CDN links: no Authorization
            // header - the bot token must not travel to the CDN, and the
            // failure messages stay static so no URL reaches a log line.
            download_capped(&self.agent, url, MAX_VOICE_BYTES, "DISCORD_HTTP")
        }
        fn send_audio(
            &self,
            channel_id: &str,
            audio: &[u8],
            format: AudioFormat,
        ) -> Result<(), ChannelError> {
            let (url, auth, _) = self.build_send_request(channel_id, &json!({}));
            let filename = format!("reply.{}", format.as_str());
            let (content_type, body) = multipart(
                &[("payload_json", &json!({"content": ""}).to_string())],
                "files[0]",
                &filename,
                crate::voice_group::voice::content_type_for(format),
                audio,
            );
            let resp = self
                .agent
                .post(&url)
                .header("Authorization", auth.as_str())
                .header("Content-Type", content_type.as_str())
                .send(&body)
                .map_err(|e| {
                    ChannelError::new(
                        "DISCORD_HTTP",
                        format!("send audio failed: {}", ureq_kind(&e)),
                    )
                })?;
            let code = resp.status().as_u16();
            if code >= 400 {
                return Err(ChannelError::new(
                    "DISCORD_HTTP",
                    format!("send audio failed: http {code}"),
                ));
            }
            Ok(())
        }
    }

    /// An audio attachment on a Discord message, reduced to its download
    /// handle. Pure: fixture-tested.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DiscordAudioRef {
        pub url: String,
        /// Extension for the staged temp file (`ogg`, `mp3`, ...).
        pub ext: String,
    }

    /// Extensions treated as audio when the attachment has no `content_type`.
    /// Voice notes arrive as ogg; keep the list to formats STT backends
    /// plausibly decode.
    fn is_audio_filename(name: &str) -> bool {
        matches!(
            ext_for_filename(name).as_str(),
            "ogg" | "oga" | "opus" | "mp3" | "wav" | "m4a" | "flac" | "webm"
        )
    }

    /// First audio attachment of a MESSAGE_CREATE data object, if any. An
    /// attachment counts as audio when its `content_type` starts with
    /// `audio/` or its filename has an audio extension.
    pub fn audio_attachment(data: &Value) -> Option<DiscordAudioRef> {
        data.get("attachments")?
            .as_array()?
            .iter()
            .find(|a| {
                let by_type = a
                    .get("content_type")
                    .and_then(Value::as_str)
                    .map(|t| t.starts_with("audio/"))
                    .unwrap_or(false);
                let by_name = a
                    .get("filename")
                    .and_then(Value::as_str)
                    .map(is_audio_filename)
                    .unwrap_or(false);
                by_type || by_name
            })
            .and_then(|a| {
                let url = a.get("url")?.as_str().filter(|s| !s.is_empty())?;
                let ext = a
                    .get("content_type")
                    .and_then(Value::as_str)
                    .map(ext_for_mime)
                    .filter(|e| *e != "bin")
                    .map(str::to_string)
                    .or_else(|| {
                        a.get("filename")
                            .and_then(Value::as_str)
                            .map(ext_for_filename)
                    })
                    .unwrap_or_else(|| "bin".to_string());
                Some(DiscordAudioRef {
                    url: url.to_string(),
                    ext,
                })
            })
    }

    /// The message/interaction kind, shared by `parse_event` and `ingest`
    /// so the two cannot disagree about what a payload is.
    fn message_kind(payload: &Value) -> Option<u64> {
        payload.get("type").and_then(Value::as_u64).or_else(|| {
            match payload.get("t").and_then(Value::as_str) {
                Some("MESSAGE_CREATE") => Some(0),
                Some("INTERACTION_CREATE") => payload
                    .get("data")
                    .and_then(|data| data.get("type"))
                    .and_then(Value::as_u64),
                _ => None,
            }
        })
    }

    /// Normalize a Discord gateway MESSAGE_CREATE or interaction payload into the
    /// channel seam. The transport bridge can feed raw gateway JSON here instead
    /// of reimplementing approval routing at every call site.
    pub fn parse_event(payload: &Value) -> Result<Option<ChannelEvent>, ChannelError> {
        let kind = message_kind(payload);
        let data = payload.get("data").unwrap_or(payload);
        if kind == Some(0) {
            let channel_id = data
                .get("channel_id")
                .and_then(Value::as_str)
                .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "message has no channel_id"))?;
            let text = data.get("content").and_then(Value::as_str).unwrap_or("");
            let sender = data
                .get("author")
                .and_then(|author| author.get("id"))
                .and_then(Value::as_str)
                .map(|id| id.to_string());
            return Ok(Some(ChannelEvent {
                thread_id: channel_id.to_string(),
                run_id: None,
                text: text.to_string(),
                approval: None,
                scope: None,
                sender,
            }));
        }
        if kind == Some(2) {
            let custom_id = data
                .get("custom_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ChannelError::new("DISCORD_EVENT", "interaction has no custom_id")
                })?;
            let (approval, run_id, scope) =
                crate::channels::channel::parse_approval_callback(custom_id).ok_or_else(|| {
                    ChannelError::new("DISCORD_EVENT", "invalid approval custom_id")
                })?;
            let approval = Some(approval);
            let channel_id = data
                .get("channel_id")
                .and_then(Value::as_str)
                .or_else(|| payload.get("channel_id").and_then(Value::as_str))
                .ok_or_else(|| {
                    ChannelError::new("DISCORD_EVENT", "interaction has no channel_id")
                })?;
            let sender = payload
                .get("user")
                .and_then(|user| user.get("id"))
                .and_then(Value::as_str)
                .or_else(|| {
                    payload
                        .get("member")
                        .and_then(|member| member.get("user"))
                        .and_then(|user| user.get("id"))
                        .and_then(Value::as_str)
                })
                .map(|id| id.to_string());
            return Ok(Some(ChannelEvent {
                thread_id: channel_id.to_string(),
                run_id,
                text: String::new(),
                approval,
                scope: Some(scope),
                sender,
            }));
        }
        Ok(None)
    }

    pub struct DiscordChannel {
        pub token: String,
        transport: Arc<dyn DiscordTransport>,
        inbox: Mutex<Vec<ChannelEvent>>,
        voice: VoicePipes,
    }

    /// Hand-written so the bot token never reaches a log line. A `#[derive(Debug)]`
    /// here would print a live credential, and the token is a `pub` field, so
    /// any future derive is a leak. Same intent as `SecretValue`'s Debug, kept
    /// local to avoid a gateway -> secrets dependency for three fields.
    impl fmt::Debug for DiscordChannel {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "DiscordChannel {{ token: *** }}")
        }
    }

    impl DiscordChannel {
        pub fn new(token: impl Into<String>, transport: Arc<dyn DiscordTransport>) -> Self {
            Self {
                token: token.into(),
                transport,
                inbox: Mutex::new(Vec::new()),
                voice: VoicePipes::disabled(),
            }
        }
        pub fn rest(token: impl Into<String>) -> Self {
            let token = token.into();
            let transport = Arc::new(DiscordRestTransport::new(token.clone()));
            Self::new(token, transport)
        }
        /// Attach the channel's voice capability (STT/TTS backends plus the
        /// `voice_replies` preference). Default is disabled: inbound voice is
        /// declined, outbound stays text.
        pub fn with_voice(mut self, voice: VoicePipes) -> Self {
            self.voice = voice;
            self
        }
        pub fn push_inbound(&self, event: ChannelEvent) {
            crate::channels::channel::lock(&self.inbox).push(event);
        }

        /// Voice-aware inbound: messages with audio attachments are
        /// downloaded through the transport and transcribed with the
        /// channel's voice pipes; everything else flows through
        /// [`parse_event`]. The gateway bridge calls this per dispatch and
        /// routes the outcomes (events to the inbox, replies straight back).
        pub fn ingest(&self, payload: &Value) -> Vec<VoiceOutcome> {
            let data = payload.get("data").unwrap_or(payload);
            if message_kind(payload) != Some(0) {
                // Interactions etc.: plain normalization.
                return parse_event(payload)
                    .ok()
                    .flatten()
                    .map(|e| vec![VoiceOutcome::Event(e)])
                    .unwrap_or_default();
            }
            let channel_id = data.get("channel_id").and_then(Value::as_str);
            let sender = data
                .get("author")
                .and_then(|author| author.get("id"))
                .and_then(Value::as_str)
                .map(|id| id.to_string());
            let Some(channel_id) = channel_id else {
                return Vec::new();
            };
            match audio_attachment(data) {
                Some(audio) => {
                    let outcome = match self.transport.download(&audio.url) {
                        Ok(bytes) => self
                            .voice
                            .handle_voice_bytes(channel_id, sender, &bytes, &audio.ext),
                        Err(e) => {
                            eprintln!("discord: voice download failed [{}]", e.code);
                            VoiceOutcome::Reply {
                                thread_id: channel_id.to_string(),
                                text: STT_FAILED.to_string(),
                            }
                        }
                    };
                    vec![outcome]
                }
                None => parse_event(payload)
                    .ok()
                    .flatten()
                    .map(|e| vec![VoiceOutcome::Event(e)])
                    .unwrap_or_default(),
            }
        }

        /// Send a plain-text message, bypassing the voice-reply path. Used
        /// for voice declines: short system messages are never spoken.
        pub fn send_text(&self, thread_id: &str, text: &str) -> Result<(), ChannelError> {
            let envelope = ChannelEnvelope {
                thread_id: thread_id.to_string(),
                frame: UiFrame {
                    id: 0,
                    kind: UiFrameKind::Text,
                    run_id: String::new(),
                    thread_id: thread_id.to_string(),
                    name: "delta".to_string(),
                    text: text.to_string(),
                    interrupt: false,
                    genui: None,
                    component: None,
                },
            };
            for payload in self.payloads(&envelope) {
                self.transport.send_message(thread_id, &payload)?;
            }
            Ok(())
        }

        fn payload(frame: &UiFrame, text: String) -> Value {
            if frame.kind == UiFrameKind::Approval && frame.name == "requested" {
                json!({
                    "content": text,
                    "components": [{
                        "type": 1,
                        "components": [
                            {"type": 2, "style": 2, "label": "Grant", "custom_id": format!("grant:{}", frame.text)},
                            {"type": 2, "style": 2, "label": "Deny", "custom_id": format!("deny:{}", frame.text)}
                        ]
                    }]
                })
            } else {
                json!({ "content": text })
            }
        }

        /// Exposed for protocol tests and gateway bridges.
        pub fn payloads(&self, envelope: &ChannelEnvelope) -> Vec<Value> {
            let text = format_text(&envelope.frame);
            let mut payloads = chunk_text(&text, DISCORD_CONTENT_LIMIT)
                .into_iter()
                .map(|part| Self::payload(&envelope.frame, part))
                .collect::<Vec<_>>();
            if payloads.is_empty() {
                payloads.push(json!({"content": ""}));
            }
            payloads
        }
    }

    impl Channel for DiscordChannel {
        fn name(&self) -> &str {
            "discord"
        }
        fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
            // Voice replies: agent text goes through the [tts] backend and out
            // as an audio attachment - but only for plain Text frames.
            // Approval requests keep their buttons: they cannot be spoken.
            if self.voice.speak_replies() && envelope.frame.kind == UiFrameKind::Text {
                let text = format_text(&envelope.frame);
                if !text.trim().is_empty() {
                    match self.voice.synthesize(&text, AudioFormat::Mp3) {
                        Ok(audio) => {
                            match self.transport.send_audio(
                                &envelope.thread_id,
                                &audio.bytes,
                                audio.format,
                            ) {
                                Ok(()) => return Ok(()),
                                Err(e) if e.code == "DISCORD_TRANSPORT" => {
                                    warn_voice_unsupported_once("discord");
                                }
                                Err(e) => return Err(e),
                            }
                        }
                        Err(code) => {
                            eprintln!("discord: tts failed [{code}]; falling back to text");
                        }
                    }
                }
            }
            // The thread id is the canonical Discord channel id.  Integrations
            // that use a thread append the thread id before handing it here.
            for payload in self.payloads(&envelope) {
                self.transport.send_message(&envelope.thread_id, &payload)?;
            }
            Ok(())
        }
        fn poll(&self) -> Vec<ChannelEvent> {
            let mut events = std::mem::take(&mut *crate::channels::channel::lock(&self.inbox));
            events.extend(self.transport.poll_events());
            events
        }
    }
}
pub mod discord_gateway {
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

    use crate::channels::discord::DiscordChannel;

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
        /// `crate::channels::discord::parse_event` and pushed to `inbox`.
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
                                    crate::voice_group::channel_voice::VoiceOutcome::Event(event) => {
                                        inbox.push_inbound(event);
                                    }
                                    crate::voice_group::channel_voice::VoiceOutcome::Reply {
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
                        let wait = crate::channels::delivery::backoff_ms(failures.min(6));
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
}
pub mod delivery {
    //! Delivery planning and reconnect handling (§16).
    //!
    //! Surfacing failures must never lose agent output or silently duplicate it:
    //! a retryable failure is retried with backoff, a permanent failure is
    //! handed back with a reason, and messages queued while a surface was down
    //! are flushed in order on reconnect.

    use crate::OutboundMessage;

    /// What to do with one delivery attempt.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum DeliveryOutcome {
        Delivered,
        /// Try again in `after_ms`.
        Retry {
            attempt: u32,
            after_ms: u64,
        },
        /// Do not retry; report why (bad target, blocked, message too large).
        Dropped {
            reason: String,
        },
    }

    /// Cap on backoff so a long outage does not push retries hours out.
    pub const MAX_BACKOFF_MS: u64 = 60_000;

    /// Exponential backoff: 1s, 2s, 4s, ... capped at [`MAX_BACKOFF_MS`].
    pub fn backoff_ms(attempt: u32) -> u64 {
        // Shift is capped before use so a large attempt cannot overflow.
        1_000u64
            .checked_shl(attempt.min(6))
            .unwrap_or(MAX_BACKOFF_MS)
            .min(MAX_BACKOFF_MS)
    }

    /// Wait before the next attempt when the platform rate-limited us: the
    /// server's own `Retry-After` hint wins over our exponential backoff (it
    /// knows its window; we are guessing), still capped at [`MAX_BACKOFF_MS`]
    /// so one hostile header cannot park the daemon. A missing/zero hint falls
    /// back to [`backoff_ms`].
    pub fn retry_delay_ms(err: &crate::channels::channel::ChannelError, attempt: u32) -> u64 {
        match err.retry_after_secs {
            Some(hint) if hint > 0 => hint.saturating_mul(1000).min(MAX_BACKOFF_MS),
            _ => backoff_ms(attempt),
        }
    }

    /// Plan the next step for a delivery attempt (`attempt` is 1-based).
    pub fn plan_delivery(attempt: u32, max_attempts: u32, retryable: bool) -> DeliveryOutcome {
        if !retryable {
            return DeliveryOutcome::Dropped {
                reason: "surface rejected the message permanently".to_string(),
            };
        }
        if attempt >= max_attempts {
            return DeliveryOutcome::Dropped {
                reason: format!("gave up after {attempt} attempts"),
            };
        }
        DeliveryOutcome::Retry {
            attempt,
            after_ms: backoff_ms(attempt),
        }
    }

    /// Messages waiting for a surface to come back.
    ///
    /// Kept in order: a conversation must not see replies out of sequence after
    /// a reconnect.
    #[derive(Debug, Default)]
    pub struct Outbox {
        pending: Vec<OutboundMessage>,
    }

    impl Outbox {
        pub fn new() -> Self {
            Self::default()
        }

        /// Queue a message for a surface that is currently down.
        pub fn enqueue(&mut self, message: OutboundMessage) {
            self.pending.push(message);
        }

        /// Surface is back: hand over everything queued, in order.
        pub fn on_reconnect(&mut self) -> Vec<OutboundMessage> {
            std::mem::take(&mut self.pending)
        }

        pub fn len(&self) -> usize {
            self.pending.len()
        }

        pub fn is_empty(&self) -> bool {
            self.pending.is_empty()
        }
    }
}
pub mod schedule_delivery {
    //! Scheduled-task delivery: route a fired job's result somewhere user-facing.
    //!
    //! A job declares `deliver` (`log` | `telegram` | `discord` | `notify` |
    //! `file:<path>` | `mobile` | `home`); after the run completes, the
    //! run's final summary - the last assistant message, redacted and truncated
    //! is sent there. The Telegram/Discord senders are the gateway's existing
    //! REST transports, reused rather than reinvented; `notify` uses the
    //! cross-platform desktop notifier ([`crate::notify`]: notify-send on Linux,
    //! osascript on macOS, PowerShell toast on Windows). `mobile` and `home`
    //! are session targets, not channel transports: they name the well-known
    //! home session (id `home`) and are resolved by the gateway's
    //! default-delivery routing, never by [`ChannelSender`]. Delivery failure
    //! never fails the job:

    use std::path::{Path, PathBuf};

    use crate::channels::discord::DiscordTransport;
    use crate::channels::telegram::TelegramTransport;

    /// Where a scheduled job's result goes. Default: [`Deliver::Log`], which
    /// keeps today's behavior (the run sits in the ledger; the tick printed it).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Deliver {
        /// Keep today's behavior: result stays in the ledger / tick stdout.
        Log,
        /// Telegram DM via the gateway's REST transport.
        Telegram,
        /// Discord DM via the gateway's REST transport.
        Discord,
        /// Email via SMTP (PANTHEON_SMTP_HOST, PANTHEON_SMTP_USER, PANTHEON_SMTP_PASS, PANTHEON_DELIVER_EMAIL_TO).
        Email,
        /// Desktop notification (cross-platform: notify-send / osascript /
        /// PowerShell toast).
        Notify,
        /// Append to a file.
        File(PathBuf),
        /// Post into the well-known home session (session id `home`). The
        /// session auto-creates and is pinned by the gateway; senders just
        /// target the id. A session id is not a channel transport, so this is
        /// resolved by the gateway's default-delivery routing, not by
        /// [`ChannelSender`] - see [`deliver_summary`].
        Home,
        /// Deliver to the user's mobile device, with the home session as the
        /// conversation context. Same session-targeted routing as [`Deliver::Home`].
        Mobile,
    }

    impl Deliver {
        /// Parse a `--deliver` value. Unknown targets are a loud error at
        /// registration, not a silent no-delivery at 3am.
        pub fn parse(s: &str) -> Result<Self, String> {
            match s {
            "log" => Ok(Self::Log),
            "telegram" => Ok(Self::Telegram),
            "discord" => Ok(Self::Discord),
            "email" => Ok(Self::Email),
            "notify" => Ok(Self::Notify),
            "home" => Ok(Self::Home),
            "mobile" => Ok(Self::Mobile),
            other => match other.strip_prefix("file:") {
                Some(p) if !p.trim().is_empty() => Ok(Self::File(PathBuf::from(p.trim()))),
                _ => Err(format!(
                    "unknown --deliver target '{other}': log|telegram|discord|email|notify|file:<path>|mobile|home"
                )),
            },
        }
        }

        /// Short label for `schedule list`.
        pub fn label(&self) -> String {
            match self {
                Self::Log => "log".to_string(),
                Self::Telegram => "telegram".to_string(),
                Self::Discord => "discord".to_string(),
                Self::Email => "email".to_string(),
                Self::Notify => "notify".to_string(),
                Self::Home => "home".to_string(),
                Self::Mobile => "mobile".to_string(),
                Self::File(p) => format!("file:{}", p.display()),
            }
        }
    }

    /// Build the deliverable summary: redact secrets, truncate to ~2000 chars.
    pub fn build_summary(raw: &str) -> String {
        const MAX_CHARS: usize = 2000;
        let redacted = pantheon_api::logging::redact(raw.trim());
        if redacted.chars().count() <= MAX_CHARS {
            redacted
        } else {
            let mut s: String = redacted.chars().take(MAX_CHARS - 3).collect();
            s.push_str("...");
            s
        }
    }

    /// Channel sending, injectable so delivery routing is testable without
    /// hitting Telegram/Discord.
    pub trait ChannelSender: Send + Sync {
        fn send_telegram(&self, text: &str) -> Result<(), String>;
        fn send_discord(&self, text: &str) -> Result<(), String>;
        fn send_email(&self, text: &str) -> Result<(), String>;
    }

    fn env_first(keys: &[&str]) -> Result<String, String> {
        for k in keys {
            if let Ok(v) = std::env::var(k) {
                if !v.trim().is_empty() {
                    return Ok(v);
                }
            }
        }
        Err(format!("none of {} is set", keys.join(" / ")))
    }

    /// The real sender: the gateway's existing REST transports.
    ///
    /// Token resolution matches the gateway daemon's
    /// (`service::read_channel_tokens`): process environment first, then
    /// `<data_dir>/gateway.env` - the file the setup wizard writes. The
    /// old sender read only the process environment, so wizard-configured
    /// installs had working channels but failing scheduled delivery and
    /// `pantheon send`. A sender built without a data dir keeps the old
    /// env-only behavior (its callers have no dir to offer).
    pub struct RestChannelSender {
        data_dir: Option<PathBuf>,
    }

    impl RestChannelSender {
        pub fn new() -> Self {
            Self { data_dir: None }
        }

        pub fn with_data_dir(data_dir: &Path) -> Self {
            Self {
                data_dir: Some(data_dir.to_path_buf()),
            }
        }

        fn channel_token(&self, env_names: &[&str], secret_name: &str) -> Result<String, String> {
            if let Ok(v) = env_first(env_names) {
                return Ok(v);
            }
            if let Some(dir) = &self.data_dir {
                if let Some(v) = pantheon_secrets::gateway_token(dir, secret_name) {
                    let v = v.expose().to_string();
                    if !v.trim().is_empty() {
                        return Ok(v);
                    }
                }
            }
            Err(format!("none of {} is set", env_names.join(" / ")))
        }
    }

    impl Default for RestChannelSender {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ChannelSender for RestChannelSender {
        fn send_telegram(&self, text: &str) -> Result<(), String> {
            let token = self.channel_token(
                &["PANTHEON_TELEGRAM_BOT_TOKEN"],
                pantheon_secrets::TELEGRAM_TOKEN_NAME,
            )?;
            let chat = env_first(&["PANTHEON_DELIVER_TELEGRAM_TO"]).map_err(|_| {
                "PANTHEON_DELIVER_TELEGRAM_TO is not set (the chat id to deliver to)".to_string()
            })?;
            let transport = crate::channels::telegram::TelegramRestTransport::new(token);
            let payload = serde_json::json!({ "text": text });
            transport
                .send_message(&chat, &payload)
                .map_err(|e| format!("telegram send: {e}"))?;
            Ok(())
        }

        fn send_discord(&self, text: &str) -> Result<(), String> {
            let token = self.channel_token(
                &["PANTHEON_DISCORD_TOKEN", "PANTHEON_DISCORD_BOT_TOKEN"],
                pantheon_secrets::DISCORD_TOKEN_NAME,
            )?;
            let channel = env_first(&["PANTHEON_DELIVER_DISCORD_TO"]).map_err(|_| {
                "PANTHEON_DELIVER_DISCORD_TO is not set (the channel id to deliver to)".to_string()
            })?;
            let transport = crate::channels::discord::DiscordRestTransport::new(token);
            let payload = serde_json::json!({ "content": text });
            transport
                .send_message(&channel, &payload)
                .map_err(|e| format!("discord send: {e}"))?;
            Ok(())
        }

        fn send_email(&self, text: &str) -> Result<(), String> {
            // Minimal SMTP client over plain TCP with optional STARTTLS is out of scope for the sync transport.
            // Use the `sendmail` binary if available as a pragmatic local delivery path, else fail loud.
            let to = env_first(&["PANTHEON_DELIVER_EMAIL_TO"])
                .map_err(|_| "PANTHEON_DELIVER_EMAIL_TO is not set".to_string())?;
            let from = env_first(&["PANTHEON_SMTP_FROM"]).unwrap_or_else(|_| to.clone());
            let subject = "pantheon";
            let body = format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\n\r\n{text}");
            // Try sendmail -t
            match std::process::Command::new("sendmail").arg("-t").stdin(std::process::Stdio::piped()).spawn() {
            Ok(mut child) => {
                use std::io::Write as _;
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(body.as_bytes());
                }
                let out = child.wait().map_err(|e| format!("sendmail wait: {e}"))?;
                if out.success() {
                    Ok(())
                } else {
                    Err(format!("sendmail exited {}", out.code().unwrap_or(-1)))
                }
            }
            Err(e) => Err(format!("email delivery needs sendmail or SMTP config, sendmail spawn failed: {e}; set PANTHEON_SMTP_HOST for future SMTP support")),
        }
        }
    }

    fn notify_desktop(title: &str, body: &str) -> Result<(), String> {
        // Cross-platform: notify-send on Linux, osascript on macOS, PowerShell
        // toast on Windows. Shared with the TUI turn-complete notifier.
        crate::server::notify::desktop_notify(title, body)
    }

    fn append_file(path: &Path, job_label: &str, summary: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
        }
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let entry = format!("## {job_label} - {now_ms}\n\n{summary}\n\n");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| {
                use std::io::Write as _;
                f.write_all(entry.as_bytes())
            })
            .map_err(|e| format!("append to {}: {e}", path.display()))?;
        Ok(())
    }

    /// Route a job's summary to its delivery target. `job_label` identifies the
    /// job in the message (id or short name).
    pub fn deliver_summary(
        sender: &dyn ChannelSender,
        target: &Deliver,
        job_label: &str,
        summary: &str,
    ) -> Result<(), String> {
        match target {
            Deliver::Log => Ok(()),
            Deliver::Telegram => sender.send_telegram(&format!("⏰ `{job_label}`:\n\n{summary}")),
            Deliver::Discord => sender.send_discord(&format!("⏰ **{job_label}**:\n\n{summary}")),
            Deliver::Email => sender.send_email(&format!("{job_label}:\n\n{summary}")),
            Deliver::Notify => {
                notify_desktop("Pantheon scheduled job", &format!("{job_label}: {summary}"))
            }
            Deliver::File(path) => append_file(path, job_label, summary),
            // Session targets, not channel transports: there is no
            // `ChannelSender` method that posts into a session id, and
            // inventing one here would silently misroute. The gateway's
            // default-delivery routing resolves the home session id to a
            // surface; until a caller goes through that routing, this is a
            // loud error - never a silent no-delivery.
            Deliver::Home | Deliver::Mobile => Err(format!(
                "'{}' is a session target, not a channel transport: it needs the \
             gateway's default-delivery routing to resolve the home session",
                target.label()
            )),
        }
    }

    /// Never-failing delivery: returns `Some(error)` instead of propagating.
    /// A failed delivery must never fail the job itself - the caller logs the
    /// message and moves on.
    pub fn deliver_best_effort(
        sender: &dyn ChannelSender,
        target: &Deliver,
        job_label: &str,
        summary: &str,
    ) -> Option<String> {
        deliver_summary(sender, target, job_label, summary).err()
    }

    // ---------------------------------------------------------------------------
    // Home session delivery
    // ---------------------------------------------------------------------------

    /// Whether a job's raw `deliver` value routes through the home session:
    /// no explicit target (`None`), `"mobile"`, and `"home"` all do. Matched
    /// against the raw string rather than the parsed [`Deliver`] so this works
    /// whether or not `Deliver::parse` knows a `mobile` spelling - `parse`
    /// itself is owned elsewhere and stays untouched.
    pub fn routes_via_home_session(raw: Option<&str>) -> bool {
        match raw {
            // No explicit target: the home session is the default delivery
            // target.
            None => true,
            Some(s) => {
                let t = s.trim();
                t.eq_ignore_ascii_case("mobile") || t.eq_ignore_ascii_case("home")
            }
        }
    }

    /// Write a job result into the home session's ledger so the user finds it
    /// in the pinned session (dashboard, mobile app, TUI picker). The home
    /// session is auto-created on first use. Never fails: returns
    /// `Some(error)` instead, mirroring [`deliver_best_effort`] - a delivery
    /// failure must never fail the job.
    pub fn deliver_to_home_session(
        data_dir: &Path,
        job_label: &str,
        summary: &str,
    ) -> Option<String> {
        let ledger = match pantheon_storage::Ledger::open(&data_dir.join("ledger.db")) {
            Ok(l) => l,
            Err(e) => return Some(format!("open ledger: {e}")),
        };
        if let Err(e) = ledger.ensure_home_session() {
            return Some(format!("home session: {e}"));
        }
        let event = pantheon_api::events::Event::RunProgress {
            run_id: pantheon_storage::HOME_SESSION_ID.to_string(),
            detail: format!("scheduled job {job_label}: {summary}"),
        };
        ledger
            .append(&event)
            .err()
            .map(|e| format!("append to home session: {e}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn channel_token_falls_back_to_gateway_env() {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-delivery-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            pantheon_secrets::set_gateway_token(
                &dir,
                pantheon_secrets::TELEGRAM_TOKEN_NAME,
                pantheon_secrets::SecretValue::new("wizard-token-123"),
            )
            .expect("store wizard token");
            let sender = RestChannelSender::with_data_dir(&dir);
            // No such process env var exists; the wizard's gateway.env must win.
            let got = sender
                .channel_token(
                    &["PANTHEON_TEST_NO_SUCH_VAR_XYZ"],
                    pantheon_secrets::TELEGRAM_TOKEN_NAME,
                )
                .expect("gateway.env fallback");
            assert_eq!(got, "wizard-token-123");
            // And a sender with no data dir still fails closed on env only.
            let err = RestChannelSender::new()
                .channel_token(
                    &["PANTHEON_TEST_NO_SUCH_VAR_XYZ"],
                    pantheon_secrets::TELEGRAM_TOKEN_NAME,
                )
                .expect_err("no dir, no env must error");
            assert!(err.contains("PANTHEON_TEST_NO_SUCH_VAR_XYZ"), "{err}");
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
