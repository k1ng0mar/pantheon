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
//! (Telegram file URLs embed the bot token; ureq errors echo URLs) and
//! never key material. Downloaded audio is staged to the temp dir and
//! deleted on every path via [`TempAudio`]'s `Drop`.

use crate::channel::{ChannelError, ChannelEvent};
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

fn stage_and_transcribe(stt: &dyn SttProvider, bytes: &[u8], ext: &str) -> Result<String, String> {
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
/// embed the bot token and ureq's `Display` prints the URL, so the raw
/// error must never reach a `ChannelError` message or log line.
pub(crate) fn ureq_kind(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, _) => format!("http {code}"),
        ureq::Error::Transport(_) => "transport error".to_string(),
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
    let mut body = Vec::new();
    resp.into_reader()
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

/// Minimal multipart/form-data encoder (ureq 2 has no multipart
/// support). Returns `(content_type_header_value, body)`.
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
