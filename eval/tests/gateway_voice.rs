//! Behavioral tests for gateway voice wiring: Telegram/Discord inbound
//! voice fixtures, the STT/TTS double gate, and a fake-backend round trip
//! (no network): voice bytes → transcript event, text → TTS audio →
//! channel send. Live Telegram/Discord API calls are unverifiable in this
//! environment; the transports and backends here are fakes, and the
//! multipart/upload wire shapes are asserted structurally.
use pantheon_api::config::{ToolGroup, ToolsSection, VoiceSection};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_gateway::channel::{Channel, ChannelEnvelope, ChannelError};
use pantheon_gateway::discord::{audio_attachment, DiscordChannel, DiscordTransport};
use pantheon_gateway::stream::{UiFrame, UiFrameKind};
use pantheon_gateway::telegram::{voice_ref, TelegramTransport};
use pantheon_gateway::{
    TelegramChannel, VoiceOutcome, VoicePipes, VoiceSlot, STT_NOT_CONFIGURED,
    VOICE_TRANSCRIPT_PREFIX,
};
use pantheon_providers::voice::{
    AudioFormat, SttProvider, SttRequest, SttResult, TtsProvider, TtsRequest, TtsResult,
};
use pantheon_secrets::SecretsBroker;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

// ── fakes ─────────────────────────────────────────────────────────────

/// TTS fake: echoes the requested format back with fixed audio bytes.
struct FakeTts;

impl TtsProvider for FakeTts {
    fn name(&self) -> &str {
        "fake-tts"
    }
    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        assert!(!req.text.trim().is_empty());
        // Voice selection comes from the [tts] options map, read by the
        // backend at construction - the request must not override it.
        assert!(
            req.voice.is_none(),
            "channel must not override the configured voice"
        );
        Ok(TtsResult {
            bytes: b"SYNTHESIZED-AUDIO".to_vec(),
            format: req.format,
            provider: "fake-tts".to_string(),
        })
    }
}

struct FakeTelegramTransport {
    files: HashMap<String, Vec<u8>>,
    updates: Mutex<Vec<Value>>,
    sent_messages: Mutex<Vec<(String, Value)>>,
    sent_voices: Mutex<Vec<(String, Vec<u8>, AudioFormat)>>,
}

impl TelegramTransport for FakeTelegramTransport {
    fn get_updates(&self, offset: i64, _timeout_secs: u64) -> Result<Vec<Value>, ChannelError> {
        let all = self.updates.lock().unwrap();
        Ok(all
            .iter()
            .filter(|u| u["update_id"].as_i64().unwrap_or(0) >= offset)
            .cloned()
            .collect())
    }
    fn send_message(&self, chat_id: &str, payload: &Value) -> Result<(), ChannelError> {
        self.sent_messages
            .lock()
            .unwrap()
            .push((chat_id.to_string(), payload.clone()));
        Ok(())
    }
    fn download_file(&self, file_id: &str) -> Result<Vec<u8>, ChannelError> {
        self.files
            .get(file_id)
            .cloned()
            .ok_or_else(|| ChannelError::new("FAKE", "no such file"))
    }
    fn send_voice(
        &self,
        chat_id: &str,
        audio: &[u8],
        format: AudioFormat,
    ) -> Result<(), ChannelError> {
        self.sent_voices
            .lock()
            .unwrap()
            .push((chat_id.to_string(), audio.to_vec(), format));
        Ok(())
    }
}

struct FakeDiscordTransport {
    downloads: HashMap<String, Vec<u8>>,
    sent_messages: Mutex<Vec<(String, Value)>>,
    sent_audios: Mutex<Vec<(String, Vec<u8>, AudioFormat)>>,
}

impl DiscordTransport for FakeDiscordTransport {
    fn send_message(&self, channel_id: &str, payload: &Value) -> Result<(), ChannelError> {
        self.sent_messages
            .lock()
            .unwrap()
            .push((channel_id.to_string(), payload.clone()));
        Ok(())
    }
    fn download(&self, url: &str) -> Result<Vec<u8>, ChannelError> {
        self.downloads
            .get(url)
            .cloned()
            .ok_or_else(|| ChannelError::new("FAKE", "no such url"))
    }
    fn send_audio(
        &self,
        channel_id: &str,
        audio: &[u8],
        format: AudioFormat,
    ) -> Result<(), ChannelError> {
        self.sent_audios
            .lock()
            .unwrap()
            .push((channel_id.to_string(), audio.to_vec(), format));
        Ok(())
    }
}

fn text_envelope(thread_id: &str, text: &str) -> ChannelEnvelope {
    ChannelEnvelope {
        thread_id: thread_id.to_string(),
        frame: UiFrame {
            id: 1,
            kind: UiFrameKind::Text,
            run_id: String::new(),
            thread_id: thread_id.to_string(),
            name: "delta".to_string(),
            text: text.to_string(),
            interrupt: false,
            genui: None,
        },
    }
}

// ── fixture parsing: Telegram ─────────────────────────────────────────

#[test]
fn telegram_voice_fixture_extracts_file_ref() {
    let message = json!({
        "message_id": 10,
        "chat": {"id": 100},
        "from": {"id": 7},
        "voice": {"file_id": "AwACAgE voice file id", "duration": 3, "mime_type": "audio/ogg"}
    });
    let vr = voice_ref(&message).expect("voice message must yield a file ref");
    assert_eq!(vr.file_id, "AwACAgE voice file id");
    assert_eq!(vr.ext, "ogg");
}

#[test]
fn telegram_audio_fixture_extracts_file_ref() {
    let message = json!({
        "message_id": 11,
        "chat": {"id": 100},
        "from": {"id": 7},
        "audio": {"file_id": "audio file id", "duration": 42, "mime_type": "audio/mpeg", "title": "song"}
    });
    let vr = voice_ref(&message).expect("audio message must yield a file ref");
    assert_eq!(vr.file_id, "audio file id");
    assert_eq!(vr.ext, "mp3");
}

#[test]
fn telegram_text_message_has_no_voice_ref() {
    let message = json!({
        "message_id": 12,
        "chat": {"id": 100},
        "from": {"id": 7},
        "text": "just text"
    });
    assert!(voice_ref(&message).is_none());
}

// ── fixture parsing: Discord ──────────────────────────────────────────

fn discord_message_with_attachment() -> Value {
    json!({
        "type": 0,
        "id": "m1",
        "channel_id": "chan-1",
        "author": {"id": "user-9"},
        "content": "",
        "attachments": [{
            "id": "a1",
            "filename": "voice-message.ogg",
            "content_type": "audio/ogg",
            "url": "https://cdn.discordapp.com/attachments/x/voice-message.ogg",
            "size": 1234
        }]
    })
}

#[test]
fn discord_audio_attachment_fixture_detected() {
    let a = audio_attachment(&discord_message_with_attachment())
        .expect("audio attachment must be detected");
    assert_eq!(
        a.url,
        "https://cdn.discordapp.com/attachments/x/voice-message.ogg"
    );
    assert_eq!(a.ext, "ogg");
}

#[test]
fn discord_image_attachment_ignored() {
    let mut msg = discord_message_with_attachment();
    msg["attachments"][0]["content_type"] = json!("image/png");
    msg["attachments"][0]["filename"] = json!("pic.png");
    assert!(audio_attachment(&msg).is_none());
}

#[test]
fn discord_message_without_attachments_has_none() {
    let msg = json!({
        "type": 0, "id": "m2", "channel_id": "chan-1",
        "author": {"id": "user-9"}, "content": "hello", "attachments": []
    });
    assert!(audio_attachment(&msg).is_none());
}

// ── double gate ───────────────────────────────────────────────────────

fn command_stt_section() -> VoiceSection {
    VoiceSection {
        backend: "command".to_string(),
        options: [("cmd".to_string(), "echo".to_string())]
            .into_iter()
            .collect(),
    }
}

#[test]
fn voice_double_gate_requires_toggle_and_section() {
    let secrets = SecretsBroker::new();
    // Toggle off + section present: disabled, backend never constructed.
    let mut tools_off = ToolsSection::default();
    tools_off.voice = Some(false);
    let pipes = VoicePipes::from_config(
        Some(&tools_off),
        Some(&command_stt_section()),
        None,
        &secrets,
        false,
    );
    assert!(matches!(pipes.stt, VoiceSlot::Disabled));

    // Toggle on (absent = on) + section present: ready.
    let pipes = VoicePipes::from_config(None, Some(&command_stt_section()), None, &secrets, false);
    assert!(matches!(pipes.stt, VoiceSlot::Ready(_)));

    // Section absent: disabled even with the toggle on.
    let pipes = VoicePipes::from_config(None, None, None, &secrets, false);
    assert!(matches!(pipes.stt, VoiceSlot::Disabled));

    // Unknown backend: unavailable (surfaced, never a model error), and
    // the error slot carries a code, not key material.
    let bad = VoiceSection {
        backend: "nope".to_string(),
        options: HashMap::new(),
    };
    let pipes = VoicePipes::from_config(None, Some(&bad), None, &secrets, false);
    match &pipes.stt {
        VoiceSlot::Unavailable(code) => assert_eq!(code, "VOICE_BACKEND_UNKNOWN"),
        other => panic!("expected Unavailable, got {}", slot_name(other)),
    }

    // voice_replies alone does not enable speech: speak_replies needs a
    // working [tts] backend too.
    let pipes = VoicePipes::from_config(None, None, None, &secrets, true);
    assert!(!pipes.speak_replies());
}

fn slot_name<T>(slot: &VoiceSlot<T>) -> &'static str {
    match slot {
        VoiceSlot::Disabled => "Disabled",
        VoiceSlot::Ready(_) => "Ready",
        VoiceSlot::Unavailable(_) => "Unavailable",
    }
}

#[test]
fn voice_tool_group_toggle_checked_by_name() {
    // Sanity: the gate really keys on the voice group, not another group.
    let mut tools = ToolsSection::default();
    tools.voice = Some(false);
    assert!(!tools.is_enabled(ToolGroup::Voice));
    assert!(ToolsSection::default().is_enabled(ToolGroup::Voice));
}

// ── inbound STT round trip: Telegram ──────────────────────────────────

#[test]
fn telegram_voice_update_transcribes_and_cleans_up() {
    let audio = b"FAKEAUDIO".to_vec();
    let transport = Arc::new(FakeTelegramTransport {
        files: [("voice-1".to_string(), audio.clone())]
            .into_iter()
            .collect(),
        updates: Mutex::new(vec![
            json!({"update_id": 1, "message": {
                "message_id": 10, "chat": {"id": 100}, "from": {"id": 7},
                "voice": {"file_id": "voice-1", "duration": 3}}}),
            json!({"update_id": 2, "message": {
                "message_id": 11, "chat": {"id": 100}, "from": {"id": 7},
                "text": "typed hello"}}),
        ]),
        sent_messages: Mutex::new(vec![]),
        sent_voices: Mutex::new(vec![]),
    });
    let seen_path: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let seen_path2 = seen_path.clone();
    let audio2 = audio.clone();

    struct ProbeStt {
        transcript: String,
        expected: Vec<u8>,
        seen: Arc<Mutex<Option<PathBuf>>>,
    }
    impl SttProvider for ProbeStt {
        fn name(&self) -> &str {
            "probe-stt"
        }
        fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
            let bytes = std::fs::read(&req.path).unwrap();
            assert_eq!(bytes, self.expected);
            *self.seen.lock().unwrap() = Some(req.path.clone());
            Ok(SttResult {
                text: self.transcript.clone(),
                language: None,
                duration_secs: None,
                provider: "probe-stt".to_string(),
            })
        }
    }

    let pipes = VoicePipes {
        stt: VoiceSlot::Ready(Box::new(ProbeStt {
            transcript: "hello from speech".to_string(),
            expected: audio2,
            seen: seen_path2,
        })),
        tts: VoiceSlot::Disabled,
        voice_replies: false,
    };
    let channel = TelegramChannel::new("tok", transport.clone()).with_voice(pipes);

    // End to end through the channel's own poll_updates: fixture update
    // → fake download → temp file → fake STT → transcript event; the
    // plain-text update passes through untouched.
    let (outcomes, next) = channel
        .poll_updates(transport.as_ref(), 0, 1)
        .expect("poll_updates");
    assert_eq!(next, 3);
    assert_eq!(outcomes.len(), 2);
    match &outcomes[0] {
        VoiceOutcome::Event(event) => {
            assert_eq!(event.thread_id, "100");
            assert_eq!(event.sender.as_deref(), Some("7"));
            assert_eq!(
                event.text,
                format!("{VOICE_TRANSCRIPT_PREFIX}hello from speech")
            );
        }
        VoiceOutcome::Reply { .. } => panic!("ready pipes must transcribe"),
    }
    match &outcomes[1] {
        VoiceOutcome::Event(event) => assert_eq!(event.text, "typed hello"),
        VoiceOutcome::Reply { .. } => panic!("text message must stay an event"),
    }
    // The staged temp file is gone on the success path.
    let staged = seen_path.lock().unwrap().clone().expect("backend ran");
    assert!(
        !staged.exists(),
        "temp audio must be deleted after transcription"
    );
    let _ = audio;
}

#[test]
fn telegram_voice_is_declined_not_dropped_when_unconfigured() {
    let transport = Arc::new(FakeTelegramTransport {
        files: [("voice-9".to_string(), b"bytes".to_vec())]
            .into_iter()
            .collect(),
        updates: Mutex::new(vec![json!({"update_id": 5, "message": {
            "message_id": 10, "chat": {"id": 55}, "from": {"id": 3},
            "voice": {"file_id": "voice-9", "duration": 2}}})]),
        sent_messages: Mutex::new(vec![]),
        sent_voices: Mutex::new(vec![]),
    });
    let channel = TelegramChannel::new("tok", transport.clone()); // voice disabled by default
    let (outcomes, next) = channel
        .poll_updates(transport.as_ref(), 4, 1)
        .expect("poll_updates");
    assert_eq!(next, 6);
    assert_eq!(
        outcomes.len(),
        1,
        "a voice message always yields an outcome"
    );
    match &outcomes[0] {
        VoiceOutcome::Reply { thread_id, text } => {
            assert_eq!(thread_id, "55");
            assert_eq!(text, STT_NOT_CONFIGURED);
        }
        VoiceOutcome::Event(_) => panic!("no STT backend: must decline, not transcribe"),
    }
}

// ── inbound STT round trip: Discord ───────────────────────────────────

#[test]
fn discord_audio_attachment_transcribes() {
    let url = "https://cdn.discordapp.com/attachments/x/voice-message.ogg";
    let transport = Arc::new(FakeDiscordTransport {
        downloads: [(url.to_string(), b"DISCORDAUDIO".to_vec())]
            .into_iter()
            .collect(),
        sent_messages: Mutex::new(vec![]),
        sent_audios: Mutex::new(vec![]),
    });
    let seen: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let seen2 = seen.clone();
    struct ProbeStt {
        seen: Arc<Mutex<Option<PathBuf>>>,
    }
    impl SttProvider for ProbeStt {
        fn name(&self) -> &str {
            "probe-stt"
        }
        fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
            assert_eq!(std::fs::read(&req.path).unwrap(), b"DISCORDAUDIO");
            *self.seen.lock().unwrap() = Some(req.path.clone());
            Ok(SttResult {
                text: "discord voice note".to_string(),
                language: None,
                duration_secs: None,
                provider: "probe-stt".to_string(),
            })
        }
    }
    let pipes = VoicePipes {
        stt: VoiceSlot::Ready(Box::new(ProbeStt { seen: seen2 })),
        tts: VoiceSlot::Disabled,
        voice_replies: false,
    };
    let channel = DiscordChannel::new("tok", transport).with_voice(pipes);

    let outcomes = channel.ingest(&discord_message_with_attachment());
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        VoiceOutcome::Event(event) => {
            assert_eq!(event.thread_id, "chan-1");
            assert_eq!(event.sender.as_deref(), Some("user-9"));
            assert_eq!(
                event.text,
                format!("{VOICE_TRANSCRIPT_PREFIX}discord voice note")
            );
        }
        VoiceOutcome::Reply { .. } => panic!("ready pipes must transcribe"),
    }
    let staged = seen.lock().unwrap().clone().expect("backend ran");
    assert!(
        !staged.exists(),
        "temp audio must be deleted after transcription"
    );
}

#[test]
fn discord_audio_attachment_declined_without_stt() {
    let url = "https://cdn.discordapp.com/attachments/x/voice-message.ogg";
    let transport = Arc::new(FakeDiscordTransport {
        downloads: [(url.to_string(), b"DISCORDAUDIO".to_vec())]
            .into_iter()
            .collect(),
        sent_messages: Mutex::new(vec![]),
        sent_audios: Mutex::new(vec![]),
    });
    let channel = DiscordChannel::new("tok", transport); // voice disabled by default
    let outcomes = channel.ingest(&discord_message_with_attachment());
    assert_eq!(
        outcomes.len(),
        1,
        "a voice message always yields an outcome"
    );
    match &outcomes[0] {
        VoiceOutcome::Reply { thread_id, text } => {
            assert_eq!(thread_id, "chan-1");
            assert_eq!(text, STT_NOT_CONFIGURED);
        }
        VoiceOutcome::Event(_) => panic!("no STT backend: must decline, not transcribe"),
    }
}

#[test]
fn discord_plain_message_still_parses() {
    let transport = Arc::new(FakeDiscordTransport {
        downloads: HashMap::new(),
        sent_messages: Mutex::new(vec![]),
        sent_audios: Mutex::new(vec![]),
    });
    let channel = DiscordChannel::new("tok", transport);
    let msg = json!({
        "type": 0, "id": "m2", "channel_id": "chan-1",
        "author": {"id": "user-9"}, "content": "hello", "attachments": []
    });
    let outcomes = channel.ingest(&msg);
    assert_eq!(outcomes.len(), 1);
    match &outcomes[0] {
        VoiceOutcome::Event(event) => assert_eq!(event.text, "hello"),
        VoiceOutcome::Reply { .. } => panic!("plain message must stay an event"),
    }
}

// ── outbound TTS ──────────────────────────────────────────────────────

fn tts_pipes(voice_replies: bool) -> VoicePipes {
    VoicePipes {
        stt: VoiceSlot::Disabled,
        tts: VoiceSlot::Ready(Box::new(FakeTts)),
        voice_replies,
    }
}

#[test]
fn telegram_voice_reply_sends_voice_not_text() {
    let transport = Arc::new(FakeTelegramTransport {
        files: HashMap::new(),
        updates: Mutex::new(vec![]),
        sent_messages: Mutex::new(vec![]),
        sent_voices: Mutex::new(vec![]),
    });
    let channel = TelegramChannel::new("tok", transport.clone()).with_voice(tts_pipes(true));
    channel
        .send(text_envelope("chat-1", "hello out loud"))
        .expect("send");
    let voices = transport.sent_voices.lock().unwrap();
    assert_eq!(voices.len(), 1, "voice reply must go out as audio");
    assert_eq!(voices[0].0, "chat-1");
    assert_eq!(voices[0].1, b"SYNTHESIZED-AUDIO");
    assert_eq!(
        voices[0].2,
        AudioFormat::Ogg,
        "telegram requests ogg for sendVoice"
    );
    assert!(
        transport.sent_messages.lock().unwrap().is_empty(),
        "no text fallback when voice send works"
    );
}

#[test]
fn telegram_text_reply_default_when_voice_replies_off() {
    let transport = Arc::new(FakeTelegramTransport {
        files: HashMap::new(),
        updates: Mutex::new(vec![]),
        sent_messages: Mutex::new(vec![]),
        sent_voices: Mutex::new(vec![]),
    });
    // TTS backend present but the per-channel flag off: text stays default.
    let channel = TelegramChannel::new("tok", transport.clone()).with_voice(tts_pipes(false));
    channel
        .send(text_envelope("chat-1", "hello"))
        .expect("send");
    assert!(transport.sent_voices.lock().unwrap().is_empty());
    let sent = transport.sent_messages.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["text"], "hello");
}

#[test]
fn telegram_approval_request_keeps_buttons_even_with_voice_on() {
    let transport = Arc::new(FakeTelegramTransport {
        files: HashMap::new(),
        updates: Mutex::new(vec![]),
        sent_messages: Mutex::new(vec![]),
        sent_voices: Mutex::new(vec![]),
    });
    let channel = TelegramChannel::new("tok", transport.clone()).with_voice(tts_pipes(true));
    let envelope = ChannelEnvelope {
        thread_id: "chat-1".to_string(),
        frame: UiFrame {
            id: 1,
            kind: UiFrameKind::Approval,
            run_id: String::new(),
            thread_id: "chat-1".to_string(),
            name: "requested".to_string(),
            text: "scope".to_string(),
            interrupt: true,
            genui: None,
        },
    };
    channel.send(envelope).expect("send");
    assert!(
        transport.sent_voices.lock().unwrap().is_empty(),
        "approval requests must never be spoken"
    );
    let sent = transport.sent_messages.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].1["reply_markup"]["inline_keyboard"][0][0]["callback_data"], "grant:scope",
        "approval buttons survive the voice path"
    );
}

#[test]
fn telegram_voice_reply_falls_back_to_text_when_tts_fails() {
    struct FailingTts;
    impl TtsProvider for FailingTts {
        fn name(&self) -> &str {
            "failing-tts"
        }
        fn synthesize(&self, _req: &TtsRequest) -> Result<TtsResult, PantheonError> {
            Err(PantheonError::new(
                "TTS_DOWN",
                Layer::Provider,
                true,
                "boom",
                "retry",
                "",
            ))
        }
    }
    let transport = Arc::new(FakeTelegramTransport {
        files: HashMap::new(),
        updates: Mutex::new(vec![]),
        sent_messages: Mutex::new(vec![]),
        sent_voices: Mutex::new(vec![]),
    });
    let pipes = VoicePipes {
        stt: VoiceSlot::Disabled,
        tts: VoiceSlot::Ready(Box::new(FailingTts)),
        voice_replies: true,
    };
    let channel = TelegramChannel::new("tok", transport.clone()).with_voice(pipes);
    channel
        .send(text_envelope("chat-1", "fallback me"))
        .expect("send");
    assert!(transport.sent_voices.lock().unwrap().is_empty());
    let sent = transport.sent_messages.lock().unwrap();
    assert_eq!(sent.len(), 1, "tts failure falls back to text");
    assert_eq!(sent[0].1["text"], "fallback me");
}

#[test]
fn discord_voice_reply_sends_audio_attachment() {
    let transport = Arc::new(FakeDiscordTransport {
        downloads: HashMap::new(),
        sent_messages: Mutex::new(vec![]),
        sent_audios: Mutex::new(vec![]),
    });
    let channel = DiscordChannel::new("tok", transport.clone()).with_voice(tts_pipes(true));
    channel
        .send(text_envelope("chan-9", "hello out loud"))
        .expect("send");
    let audios = transport.sent_audios.lock().unwrap();
    assert_eq!(audios.len(), 1, "voice reply must go out as an attachment");
    assert_eq!(audios[0].0, "chan-9");
    assert_eq!(audios[0].1, b"SYNTHESIZED-AUDIO");
    assert_eq!(audios[0].2, AudioFormat::Mp3);
    assert!(
        transport.sent_messages.lock().unwrap().is_empty(),
        "no text fallback when audio send works"
    );
}

#[test]
fn discord_text_reply_default_when_voice_replies_off() {
    let transport = Arc::new(FakeDiscordTransport {
        downloads: HashMap::new(),
        sent_messages: Mutex::new(vec![]),
        sent_audios: Mutex::new(vec![]),
    });
    let channel = DiscordChannel::new("tok", transport.clone()).with_voice(tts_pipes(false));
    channel
        .send(text_envelope("chan-9", "hello"))
        .expect("send");
    assert!(transport.sent_audios.lock().unwrap().is_empty());
    let sent = transport.sent_messages.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1["content"], "hello");
}

// ── mobile HTTP edge (`POST /agui/voice/transcribe`, `/agui/voice/speak`) ──
//
// The gateway owns the voice/network edge for the mobile app: base64
// audio up, transcript JSON back; text in, raw audio bytes out. These
// exercise the real handler contract (request bytes →
// status/content-type/body) with fake STT/TTS backends, plus one
// end-to-end pass through the real `command` backend. Socket framing,
// token auth, and the unconfigured 400s are covered at the TCP level
// in `pantheon-runtime`'s `serve_tests.rs`.
// Fixture audio is synthetic bytes, never a recording of anyone.

use pantheon_gateway::voice::{VoiceEdge, MAX_AUDIO_BYTES, MAX_TEXT_CHARS};
use std::collections::HashSet;
use std::time::Duration;

fn mobile_b64encode(bytes: &[u8]) -> String {
    const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPH[((n >> 18) & 63) as usize] as char);
        out.push(ALPH[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPH[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPH[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

#[derive(Default)]
struct MobileSeenStt {
    audio: Vec<u8>,
    language: Option<String>,
    prompt: Option<String>,
}

struct MobileFakeStt {
    transcript: String,
    seen: Arc<Mutex<MobileSeenStt>>,
    fail_code: Option<String>,
    sleep: Duration,
}

impl SttProvider for MobileFakeStt {
    fn name(&self) -> &str {
        "mobile-fake-stt"
    }
    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        if !self.sleep.is_zero() {
            std::thread::sleep(self.sleep);
        }
        if let Some(code) = &self.fail_code {
            return Err(PantheonError::new(
                code.clone(),
                Layer::Provider,
                false,
                "fake backend failure",
                "",
                "",
            ));
        }
        let audio = std::fs::read(&req.path).expect("staged audio is readable");
        let mut seen = self.seen.lock().unwrap();
        seen.audio = audio;
        seen.language = req.language.clone();
        seen.prompt = req.prompt.clone();
        Ok(SttResult {
            text: self.transcript.clone(),
            language: req.language.clone(),
            duration_secs: None,
            provider: self.name().to_string(),
        })
    }
}

/// Mobile-edge TTS fake: unlike the channel `FakeTts` (which forbids
/// voice overrides), the mobile client passes `voice` per request, so
/// this fake records it.
struct MobileFakeTts {
    audio: Vec<u8>,
    seen_voice: Arc<Mutex<Option<String>>>,
    seen_format: Arc<Mutex<AudioFormat>>,
}

impl TtsProvider for MobileFakeTts {
    fn name(&self) -> &str {
        "mobile-fake-tts"
    }
    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        *self.seen_voice.lock().unwrap() = req.voice.clone();
        *self.seen_format.lock().unwrap() = req.format;
        // Echo the requested format back as the produced format so the
        // content-type mapping is exercised end to end.
        Ok(TtsResult {
            bytes: self.audio.clone(),
            format: req.format,
            provider: self.name().to_string(),
        })
    }
}

fn mobile_stt_edge(transcript: &str) -> (VoiceEdge, Arc<Mutex<MobileSeenStt>>) {
    let seen = Arc::new(Mutex::new(MobileSeenStt::default()));
    let fake = MobileFakeStt {
        transcript: transcript.to_string(),
        seen: seen.clone(),
        fail_code: None,
        sleep: Duration::ZERO,
    };
    (VoiceEdge::with_backends(Some(Arc::new(fake)), None), seen)
}

fn mobile_tts_edge() -> (
    VoiceEdge,
    Arc<Mutex<Option<String>>>,
    Arc<Mutex<AudioFormat>>,
) {
    let seen_voice = Arc::new(Mutex::new(None));
    let seen_format = Arc::new(Mutex::new(AudioFormat::Wav));
    let fake = MobileFakeTts {
        audio: b"FAKEAUDIO".to_vec(),
        seen_voice: seen_voice.clone(),
        seen_format: seen_format.clone(),
    };
    (
        VoiceEdge::with_backends(None, Some(Arc::new(fake))),
        seen_voice,
        seen_format,
    )
}

fn mobile_err_code(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["error"]["code"].as_str().unwrap_or("").to_string()
}

fn mobile_err_message(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["error"]["message"].as_str().unwrap_or("").to_string()
}

#[test]
fn mobile_transcribe_round_trip() {
    let (edge, seen) = mobile_stt_edge("hello from the mic");
    let audio: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    let body = json!({
        "audio": mobile_b64encode(&audio),
        "language": "en",
        "prompt": "meeting notes",
    });
    let r = edge.handle_transcribe(body.to_string().as_bytes());
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.content_type, "application/json");
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["transcript"], "hello from the mic");
    assert_eq!(v["backend"], "mobile-fake-stt");
    // The backend saw exactly the bytes we sent: base64 and the temp
    // file round-tripped without corruption, plus the hints.
    let seen = seen.lock().unwrap();
    assert_eq!(seen.audio, audio);
    assert_eq!(seen.language.as_deref(), Some("en"));
    assert_eq!(seen.prompt.as_deref(), Some("meeting notes"));
}

#[test]
fn mobile_speak_round_trip_with_voice_override() {
    let (edge, seen_voice, seen_format) = mobile_tts_edge();
    let r = edge.handle_speak(br#"{"text":"hello world","voice":"zira"}"#);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.content_type, "audio/wav");
    assert_eq!(r.body, b"FAKEAUDIO");
    assert_eq!(seen_voice.lock().unwrap().as_deref(), Some("zira"));
    assert_eq!(*seen_format.lock().unwrap(), AudioFormat::Wav);
}

#[test]
fn mobile_speak_format_selects_content_type() {
    let (edge, _, _) = mobile_tts_edge();
    let r = edge.handle_speak(br#"{"text":"x","format":"mp3"}"#);
    assert_eq!(r.status, 200);
    assert_eq!(r.content_type, "audio/mpeg");
    let r = edge.handle_speak(br#"{"text":"x","format":"ogg"}"#);
    assert_eq!(r.content_type, "audio/ogg");
    let r = edge.handle_speak(br#"{"text":"x","format":"flac"}"#);
    assert_eq!(r.status, 400);
    assert_eq!(mobile_err_code(&r.body), "voice_bad_request");
}

#[test]
fn mobile_unconfigured_backends_400_naming_the_section() {
    let edge = VoiceEdge::unconfigured();
    let r = edge.handle_transcribe(br#"{"audio":"aGk="}"#);
    assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(mobile_err_code(&r.body), "voice_not_configured");
    assert!(
        mobile_err_message(&r.body).contains("[stt]"),
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    let r = edge.handle_speak(br#"{"text":"hi"}"#);
    assert_eq!(r.status, 400);
    assert_eq!(mobile_err_code(&r.body), "voice_not_configured");
    assert!(mobile_err_message(&r.body).contains("[tts]"));
}

#[test]
fn mobile_tools_toggle_off_400s_naming_the_toggle() {
    // The [tools] voice group gates the whole surface: off means the
    // backend is never constructed, even when the section is present.
    let tools = ToolsSection::from_enabled(&[ToolGroup::Browser]).unwrap();
    assert!(!tools.is_enabled(ToolGroup::Voice));
    let mut options = HashMap::new();
    options.insert("cmd".to_string(), "cat".to_string());
    let section = VoiceSection {
        backend: "command".to_string(),
        options,
    };
    let edge = VoiceEdge::from_config(Some(&tools), Some(&section), None, &SecretsBroker::new());
    let r = edge.handle_transcribe(br#"{"audio":"aGk="}"#);
    assert_eq!(r.status, 400);
    assert_eq!(mobile_err_code(&r.body), "voice_not_configured");
    assert!(mobile_err_message(&r.body).contains("[tools] voice"));
}

#[test]
fn mobile_misconfigured_backend_is_a_500_with_the_provider_code() {
    let section = VoiceSection {
        backend: "no-such-backend".to_string(),
        options: HashMap::new(),
    };
    let edge = VoiceEdge::from_config(None, Some(&section), None, &SecretsBroker::new());
    let r = edge.handle_transcribe(br#"{"audio":"aGk="}"#);
    assert_eq!(r.status, 500, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(mobile_err_code(&r.body), "voice_backend_misconfigured");
    assert!(mobile_err_message(&r.body).contains("VOICE_BACKEND_UNKNOWN"));
}

#[test]
fn mobile_backend_failure_is_a_502() {
    let seen = Arc::new(Mutex::new(MobileSeenStt::default()));
    let fake = MobileFakeStt {
        transcript: String::new(),
        seen,
        fail_code: Some("STT_EXIT".to_string()),
        sleep: Duration::ZERO,
    };
    let edge = VoiceEdge::with_backends(Some(Arc::new(fake)), None);
    let r = edge.handle_transcribe(br#"{"audio":"aGk="}"#);
    assert_eq!(r.status, 502, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(mobile_err_code(&r.body), "voice_transcribe_failed");
    let msg = mobile_err_message(&r.body);
    assert!(
        msg.contains("mobile-fake-stt") && msg.contains("STT_EXIT"),
        "{msg}"
    );
}

#[test]
fn mobile_slow_backend_times_out() {
    let seen = Arc::new(Mutex::new(MobileSeenStt::default()));
    let fake = MobileFakeStt {
        transcript: "too late".to_string(),
        seen,
        fail_code: None,
        sleep: Duration::from_millis(500),
    };
    let edge = VoiceEdge::with_backends(Some(Arc::new(fake)), None)
        .with_timeout(Duration::from_millis(50));
    let r = edge.handle_transcribe(br#"{"audio":"aGk="}"#);
    assert_eq!(r.status, 504, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(mobile_err_code(&r.body), "voice_timeout");
}

#[test]
fn mobile_oversized_audio_is_413() {
    let (edge, _) = mobile_stt_edge("x");
    let big = vec![7u8; MAX_AUDIO_BYTES + 1];
    let body = json!({"audio": mobile_b64encode(&big)});
    let r = edge.handle_transcribe(body.to_string().as_bytes());
    assert_eq!(r.status, 413);
    assert_eq!(mobile_err_code(&r.body), "voice_audio_too_large");
}

#[test]
fn mobile_oversized_text_is_413() {
    let (edge, _, _) = mobile_tts_edge();
    let text = "a".repeat(MAX_TEXT_CHARS + 1);
    let body = json!({"text": text});
    let r = edge.handle_speak(body.to_string().as_bytes());
    assert_eq!(r.status, 413);
    assert_eq!(mobile_err_code(&r.body), "voice_text_too_large");
}

#[test]
fn mobile_malformed_requests_are_400() {
    let (stt, _) = mobile_stt_edge("x");
    let (tts, _, _) = mobile_tts_edge();
    for raw in [
        b"{oops".as_slice(),
        br#"{"audio":"!!!not-base64!!!"}"#.as_slice(),
        br#"{"audio":""}"#.as_slice(),
        br#"{"audio":"===="}"#.as_slice(),
        br#"{}"#.as_slice(),
    ] {
        let r = stt.handle_transcribe(raw);
        assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(raw));
        assert_eq!(mobile_err_code(&r.body), "voice_bad_request");
    }
    for raw in [
        br#"{"text":""}"#.as_slice(),
        br#"{"text":"   "}"#.as_slice(),
        br#"{}"#.as_slice(),
        b"{oops".as_slice(),
    ] {
        let r = tts.handle_speak(raw);
        assert_eq!(r.status, 400, "{}", String::from_utf8_lossy(raw));
        assert_eq!(mobile_err_code(&r.body), "voice_bad_request");
    }
}

fn mobile_voice_tmps() -> HashSet<PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("pantheon-agui-voice-"))
        })
        .collect()
}

#[test]
fn mobile_temp_files_are_cleaned_up_on_all_paths() {
    let before = mobile_voice_tmps();
    // Success path.
    let (edge, _) = mobile_stt_edge("ok");
    let r = edge.handle_transcribe(br#"{"audio":"aGk="}"#);
    assert_eq!(r.status, 200);
    // Backend-failure path.
    let seen = Arc::new(Mutex::new(MobileSeenStt::default()));
    let failing = MobileFakeStt {
        transcript: String::new(),
        seen,
        fail_code: Some("STT_EXIT".to_string()),
        sleep: Duration::ZERO,
    };
    let edge = VoiceEdge::with_backends(Some(Arc::new(failing)), None);
    assert_eq!(edge.handle_transcribe(br#"{"audio":"aGk="}"#).status, 502);
    // Timeout path: the abandoned worker owns its temp file and drops
    // it when it finishes, so wait for that before asserting.
    let seen = Arc::new(Mutex::new(MobileSeenStt::default()));
    let slow = MobileFakeStt {
        transcript: String::new(),
        seen,
        fail_code: None,
        sleep: Duration::from_millis(300),
    };
    let edge = VoiceEdge::with_backends(Some(Arc::new(slow)), None)
        .with_timeout(Duration::from_millis(50));
    assert_eq!(edge.handle_transcribe(br#"{"audio":"aGk="}"#).status, 504);
    for _ in 0..100 {
        let now = mobile_voice_tmps();
        let leaked: Vec<_> = now.difference(&before).collect();
        if leaked.is_empty() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "temp audio files leaked: {:?}",
        mobile_voice_tmps().difference(&before).collect::<Vec<_>>()
    );
}

fn mobile_find_cat() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|d| {
        let c = d.join("cat");
        c.is_file().then_some(c)
    })
}

#[test]
fn mobile_from_config_wires_a_real_command_backend_end_to_end() {
    // The path `pantheon serve` actually uses: config section →
    // stt_from_config → command backend → temp file → transcript. `cat`
    // stands in for whisper.cpp: whatever bytes go in come back out.
    let Some(cat) = mobile_find_cat() else {
        return; // non-unix host without cat: nothing to run
    };
    let mut options = HashMap::new();
    options.insert("cmd".to_string(), cat.to_string_lossy().into_owned());
    options.insert("args".to_string(), "{file}".to_string());
    let section = VoiceSection {
        backend: "command".to_string(),
        options,
    };
    let edge = VoiceEdge::from_config(None, Some(&section), None, &SecretsBroker::new());
    let audio = b"not really audio, but cat does not care";
    let body = json!({"audio": mobile_b64encode(audio)});
    let r = edge.handle_transcribe(body.to_string().as_bytes());
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v: Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["transcript"], "not really audio, but cat does not care");
    assert_eq!(v["backend"], "command");
}

#[test]
fn mobile_from_config_wires_a_real_command_tts_backend() {
    let Some(cat) = mobile_find_cat() else {
        return;
    };
    let mut options = HashMap::new();
    options.insert("cmd".to_string(), cat.to_string_lossy().into_owned());
    let section = VoiceSection {
        backend: "command".to_string(),
        options,
    };
    let edge = VoiceEdge::from_config(None, None, Some(&section), &SecretsBroker::new());
    let r = edge.handle_speak(br#"{"text":"say it back","voice":"v1"}"#);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert_eq!(r.content_type, "audio/wav");
    assert_eq!(r.body, b"say it back");
}
