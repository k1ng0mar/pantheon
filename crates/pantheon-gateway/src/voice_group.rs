//! Gateway voice_group merged group.

pub mod voice {
    //! Voice edge for the mobile app: audio in → transcript out, text in →
    //! audio out.
    //!
    //! This is the gateway's network edge for speech. The phone app posts
    //! audio here and gets a transcript back to send as a normal message,
    //! or posts text and gets playable audio bytes. Voice is a *channel*
    //! capability, never an agent tool: nothing in this module touches the
    //! model, the ledger, or tool calls.
    //!
    //! Wire shape (base64 JSON, matching the `/agui` surface's JSON bodies
    //! multipart would not fit the transport, which hands handlers a body
    //! capped at 1 MiB except the voice transcribe path at 12 MiB):
    //!
    //! - `POST /agui/voice/transcribe`
    //!   `{ "audio": "<base64>", "language": "en"?, "prompt": "..." ? }`
    //!   → `200 { "transcript": "...", "backend": "groq" }`
    //! - `POST /agui/voice/speak`
    //!   `{ "text": "...", "voice": "..."?, "format": "wav|mp3|ogg"? }`
    //!   → `200` raw audio bytes, `Content-Type: audio/wav` (etc.)
    //!
    //! Hygiene, enforced here rather than left to callers:
    //!
    //! - The `[tools] voice` group toggle gates the whole surface (the same
    //!   double gate the browser and the channel voice pipes use): toggle
    //!   off = the backend is never constructed and requests name the toggle.
    //! - Audio lives only in a temp file that deletes itself on drop - every
    //!   exit path, including the request-timeout path (the worker thread
    //!   owns the guard and drops it when it finishes).
    //! - Decoded audio is capped at [`MAX_AUDIO_BYTES`]; each backend call is
    //!   bounded by the edge's request timeout ([`DEFAULT_REQUEST_TIMEOUT`]).
    //! - Error bodies never carry key material: provider errors name env
    //!   vars, never values, and option maps are never echoed. This module
    //!   does no logging at all.

    use pantheon_api::config::{ToolGroup, ToolsSection, VoiceSection};
    use pantheon_api::error::PantheonError;
    use pantheon_providers::voice::{
        b64decode, stt_from_config, tts_from_config, AudioFormat, SttProvider, SttRequest,
        TtsProvider, TtsRequest,
    };
    use pantheon_secrets::SecretsBroker;
    use serde::Deserialize;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    /// Route paths, shared with the serve dispatcher so the two cannot drift.
    pub const TRANSCRIBE_PATH: &str = "/agui/voice/transcribe";
    pub const SPEAK_PATH: &str = "/agui/voice/speak";

    /// Decoded-audio cap: 8 MiB ≈ several minutes of voice audio at phone
    /// quality (the mobile app caps voice notes at 3 min ≈ 5.6 MB at
    /// 32 KB/s). The HTTP transport allows larger wire bodies on the
    /// transcribe path (see [`MAX_VOICE_BODY_BYTES`]); this caps what one
    /// request can make us buffer and hand to a backend.
    pub const MAX_AUDIO_BYTES: usize = 25 * 1024 * 1024;

    /// Wire-body cap for `POST /agui/voice/transcribe`: 12 MiB. The audio
    /// rides as base64 inside JSON, so 8 MiB of audio is ~10.7 MiB on the
    /// wire; the extra headroom covers the JSON envelope. Every other
    /// route keeps the transport's 1 MiB [`crate::server::http::MAX_BODY`] guard.
    pub const MAX_VOICE_BODY_BYTES: usize = 32 * 1024 * 1024;

    /// Bound on one transcribe/synthesize call. Backends carry their own
    /// (shorter) bounds; this is the edge's guarantee to the HTTP client.
    pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

    /// Synthesis input cap: 32 KiB of text is ~30 minutes of spoken audio.
    /// Anything bigger is abuse, not speech.
    pub const MAX_TEXT_CHARS: usize = 64 * 1024;

    /// `POST /agui/voice/transcribe` body.
    #[derive(Debug, Deserialize)]
    pub struct TranscribeBody {
        /// Base64 audio (wav/mp3/ogg/flac - whatever the backend accepts).
        pub audio: String,
        /// ISO language hint, when known.
        #[serde(default)]
        pub language: Option<String>,
        /// Context hint for the transcription, when known.
        #[serde(default)]
        pub prompt: Option<String>,
    }

    /// `POST /agui/voice/speak` body.
    #[derive(Debug, Deserialize)]
    pub struct SpeakBody {
        pub text: String,
        /// Backend-specific voice id; `None` = the `[tts]` default voice.
        #[serde(default)]
        pub voice: Option<String>,
        /// `wav` (default), `mp3`, or `ogg`.
        #[serde(default)]
        pub format: Option<String>,
    }

    /// What the edge hands back to the HTTP transport: status, content type,
    /// and the exact response bytes.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct VoiceHttp {
        pub status: u16,
        pub content_type: &'static str,
        pub body: Vec<u8>,
    }

    impl VoiceHttp {
        pub fn json(status: u16, v: serde_json::Value) -> Self {
            Self {
                status,
                content_type: "application/json",
                body: serde_json::to_string(&v)
                    .unwrap_or_else(|_| "{}".into())
                    .into_bytes(),
            }
        }

        /// `{"error":{"code":...,"message":...}}` - the shape every voice error
        /// takes, so clients can switch on `code` without parsing prose.
        pub fn err(status: u16, code: &'static str, message: impl Into<String>) -> Self {
            Self::json(
                status,
                serde_json::json!({"error": {"code": code, "message": message.into()}}),
            )
        }
    }

    /// Content-Type for synthesized audio, from what the backend produced.
    pub fn content_type_for(format: AudioFormat) -> &'static str {
        match format {
            AudioFormat::Wav => "audio/wav",
            AudioFormat::Mp3 => "audio/mpeg",
            AudioFormat::Ogg => "audio/ogg",
        }
    }

    /// One configured direction: absent (no config section), broken (section
    /// present but the backend would not construct - surfaced per request),
    /// or ready.
    enum Backend<T: ?Sized> {
        Unset,
        Broken(String),
        Ready(Arc<T>),
    }

    impl<T: ?Sized> Clone for Backend<T> {
        fn clone(&self) -> Self {
            match self {
                Backend::Unset => Backend::Unset,
                Backend::Broken(m) => Backend::Broken(m.clone()),
                Backend::Ready(p) => Backend::Ready(p.clone()),
            }
        }
    }

    /// The voice edge: configured backends plus the request bound. `Clone`
    /// is cheap (backends are shared); the HTTP server clones one per
    /// connection.
    #[derive(Clone)]
    pub struct VoiceEdge {
        stt: Backend<dyn SttProvider>,
        tts: Backend<dyn TtsProvider>,
        request_timeout: Duration,
        /// The `[tools] voice` group toggle is off: both slots stay unset and
        /// requests name the toggle rather than a missing section. This is the
        /// same double gate the browser and the channel voice pipes use
        /// (`[tools] voice` AND the `[stt]`/`[tts]` section must both allow).
        voice_disabled: bool,
    }

    impl std::fmt::Debug for VoiceEdge {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            fn state<T: ?Sized>(b: &Backend<T>, name: &dyn Fn(&T) -> &str) -> String {
                match b {
                    Backend::Unset => "unset".to_string(),
                    Backend::Broken(m) => format!("broken: {m}"),
                    Backend::Ready(p) => format!("ready({})", name(p)),
                }
            }
            f.debug_struct("VoiceEdge")
                .field("stt", &state(&self.stt, &|p| p.name()))
                .field("tts", &state(&self.tts, &|p| p.name()))
                .field("request_timeout", &self.request_timeout)
                .field("voice_disabled", &self.voice_disabled)
                .finish()
        }
    }

    fn build<T: ?Sized>(
        section: Option<&VoiceSection>,
        open: impl FnOnce(&VoiceSection) -> Option<Result<Box<T>, PantheonError>>,
    ) -> Backend<T> {
        match section {
            None => Backend::Unset,
            Some(s) => match open(s) {
                Some(Ok(b)) => Backend::Ready(Arc::from(b)),
                // Code only: `cause` can carry paths or option text, so it
                // never leaves this module. The 500 below pairs the code
                // with static remediation.
                Some(Err(e)) => Backend::Broken(e.code.clone()),
                // `open` never returns None for `Some(section)` today; stay
                // total rather than panicking if that changes.
                None => Backend::Unset,
            },
        }
    }

    impl VoiceEdge {
        /// Build from the `[tools]` toggle plus the `[stt]` / `[tts]` config
        /// sections. Construction is eager so a misconfigured backend fails
        /// identically on every request instead of sometimes working. Toggle
        /// off (or absent sections) = the backend is never constructed.
        pub fn from_config(
            tools: Option<&ToolsSection>,
            stt: Option<&VoiceSection>,
            tts: Option<&VoiceSection>,
            secrets: &SecretsBroker,
        ) -> Self {
            let group_on = tools
                .map(|t| t.is_enabled(ToolGroup::Voice))
                .unwrap_or(true);
            let (stt, tts) = if group_on {
                (
                    build(stt, |s| stt_from_config(Some(s), secrets)),
                    build(tts, |s| tts_from_config(Some(s), secrets)),
                )
            } else {
                (Backend::Unset, Backend::Unset)
            };
            Self {
                stt,
                tts,
                request_timeout: DEFAULT_REQUEST_TIMEOUT,
                voice_disabled: !group_on,
            }
        }

        /// Neither direction configured: every request 400s naming the
        /// missing section.
        pub fn unconfigured() -> Self {
            Self {
                stt: Backend::Unset,
                tts: Backend::Unset,
                request_timeout: DEFAULT_REQUEST_TIMEOUT,
                voice_disabled: false,
            }
        }

        /// Inject pre-built backends (tests, embedders). `None` = that
        /// direction is unconfigured.
        pub fn with_backends(
            stt: Option<Arc<dyn SttProvider>>,
            tts: Option<Arc<dyn TtsProvider>>,
        ) -> Self {
            Self {
                stt: stt.map_or(Backend::Unset, Backend::Ready),
                tts: tts.map_or(Backend::Unset, Backend::Ready),
                request_timeout: DEFAULT_REQUEST_TIMEOUT,
                voice_disabled: false,
            }
        }

        /// Override the per-request bound (tests use a short one).
        pub fn with_timeout(mut self, timeout: Duration) -> Self {
            self.request_timeout = timeout;
            self
        }

        /// The 400 for "no usable backend here": names the Tools-screen
        /// toggle when that is what turned voice off, otherwise the missing
        /// config section.
        fn not_configured(&self, section: &str) -> VoiceHttp {
            let message = if self.voice_disabled {
                "voice is disabled on the Tools screen ([tools] voice = false); \
             turn the Voice group back on to use speech"
                    .to_string()
            } else {
                format!(
                    "{section} is not configured: add a [{section}] section with a \
                 backend to config.toml (run `pantheon setup` and pick a speech provider)"
                )
            };
            VoiceHttp::err(400, "voice_not_configured", message)
        }

        /// Handle `POST /agui/voice/transcribe`: base64 audio → transcript.
        pub fn handle_transcribe(&self, raw: &[u8]) -> VoiceHttp {
            let body: TranscribeBody = match serde_json::from_slice(raw) {
                Ok(b) => b,
                Err(e) => {
                    return VoiceHttp::err(
                        400,
                        "voice_bad_request",
                        format!("invalid JSON body: {e}"),
                    )
                }
            };
            if body.audio.trim().is_empty() {
                return VoiceHttp::err(
                    400,
                    "voice_bad_request",
                    "field `audio` (base64) is required",
                );
            }
            let bytes = match b64decode(&body.audio) {
                Ok(b) => b,
                Err(e) => {
                    return VoiceHttp::err(
                        400,
                        "voice_bad_request",
                        format!("field `audio` is not valid base64: {e}"),
                    )
                }
            };
            if bytes.is_empty() {
                return VoiceHttp::err(400, "voice_bad_request", "field `audio` decoded to empty");
            }
            if bytes.len()
                > pantheon_api::config::limit_or_env("voice_audio_bytes", None, MAX_AUDIO_BYTES)
            {
                return VoiceHttp::err(
                    413,
                    "voice_audio_too_large",
                    format!(
                        "audio is {} bytes; the limit is {MAX_AUDIO_BYTES} bytes",
                        bytes.len()
                    ),
                );
            }
            let provider = match &self.stt {
                Backend::Unset => return self.not_configured("stt"),
                Backend::Broken(msg) => {
                    return VoiceHttp::err(
                        500,
                        "voice_backend_misconfigured",
                        format!(
                            "stt backend is misconfigured ({msg}): check the [stt] \
                         section in config.toml or re-run `pantheon setup`"
                        ),
                    )
                }
                Backend::Ready(p) => p.clone(),
            };
            let tmp = match TempAudio::write(&bytes) {
                Ok(t) => t,
                Err(e) => {
                    return VoiceHttp::err(
                        500,
                        "voice_temp_file",
                        format!("could not stage audio for transcription: {e}"),
                    )
                }
            };
            let mut req = SttRequest::new(tmp.path());
            if let Some(l) = body.language.filter(|s| !s.trim().is_empty()) {
                req = req.with_language(l);
            }
            if let Some(p) = body.prompt.filter(|s| !s.trim().is_empty()) {
                req.prompt = Some(p);
            }
            let name = provider.name().to_string();
            // The temp file moves into the worker: it is deleted when the
            // worker finishes, even if we stop waiting first.
            match bounded(self.request_timeout, move || {
                let _keep = tmp;
                provider.transcribe(&req)
            }) {
                Ok(Ok(res)) => VoiceHttp::json(
                    200,
                    serde_json::json!({"transcript": res.text, "backend": name}),
                ),
                Ok(Err(e)) => VoiceHttp::err(
                    502,
                    "voice_transcribe_failed",
                    format!("stt backend {name} failed ({}): {}", e.code, e.cause),
                ),
                Err(()) => VoiceHttp::err(
                    504,
                    "voice_timeout",
                    format!(
                        "stt backend {name} did not answer within {}s",
                        self.request_timeout.as_secs()
                    ),
                ),
            }
        }

        /// Handle `POST /agui/voice/speak`: text → audio bytes.
        pub fn handle_speak(&self, raw: &[u8]) -> VoiceHttp {
            let body: SpeakBody = match serde_json::from_slice(raw) {
                Ok(b) => b,
                Err(e) => {
                    return VoiceHttp::err(
                        400,
                        "voice_bad_request",
                        format!("invalid JSON body: {e}"),
                    )
                }
            };
            let text = body.text.trim();
            if text.is_empty() {
                return VoiceHttp::err(400, "voice_bad_request", "field `text` is required");
            }
            if text.chars().count()
                > pantheon_api::config::limit_or_env("voice_text_chars", None, MAX_TEXT_CHARS)
            {
                return VoiceHttp::err(
                    413,
                    "voice_text_too_large",
                    format!(
                        "text is {} chars; the limit is {MAX_TEXT_CHARS} chars",
                        text.chars().count()
                    ),
                );
            }
            let format = match body.format.as_deref() {
                None | Some("wav") => AudioFormat::Wav,
                Some("mp3") => AudioFormat::Mp3,
                Some("ogg") => AudioFormat::Ogg,
                Some(other) => {
                    return VoiceHttp::err(
                        400,
                        "voice_bad_request",
                        format!("unknown format {other:?}; want \"wav\", \"mp3\", or \"ogg\""),
                    )
                }
            };
            let provider = match &self.tts {
                Backend::Unset => return self.not_configured("tts"),
                Backend::Broken(msg) => {
                    return VoiceHttp::err(
                        500,
                        "voice_backend_misconfigured",
                        format!(
                            "tts backend is misconfigured ({msg}): check the [tts] \
                         section in config.toml or re-run `pantheon setup`"
                        ),
                    )
                }
                Backend::Ready(p) => p.clone(),
            };
            let mut req = TtsRequest::new(text);
            req.format = format;
            if let Some(v) = body.voice.filter(|s| !s.trim().is_empty()) {
                req.voice = Some(v);
            }
            let name = provider.name().to_string();
            match bounded(self.request_timeout, move || provider.synthesize(&req)) {
                Ok(Ok(res)) => VoiceHttp {
                    status: 200,
                    content_type: content_type_for(res.format),
                    body: res.bytes,
                },
                Ok(Err(e)) => VoiceHttp::err(
                    502,
                    "voice_synthesize_failed",
                    format!("tts backend {name} failed ({}): {}", e.code, e.cause),
                ),
                Err(()) => VoiceHttp::err(
                    504,
                    "voice_timeout",
                    format!(
                        "tts backend {name} did not answer within {}s",
                        self.request_timeout.as_secs()
                    ),
                ),
            }
        }
    }

    /// Run `f` on a worker thread, giving up after `timeout`. The worker owns
    /// its resources (the temp audio guard) and cleans them up when it
    /// finishes, even after the waiter has moved on.
    fn bounded<T: Send + 'static>(
        timeout: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, ()> {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(timeout).map_err(|_| ())
    }

    /// A staged audio file that deletes itself on drop - every exit path
    /// cleans up, including the timeout path (the worker owns the guard).
    struct TempAudio {
        path: PathBuf,
    }

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    impl TempAudio {
        fn write(bytes: &[u8]) -> std::io::Result<Self> {
            let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("pantheon-agui-voice-{}-{n}", std::process::id()));
            std::fs::write(&path, bytes)?;
            Ok(Self { path })
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempAudio {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
pub mod live_voice {
    //! Live voice mode: `GET /agui/voice/live` WebSocket sessions for the
    //! mobile client (docs/live-voice-mode.md).
    //!
    //! Wire protocol (16 kHz mono 16-bit PCM both ways):
    //!
    //! - Client → server: text `{"type":"start"}` begins an utterance, binary
    //!   frames carry PCM chunks, text `{"type":"end"}` closes the utterance
    //!   for transcription, text `{"type":"stop"}` ends the session.
    //! - Server → client: `ready` once the session is accepted; `transcript`
    //!   (STT result, `final: true`); `reply_text` (the agent's reply as text,
    //!   always sent); binary PCM chunks of the spoken reply; `audio_end`;
    //!   `busy` when audio arrives while a turn is in flight (dropped, never
    //!   queued - no barge-in in v1); `approval_needed` when a tool approval
    //!   parks the turn (never auto-approved); `error` with a machine-readable
    //!   `code`; `end` when the server closes the session.
    //!
    //! Turn-taking is client-driven with a server-side safety net: an
    //! energy-based VAD auto-closes the utterance after `live_silence_timeout_ms`
    //! of silence once speech was seen, and `live_max_utterance_secs`
    //! force-closes runaway utterances.
    //!
    //! The STT/TTS backends come from the same double gate as the channel
    //! voice pipes (`[tools] voice` AND `[stt]`/`[tts]` present and
    //! constructible); the session is refused up front when the gate fails.
    //! The agent turn itself is driven by [`LiveTurnDriver`], implemented in
    //! `pantheon-runtime` (the same `agui.send` dispatcher path `/agui/rpc`
    //! uses), so this crate stays free of session/dispatcher types.

    use crate::voice_group::channel_voice::{VoicePipes, VoiceSlot};
    use pantheon_api::config::{LiveVoiceSection, ToolsSection, VoiceSection};
    use pantheon_providers::voice::{AudioFormat, SttProvider, SttRequest};
    use pantheon_secrets::SecretsBroker;
    use std::io::{ErrorKind, Read, Write};
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::{Duration, Instant};
    use tungstenite::{Message, WebSocket};

    /// Route served by `pantheon-runtime/src/serve.rs`.
    pub const LIVE_VOICE_PATH: &str = "/agui/voice/live";
    /// Marks a live transcript so the agent knows it came from speech, not
    /// typed text (mirrors the channel `[voice message]` prefix).
    pub const LIVE_TRANSCRIPT_PREFIX: &str = "[live voice message] ";
    /// Wire audio format, both directions.
    pub const SAMPLE_RATE: u32 = 16_000;

    /// One server→client audio chunk: ~100 ms of 16 kHz mono 16-bit PCM.
    const CHUNK_BYTES: usize = 3200;
    /// Sanity cap per inbound binary frame (2 s of audio); larger frames are
    /// rejected, not buffered.
    const MAX_CHUNK_BYTES: usize = 64 * 1024;
    /// Energy VAD: a chunk whose RMS clears this (16-bit sample units) counts
    /// as speech. Quiet-room noise sits well under 100; normal speech clears
    /// 1000 easily.
    const SPEECH_RMS_THRESHOLD: f32 = 400.0;
    /// Poll cadence of the session loop: socket reads, worker results, VAD
    /// deadlines.
    const POLL_INTERVAL: Duration = Duration::from_millis(20);

    /// Process-wide live-voice state: `[voice]` limits plus the double-gated
    /// STT/TTS pipes. Built once; shared by every session.
    pub struct LiveVoiceConfig {
        /// Limits from `[voice]` (`Config::live_voice()`).
        pub limits: LiveVoiceSection,
        /// STT/TTS backends behind the `[tools] voice` + `[stt]`/`[tts]`
        /// double gate. Unused when `realtime` is active.
        pub pipes: VoicePipes,
        /// Optional provider-native realtime adapter. `None` keeps the local
        /// STT → agent → TTS pipeline.
        pub realtime: Option<crate::voice_group::realtime_voice::RealtimeConfig>,
    }

    impl std::fmt::Debug for LiveVoiceConfig {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // VoicePipes holds backend trait objects; summarize secret-free.
            f.debug_struct("LiveVoiceConfig")
                .field("limits", &self.limits)
                .field("pipes", &self.pipes.describe())
                .field("realtime", &self.realtime)
                .finish()
        }
    }

    impl LiveVoiceConfig {
        /// Build from the `[tools]` toggle, the `[stt]`/`[tts]` sections, and
        /// the `[voice]` limits. Mirrors `VoicePipes::from_config` gating.
        pub fn from_config(
            tools: Option<&ToolsSection>,
            stt: Option<&VoiceSection>,
            tts: Option<&VoiceSection>,
            limits: LiveVoiceSection,
            secrets: &SecretsBroker,
        ) -> Self {
            let realtime =
                crate::voice_group::realtime_voice::resolve_openai_key(Some(&limits), secrets).map(
                    |key| {
                        crate::voice_group::realtime_voice::RealtimeConfig::openai(
                            limits.realtime_model.as_deref(),
                            limits.realtime_voice.as_deref(),
                            key,
                        )
                    },
                );
            Self {
                limits,
                pipes: VoicePipes::from_config(tools, stt, tts, secrets, false),
                realtime,
            }
        }

        /// Live voice unavailable: every session is refused up front.
        pub fn disabled() -> Self {
            Self {
                limits: LiveVoiceSection::default(),
                pipes: VoicePipes::disabled(),
                realtime: None,
            }
        }

        /// Up-front gate: the session is refused unless `[voice] live_enabled`
        /// is true AND both backends constructed. `Err` is the machine-readable
        /// `error` code sent before `end` - never key material.
        pub fn gate(&self) -> Result<(), &'static str> {
            if !self.limits.live_enabled {
                return Err("live_disabled");
            }
            if self.realtime.is_some() {
                return Ok(());
            }
            fn ready<T: ?Sized>(slot: &VoiceSlot<Box<T>>) -> Result<(), &'static str> {
                match slot {
                    VoiceSlot::Ready(_) => Ok(()),
                    VoiceSlot::Disabled => Err("voice_not_configured"),
                    VoiceSlot::Unavailable(_) => Err("voice_backend_misconfigured"),
                }
            }
            ready(&self.pipes.stt)?;
            ready(&self.pipes.tts)?;
            Ok(())
        }
    }

    /// What the agent turn produced. Built by the serve layer
    /// (`pantheon-runtime`), which owns the dispatcher path.
    #[derive(Debug, Clone)]
    pub struct TurnOutcome {
        /// The agent's reply text (spoken back when non-empty).
        pub reply_text: String,
        /// Set when the run parked on a tool approval: the scope text for the
        /// `approval_needed` event. The turn pauses; nothing is auto-approved.
        pub approval: Option<String>,
        /// Set when the turn failed: machine-readable code for `error`.
        pub error: Option<String>,
    }

    impl TurnOutcome {
        pub fn answered(text: String) -> Self {
            Self {
                reply_text: text,
                approval: None,
                error: None,
            }
        }
        pub fn approval_needed(scope: String) -> Self {
            Self {
                reply_text: String::new(),
                approval: Some(scope),
                error: None,
            }
        }
        pub fn failed(code: impl Into<String>) -> Self {
            Self {
                reply_text: String::new(),
                approval: None,
                error: Some(code.into()),
            }
        }
    }

    /// Drives one agent turn for a live transcript. Implemented by the serve
    /// layer via the same `agui.send` dispatcher path `/agui/rpc` chat uses,
    /// so live turns behave exactly like text turns (approvals park, never
    /// auto-approve). Runs on a worker thread; may block.
    pub trait LiveTurnDriver: Send + Sync {
        fn run_turn(&self, transcript: &str) -> TurnOutcome;
    }

    /// RMS energy of 16-bit PCM samples. 0.0 for empty input.
    pub fn pcm_rms(samples: &[i16]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum: f64 = samples
            .iter()
            .map(|&s| {
                let v = s as f64;
                v * v
            })
            .sum();
        (sum / samples.len() as f64).sqrt() as f32
    }

    /// Energy VAD: speech iff the chunk's RMS clears the threshold.
    pub fn is_speech(samples: &[i16]) -> bool {
        pcm_rms(samples) > SPEECH_RMS_THRESHOLD
    }

    /// Wrap 16 kHz mono 16-bit PCM in a 44-byte WAV header. STT backends take
    /// a file path and several sniff the container, so raw PCM goes out with
    /// a header it can parse.
    pub fn wav_wrap(pcm: &[i16]) -> Vec<u8> {
        let data_len = (pcm.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + pcm.len() * 2);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        out.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // byte rate
        out.extend_from_slice(&2u16.to_le_bytes()); // block align
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in pcm {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    /// Decode a WAV file's PCM data to 16 kHz mono 16-bit samples: downmixes
    /// channels and resamples when the backend did not produce 16 kHz mono.
    /// `Err` is a machine-readable code (`wav_*`), never file content.
    pub fn wav_to_pcm_16k(bytes: &[u8]) -> Result<Vec<i16>, &'static str> {
        if bytes.len() < 44 {
            return Err("wav_too_small");
        }
        if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
            return Err("wav_bad_header");
        }
        let mut channels = 0u16;
        let mut rate = 0u32;
        let mut bits = 0u16;
        let mut fmt_seen = false;
        let mut data: &[u8] = &[];
        let mut pos = 12usize;
        while pos + 8 <= bytes.len() {
            let id = &bytes[pos..pos + 4];
            let size = u32::from_le_bytes(
                bytes[pos + 4..pos + 8]
                    .try_into()
                    .map_err(|_| "wav_bad_fmt")?,
            ) as usize;
            let body = pos + 8;
            let end = body.saturating_add(size).min(bytes.len());
            if id == b"fmt " {
                if end - body < 16 {
                    return Err("wav_bad_fmt");
                }
                let audio_fmt = u16::from_le_bytes(
                    bytes[body..body + 2]
                        .try_into()
                        .map_err(|_| "wav_bad_fmt")?,
                );
                if audio_fmt != 1 {
                    return Err("wav_unsupported"); // PCM only, no float/ADPCM
                }
                channels = u16::from_le_bytes(
                    bytes[body + 2..body + 4]
                        .try_into()
                        .map_err(|_| "wav_bad_fmt")?,
                );
                rate = u32::from_le_bytes(
                    bytes[body + 4..body + 8]
                        .try_into()
                        .map_err(|_| "wav_bad_fmt")?,
                );
                bits = u16::from_le_bytes(
                    bytes[body + 14..body + 16]
                        .try_into()
                        .map_err(|_| "wav_bad_fmt")?,
                );
                if bits != 8 && bits != 16 {
                    return Err("wav_unsupported");
                }
                if channels == 0 || rate == 0 {
                    return Err("wav_bad_fmt");
                }
                fmt_seen = true;
            } else if id == b"data" {
                data = &bytes[body..end];
            }
            // Chunks are word-aligned: odd sizes carry a pad byte.
            pos = end + (size & 1);
        }
        if !fmt_seen || data.is_empty() {
            return Err("wav_bad_fmt");
        }
        let bytes_per_sample = (bits / 8) as usize;
        let frame_bytes = bytes_per_sample * channels as usize;
        let mut mono = Vec::with_capacity(data.len() / frame_bytes);
        for frame in data.chunks_exact(frame_bytes) {
            let mut acc: i32 = 0;
            for ch in 0..channels as usize {
                let off = ch * bytes_per_sample;
                let s: i16 = if bits == 16 {
                    i16::from_le_bytes(frame[off..off + 2].try_into().map_err(|_| "wav_bad_fmt")?)
                } else {
                    ((frame[off] as i16) - 128) * 256
                };
                acc += s as i32;
            }
            mono.push((acc / channels as i32) as i16);
        }
        // Bound pathological headers (rate=1 would 16000x the sample count).
        let out_len = (mono.len() as u64 * u64::from(SAMPLE_RATE)) / u64::from(rate);
        if out_len > u64::from(SAMPLE_RATE) * 900 {
            return Err("wav_too_large");
        }
        Ok(resample_linear(&mono, rate, SAMPLE_RATE))
    }

    /// Linear-interpolation resampler. Pure; the VAD/WAV unit tests pin it.
    fn resample_linear(samples: &[i16], from_rate: u32, to_rate: u32) -> Vec<i16> {
        if samples.is_empty() || from_rate == 0 || to_rate == 0 || from_rate == to_rate {
            return samples.to_vec();
        }
        let out_len = ((samples.len() as u64 * u64::from(to_rate)) / u64::from(from_rate)) as usize;
        let mut out = Vec::with_capacity(out_len);
        for i in 0..out_len {
            let pos = i as f64 * f64::from(from_rate) / f64::from(to_rate);
            let i0 = pos.floor() as usize;
            let i1 = (i0 + 1).min(samples.len() - 1);
            let frac = (pos - i0 as f64) as f32;
            let v = samples[i0] as f32 * (1.0 - frac) + samples[i1] as f32 * frac;
            out.push(v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16);
        }
        out
    }

    /// Staged utterance WAV. `Drop` removes the file on every path - STT
    /// errors, aborts, and client disconnects included.
    struct TempWav {
        path: PathBuf,
    }

    impl TempWav {
        fn stage(pcm: &[i16]) -> std::io::Result<Self> {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "pantheon-live-{}-{}.wav",
                std::process::id(),
                nanos()
            ));
            std::fs::write(&path, wav_wrap(pcm))?;
            Ok(Self { path })
        }
    }

    impl Drop for TempWav {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    /// Replays the already-consumed HTTP request head, then delegates to the
    /// socket, so `tungstenite::accept` can parse the WS handshake itself even
    /// though the HTTP layer already read the request line and headers.
    pub(crate) struct HeadReplay {
        head: Vec<u8>,
        pos: usize,
        stream: TcpStream,
    }

    impl HeadReplay {
        pub(crate) fn stream_mut(&mut self) -> &mut TcpStream {
            &mut self.stream
        }
    }

    impl Read for HeadReplay {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos < self.head.len() {
                let n = (self.head.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            self.stream.read(buf)
        }
    }

    impl Write for HeadReplay {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.stream.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.stream.flush()
        }
    }

    /// Accept the WS handshake on `stream` (the HTTP layer already consumed
    /// the request head; `head` is the rebuilt raw head) and run the live
    /// session to completion: `ready`, then the turn loop until `stop`,
    /// client disconnect, or a limit fires.
    pub fn serve_live_session(
        stream: TcpStream,
        head: Vec<u8>,
        cfg: Arc<LiveVoiceConfig>,
        turn: Arc<dyn LiveTurnDriver>,
    ) {
        let replay = HeadReplay {
            head,
            pos: 0,
            stream,
        };
        let mut ws = match tungstenite::accept(replay) {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("live voice: websocket handshake failed: {e}");
                return;
            }
        };
        if let Err(code) = cfg.gate() {
            // Refused up front, through the protocol: error, then end.
            send_msg(
                &mut ws,
                json_text(&serde_json::json!({"type": "error", "code": code})),
            );
            send_msg(&mut ws, Message::text(r#"{"type":"end"}"#));
            let _ = ws.close(None);
            return;
        }
        if let Some(realtime) = cfg.realtime.clone() {
            // Provider-native duplex path: the provider owns turn detection
            // and speech generation. The local pipeline remains the fallback
            // when no realtime key/backend is configured.
            crate::voice_group::realtime_voice::run_session(&mut ws, realtime, cfg.limits.clone());
            return;
        }
        send_msg(&mut ws, Message::text(r#"{"type":"ready"}"#));
        // The session loop polls; the socket goes nonblocking and short
        // WouldBlock stalls are retried inside `send_msg`.
        if let Err(e) = ws.get_mut().stream_mut().set_nonblocking(true) {
            eprintln!("live voice: set_nonblocking failed: {e}");
            return;
        }
        Runner {
            ws,
            cfg,
            turn,
            abort: Arc::new(AtomicBool::new(false)),
            phase: Phase::Idle,
            worker_rx: None,
            session_start: Instant::now(),
        }
        .run();
    }

    pub(crate) fn json_text(v: &serde_json::Value) -> Message {
        Message::text(v.to_string())
    }

    /// Send with a short WouldBlock retry: the socket is nonblocking and a
    /// full send buffer must not silently drop a reply audio chunk.
    pub(crate) fn send_msg(ws: &mut WebSocket<HeadReplay>, msg: Message) {
        for _ in 0..50 {
            match ws.send(msg.clone()) {
                Ok(()) => return,
                Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(_) => return,
            }
        }
    }

    enum Phase {
        Idle,
        Utterance(OpenUtterance),
        /// STT → agent turn → TTS running on the worker thread. Inbound audio
        /// is dropped (a `busy` answers `start`); no barge-in in v1.
        InFlight,
    }

    struct OpenUtterance {
        pcm: Vec<i16>,
        speech_seen: bool,
        last_voice: Instant,
        started: Instant,
    }

    /// Worker → session-loop messages, in send order.
    enum WorkerMsg {
        Transcript(String),
        ReplyText(String),
        AudioChunk(Vec<u8>),
        AudioEnd,
        ApprovalNeeded(String),
        /// Machine-readable code for the `error` frame.
        Error(String),
        /// Terminal: the turn is over, the session returns to idle.
        TurnDone,
    }

    struct Runner {
        ws: WebSocket<HeadReplay>,
        cfg: Arc<LiveVoiceConfig>,
        turn: Arc<dyn LiveTurnDriver>,
        abort: Arc<AtomicBool>,
        phase: Phase,
        worker_rx: Option<mpsc::Receiver<WorkerMsg>>,
        session_start: Instant,
    }

    impl Runner {
        fn run(mut self) {
            let limits = self.cfg.limits.clone();
            let session_cap = Duration::from_secs(limits.live_max_session_secs.max(1));
            let utterance_cap = Duration::from_secs(limits.live_max_utterance_secs.max(1));
            let silence_cap = Duration::from_millis(limits.live_silence_timeout_ms.max(100));
            let utterance_samples =
                limits.live_max_utterance_secs.max(1) as usize * SAMPLE_RATE as usize;

            'run: loop {
                match self.ws.read() {
                    Ok(Message::Text(t)) => {
                        if !self.on_text(t.as_str()) {
                            break 'run;
                        }
                    }
                    Ok(Message::Binary(b)) => self.on_binary(&b, utterance_samples),
                    Ok(Message::Ping(p)) => send_msg(&mut self.ws, Message::Pong(p)),
                    Ok(Message::Pong(_)) | Ok(Message::Frame(_)) => {}
                    Ok(Message::Close(_)) => break 'run,
                    Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(e) => {
                        eprintln!("live voice: read error: {e}");
                        break 'run;
                    }
                }
                self.drain_worker();
                if self.session_start.elapsed() >= session_cap {
                    self.send_text(r#"{"type":"end"}"#);
                    break 'run;
                }
                // Server-side VAD safety net: silence after speech auto-closes;
                // runaway utterances are force-closed.
                let auto_close = match &self.phase {
                    Phase::Utterance(u) => {
                        (u.speech_seen && u.last_voice.elapsed() >= silence_cap)
                            || u.started.elapsed() >= utterance_cap
                    }
                    _ => false,
                };
                if auto_close {
                    self.close_utterance();
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            // Every path out of the loop abandons in-flight work: the worker's
            // sends fail on the dropped receiver and its TempWav guard cleans up.
            self.abort.store(true, Ordering::SeqCst);
            let _ = self.ws.close(None);
        }

        /// Handle one client text frame. Returns false when the session ends.
        fn on_text(&mut self, t: &str) -> bool {
            let v: serde_json::Value = match serde_json::from_str(t) {
                Ok(v) => v,
                Err(_) => {
                    self.send_error("bad_frame");
                    return true;
                }
            };
            match v.get("type").and_then(|v| v.as_str()) {
                Some("start") => match self.phase {
                    Phase::Idle => {
                        let now = Instant::now();
                        self.phase = Phase::Utterance(OpenUtterance {
                            pcm: Vec::new(),
                            speech_seen: false,
                            last_voice: now,
                            started: now,
                        });
                    }
                    Phase::Utterance(_) => {}
                    Phase::InFlight => self.send_text(r#"{"type":"busy"}"#),
                },
                Some("end") => {
                    if matches!(self.phase, Phase::Utterance(_)) {
                        self.close_utterance();
                    }
                }
                Some("stop") => {
                    self.send_text(r#"{"type":"end"}"#);
                    return false;
                }
                _ => self.send_error("unknown_frame"),
            }
            true
        }

        /// Handle one client binary frame: 16-bit LE PCM samples appended to
        /// the open utterance. Binary outside an utterance is dropped; binary
        /// while a turn is in flight is dropped with a `busy` event.
        fn on_binary(&mut self, b: &[u8], utterance_samples: usize) {
            if matches!(self.phase, Phase::InFlight) {
                self.send_text(r#"{"type":"busy"}"#);
                return;
            }
            if !matches!(self.phase, Phase::Utterance(_)) {
                return;
            }
            if b.len() > MAX_CHUNK_BYTES {
                self.send_error("chunk_too_large");
                return;
            }
            let mut samples = Vec::with_capacity(b.len() / 2);
            for pair in b.as_chunks::<2>().0 {
                samples.push(i16::from_le_bytes(*pair));
            }
            let speech = is_speech(&samples);
            let mut force_close = false;
            if let Phase::Utterance(u) = &mut self.phase {
                if speech {
                    u.speech_seen = true;
                    u.last_voice = Instant::now();
                }
                u.pcm.extend_from_slice(&samples);
                if u.pcm.len() >= utterance_samples {
                    u.pcm.truncate(utterance_samples);
                    force_close = true;
                }
            }
            if force_close {
                self.close_utterance();
            }
        }

        /// Close the open utterance and run STT → turn → TTS on a worker
        /// thread; the loop keeps serving the socket (busy-drop, stop).
        fn close_utterance(&mut self) {
            let utterance = match std::mem::replace(&mut self.phase, Phase::InFlight) {
                Phase::Utterance(u) => u,
                other => {
                    self.phase = other;
                    return;
                }
            };
            let (tx, rx) = mpsc::channel();
            self.worker_rx = Some(rx);
            let cfg = Arc::clone(&self.cfg);
            let turn = Arc::clone(&self.turn);
            let abort = Arc::clone(&self.abort);
            std::thread::spawn(move || {
                run_utterance(
                    utterance.pcm,
                    utterance.speech_seen,
                    &cfg,
                    &turn,
                    &abort,
                    &tx,
                );
            });
        }

        fn drain_worker(&mut self) {
            // Drain first, then forward: holding the receiver borrow across a
            // `send_*` (which borrows `self` mutably) does not compile.
            let mut msgs = Vec::new();
            let mut done = false;
            if let Some(rx) = self.worker_rx.as_ref() {
                while let Ok(msg) = rx.try_recv() {
                    if matches!(msg, WorkerMsg::TurnDone) {
                        done = true;
                    } else {
                        msgs.push(msg);
                    }
                }
            }
            for msg in msgs {
                match msg {
                    WorkerMsg::TurnDone => {}
                    WorkerMsg::Transcript(t) => {
                        self.send_json(
                            &serde_json::json!({"type":"transcript","text":t,"final":true}),
                        );
                    }
                    WorkerMsg::ReplyText(t) => {
                        self.send_json(&serde_json::json!({"type":"reply_text","text":t}));
                    }
                    WorkerMsg::AudioChunk(b) => {
                        send_msg(&mut self.ws, Message::Binary(b.into()));
                    }
                    WorkerMsg::AudioEnd => self.send_text(r#"{"type":"audio_end"}"#),
                    WorkerMsg::ApprovalNeeded(s) => {
                        // The scope is `call_id:tool:args` and can carry
                        // secrets in tool args; redact like every other
                        // surface (dashboard timeline, live UI frames).
                        self.send_json(&serde_json::json!({"type":"approval_needed","text":pantheon_api::logging::redact(&s)}));
                    }
                    WorkerMsg::Error(code) => {
                        self.send_json(&serde_json::json!({"type":"error","code":code}));
                    }
                }
            }
            if done {
                self.worker_rx = None;
                self.phase = Phase::Idle;
            }
        }

        fn send_text(&mut self, t: &str) {
            send_msg(&mut self.ws, Message::text(t));
        }

        fn send_json(&mut self, v: &serde_json::Value) {
            send_msg(&mut self.ws, json_text(v));
        }

        fn send_error(&mut self, code: &str) {
            self.send_json(&serde_json::json!({"type":"error","code":code}));
        }
    }

    /// One utterance, on the worker thread: stage WAV → STT → agent turn →
    /// TTS → PCM chunks. Always ends with `TurnDone`; the temp WAV is removed
    /// by its guard on every return path.
    fn run_utterance(
        pcm: Vec<i16>,
        speech_seen: bool,
        cfg: &LiveVoiceConfig,
        turn: &Arc<dyn LiveTurnDriver>,
        abort: &AtomicBool,
        tx: &mpsc::Sender<WorkerMsg>,
    ) {
        let send = |m: WorkerMsg| {
            let _ = tx.send(m);
        };
        let finish = || {
            let _ = tx.send(WorkerMsg::TurnDone);
        };
        if !speech_seen || abort.load(Ordering::SeqCst) {
            // Nothing but silence was captured: report the empty transcript,
            // no turn.
            send(WorkerMsg::Transcript(String::new()));
            finish();
            return;
        }
        let staged = match TempWav::stage(&pcm) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("live voice: could not stage utterance audio: {e}");
                send(WorkerMsg::Error("stt_stage_failed".to_string()));
                finish();
                return;
            }
        };
        let stt: &dyn SttProvider = match &cfg.pipes.stt {
            VoiceSlot::Ready(b) => b.as_ref(),
            // The gate held at session start; stay total if it ever didn't.
            _ => {
                send(WorkerMsg::Error("voice_not_configured".to_string()));
                finish();
                return;
            }
        };
        if abort.load(Ordering::SeqCst) {
            finish();
            return;
        }
        let text = match stt.transcribe(&SttRequest::new(staged.path.clone())) {
            Ok(r) => r.text,
            Err(e) => {
                eprintln!("live voice: transcription failed [{}]", e.code);
                send(WorkerMsg::Error("stt_failed".to_string()));
                finish();
                return;
            }
        };
        // The utterance file is no longer needed; delete it now rather than
        // at thread exit so a long agent turn doesn't hold the temp file.
        drop(staged);
        let text = text.trim().to_string();
        send(WorkerMsg::Transcript(text.clone()));
        if text.is_empty() || abort.load(Ordering::SeqCst) {
            finish();
            return;
        }
        let outcome = turn.run_turn(&format!("{LIVE_TRANSCRIPT_PREFIX}{text}"));
        if let Some(scope) = outcome.approval {
            // Parked on approval: surface it, never auto-approve. The operator
            // answers through the normal (text) approval path.
            send(WorkerMsg::ApprovalNeeded(scope));
            finish();
            return;
        }
        if let Some(code) = outcome.error {
            send(WorkerMsg::Error(code));
            finish();
            return;
        }
        // `reply_text` is always sent, even when TTS then fails.
        send(WorkerMsg::ReplyText(outcome.reply_text.clone()));
        if outcome.reply_text.trim().is_empty() || abort.load(Ordering::SeqCst) {
            finish();
            return;
        }
        let tts_audio = match cfg.pipes.synthesize(&outcome.reply_text, AudioFormat::Wav) {
            Ok(r) => r,
            Err(code) => {
                eprintln!("live voice: tts failed [{code}]");
                send(WorkerMsg::Error(code));
                finish();
                return;
            }
        };
        if tts_audio.format != AudioFormat::Wav {
            send(WorkerMsg::Error("tts_format_unsupported".to_string()));
            finish();
            return;
        }
        match wav_to_pcm_16k(&tts_audio.bytes) {
            Ok(pcm16) => {
                for chunk in pcm16.chunks(CHUNK_BYTES / 2) {
                    if abort.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut bytes = Vec::with_capacity(chunk.len() * 2);
                    for s in chunk {
                        bytes.extend_from_slice(&s.to_le_bytes());
                    }
                    send(WorkerMsg::AudioChunk(bytes));
                }
                send(WorkerMsg::AudioEnd);
            }
            Err(code) => {
                eprintln!("live voice: tts output not decodable [{code}]");
                send(WorkerMsg::Error(code.to_string()));
            }
        }
        finish();
    }
}
pub mod realtime_voice {
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

    use crate::voice_group::live_voice::{json_text, send_msg, HeadReplay};
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
                    .or_else(|| {
                        v.get("error")
                            .and_then(|e| e.get("type"))
                            .and_then(|c| c.as_str())
                    })
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
                        _ => send_msg(
                            ws,
                            json_text(&json!({"type":"error","code":"unknown_frame"})),
                        ),
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
                    let cap = limits.live_max_utterance_secs.max(1) as usize
                        * SAMPLE_RATE_CLIENT as usize;
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
        std::thread::spawn(move || loop {
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
        format!(
            "wss://api.openai.com/v1/realtime?model={}",
            url_encode(model)
        )
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
}
pub mod channel_voice {
    //! Voice wiring for the Telegram and Discord channels: inbound
    //! speech-to-text and outbound text-to-speech.
    //!
    //! Umar's directive: voice is a Gateway-channel capability (and the mobile
    //! app), never an agent tool - so no voice tool is registered anywhere;
    //! the backends are consumed here, at the channel seam. (The mobile app's
    //! `/agui/voice` edge lives in [`crate::voice`]; this module is the
    //! Discord/Telegram counterpart.)
    //!
    //! Backends come from `[stt]` / `[tts]` (built by
    //! [`stt_from_config`](pantheon_providers::voice::stt_from_config) /
    //! [`tts_from_config`](pantheon_providers::voice::tts_from_config)).
    //! Both additionally require the `[tools] voice` group toggle - the same
    //! double gate the browser uses (`[browser]` enabled + `[tools] browser`):
    //! toggle off or section absent = the backend is never constructed.
    //!
    //! Hygiene: secrets never reach logs or the ledger. Every error surfaced
    //! from this module carries a code or a static message - never a URL
    //! (Telegram file URLs embed the bot token, and transport error text is
    //! not ours to audit) and never key material. Downloaded audio is staged
    //! to the temp dir and deleted on every path via [`TempAudio`]'s `Drop`.

    use crate::channels::channel::{ChannelError, ChannelEvent};
    use pantheon_api::config::{ToolGroup, ToolsSection, VoiceSection};
    use pantheon_providers::voice::{
        stt_from_config, tts_from_config, AudioFormat, SttProvider, SttRequest, TtsProvider,
        TtsRequest, TtsResult,
    };
    use pantheon_secrets::SecretsBroker;
    use std::io::Read;
    use std::path::PathBuf;

    /// Prefix marking a transcript so the agent/session knows it came from
    /// speech rather than typed text.
    pub const VOICE_TRANSCRIPT_PREFIX: &str = "[voice message] ";
    /// Polite decline: a voice message arrived but no `[stt]` backend is
    /// configured. Sent as a reply - never silently dropped.
    pub const STT_NOT_CONFIGURED: &str =
        "I can't listen to voice messages yet - no speech-to-text backend configured.";
    /// The `[stt]` section exists but the backend failed to construct.
    pub const STT_UNAVAILABLE: &str =
        "I can't listen to voice messages right now - speech-to-text is unavailable.";
    /// Download or transcription failed mid-flight.
    pub const STT_FAILED: &str =
        "I couldn't make out that voice message - please try again or type it instead.";
    /// The backend returned an empty transcript.
    pub const STT_EMPTY: &str =
        "I couldn't hear anything in that voice message - please try again or type it instead.";

    /// Download cap for inbound voice files. Telegram voice notes run ~1 MB
    /// per minute; 25 MB is far beyond any legitimate voice message and
    /// bounds what a malicious sender can make us buffer.
    pub const MAX_VOICE_BYTES: usize = 25 * 1024 * 1024;

    /// One side of the voice double-gate: the `[tools] voice` toggle AND the
    /// `[stt]`/`[tts]` section must both allow the backend (mirrors how the
    /// browser requires `[browser]` enabled + `[tools] browser`).
    #[derive(Default)]
    pub enum VoiceSlot<T> {
        /// Toggle off or section absent: the backend is never constructed.
        #[default]
        Disabled,
        /// Backend constructed and ready.
        Ready(T),
        /// Section present but construction failed. Carries the backend's
        /// error *code* only - never a message that could hold a secret.
        Unavailable(String),
    }

    /// The voice capability attached to one channel: optional STT/TTS
    /// backends plus the per-channel `voice_replies` preference
    /// (`[gateway.channels.<name>] voice_replies`, default off).
    pub struct VoicePipes {
        pub stt: VoiceSlot<Box<dyn SttProvider>>,
        pub tts: VoiceSlot<Box<dyn TtsProvider>>,
        pub voice_replies: bool,
    }

    impl Default for VoicePipes {
        fn default() -> Self {
            Self::disabled()
        }
    }

    impl VoicePipes {
        /// No voice capability at all: inbound voice is politely declined,
        /// outbound stays text.
        pub fn disabled() -> Self {
            Self {
                stt: VoiceSlot::Disabled,
                tts: VoiceSlot::Disabled,
                voice_replies: false,
            }
        }

        /// Build from config. The `[tools] voice` group toggle (absent = on,
        /// per [`ToolsSection::is_enabled`]) AND the `[stt]`/`[tts]` section
        /// must both be present; either missing disables that side without
        /// constructing a backend. A section that fails to construct becomes
        /// `Unavailable` - surfaced as "STT/TTS unavailable", never a model
        /// error.
        pub fn from_config(
            tools: Option<&ToolsSection>,
            stt: Option<&VoiceSection>,
            tts: Option<&VoiceSection>,
            secrets: &SecretsBroker,
            voice_replies: bool,
        ) -> Self {
            let group_on = tools
                .map(|t| t.is_enabled(ToolGroup::Voice))
                .unwrap_or(true);
            let stt = if group_on {
                match stt_from_config(stt, secrets) {
                    None => VoiceSlot::Disabled,
                    Some(Ok(backend)) => VoiceSlot::Ready(backend),
                    Some(Err(e)) => VoiceSlot::Unavailable(e.code),
                }
            } else {
                VoiceSlot::Disabled
            };
            let tts = if group_on {
                match tts_from_config(tts, secrets) {
                    None => VoiceSlot::Disabled,
                    Some(Ok(backend)) => VoiceSlot::Ready(backend),
                    Some(Err(e)) => VoiceSlot::Unavailable(e.code),
                }
            } else {
                VoiceSlot::Disabled
            };
            Self {
                stt,
                tts,
                voice_replies,
            }
        }

        /// True when agent text replies on this channel should go out as
        /// voice. Requires the operator's `voice_replies` opt-in AND a
        /// working `[tts]` backend - text stays the default otherwise.
        pub fn speak_replies(&self) -> bool {
            self.voice_replies && matches!(self.tts, VoiceSlot::Ready(_))
        }

        /// One-line, secret-free summary for startup logs (`stt=groq tts=off
        /// voice_replies=false`): backend names and codes only.
        pub fn describe(&self) -> String {
            let stt = match &self.stt {
                VoiceSlot::Disabled => "off".to_string(),
                VoiceSlot::Ready(b) => b.name().to_string(),
                VoiceSlot::Unavailable(code) => format!("unavailable[{code}]"),
            };
            let tts = match &self.tts {
                VoiceSlot::Disabled => "off".to_string(),
                VoiceSlot::Ready(b) => b.name().to_string(),
                VoiceSlot::Unavailable(code) => format!("unavailable[{code}]"),
            };
            format!("stt={stt} tts={tts} voice_replies={}", self.voice_replies)
        }

        /// Handle one downloaded voice clip: transcribe it, or produce the
        /// polite decline reply. Never silently drops: every path returns an
        /// outcome the caller must deliver.
        pub fn handle_voice_bytes(
            &self,
            thread_id: &str,
            sender: Option<String>,
            bytes: &[u8],
            ext: &str,
        ) -> VoiceOutcome {
            let reply = |text: &str| VoiceOutcome::Reply {
                thread_id: thread_id.to_string(),
                text: text.to_string(),
            };
            match &self.stt {
                VoiceSlot::Disabled => reply(STT_NOT_CONFIGURED),
                VoiceSlot::Unavailable(code) => {
                    eprintln!("voice: stt backend unavailable [{code}]; declining voice message");
                    reply(STT_UNAVAILABLE)
                }
                VoiceSlot::Ready(stt) => match stage_and_transcribe(stt.as_ref(), bytes, ext) {
                    Ok(text) => {
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            reply(STT_EMPTY)
                        } else {
                            VoiceOutcome::Event(ChannelEvent {
                                thread_id: thread_id.to_string(),
                                run_id: None,
                                text: format!("{VOICE_TRANSCRIPT_PREFIX}{text}"),
                                approval: None,
                                scope: None,
                                sender,
                            })
                        }
                    }
                    Err(code) => {
                        eprintln!("voice: transcription failed [{code}]");
                        reply(STT_FAILED)
                    }
                },
            }
        }

        /// Synthesize `text` for a voice reply. Voice selection comes from
        /// the `[tts]` options map (`voice = "..."`): the backends read it at
        /// construction, so the request leaves `voice` unset and the
        /// configured voice wins. `Err` is a secret-free code.
        pub fn synthesize(&self, text: &str, format: AudioFormat) -> Result<TtsResult, String> {
            let tts = match &self.tts {
                VoiceSlot::Ready(t) => t,
                VoiceSlot::Disabled => return Err("tts-disabled".to_string()),
                VoiceSlot::Unavailable(code) => return Err(code.clone()),
            };
            let req = TtsRequest {
                text: text.to_string(),
                voice: None,
                format,
            };
            tts.synthesize(&req).map_err(|e| e.code)
        }
    }

    /// What inbound voice processing produced for one message/update: either
    /// a normal channel event for the sink, or a plain-text reply the surface
    /// must send back (declines - never silently dropped).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum VoiceOutcome {
        Event(ChannelEvent),
        Reply { thread_id: String, text: String },
    }

    /// Downloaded audio staged to the temp dir. The backends take a file
    /// path, so the bytes must touch disk - briefly: `Drop` removes the
    /// file on every path, including transcription errors and panics. (The
    /// mobile edge in [`crate::voice`] has its own guard without an
    /// extension; this one keeps the suffix so backends that sniff by
    /// extension decode correctly.)
    struct TempAudio {
        path: PathBuf,
    }

    impl TempAudio {
        fn stage(bytes: &[u8], ext: &str) -> std::io::Result<Self> {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "pantheon-voice-{}-{}.{}",
                std::process::id(),
                nanos(),
                sanitize_ext(ext)
            ));
            std::fs::write(&path, bytes)?;
            Ok(Self { path })
        }
    }

    impl Drop for TempAudio {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    }

    /// Keep only ASCII alphanumerics (max 8 chars) for the staged file's
    /// extension; anything else becomes `bin`. The extension lands in a temp
    /// filename, so a hostile `mime_type` must not smuggle path separators.
    fn sanitize_ext(ext: &str) -> String {
        let clean: String = ext
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .take(8)
            .collect();
        if clean.is_empty() {
            "bin".to_string()
        } else {
            clean.to_ascii_lowercase()
        }
    }

    fn stage_and_transcribe(
        stt: &dyn SttProvider,
        bytes: &[u8],
        ext: &str,
    ) -> Result<String, String> {
        let staged = TempAudio::stage(bytes, ext).map_err(|e| format!("stage: {e}"))?;
        let out = stt
            .transcribe(&SttRequest::new(&staged.path))
            .map(|r| r.text)
            .map_err(|e| e.code);
        // `staged` drops here: the temp file is removed whether
        // transcription succeeded or failed.
        out
    }

    /// Map a Telegram/Discord audio MIME subtype to a file extension for the
    /// staged temp file.
    pub fn ext_for_mime(mime: &str) -> &'static str {
        match mime.split('/').nth(1).unwrap_or("").trim() {
            "ogg" => "ogg",
            "mpeg" | "mp3" => "mp3",
            "mp4" | "x-m4a" => "m4a",
            "wav" | "x-wav" => "wav",
            "flac" | "x-flac" => "flac",
            "opus" | "x-opus" => "opus",
            "webm" => "webm",
            _ => "bin",
        }
    }

    /// Extension from a filename (`clip.ogg` -> `ogg`), sanitized.
    pub fn ext_for_filename(name: &str) -> String {
        sanitize_ext(name.rsplit('.').next().unwrap_or(""))
    }

    /// Classify a ureq failure without echoing the URL: Telegram file URLs
    /// embed the bot token, and transport error text is outside our control,
    /// so the raw error must never reach a `ChannelError` message or log
    /// line. The channel agents run with statuses-as-responses, so in
    /// practice only transport failures land here; the status arm stays as a
    /// guard in case a future agent flips that setting.
    pub(crate) fn ureq_kind(e: &ureq::Error) -> String {
        match e {
            ureq::Error::StatusCode(code) => format!("http {code}"),
            _ => "transport error".to_string(),
        }
    }

    /// GET a URL into memory, capped at `cap` bytes. All failures map to
    /// static, secret-free messages (see [`ureq_kind`]).
    pub(crate) fn download_capped(
        agent: &ureq::Agent,
        url: &str,
        cap: usize,
        code_prefix: &'static str,
    ) -> Result<Vec<u8>, ChannelError> {
        let resp = agent
            .get(url)
            .call()
            .map_err(|_| ChannelError::new(code_prefix, "download failed"))?;
        // HTTP failures map to the same static message as transport ones:
        // the URL (pre-signed or token-bearing) must stay out of the text.
        if resp.status().as_u16() >= 400 {
            return Err(ChannelError::new(code_prefix, "download failed"));
        }
        let mut body = Vec::new();
        resp.into_body()
            .into_reader()
            .take(cap as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| ChannelError::new(code_prefix, "download: body read failed"))?;
        if body.len() > cap {
            return Err(ChannelError::new(
                code_prefix,
                "download: file exceeds size cap",
            ));
        }
        Ok(body)
    }

    /// Minimal multipart/form-data encoder: hand-rolled so the body is plain
    /// bytes we control exactly (no streaming wrappers, one allocation).
    /// Returns `(content_type_header_value, body)`.
    pub(crate) fn multipart(
        fields: &[(&str, &str)],
        file_field: &str,
        filename: &str,
        file_content_type: &str,
        file_bytes: &[u8],
    ) -> (String, Vec<u8>) {
        let boundary = format!("----pantheon-voice-{}-{}", std::process::id(), nanos());
        let mut body = Vec::new();
        for (name, value) in fields {
            body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            );
            body.extend_from_slice(value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!(
            "Content-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\n"
        )
            .as_bytes(),
        );
        body.extend_from_slice(format!("Content-Type: {file_content_type}\r\n\r\n").as_bytes());
        body.extend_from_slice(file_bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        (format!("multipart/form-data; boundary={boundary}"), body)
    }

    /// Warn-once helper for transports that cannot upload voice: text is the
    /// honest fallback, but the operator should know `voice_replies` is a
    /// no-op on this transport.
    pub(crate) fn warn_voice_unsupported_once(channel: &'static str) {
        use std::sync::atomic::{AtomicBool, Ordering};
        static WARNED: AtomicBool = AtomicBool::new(false);
        if !WARNED.swap(true, Ordering::SeqCst) {
            eprintln!(
            "{channel}: voice_replies is on but this transport cannot upload audio; falling back to text"
        );
        }
    }
}
