//! Provider-native realtime voice (OpenAI Realtime WebSocket).
//!
//! This is the opt-in fast path behind the same `/agui/voice/live`
//! protocol as the local STT → Pantheon agent → TTS pipeline. When
//! `[voice].realtime_backend = "openai"` and `OPENAI_API_KEY` resolves,
//! the session bridges the client's 16 kHz PCM to the provider's
//! full-duplex 24 kHz PCM stream. The provider owns turn detection and
//! speech generation; Pantheon does not run a local agent turn in this
//! mode, so provider tools are limited to what the realtime session
//! offers. Omitting the backend keeps the existing pipeline, which is the
//! only path with Pantheon's full tool/approval surface.
//!
//! The module is deliberately pure where it can be: protocol builders and
//! event decoders are unit-tested without a key or network.

use crate::live_voice::{json_text, send_msg, HeadReplay};
use pantheon_api::config::LiveVoiceSection;
use pantheon_secrets::SecretValue;
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tungstenite::client::IntoClientRequest;
use tungstenite::{Message, WebSocket};

const DEFAULT_MODEL: &str = "gpt-4o-realtime-preview";
const DEFAULT_VOICE: &str = "alloy";
const POLL: Duration = Duration::from_millis(20);
const SAMPLE_RATE_CLIENT: u32 = 16_000;
const SAMPLE_RATE_PROVIDER: u32 = 24_000;

/// Resolved realtime adapter settings. `Debug` never prints the key.
#[derive(Clone)]
pub struct RealtimeConfig {
    pub backend: String,
    pub model: String,
    pub voice: String,
    api_key: SecretValue,
}

impl std::fmt::Debug for RealtimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RealtimeConfig")
            .field("backend", &self.backend)
            .field("model", &self.model)
            .field("voice", &self.voice)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl RealtimeConfig {
    pub fn openai(model: Option<&str>, voice: Option<&str>, api_key: SecretValue) -> Self {
        Self {
            backend: "openai".into(),
            model: model
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_MODEL)
                .to_string(),
            voice: voice
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(DEFAULT_VOICE)
                .to_string(),
            api_key,
        }
    }

    fn endpoint(&self) -> String {
        format!(
            "wss://api.openai.com/v1/realtime?model={}",
            url_encode(&self.model)
        )
    }
}

/// Wire-to-wire events the runner forwards to the live-voice client.
#[derive(Debug, Clone, PartialEq)]
pub enum ProviderEvent {
    /// Final input transcription.
    Transcript(String),
    /// Incremental assistant transcript; `final_text` is set on `.done`.
    ReplyDelta(String),
    ReplyDone(String),
    /// Base64-decoded provider PCM (24 kHz mono i16 LE).
    Audio(Vec<u8>),
    AudioDone,
    /// Provider response finished; the client session returns to idle.
    Done,
    Error(String),
}

/// Decode one provider event. Unknown events are ignored.
pub fn provider_event(v: &Value) -> Option<ProviderEvent> {
    let kind = v.get("type").and_then(|t| t.as_str())?;
    match kind {
        "conversation.item.input_audio_transcription.completed" => v
            .get("transcript")
            .and_then(|t| t.as_str())
            .map(|t| ProviderEvent::Transcript(t.trim().to_string())),
        "response.audio_transcript.delta" => v
            .get("delta")
            .and_then(|t| t.as_str())
            .map(|t| ProviderEvent::ReplyDelta(t.to_string())),
        "response.audio_transcript.done" => v
            .get("transcript")
            .and_then(|t| t.as_str())
            .map(|t| ProviderEvent::ReplyDone(t.to_string())),
        "response.audio.delta" => v
            .get("delta")
            .and_then(|t| t.as_str())
            .and_then(|b64| base64_decode(b64).ok())
            .map(ProviderEvent::Audio),
        "response.audio.done" => Some(ProviderEvent::AudioDone),
        "response.done" => Some(ProviderEvent::Done),
        "error" => {
            let code = v
                .get("error")
                .and_then(|e| e.get("code"))
                .and_then(|c| c.as_str())
                .or_else(|| v.get("error").and_then(|e| e.get("type")).and_then(|c| c.as_str()))
                .unwrap_or("provider_error");
            Some(ProviderEvent::Error(code.to_string()))
        }
        _ => None,
    }
}

/// `session.update` sent immediately after the provider handshake.
pub fn session_update(cfg: &RealtimeConfig) -> Value {
    json!({
        "type": "session.update",
        "session": {
            "modalities": ["audio", "text"],
            "instructions": "You are Pantheon's realtime voice interface. Keep replies concise and conversational. Do not claim to have run tools unless the provider actually ran them.",
            "voice": cfg.voice,
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "input_audio_transcription": {"model": "gpt-4o-mini-transcribe"},
            "turn_detection": null
        }
    })
}

/// Commands sent to the provider for one client utterance.
pub fn append_audio(pcm_16k: &[i16]) -> Value {
    let pcm_24k = resample(pcm_16k, SAMPLE_RATE_CLIENT, SAMPLE_RATE_PROVIDER);
    let mut bytes = Vec::with_capacity(pcm_24k.len() * 2);
    for s in pcm_24k {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    json!({"type": "input_audio_buffer.append", "audio": base64_encode(&bytes)})
}

pub fn commit_audio() -> Value {
    json!({"type": "input_audio_buffer.commit"})
}

pub fn create_response() -> Value {
    json!({
        "type": "response.create",
        "response": {"modalities": ["audio", "text"]}
    })
}

/// 24 kHz provider PCM → 16 kHz client PCM, preserving the wire bytes.
pub fn provider_audio_to_client(bytes: &[u8]) -> Vec<u8> {
    let mut samples = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        samples.push(i16::from_le_bytes(*pair));
    }
    let out = resample(&samples, SAMPLE_RATE_PROVIDER, SAMPLE_RATE_CLIENT);
    let mut bytes = Vec::with_capacity(out.len() * 2);
    for s in out {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    bytes
}

/// Optional provider key. The caller supplies the broker; the adapter
/// only keeps the zeroizing handed-out value.
pub fn resolve_openai_key(
    cfg: Option<&LiveVoiceSection>,
    secrets: &pantheon_secrets::SecretsBroker,
) -> Option<SecretValue> {
    let section = cfg?;
    let backend = section.realtime_backend.as_deref()?.trim();
    if backend != "openai" {
        return None;
    }
    pantheon_providers::voice::voice_api_key(
        secrets,
        "openai",
        &std::collections::HashMap::new(),
    )
    .ok()
    .flatten()
    .filter(|k| !k.expose().trim().is_empty())
}

/// Run one realtime session until the client stops or disconnects.
pub(crate) fn run_session(
    ws: &mut WebSocket<HeadReplay>,
    cfg: RealtimeConfig,
    limits: LiveVoiceSection,
) {
    let (provider_tx, provider_events) = match open_provider(&cfg) {
        Ok(p) => p,
        Err(code) => {
            send_msg(ws, json_text(&json!({"type":"error","code":code})));
            send_msg(ws, Message::text(r#"{"type":"end"}"#));
            return;
        }
    };
    provider_tx
        .send(Message::text(session_update(&cfg).to_string()))
        .ok();
    let _ = ws.get_mut().stream_mut().set_nonblocking(true);
    send_msg(ws, Message::text(r#"{"type":"ready"}"#));
    let deadline = Instant::now() + Duration::from_secs(limits.live_max_session_secs.max(1));
    let mut utterance: Vec<i16> = Vec::new();
    let mut utterance_open = false;
    let mut in_response = false;
    let mut reply = String::new();
    let mut reply_sent = false;
    let mut audio_sent = false;
    loop {
        if Instant::now() >= deadline {
            send_msg(ws, Message::text(r#"{"type":"end"}"#));
            break;
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let v: Value = match serde_json::from_str(t.as_str()) {
                    Ok(v) => v,
                    Err(_) => {
                        send_msg(ws, json_text(&json!({"type":"error","code":"bad_frame"})));
                        continue;
                    }
                };
                match v.get("type").and_then(|x| x.as_str()) {
                    Some("start") => {
                        if in_response {
                            send_msg(ws, Message::text(r#"{"type":"busy"}"#));
                        } else {
                            utterance.clear();
                            utterance_open = true;
                            provider_tx
                                .send(Message::text(
                                    json!({"type":"input_audio_buffer.clear"}).to_string(),
                                ))
                                .ok();
                        }
                    }
                    Some("end") => {
                        if utterance_open && !in_response {
                            utterance_open = false;
                            provider_tx
                                .send(Message::text(commit_audio().to_string()))
                                .ok();
                            provider_tx
                                .send(Message::text(create_response().to_string()))
                                .ok();
                            in_response = true;
                            reply.clear();
                            reply_sent = false;
                            audio_sent = false;
                        }
                    }
                    Some("stop") => {
                        send_msg(ws, Message::text(r#"{"type":"end"}"#));
                        break;
                    }
                    _ => send_msg(ws, json_text(&json!({"type":"error","code":"unknown_frame"}))),
                }
            }
            Ok(Message::Binary(b)) => {
                if in_response || !utterance_open {
                    continue;
                }
                let mut samples = Vec::with_capacity(b.len() / 2);
                for pair in b.as_chunks::<2>().0 {
                    samples.push(i16::from_le_bytes(*pair));
                }
                utterance.extend_from_slice(&samples);
                let cap =
                    limits.live_max_utterance_secs.max(1) as usize * SAMPLE_RATE_CLIENT as usize;
                if utterance.len() > cap {
                    utterance.truncate(cap);
                }
                provider_tx
                    .send(Message::text(append_audio(&samples).to_string()))
                    .ok();
            }
            Ok(Message::Ping(p)) => send_msg(ws, Message::Pong(p)),
            Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
            Ok(Message::Close(_)) => break,
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
            Err(_) => break,
        }

        while let Ok(v) = provider_events.try_recv() {
            match v {
                ProviderEvent::Transcript(t) => send_msg(
                    ws,
                    json_text(&json!({"type":"transcript","text":t,"final":true})),
                ),
                ProviderEvent::ReplyDelta(t) => reply.push_str(&t),
                ProviderEvent::ReplyDone(t) => {
                    reply = t;
                    if !reply_sent {
                        send_msg(ws, json_text(&json!({"type":"reply_text","text":reply})));
                        reply_sent = true;
                    }
                }
                ProviderEvent::Audio(bytes) => {
                    let pcm = provider_audio_to_client(&bytes);
                    if !pcm.is_empty() {
                        if !reply_sent {
                            send_msg(ws, json_text(&json!({"type":"reply_text","text":reply})));
                            reply_sent = true;
                        }
                        audio_sent = true;
                        send_msg(ws, Message::Binary(pcm.into()));
                    }
                }
                ProviderEvent::AudioDone => {
                    if audio_sent {
                        send_msg(ws, Message::text(r#"{"type":"audio_end"}"#));
                        audio_sent = false;
                    }
                }
                ProviderEvent::Done => {
                    if !reply_sent && !reply.is_empty() {
                        send_msg(ws, json_text(&json!({"type":"reply_text","text":reply})));
                    }
                    if audio_sent {
                        send_msg(ws, Message::text(r#"{"type":"audio_end"}"#));
                    }
                    in_response = false;
                    utterance.clear();
                    reply.clear();
                    reply_sent = false;
                    audio_sent = false;
                }
                ProviderEvent::Error(code) => {
                    send_msg(ws, json_text(&json!({"type":"error","code":code})));
                    in_response = false;
                }
            }
        }
        std::thread::sleep(POLL);
    }
    provider_tx.send(Message::Close(None)).ok();
    let _ = ws.close(None);
}

fn open_provider(
    cfg: &RealtimeConfig,
) -> Result<(mpsc::Sender<Message>, mpsc::Receiver<ProviderEvent>), &'static str> {
    let mut req = cfg
        .endpoint()
        .into_client_request()
        .map_err(|_| "realtime_bad_url")?;
    req.headers_mut().insert(
        "Authorization",
        format!("Bearer {}", cfg.api_key.expose())
            .parse()
            .map_err(|_| "realtime_bad_key")?,
    );
    req.headers_mut().insert(
        "OpenAI-Beta",
        "realtime=v1".parse().map_err(|_| "realtime_bad_header")?,
    );
    let (mut ws, _resp) = tungstenite::connect(req).map_err(|_| "realtime_connect")?;
    set_read_timeout(&mut ws);
    let (cmd_tx, cmd_rx) = mpsc::channel::<Message>();
    let (ev_tx, ev_rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            while let Ok(cmd) = cmd_rx.try_recv() {
                let closing = matches!(cmd, Message::Close(_));
                if ws.send(cmd).is_err() || closing {
                    return;
                }
            }
            match ws.read() {
                Ok(Message::Text(t)) => {
                    if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                        if let Some(ev) = provider_event(&v) {
                            if ev_tx.send(ev).is_err() {
                                return;
                            }
                        }
                    }
                }
                Ok(Message::Close(_)) => return,
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
                Err(_) => return,
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    Ok((cmd_tx, ev_rx))
}

fn set_read_timeout(ws: &mut WebSocket<tungstenite::stream::MaybeTlsStream<TcpStream>>) {
    let timeout = Some(Duration::from_millis(30));
    match ws.get_mut() {
        tungstenite::stream::MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(timeout);
        }
        tungstenite::stream::MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(timeout);
        }
        _ => {}
    }
}

/// Convenience used by tests and callers that only need the URL.
pub fn endpoint_for(model: &str) -> String {
    format!("wss://api.openai.com/v1/realtime?model={}", url_encode(model))
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn resample(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if samples.is_empty() || from == 0 || to == 0 || from == to {
        return samples.to_vec();
    }
    let out_len = ((samples.len() as u64 * u64::from(to)) / u64::from(from)) as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * f64::from(from) / f64::from(to);
        let i0 = pos.floor() as usize;
        let i1 = (i0 + 1).min(samples.len() - 1);
        let frac = (pos - i0 as f64) as f32;
        let v = samples[i0] as f32 * (1.0 - frac) + samples[i1] as f32 * frac;
        out.push(v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
    }
    out
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map(u32::from);
        let b2 = chunk.get(2).copied().map(u32::from);
        out.push(T[(b0 >> 2) as usize] as char);
        out.push(T[(((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4)) as usize] as char);
        out.push(if b1.is_some() {
            T[(((b1.unwrap_or(0) & 0x0f) << 2) | (b2.unwrap_or(0) >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if b2.is_some() {
            T[(b2.unwrap_or(0) & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    fn val(c: u8) -> Option<u8> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    }
    let clean: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        if chunk.len() < 2 {
            return Err(());
        }
        let a = val(chunk[0]).ok_or(())?;
        let b = val(chunk[1]).ok_or(())?;
        out.push((a << 2) | (b >> 4));
        if chunk.len() > 2 && chunk[2] != b'=' {
            let c = val(chunk[2]).ok_or(())?;
            out.push((b << 4) | (c >> 2));
            if chunk.len() > 3 && chunk[3] != b'=' {
                let d = val(chunk[3]).ok_or(())?;
                out.push((c << 6) | d);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_pcm_bytes() {
        for len in 0..20 {
            let bytes: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&bytes)).unwrap(), bytes);
        }
    }

    #[test]
    fn resampling_doubles_and_halves_length() {
        let x: Vec<i16> = (0..16).collect();
        assert_eq!(resample(&x, 16_000, 24_000).len(), 24);
        assert_eq!(resample(&x, 24_000, 16_000).len(), 10);
    }

    #[test]
    fn event_decoder_ignores_unknown_and_maps_transcript() {
        assert_eq!(provider_event(&json!({"type":"nope"})), None);
        assert_eq!(
            provider_event(&json!({
                "type":"conversation.item.input_audio_transcription.completed",
                "transcript":" hello "
            })),
            Some(ProviderEvent::Transcript("hello".into()))
        );
        assert_eq!(
            provider_event(&json!({"type":"response.done"})),
            Some(ProviderEvent::Done)
        );
    }

    #[test]
    fn append_audio_is_pcm16_base64_and_grows_24k() {
        let v = append_audio(&[1, 2, 3]);
        assert_eq!(v["type"], "input_audio_buffer.append");
        let bytes = base64_decode(v["audio"].as_str().unwrap()).unwrap();
        assert_eq!(bytes.len(), 8); // 4 resampled samples * 2 bytes (3→4)
    }

    #[test]
    fn session_update_turns_off_server_vad() {
        let cfg = RealtimeConfig::openai(None, None, SecretValue::new("test"));
        let v = session_update(&cfg);
        assert_eq!(v["session"]["turn_detection"], Value::Null);
        assert_eq!(v["session"]["input_audio_format"], "pcm16");
    }
}
