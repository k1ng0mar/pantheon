//! Telegram Channel adapter.

use crate::channel::{
    chunk_text, format_text, Channel, ChannelEnvelope, ChannelError, ChannelEvent,
};
use crate::channel_voice::{
    download_capped, ext_for_mime, multipart, ureq_kind, warn_voice_unsupported_once, VoiceOutcome,
    VoicePipes, MAX_VOICE_BYTES, STT_FAILED,
};
use crate::stream::{UiFrame, UiFrameKind};
use pantheon_providers::voice::AudioFormat;
use serde_json::{json, Value};
use std::fmt;
use std::sync::{Arc, Mutex};

pub const TELEGRAM_MESSAGE_LIMIT: usize = 4_096;

/// Map a send failure. Telegram puts the 429 wait in the JSON body
/// (`parameters.retry_after`, seconds) rather than a `Retry-After` header,
/// so the generic header parse in `from_ureq` would miss it; read the body
/// first (falling back to the header), and cap like the shared parser.
fn send_err(e: ureq::Error) -> ChannelError {
    match e {
        ureq::Error::Status(429, resp) => {
            let header = crate::channel::parse_retry_after(resp.header("retry-after"));
            let body_secs = resp
                .into_string()
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
        }
        other => ChannelError::from_ureq("TELEGRAM_HTTP", other),
    }
}

/// Bot API boundary. A live implementation can use any HTTP client; the
/// adapter only owns Telegram's inline keyboard and message limit semantics.
pub trait TelegramTransport: Send + Sync {
    fn send_message(&self, chat_id: &str, payload: &Value) -> Result<(), ChannelError>;
    /// Download a file by its `file_id` (`getFile` + the file download
    /// URL). Default: unsupported — test recorders only implement
    /// `send_message`.
    fn download_file(&self, file_id: &str) -> Result<Vec<u8>, ChannelError> {
        let _ = file_id;
        Err(ChannelError::new(
            "TELEGRAM_TRANSPORT",
            "file download not supported by this transport",
        ))
    }
    /// Send synthesized audio back (`sendVoice` for ogg, `sendAudio`
    /// otherwise). Default: unsupported — the channel falls back to text.
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
            agent: ureq::Agent::new(),
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
        self.agent.post(&url).send_json(body).map_err(send_err)?;
        Ok(())
    }
    fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, ChannelError> {
        let url = format!("{}/bot{}/getUpdates", self.api_base, self.bot_token);
        let body: Value = self
            .agent
            .post(&url)
            .timeout(std::time::Duration::from_secs(timeout_secs + 5))
            .send_json(json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message", "callback_query"],
            }))
            .map_err(|e| ChannelError::from_ureq("TELEGRAM_HTTP", e))?
            .into_json()
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
        let body: Value = self
            .agent
            .post(&url)
            .send_json(json!({ "file_id": file_id }))
            .map_err(|_| ChannelError::new("TELEGRAM_HTTP", "getFile failed"))
            .and_then(|resp| {
                resp.into_json()
                    .map_err(|_| ChannelError::new("TELEGRAM_HTTP", "getFile: bad response body"))
            })?;
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
            crate::voice::content_type_for(format),
            audio,
        );
        // The URL carries the bot token: the raw ureq error (which echoes
        // the URL) must never reach a log line.
        self.agent
            .post(&url)
            .set("Content-Type", &content_type)
            .send_bytes(&body)
            .map_err(|e| {
                ChannelError::new(
                    "TELEGRAM_HTTP",
                    format!("{method} failed: {}", ureq_kind(&e)),
                )
            })?;
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
/// chat id is missing — the same condition `parse_event` rejects with.
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
        let (approval, run_id, scope) = crate::channel::parse_approval_callback(data)
            .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "invalid approval callback_data"))?;
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
        crate::channel::lock(&self.inbox).push(event);
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
        // Token-safe: the getUpdates URL embeds the bot token and the
        // transport's error message may echo it (ureq's Display prints
        // the URL), so only the error code and (for HTTP failures) the
        // numeric status are surfaced — the daemon's health tracker needs
        // the 401 to spot a revoked token.
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
        // as sendVoice/sendAudio — but only for plain Text frames.
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
        let mut events = std::mem::take(&mut *crate::channel::lock(&self.inbox));
        events.extend(self.transport.poll_events());
        events
    }
}
