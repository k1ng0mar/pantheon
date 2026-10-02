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
    b64decode, stt_from_config, tts_from_config, AudioFormat, SttProvider, SttRequest, TtsProvider,
    TtsRequest,
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
pub const MAX_AUDIO_BYTES: usize = 8 * 1024 * 1024;

/// Wire-body cap for `POST /agui/voice/transcribe`: 12 MiB. The audio
/// rides as base64 inside JSON, so 8 MiB of audio is ~10.7 MiB on the
/// wire; the extra headroom covers the JSON envelope. Every other
/// route keeps the transport's 1 MiB [`crate::http::MAX_BODY`] guard.
pub const MAX_VOICE_BODY_BYTES: usize = 12 * 1024 * 1024;

/// Bound on one transcribe/synthesize call. Backends carry their own
/// (shorter) bounds; this is the edge's guarantee to the HTTP client.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

/// Synthesis input cap: 32 KiB of text is ~30 minutes of spoken audio.
/// Anything bigger is abuse, not speech.
pub const MAX_TEXT_CHARS: usize = 32 * 1024;

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
                return VoiceHttp::err(400, "voice_bad_request", format!("invalid JSON body: {e}"))
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
        if bytes.len() > MAX_AUDIO_BYTES {
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
                return VoiceHttp::err(400, "voice_bad_request", format!("invalid JSON body: {e}"))
            }
        };
        let text = body.text.trim();
        if text.is_empty() {
            return VoiceHttp::err(400, "voice_bad_request", "field `text` is required");
        }
        if text.chars().count() > MAX_TEXT_CHARS {
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
        let path =
            std::env::temp_dir().join(format!("pantheon-agui-voice-{}-{n}", std::process::id()));
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
