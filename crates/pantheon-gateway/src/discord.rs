//! Discord Channel adapter.
//!
//! The adapter owns Discord's wire formatting and limit handling, while the
//! transport seam owns the actual HTTP client. This keeps the channel usable
//! with the std-only server, a gateway daemon, or a test recorder without
//! coupling the runtime to a Discord SDK.

use crate::channel::{
    chunk_text, format_text, Channel, ChannelEnvelope, ChannelError, ChannelEvent,
};
use crate::channel_voice::{
    download_capped, ext_for_filename, ext_for_mime, multipart, ureq_kind,
    warn_voice_unsupported_once, VoiceOutcome, VoicePipes, MAX_VOICE_BYTES, STT_FAILED,
};
use crate::stream::{UiFrame, UiFrameKind};
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
    /// Default: unsupported — test recorders only implement
    /// `send_message`.
    fn download(&self, url: &str) -> Result<Vec<u8>, ChannelError> {
        let _ = url;
        Err(ChannelError::new(
            "DISCORD_TRANSPORT",
            "attachment download not supported by this transport",
        ))
    }
    /// Send synthesized audio as a message attachment. Default:
    /// unsupported — the channel falls back to text.
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
            agent: ureq::Agent::new(),
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
    pub fn build_send_request(&self, channel_id: &str, payload: &Value) -> (String, String, Value) {
        let url = format!("{}/channels/{}/messages", self.api_base, channel_id);
        let auth = format!("Bot {}", self.token);
        (url, auth, payload.clone())
    }
}

impl DiscordTransport for DiscordRestTransport {
    fn send_message(&self, channel_id: &str, payload: &Value) -> Result<(), ChannelError> {
        let (url, auth, body) = self.build_send_request(channel_id, payload);
        self.agent
            .post(&url)
            .set("Authorization", &auth)
            .send_json(body)
            .map_err(|e| ChannelError::from_ureq("DISCORD_HTTP", e))?;
        Ok(())
    }
    fn download(&self, url: &str) -> Result<Vec<u8>, ChannelError> {
        // Attachment URLs are pre-signed CDN links: no Authorization
        // header — the bot token must not travel to the CDN, and the
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
            crate::voice::content_type_for(format),
            audio,
        );
        self.agent
            .post(&url)
            .set("Authorization", &auth)
            .set("Content-Type", &content_type)
            .send_bytes(&body)
            .map_err(|e| {
                ChannelError::new(
                    "DISCORD_HTTP",
                    format!("send audio failed: {}", ureq_kind(&e)),
                )
            })?;
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
            .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "interaction has no custom_id"))?;
        let (approval, run_id, scope) = crate::channel::parse_approval_callback(custom_id)
            .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "invalid approval custom_id"))?;
        let approval = Some(approval);
        let channel_id = data
            .get("channel_id")
            .and_then(Value::as_str)
            .or_else(|| payload.get("channel_id").and_then(Value::as_str))
            .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "interaction has no channel_id"))?;
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
        crate::channel::lock(&self.inbox).push(event);
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
        // as an audio attachment — but only for plain Text frames.
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
        let mut events = std::mem::take(&mut *crate::channel::lock(&self.inbox));
        events.extend(self.transport.poll_events());
        events
    }
}
