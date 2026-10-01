//! STT/TTS provider plane (ARCHITECTURE §14). These are *services*, not
//! models: a whisper binary or a cloud transcription endpoint is a
//! swappable capability, never an entry in the model policy.
//!
//! Shape mirrors `pantheon-memory`'s backend registry: traits + a catalog
//! of named backends (`stt_providers()` / `tts_providers()`, recommended
//! first, for the setup wizard) + `open_*` constructors driven by config
//! (`[stt]` / `[tts]`, each `{ backend, options }`).
//!
//! Backend families:
//!
//! - `command` (Subprocess): local binaries — whisper.cpp, piper,
//!   espeak-ng. STT reads text from stdout (`{file}`/`{language}`
//!   placeholders); TTS pipes text in on stdin and takes audio from
//!   stdout. Every run is wall-clock bounded.
//! - `piper-local` (Subprocess): piper with binary + voice-model
//!   detection, mapped onto the `command` machinery.
//! - `openai` / `groq` / `mistral` (HttpCloud): one OpenAI-compatible
//!   implementation (`/audio/transcriptions`, `/audio/speech`), base URL +
//!   key resolved through the core catalog like every other provider.
//! - Bespoke cloud backends (deepgram, elevenlabs, xai, assemblyai STT;
//!   elevenlabs, deepgram, gemini, fishaudio TTS; kokoro-local and
//!   fishspeech-local subprocess TTS): live
//!   implementations built from the documented wire shapes (pure request
//!   builders / response parsers here, pinned by fixtures in
//!   `eval/tests/providers_voice_wire.rs`). The wires have not been
//!   exercised against the real APIs yet (no keys were available); treat
//!   first runs as verification runs.
//!
//! Callers gate access upstream (capability policy on the gateway/CLI
//! seam); backends only move bytes. Nothing here ever enters model
//! context except the transcription text the caller passes on. Keys are
//! resolved by the caller via the secrets broker and handed to
//! constructors; this crate never logs key material.

use crate::catalog;
use pantheon_api::config::VoiceSection;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_secrets::{SecretValue, SecretsBroker};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

fn verr(code: &str, cause: String, retryable: bool, remediation: &'static str) -> PantheonError {
    PantheonError::new(code, Layer::Provider, retryable, cause, remediation, "")
}

/// Default bound for subprocess backends: audio jobs are small but model
/// load can take a while on first run.
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 120;

// ---------------------------------------------------------------------------
// Request / result types
// ---------------------------------------------------------------------------

/// One transcription ask.
#[derive(Debug, Clone, PartialEq)]
pub struct SttRequest {
    /// Audio file on disk (ogg/wav/mp3/flac — whatever the backend takes).
    pub path: PathBuf,
    /// ISO language hint, when known.
    pub language: Option<String>,
    /// Context hint (OpenAI-compatible `prompt`): improves domain words.
    pub prompt: Option<String>,
}

impl SttRequest {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            language: None,
            prompt: None,
        }
    }
    pub fn with_language(mut self, language: impl Into<String>) -> Self {
        self.language = Some(language.into());
        self
    }
}

/// What a transcription produced.
#[derive(Debug, Clone, PartialEq)]
pub struct SttResult {
    pub text: String,
    pub language: Option<String>,
    /// Backend-reported duration in seconds, when known.
    pub duration_secs: Option<f64>,
    /// Which backend served it (ledger/provenance label).
    pub provider: String,
}

/// Wire format for synthesized audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    #[default]
    Wav,
    Mp3,
    Ogg,
}

impl AudioFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            AudioFormat::Wav => "wav",
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Ogg => "ogg",
        }
    }
}

/// One synthesis ask.
#[derive(Debug, Clone, PartialEq)]
pub struct TtsRequest {
    pub text: String,
    /// Voice id/name, backend-specific; `None` = backend default.
    pub voice: Option<String>,
    pub format: AudioFormat,
}

impl TtsRequest {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            voice: None,
            format: AudioFormat::default(),
        }
    }
    pub fn with_voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = Some(voice.into());
        self
    }
}

/// Synthesized audio.
#[derive(Debug, Clone, PartialEq)]
pub struct TtsResult {
    pub bytes: Vec<u8>,
    pub format: AudioFormat,
    pub provider: String,
}

// ---------------------------------------------------------------------------
// Traits
// ---------------------------------------------------------------------------

/// Speech-to-text service. Providers are services or local binaries —
/// never selected through `ModelPolicy`.
pub trait SttProvider: Send + Sync {
    fn name(&self) -> &str;
    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError>;
}

/// Text-to-speech service.
pub trait TtsProvider: Send + Sync {
    fn name(&self) -> &str;
    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError>;
}

// ---------------------------------------------------------------------------
// Bounded subprocess plumbing (shared by both command backends)
// ---------------------------------------------------------------------------

/// Run a command with optional stdin, capturing stdout/stderr with a hard
/// wall-clock bound. std threads, no async — same posture as the sandbox.
fn run_bounded(
    program: &str,
    args: &[String],
    stdin_text: Option<&str>,
    timeout: Duration,
) -> Result<(Vec<u8>, Vec<u8>, i32), PantheonError> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(if stdin_text.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| {
        verr(
            "VOICE_SPAWN",
            format!("spawn {program}: {e}"),
            false,
            "check the backend binary is installed and on PATH",
        )
    })?;

    if let Some(text) = stdin_text {
        if let Some(stdin) = child.stdin.as_mut() {
            // A write failure here means the child died instantly; the
            // exit-code path below reports it with stderr.
            let _ = stdin.write_all(text.as_bytes());
        }
        drop(child.stdin.take());
    }

    // Drain pipes on threads so a chatty child can never fill the 64 KB
    // pipe buffer, deadlock, and die as a false timeout.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(verr(
                        "VOICE_TIMEOUT",
                        format!("{program} exceeded {}s", timeout.as_secs()),
                        true,
                        "raise the backend's timeout_secs option or check system load",
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => {
                return Err(verr(
                    "VOICE_WAIT",
                    format!("wait for {program}: {e}"),
                    true,
                    "retry; if persistent, check system process limits",
                ))
            }
        }
    };
    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    Ok((stdout, stderr, status.code().unwrap_or(-1)))
}

fn options_get<'a>(options: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    options
        .get(key)
        .map(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
}

fn require_option<'a>(
    options: &'a HashMap<String, String>,
    key: &'static str,
) -> Result<&'a str, PantheonError> {
    options_get(options, key).ok_or_else(|| {
        verr(
            "VOICE_CONFIG",
            format!("backend option `{key}` is required"),
            false,
            "set it in config.toml under [stt]/[tts] options",
        )
    })
}

/// Split an args template on whitespace and substitute placeholders.
fn expand_args(
    template: &str,
    file: Option<&Path>,
    language: Option<&str>,
    voice: Option<&str>,
    format: Option<&str>,
) -> Vec<String> {
    template
        .split_whitespace()
        .map(|tok| {
            let mut tok = tok.to_string();
            if let Some(f) = file {
                tok = tok.replace("{file}", &f.to_string_lossy());
            }
            if let Some(l) = language {
                tok = tok.replace("{language}", l);
            }
            if let Some(v) = voice {
                tok = tok.replace("{voice}", v);
            }
            if let Some(fmt) = format {
                tok = tok.replace("{format}", fmt);
            }
            tok
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Command (subprocess) backends
// ---------------------------------------------------------------------------

/// Local STT binary: stdout is the transcript.
///
/// Options: `cmd` (required), `args` (template with `{file}`,
/// `{language}`), `language` (fallback for `{language}`), `timeout_secs`.
pub struct CommandStt {
    pub cmd: String,
    /// Args template, split on whitespace at call time (`{file}`,
    /// `{language}` placeholders).
    pub args: String,
    pub fallback_language: String,
    pub timeout: Duration,
}

impl CommandStt {
    pub fn from_options(options: &HashMap<String, String>) -> Result<Self, PantheonError> {
        let timeout_secs = options_get(options, "timeout_secs")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
        Ok(Self {
            cmd: require_option(options, "cmd")?.to_string(),
            args: options_get(options, "args").unwrap_or("").to_string(),
            fallback_language: options_get(options, "language")
                .unwrap_or("auto")
                .to_string(),
            timeout: Duration::from_secs(timeout_secs),
        })
    }
}

impl SttProvider for CommandStt {
    fn name(&self) -> &str {
        "command"
    }

    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        if !req.path.exists() {
            return Err(verr(
                "STT_INPUT",
                format!("audio file not found: {}", req.path.display()),
                false,
                "pass an existing audio file",
            ));
        }
        let language = req
            .language
            .clone()
            .unwrap_or_else(|| self.fallback_language.clone());
        let args = expand_args(&self.args, Some(&req.path), Some(&language), None, None);
        let (stdout, stderr, code) = run_bounded(&self.cmd, &args, None, self.timeout)?;
        if code != 0 {
            return Err(verr(
                "STT_EXIT",
                format!(
                    "{} exited {code}: {}",
                    self.cmd,
                    String::from_utf8_lossy(&stderr)
                        .chars()
                        .take(300)
                        .collect::<String>()
                ),
                false,
                "check the STT binary's model paths and arguments",
            ));
        }
        Ok(SttResult {
            text: String::from_utf8_lossy(&stdout).trim().to_string(),
            language: req.language.clone(),
            duration_secs: None,
            provider: self.name().to_string(),
        })
    }
}

/// Local TTS binary: text on stdin, audio bytes on stdout.
///
/// Options: `cmd` (required), `args` (template with `{voice}`,
/// `{format}`), `timeout_secs`. Works with piper and espeak-ng
/// (`espeak-ng --stdout`).
pub struct CommandTts {
    pub cmd: String,
    /// Args template, split on whitespace at call time (`{voice}`,
    /// `{format}` placeholders).
    pub args: String,
    /// Voice used when the request has none (mirrors `HttpTts`).
    pub default_voice: Option<String>,
    /// Provenance label; `"command"` for the generic backend,
    /// e.g. `"piper-local"` for the piper mapping.
    pub name: String,
    pub timeout: Duration,
}

impl CommandTts {
    pub fn from_options(options: &HashMap<String, String>) -> Result<Self, PantheonError> {
        let timeout_secs = options_get(options, "timeout_secs")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
        Ok(Self {
            cmd: require_option(options, "cmd")?.to_string(),
            args: options_get(options, "args").unwrap_or("").to_string(),
            default_voice: options_get(options, "voice").map(String::from),
            name: "command".to_string(),
            timeout: Duration::from_secs(timeout_secs),
        })
    }
}

impl TtsProvider for CommandTts {
    fn name(&self) -> &str {
        &self.name
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        let voice = req.voice.clone().or_else(|| self.default_voice.clone());
        let args = expand_args(
            &self.args,
            None,
            None,
            voice.as_deref(),
            Some(req.format.as_str()),
        );
        let (stdout, stderr, code) = run_bounded(&self.cmd, &args, Some(&req.text), self.timeout)?;
        if code != 0 {
            return Err(verr(
                "TTS_EXIT",
                format!(
                    "{} exited {code}: {}",
                    self.cmd,
                    String::from_utf8_lossy(&stderr)
                        .chars()
                        .take(300)
                        .collect::<String>()
                ),
                false,
                "check the TTS binary's voice model and arguments",
            ));
        }
        if stdout.is_empty() {
            return Err(verr(
                "TTS_EMPTY",
                format!("{} produced no audio", self.cmd),
                true,
                "check the TTS binary writes audio to stdout",
            ));
        }
        Ok(TtsResult {
            bytes: stdout,
            format: req.format,
            provider: self.name().to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// HTTP (OpenAI-compatible) backends
// ---------------------------------------------------------------------------

/// Build the multipart body for `POST {base}/audio/transcriptions`.
/// Pure, so the wire shape is unit-testable offline.
pub fn stt_multipart(
    model: &str,
    language: Option<&str>,
    prompt: Option<&str>,
    filename: &str,
    audio: &[u8],
) -> (String, Vec<u8>) {
    let boundary = "pantheon-boundary-7f3a9c";
    let mut body = Vec::with_capacity(audio.len() + 512);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("model", model);
    if let Some(l) = language {
        field("language", l);
    }
    if let Some(p) = prompt {
        field("prompt", p);
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
            audio_mime(filename)
        )
        .as_bytes(),
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// JSON body for `POST {base}/audio/speech`. Pure.
///
/// `response_format` is provider-specific: the OpenAI wire accepts
/// `mp3`/`opus`/`aac`/`flac`/`wav`/`pcm` — notably NOT `ogg`, so an Ogg
/// request maps to `opus` (the same codec, a container the API accepts).
pub fn speech_payload(req: &TtsRequest, model: &str) -> serde_json::Value {
    let response_format = match req.format {
        AudioFormat::Ogg => "opus",
        other => other.as_str(),
    };
    serde_json::json!({
        "model": model,
        "voice": req.voice.clone().unwrap_or_else(|| "alloy".into()),
        "input": req.text,
        "response_format": response_format,
    })
}

/// Best-effort audio MIME type from a filename extension. Vendor STT
/// docs send the real audio MIME type on the file part rather than
/// `application/octet-stream`; unknown extensions keep the generic one.
pub fn audio_mime(filename: &str) -> &'static str {
    match filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/ogg; codecs=opus",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "webm" => "audio/webm",
        "aiff" | "aif" => "audio/aiff",
        _ => "application/octet-stream",
    }
}

/// Default STT model per OpenAI-wire provider (overridable via the
/// `model` option). Groq and Mistral share `HttpStt`'s implementation:
/// both endpoints accept OpenAI-shaped multipart (`file`, `model`,
/// `language` — the only fields `stt_multipart` sends) and answer in the
/// OpenAI `{text, ...}` shape, so one code path covers all three.
/// Auth note: Mistral's own docs are inconsistent here — the newest
/// endpoint reference uses `Authorization: Bearer` (which is what the
/// catalog sends), while an older transcription guide shows `x-api-key`;
/// if transcription 401s on Mistral, the header is the first suspect.
/// (Mistral's documented primary flow uploads the file first and passes
/// `file_url`; sending the file inline like OpenAI is what community
/// integrations do, and is kept here.)
fn default_stt_model(provider: &str) -> &'static str {
    match provider {
        "groq" => "whisper-large-v3-turbo",
        "mistral" => "voxtral-mini-latest",
        _ => "gpt-4o-mini-transcribe",
    }
}

/// OpenAI-compatible STT over HTTP.
///
/// Options: `provider` (catalog id or base URL, required), `model`
/// (default per provider: see [`default_stt_model`]).
/// Generic OpenAI-compatible STT backend (OpenAI, Groq, Mistral).
// NOTE: built from the vendors' docs, not live-tested.
pub struct HttpStt {
    pub provider: String,
    pub model: String,
    pub api_key: Option<SecretValue>,
}

impl HttpStt {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        let provider = require_option(options, "provider")?.to_string();
        let model = options_get(options, "model")
            .unwrap_or_else(|| default_stt_model(&provider))
            .to_string();
        Ok(Self {
            provider,
            model,
            api_key,
        })
    }

    fn endpoint(&self) -> (String, String, String) {
        let base = catalog::base_url_for(&self.provider);
        let configured = self
            .api_key
            .as_ref()
            .map(|k| k.expose())
            .unwrap_or("")
            .to_string();
        let key = catalog::key_for(&self.provider, &configured);
        let key_header = catalog::key_header_for(&self.provider);
        (
            format!("{}/audio/transcriptions", base.trim_end_matches('/')),
            key_header,
            key,
        )
    }
}

impl SttProvider for HttpStt {
    fn name(&self) -> &str {
        &self.provider
    }

    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        let audio = std::fs::read(&req.path).map_err(|e| {
            verr(
                "STT_INPUT",
                format!("read {}: {e}", req.path.display()),
                false,
                "pass a readable audio file",
            )
        })?;
        let filename = req
            .path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "audio".into());
        let (content_type, body) = stt_multipart(
            &self.model,
            req.language.as_deref(),
            req.prompt.as_deref(),
            &filename,
            &audio,
        );
        let (url, key_header, key) = self.endpoint();
        // Bounded like every other provider-plane call: the shared agent
        // honors `http_timeout()`, so a hung endpoint fails the turn
        // instead of wedging the session thread forever.
        let mut request = crate::http::http_agent()
            .post(&url)
            .set("content-type", &content_type);
        if !key.is_empty() {
            let (name, value) = crate::http::auth_header_pair(&key_header, &key);
            request = request.set(&name, &value);
        }
        let resp = request.send_bytes(&body).map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let detail = error_snippet(resp);
                let cause = if detail.is_empty() {
                    format!("transcription endpoint: HTTP {code}")
                } else {
                    format!("transcription endpoint: HTTP {code}: {detail}")
                };
                verr(
                    "STT_HTTP",
                    cause,
                    !(400..500).contains(&code),
                    "check the [stt] provider, model name, and API key",
                )
            }
            e => verr(
                "STT_HTTP",
                format!("transcription endpoint: {e}"),
                true,
                "check network reachability of the [stt] endpoint",
            ),
        })?;
        let text_body = resp.into_string().map_err(|e| {
            verr(
                "STT_HTTP",
                format!("transcription response read: {e}"),
                true,
                "retry",
            )
        })?;
        let v: serde_json::Value = serde_json::from_str(&text_body).map_err(|e| {
            verr(
                "STT_PARSE",
                format!("transcription response: {e}"),
                false,
                "check the endpoint returns OpenAI-shaped JSON {text,...}",
            )
        })?;
        let text = v
            .get("text")
            .and_then(|t| t.as_str())
            .ok_or_else(|| {
                verr(
                    "STT_PARSE",
                    "transcription response has no `text` field".to_string(),
                    false,
                    "check the endpoint returns OpenAI-shaped JSON {text,...}",
                )
            })?
            .to_string();
        Ok(SttResult {
            text,
            language: v.get("language").and_then(|l| l.as_str()).map(String::from),
            duration_secs: v.get("duration").and_then(|d| d.as_f64()),
            provider: self.name().to_string(),
        })
    }
}

/// OpenAI-compatible TTS over HTTP (`POST {base}/audio/speech`).
///
/// Options: `provider` (catalog id or base URL, required), `model`
/// (default `tts-1`), `voice` (default voice when the request has none).
/// Generic OpenAI-compatible TTS backend (currently OpenAI only).
// NOTE: built from OpenAI docs, not live-tested.
pub struct HttpTts {
    pub provider: String,
    pub model: String,
    pub default_voice: Option<String>,
    pub api_key: Option<SecretValue>,
}

impl HttpTts {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            provider: require_option(options, "provider")?.to_string(),
            model: options_get(options, "model")
                .unwrap_or("gpt-4o-mini-tts")
                .to_string(),
            default_voice: options_get(options, "voice").map(String::from),
            api_key,
        })
    }
}

impl TtsProvider for HttpTts {
    fn name(&self) -> &str {
        &self.provider
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        let mut req = req.clone();
        if req.voice.is_none() {
            req.voice = self.default_voice.clone();
        }
        let base = catalog::base_url_for(&self.provider);
        let configured = self
            .api_key
            .as_ref()
            .map(|k| k.expose())
            .unwrap_or("")
            .to_string();
        let key = catalog::key_for(&self.provider, &configured);
        let key_header = catalog::key_header_for(&self.provider);
        let url = format!("{}/audio/speech", base.trim_end_matches('/'));
        // Same bound as STT: no provider-plane call runs without a deadline.
        let mut request = crate::http::http_agent()
            .post(&url)
            .set("content-type", "application/json");
        if !key.is_empty() {
            let (name, value) = crate::http::auth_header_pair(&key_header, &key);
            request = request.set(&name, &value);
        }
        let resp = request
            .send_json(speech_payload(&req, &self.model))
            .map_err(|e| match e {
                ureq::Error::Status(code, resp) => {
                    let detail = error_snippet(resp);
                    let cause = if detail.is_empty() {
                        format!("speech endpoint: HTTP {code}")
                    } else {
                        format!("speech endpoint: HTTP {code}: {detail}")
                    };
                    verr(
                        "TTS_HTTP",
                        cause,
                        !(400..500).contains(&code),
                        "check the [tts] provider, model name, and API key",
                    )
                }
                e => verr(
                    "TTS_HTTP",
                    format!("speech endpoint: {e}"),
                    true,
                    "check network reachability of the [tts] endpoint",
                ),
            })?;
        let mut bytes = Vec::new();
        resp.into_reader()
            .read_to_end(&mut bytes)
            .map_err(|e| verr("TTS_HTTP", format!("speech body read: {e}"), true, "retry"))?;
        if bytes.is_empty() {
            return Err(verr(
                "TTS_EMPTY",
                "speech endpoint produced no audio".to_string(),
                true,
                "check the endpoint and voice",
            ));
        }
        Ok(TtsResult {
            bytes,
            format: req.format,
            provider: self.name().to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Bespoke provider wire helpers (pure, offline-testable)
//
// These helpers capture the documented wire shape — request builders and
// response parsers — and the live backends below are built on them (each
// backend's section cites its vendor docs). Fixtures in
// `eval/tests/providers_voice_wire.rs` pin the shapes. Nothing here touches
// the network or reads secrets. The live wires have not been exercised
// against the real APIs yet (no keys were available); treat first runs as
// verification runs.
// ---------------------------------------------------------------------------

/// Minimal base64 decoder (standard + URL-safe alphabets, padding
/// optional). Avoids a new dependency for the one place that needs it
/// (Gemini TTS responses).
pub fn b64decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    if clean.len() % 4 == 1 {
        return Err("invalid base64 length".to_string());
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= (val(c).ok_or_else(|| format!("invalid base64 char {c:?}"))? as u32)
                << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

// --- Deepgram STT: raw audio body + query params, `Authorization: Token` ---

/// Deepgram auth scheme: `Authorization: Token <key>` — Bearer does NOT work.
pub const DEEPGRAM_AUTH_SCHEME: &str = "Token";
pub const DEEPGRAM_LISTEN_URL: &str = "https://api.deepgram.com/v1/listen";

/// Build the `/v1/listen` URL: every option is a query param. Always pass
/// an explicit model — the API default is the weaker `base` model.
pub fn deepgram_listen_url(model: &str, language: Option<&str>, diarize: bool) -> String {
    let mut url = format!("{DEEPGRAM_LISTEN_URL}?model={model}&smart_format=true");
    if let Some(l) = language {
        url.push_str(&format!("&language={l}"));
    }
    if diarize {
        url.push_str("&diarize=true");
    }
    url
}

/// `results.channels[0].alternatives[0].transcript` — not `text`.
pub fn parse_deepgram_transcript(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("results")?
        .get("channels")?
        .get(0)?
        .get("alternatives")?
        .get(0)?
        .get("transcript")?
        .as_str()
        .map(str::to_string)
}

/// Extract `(detected_language, duration_secs)` from a Deepgram STT
/// response. Pure; pinned by fixtures. `metadata.duration` is seconds;
/// `detected_language` is only present when detection is enabled, so it
/// is best-effort and never an error.
pub fn parse_deepgram_meta(body: &str) -> (Option<String>, Option<f64>) {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let language = v
        .get("results")
        .and_then(|r| r.get("channels"))
        .and_then(|c| c.get(0))
        .and_then(|ch| ch.get("detected_language"))
        .and_then(|l| l.as_str())
        .map(str::to_string);
    let duration = v
        .get("metadata")
        .and_then(|m| m.get("duration"))
        .and_then(|d| d.as_f64());
    (language, duration)
}

// --- ElevenLabs STT: bespoke multipart, `xi-api-key` header ---

pub const ELEVENLABS_STT_URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";
/// ElevenLabs auth header name — NOT `Authorization: Bearer`.
pub const ELEVENLABS_API_KEY_HEADER: &str = "xi-api-key";

/// Bespoke multipart: field names differ from OpenAI (`model_id`, not
/// `model`; `language_code`; `diarize`). Response carries `text`.
pub fn elevenlabs_stt_multipart(
    model_id: &str,
    language_code: Option<&str>,
    diarize: bool,
    filename: &str,
    audio: &[u8],
) -> (String, Vec<u8>) {
    let boundary = "pantheon-boundary-7f3a9c";
    let mut body = Vec::with_capacity(audio.len() + 512);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("model_id", model_id);
    if let Some(l) = language_code {
        field("language_code", l);
    }
    field("diarize", if diarize { "true" } else { "false" });
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
            audio_mime(filename)
        )
        .as_bytes(),
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

pub fn parse_elevenlabs_transcript(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("text")?.as_str().map(str::to_string)
}

/// Extract `(language_code, audio_duration_secs)` from an ElevenLabs STT
/// response. Pure; pinned by fixtures. Both are best-effort metadata —
/// absent fields are `None`, never an error.
pub fn parse_elevenlabs_meta(body: &str) -> (Option<String>, Option<f64>) {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let language = v
        .get("language_code")
        .and_then(|l| l.as_str())
        .map(str::to_string);
    let duration = v.get("audio_duration_secs").and_then(|d| d.as_f64());
    (language, duration)
}

// --- xAI STT: bespoke path + field ordering ---

pub const XAI_STT_URL: &str = "https://api.x.ai/v1/stt";

/// xAI gotcha: option fields must be sent BEFORE the file part in the
/// multipart body.
pub fn xai_stt_multipart(
    model: &str,
    language: Option<&str>,
    filename: &str,
    audio: &[u8],
) -> (String, Vec<u8>) {
    let boundary = "pantheon-boundary-7f3a9c";
    let mut body = Vec::with_capacity(audio.len() + 512);
    let mut field = |name: &str, value: &str| {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    };
    field("model", model);
    if let Some(l) = language {
        field("language", l);
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {}\r\n\r\n",
            audio_mime(filename)
        )
        .as_bytes(),
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Parse an xAI STT response into `(text, language, duration_secs)`.
/// The docs shape is `{text, language, duration}` at the top level.
/// Returns `None` when `text` is missing or malformed — a missing
/// transcript is an error, never an empty success. Pure; pinned by
/// fixtures.
pub fn parse_xai_transcript(body: &str) -> Option<(String, Option<String>, Option<f64>)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let text = v.get("text")?.as_str().map(str::to_string)?;
    let language = v
        .get("language")
        .and_then(|l| l.as_str())
        .map(str::to_string);
    let duration = v.get("duration").and_then(|d| d.as_f64());
    Some((text, language, duration))
}

// --- AssemblyAI STT: async 3-step (upload -> transcript -> poll) ---

pub const ASSEMBLYAI_BASE_URL: &str = "https://api.assemblyai.com";
/// Async auth is the RAW key — no `Bearer` prefix (401 otherwise).
pub const ASSEMBLYAI_AUTH_SCHEME: &str = "raw-key";

/// Step 2 body: `POST /v2/transcript` with the `upload_url` from step 1.
/// The singular `speech_model` is deprecated — the current API takes the
/// plural `speech_models` array (priority-ordered fallback list). When
/// `speech_models` is empty the param is omitted entirely and the API
/// default applies (`universal-3-5-pro`, falling back to `universal-2`).
pub fn assemblyai_transcript_body(
    audio_url: &str,
    speech_models: Option<&[&str]>,
    language_code: Option<&str>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "audio_url": audio_url });
    if let Some(models) = speech_models {
        if !models.is_empty() {
            body["speech_models"] = serde_json::json!(models);
        }
    }
    if let Some(lang) = language_code {
        if !lang.trim().is_empty() {
            body["language_code"] = serde_json::json!(lang);
        }
    }
    body
}

/// Step 3: poll `GET /v2/transcript/{id}` until `completed`.
/// Returns the `status` string (`queued`/`processing`/`completed`/`error`).
pub fn assemblyai_transcript_status(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("status")?.as_str().map(str::to_string)
}

pub fn parse_assemblyai_transcript(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("text")?.as_str().map(str::to_string)
}

/// Extract `audio_duration` (seconds) from a completed AssemblyAI
/// transcript response. Pure; pinned by fixtures. Best-effort — absent
/// fields are `None`, never an error.
pub fn parse_assemblyai_duration(body: &str) -> Option<f64> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("audio_duration")?.as_f64()
}

// --- Deepgram Aura TTS: voice is the `model` query param, `Token` auth ---

/// Build the `/v1/speak` URL: the voice is a `model` query param
/// (`aura-2-thalia-en`). Omitting it silently defaults to the weaker
/// Aura-1 voice — always set it.
pub fn deepgram_speak_url(model: &str, encoding: &str) -> String {
    format!("https://api.deepgram.com/v1/speak?model={model}&encoding={encoding}")
}

/// Map the requested [`AudioFormat`] to Deepgram `/v1/speak` query
/// params: `(encoding, container, sample_rate)`. Deepgram controls the
/// output with `encoding` + `container` query params; WAV (`linear16`)
/// needs an explicit `sample_rate` so the returned container header is
/// truthful. Pure; pinned by fixtures.
pub fn deepgram_tts_format_params(
    format: AudioFormat,
) -> (&'static str, &'static str, Option<u32>) {
    match format {
        AudioFormat::Mp3 => ("mp3", "mp3", None),
        AudioFormat::Ogg => ("opus", "ogg", None),
        AudioFormat::Wav => ("linear16", "wav", Some(24_000)),
    }
}

/// The request body is just the text.
pub fn deepgram_speak_body(text: &str) -> serde_json::Value {
    serde_json::json!({ "text": text })
}

// --- Gemini TTS: generateContent, base64 raw PCM -> WAV wrap ---

/// Gemini auth: `x-goog-api-key` header (or `?key=`).
pub const GEMINI_API_KEY_HEADER: &str = "x-goog-api-key";

pub fn gemini_tts_url(model: &str) -> String {
    format!("https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent")
}

/// The standard Gemini `generateContent` surface with an audio modality;
/// style direction is prompt-level (`"Say cheerfully: ..."`).
pub fn gemini_tts_body(text: &str, voice_name: &str) -> serde_json::Value {
    serde_json::json!({
        "contents": [{ "parts": [{ "text": text }] }],
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": {
                "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": voice_name } }
            }
        }
    })
}

/// Extract `candidates[0].content.parts[0].inlineData.data`: base64 PCM,
/// 16-bit signed LE, 24 kHz mono — no container.
pub fn gemini_extract_pcm(body: &str) -> Result<Vec<u8>, PantheonError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        verr(
            "TTS_PARSE",
            format!("gemini TTS response: {e}"),
            false,
            "check the model id — preview ids churn",
        )
    })?;
    let b64 = v
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("inlineData"))
        .and_then(|d| d.get("data"))
        .and_then(|d| d.as_str())
        .ok_or_else(|| {
            verr(
                "TTS_PARSE",
                "gemini TTS response has no candidates[0].content.parts[0].inlineData.data"
                    .to_string(),
                false,
                "the model may have answered in text instead of audio — retry once",
            )
        })?;
    b64decode(b64).map_err(|e| {
        verr(
            "TTS_PARSE",
            format!("gemini TTS base64: {e}"),
            false,
            "retry",
        )
    })
}

/// Wrap raw 16-bit PCM mono at `sample_rate` in a 44-byte WAV header so
/// it can be saved or played directly.
pub fn pcm_to_wav(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    const CHANNELS: u16 = 1;
    const BITS: u16 = 16;
    // Pad to a whole sample so the header's data length stays truthful.
    let mut data = pcm.to_vec();
    if data.len() % 2 == 1 {
        data.push(0);
    }
    let data_len = data.len() as u32;
    let byte_rate = sample_rate * u32::from(CHANNELS) * u32::from(BITS) / 8;
    let block_align = CHANNELS * BITS / 8;
    let mut wav = Vec::with_capacity(44 + data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&BITS.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(&data);
    wav
}

/// Wrap raw 16-bit PCM mono @ 24 kHz (Gemini TTS output) in a 44-byte
/// WAV header. Kept as a named helper because the Gemini wire pins the
/// sample rate; general cases use [`pcm_to_wav`].
pub fn gemini_pcm_to_wav(pcm: &[u8]) -> Vec<u8> {
    pcm_to_wav(pcm, 24_000)
}

// --- FishAudio TTS: model goes in a header, billed per byte ---

pub const FISHAUDIO_TTS_URL: &str = "https://api.fish.audio/v1/tts";
/// The backend model is selected with a `model` HTTP header, not a body
/// field — the classic integration bug.
pub const FISHAUDIO_MODEL_HEADER: &str = "model";

/// FishAudio `/v1/tts` body. `reference_id` is optional in the API
/// (default null → default voice), so an empty id is omitted rather than
/// sent as an empty string. Pure; pinned by fixtures.
pub fn fishaudio_tts_body(text: &str, reference_id: &str) -> serde_json::Value {
    let mut body = serde_json::json!({
        "text": text,
        "format": "mp3",
        "normalize": true,
        "latency": "normal",
    });
    if !reference_id.trim().is_empty() {
        body["reference_id"] = serde_json::json!(reference_id);
    }
    body
}

/// Map the requested [`AudioFormat`] to FishAudio's `format` body field.
/// FishAudio supports `wav`/`pcm`/`mp3`/`opus` — an Ogg request maps to
/// Opus (same codec family as Ogg Vorbis is not offered). Pure.
pub fn fishaudio_output_format(format: AudioFormat) -> &'static str {
    match format {
        AudioFormat::Mp3 => "mp3",
        AudioFormat::Wav => "wav",
        AudioFormat::Ogg => "opus",
    }
}

// --- ElevenLabs TTS: voice id in the path, `xi-api-key` header ---

pub fn elevenlabs_tts_url(voice_id: &str) -> String {
    format!("https://api.elevenlabs.io/v1/text-to-speech/{voice_id}")
}

/// Map the requested [`AudioFormat`] to ElevenLabs' `output_format` query
/// param, returning the param value and the actual format of the bytes.
/// ElevenLabs has no ogg/opus output — Ogg requests fall back to MP3 and
/// the result says so honestly. `pcm_44100` is raw PCM, so the backend
/// wraps it in a WAV header at the matching 44.1 kHz sample rate.
pub fn elevenlabs_output_format(format: AudioFormat) -> (&'static str, AudioFormat) {
    match format {
        AudioFormat::Mp3 => ("mp3_44100_128", AudioFormat::Mp3),
        AudioFormat::Wav => ("pcm_44100", AudioFormat::Wav),
        AudioFormat::Ogg => ("mp3_44100_128", AudioFormat::Mp3),
    }
}

pub fn elevenlabs_tts_body(text: &str, model_id: &str) -> serde_json::Value {
    serde_json::json!({
        "text": text,
        "model_id": model_id,
        "voice_settings": { "stability": 0.5, "similarity_boost": 0.75 },
    })
}

// ---------------------------------------------------------------------------
// Live bespoke provider backends
//
// The providers below are implemented from their public documented wire
// shapes (helpers above; fixtures in `eval/tests/providers_voice_wire.rs`
// pin the shapes). They have NOT been exercised against the live APIs —
// no keys were available in this environment — so each backend documents
// its source doc. Wiring a key and running one transcription/synthesis is
// the remaining verification step, not more code.
// ---------------------------------------------------------------------------

/// Read the request's audio file; shared by the bespoke STT backends.
fn read_audio_input(req: &SttRequest) -> Result<(Vec<u8>, String), PantheonError> {
    let audio = std::fs::read(&req.path).map_err(|e| {
        verr(
            "STT_INPUT",
            format!("read {}: {e}", req.path.display()),
            false,
            "pass a readable audio file",
        )
    })?;
    let filename = req
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio".into());
    Ok((audio, filename))
}

/// The configured key, or a clear error naming the env var.
fn require_key(
    api_key: &Option<SecretValue>,
    backend: &str,
    env_var: &str,
) -> Result<String, PantheonError> {
    api_key
        .as_ref()
        .map(|k| k.expose().trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            verr(
                "VOICE_CONFIG",
                format!("{backend}: no API key configured (set {env_var})"),
                false,
                "configure the key, then retry",
            )
        })
}

/// First ~300 chars of a vendor error body, whitespace-collapsed.
/// Non-2xx responses carry the actionable detail (bad key, unknown
/// model, quota exhausted); the status code alone never does.
fn error_snippet(resp: ureq::Response) -> String {
    resp.into_string()
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// POST raw bytes; returns the response body. Shared by every bespoke
/// backend so timeouts and error mapping stay uniform.
fn post_bytes(
    url: &str,
    content_type: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    area: &str,
) -> Result<Vec<u8>, PantheonError> {
    let mut request = crate::http::http_agent()
        .post(url)
        .set("content-type", content_type);
    for (k, v) in headers {
        request = request.set(k, v);
    }
    let code = format!("{area}_HTTP");
    let resp = request.send_bytes(body).map_err(|e| match e {
        ureq::Error::Status(status, resp) => {
            let detail = error_snippet(resp);
            let cause = if detail.is_empty() {
                format!("endpoint: HTTP {status}")
            } else {
                format!("endpoint: HTTP {status}: {detail}")
            };
            verr(
                &code,
                cause,
                !(400..500).contains(&status),
                "check the provider, model name, and API key",
            )
        }
        e => verr(
            &code,
            format!("endpoint: {e}"),
            true,
            "check network reachability of the endpoint",
        ),
    })?;
    let mut bytes = Vec::new();
    resp.into_reader().read_to_end(&mut bytes).map_err(|e| {
        verr(
            "VOICE_HTTP_READ",
            format!("response read: {e}"),
            true,
            "retry",
        )
    })?;
    Ok(bytes)
}

/// POST a JSON body; returns the response body.
fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    area: &str,
) -> Result<Vec<u8>, PantheonError> {
    post_bytes(
        url,
        "application/json",
        headers,
        body.to_string().as_bytes(),
        area,
    )
}

/// Parse a JSON string body into transcript text via `parser`. A missing
/// or malformed transcript field is `STT_PARSE`, never a silent empty
/// transcript — an empty success hides vendor errors and broken wires.
fn parse_text_body(
    body: &[u8],
    parser: fn(&str) -> Option<String>,
) -> Result<String, PantheonError> {
    let text_body = String::from_utf8_lossy(body);
    parser(&text_body).ok_or_else(|| {
        verr(
            "STT_PARSE",
            "transcription response has no usable transcript text".to_string(),
            false,
            "check the backend's model name — vendors rename models without warning",
        )
    })
}

// --- Deepgram STT: raw audio body + query params, `Authorization: Token` ---
//
// Source: Deepgram docs (STT). Default model `nova-3` is the documented
// current top tier; the API default (`base`) is weaker, so the model is
// always sent explicitly.
// NOTE: built from Deepgram docs, not live-tested.
pub struct DeepgramStt {
    pub model: String,
    pub diarize: bool,
    pub api_key: Option<SecretValue>,
}

impl DeepgramStt {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            model: options_get(options, "model")
                .unwrap_or("nova-3")
                .to_string(),
            diarize: options_get(options, "diarize").is_some_and(|v| v == "true"),
            api_key,
        })
    }
}

impl SttProvider for DeepgramStt {
    fn name(&self) -> &str {
        "deepgram"
    }

    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        let (audio, filename) = read_audio_input(req)?;
        let key = require_key(&self.api_key, "deepgram", "DEEPGRAM_API_KEY")?;
        let url = deepgram_listen_url(&self.model, req.language.as_deref(), self.diarize);
        let auth = format!("{DEEPGRAM_AUTH_SCHEME} {key}");
        let body = post_bytes(
            &url,
            audio_mime(&filename),
            &[("authorization", auth.as_str())],
            &audio,
            "STT",
        )?;
        let text = parse_text_body(&body, parse_deepgram_transcript)?;
        let body_str = String::from_utf8_lossy(&body);
        let (detected_language, duration_secs) = parse_deepgram_meta(&body_str);
        Ok(SttResult {
            text,
            language: detected_language.or(req.language.clone()),
            duration_secs,
            provider: self.name().to_string(),
        })
    }
}

// --- ElevenLabs STT: bespoke multipart (`model_id`, `language_code`), ---
// --- `xi-api-key` header. Source: ElevenLabs docs (STT). ---
// NOTE: built from ElevenLabs docs, not live-tested.
pub struct ElevenLabsStt {
    pub model_id: String,
    pub diarize: bool,
    pub api_key: Option<SecretValue>,
}

impl ElevenLabsStt {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            model_id: options_get(options, "model")
                .or_else(|| options_get(options, "model_id"))
                .unwrap_or("scribe_v2")
                .to_string(),
            diarize: options_get(options, "diarize").is_some_and(|v| v == "true"),
            api_key,
        })
    }
}

impl SttProvider for ElevenLabsStt {
    fn name(&self) -> &str {
        "elevenlabs"
    }

    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        let (audio, filename) = read_audio_input(req)?;
        let key = require_key(&self.api_key, "elevenlabs", "ELEVENLABS_API_KEY")?;
        let (content_type, body) = elevenlabs_stt_multipart(
            &self.model_id,
            req.language.as_deref(),
            self.diarize,
            &filename,
            &audio,
        );
        let body = post_bytes(
            ELEVENLABS_STT_URL,
            &content_type,
            &[(ELEVENLABS_API_KEY_HEADER, key.as_str())],
            &body,
            "STT",
        )?;
        let text = parse_text_body(&body, parse_elevenlabs_transcript)?;
        let body_str = String::from_utf8_lossy(&body);
        let (response_language, duration_secs) = parse_elevenlabs_meta(&body_str);
        Ok(SttResult {
            text,
            language: response_language.or(req.language.clone()),
            duration_secs,
            provider: self.name().to_string(),
        })
    }
}

// --- xAI STT: `POST /v1/stt`, option fields before the file part, ---
// --- Bearer auth. Source: xAI docs (speech-to-text). ---
// NOTE: built from xAI docs, not live-tested.

pub struct XaiStt {
    pub model: String,
    pub api_key: Option<SecretValue>,
}

impl XaiStt {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            model: options_get(options, "model")
                .unwrap_or("grok-voice-transcribe-2.0")
                .to_string(),
            api_key,
        })
    }
}

impl SttProvider for XaiStt {
    fn name(&self) -> &str {
        "xai"
    }

    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        let (audio, filename) = read_audio_input(req)?;
        let key = require_key(&self.api_key, "xai", "XAI_API_KEY")?;
        let (content_type, body) =
            xai_stt_multipart(&self.model, req.language.as_deref(), &filename, &audio);
        let auth = format!("Bearer {key}");
        let body = post_bytes(
            XAI_STT_URL,
            &content_type,
            &[("authorization", auth.as_str())],
            &body,
            "STT",
        )?;
        // xAI answers `{text, language, duration}` at the top level
        // (per the current xAI speech-to-text docs). A missing `text`
        // field is STT_PARSE, never an empty transcript.
        let text_body = String::from_utf8_lossy(&body);
        let (text, response_language, duration_secs) = parse_xai_transcript(&text_body)
            .ok_or_else(|| {
                verr(
                    "STT_PARSE",
                    "xai STT response has no usable `text` field".to_string(),
                    false,
                    "check the model id — xAI model ids churn",
                )
            })?;
        Ok(SttResult {
            text,
            language: response_language.or(req.language.clone()),
            duration_secs,
            provider: self.name().to_string(),
        })
    }
}

// --- AssemblyAI STT: async 3-step (upload -> transcript -> poll). ---
// --- Raw key auth (no Bearer prefix). Source: AssemblyAI docs (STT). ---
// NOTE: built from AssemblyAI docs, not live-tested.

pub struct AssemblyAiStt {
    pub speech_model: Option<String>,
    pub api_key: Option<SecretValue>,
    pub max_polls: u32,
}

impl AssemblyAiStt {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            speech_model: options_get(options, "speech_model").map(str::to_string),
            api_key,
            max_polls: options_get(options, "max_polls")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(60),
        })
    }
}

impl SttProvider for AssemblyAiStt {
    fn name(&self) -> &str {
        "assemblyai"
    }

    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        let (audio, _filename) = read_audio_input(req)?;
        let key = require_key(&self.api_key, "assemblyai", "ASSEMBLYAI_API_KEY")?;
        let auth = [("authorization", key.as_str())];

        // Step 1: upload the audio, get an `upload_url`.
        let upload_url = format!("{ASSEMBLYAI_BASE_URL}/v2/upload");
        let body = post_bytes(
            &upload_url,
            "application/octet-stream",
            &auth,
            &audio,
            "STT",
        )?;
        let upload_url: String =
            serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&body))
                .ok()
                .and_then(|v| v.get("upload_url")?.as_str().map(str::to_string))
                .ok_or_else(|| {
                    verr(
                        "STT_PARSE",
                        "assemblyai upload: no upload_url in response".to_string(),
                        false,
                        "check the API key",
                    )
                })?;

        // Step 2: submit the transcript job. The plural `speech_models`
        // array is the current API shape (the singular `speech_model` is
        // deprecated); it is omitted unless configured — the API then
        // defaults to `universal-3-5-pro`, falling back to `universal-2`.
        // The request language is forwarded as `language_code`.
        let models: Vec<&str> = self
            .speech_model
            .as_deref()
            .map(|m| vec![m])
            .unwrap_or_default();
        let submit = assemblyai_transcript_body(
            &upload_url,
            Some(models.as_slice()),
            req.language.as_deref(),
        );
        let transcript_url = format!("{ASSEMBLYAI_BASE_URL}/v2/transcript");
        let body = post_json(&transcript_url, &auth, &submit, "STT")?;
        let job_id: String =
            serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&body))
                .ok()
                .and_then(|v| v.get("id")?.as_str().map(str::to_string))
                .ok_or_else(|| {
                    verr(
                        "STT_PARSE",
                        "assemblyai: no transcript id in response".to_string(),
                        false,
                        "check the API key and audio",
                    )
                })?;

        // Step 3: poll until completed (bounded; ~2s cadence).
        let poll_url = format!("{transcript_url}/{job_id}");
        for _ in 0..self.max_polls.max(1) {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let mut request = crate::http::http_agent().get(&poll_url);
            for (k, v) in &auth {
                request = request.set(k, v);
            }
            let resp = request.call().map_err(|e| match e {
                ureq::Error::Status(status, resp) => {
                    let detail = error_snippet(resp);
                    let cause = if detail.is_empty() {
                        format!("assemblyai poll: HTTP {status}")
                    } else {
                        format!("assemblyai poll: HTTP {status}: {detail}")
                    };
                    verr(
                        "STT_HTTP",
                        cause,
                        !(400..500).contains(&status),
                        "check the API key",
                    )
                }
                e => verr(
                    "STT_HTTP",
                    format!("assemblyai poll: {e}"),
                    true,
                    "check network reachability",
                ),
            })?;
            let mut bytes = Vec::new();
            resp.into_reader()
                .read_to_end(&mut bytes)
                .map_err(|e| verr("VOICE_HTTP_READ", format!("poll read: {e}"), true, "retry"))?;
            let body_str = String::from_utf8_lossy(&bytes);
            match assemblyai_transcript_status(&body_str).as_deref() {
                Some("completed") => {
                    let text = parse_assemblyai_transcript(&body_str).ok_or_else(|| {
                        verr(
                            "STT_PARSE",
                            "assemblyai: completed job has no transcript text".to_string(),
                            false,
                            "check the audio and the speech_models override",
                        )
                    })?;
                    return Ok(SttResult {
                        text,
                        language: req.language.clone(),
                        duration_secs: parse_assemblyai_duration(&body_str),
                        provider: self.name().to_string(),
                    });
                }
                Some("error") => {
                    return Err(verr(
                        "STT_JOB",
                        format!(
                            "assemblyai job failed: {}",
                            body_str.chars().take(200).collect::<String>()
                        ),
                        false,
                        "check the audio file",
                    ));
                }
                _ => continue,
            }
        }
        Err(verr(
            "STT_TIMEOUT",
            format!("assemblyai job {job_id} did not complete in time"),
            true,
            "raise max_polls in [stt] options and retry",
        ))
    }
}

// --- ElevenLabs TTS: voice id in the path, `xi-api-key` header. ---
// --- Source: ElevenLabs docs (TTS). ---
// NOTE: built from ElevenLabs docs, not live-tested.
pub struct ElevenLabsTts {
    pub voice_id: String,
    pub model_id: String,
    pub api_key: Option<SecretValue>,
}

impl ElevenLabsTts {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            voice_id: require_option(options, "voice")?.to_string(),
            model_id: options_get(options, "model")
                .unwrap_or("eleven_turbo_v2_5")
                .to_string(),
            api_key,
        })
    }
}

impl TtsProvider for ElevenLabsTts {
    fn name(&self) -> &str {
        "elevenlabs"
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        let voice = req.voice.clone().unwrap_or_else(|| self.voice_id.clone());
        let key = require_key(&self.api_key, "elevenlabs", "ELEVENLABS_API_KEY")?;
        let (output_format, actual_format) = elevenlabs_output_format(req.format);
        let url = format!(
            "{}?output_format={output_format}",
            elevenlabs_tts_url(&voice)
        );
        let body = elevenlabs_tts_body(&req.text, &self.model_id);
        let bytes = post_json(
            &url,
            &[(ELEVENLABS_API_KEY_HEADER, key.as_str())],
            &body,
            "TTS",
        )?;
        if bytes.is_empty() {
            return Err(verr(
                "TTS_EMPTY",
                "elevenlabs produced no audio".to_string(),
                true,
                "check the voice id",
            ));
        }
        // `pcm_*` output formats are raw PCM — wrap in a WAV header so
        // the bytes match the reported format.
        let bytes = match actual_format {
            AudioFormat::Wav => pcm_to_wav(&bytes, 44_100),
            _ => bytes,
        };
        Ok(TtsResult {
            bytes,
            format: actual_format,
            provider: self.name().to_string(),
        })
    }
}

// --- Deepgram Aura TTS: voice is the `model` query param, ---
// --- `Authorization: Token` auth. Source: Deepgram docs (TTS). ---
// NOTE: built from Deepgram docs, not live-tested.
pub struct DeepgramTts {
    pub voice_model: String,
    pub api_key: Option<SecretValue>,
}

impl DeepgramTts {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            voice_model: options_get(options, "voice")
                .or_else(|| options_get(options, "model"))
                .unwrap_or("aura-2-thalia-en")
                .to_string(),
            api_key,
        })
    }
}

impl TtsProvider for DeepgramTts {
    fn name(&self) -> &str {
        "deepgram"
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        let voice = req
            .voice
            .clone()
            .unwrap_or_else(|| self.voice_model.clone());
        let key = require_key(&self.api_key, "deepgram", "DEEPGRAM_API_KEY")?;
        let (encoding, container, sample_rate) = deepgram_tts_format_params(req.format);
        let mut url = format!(
            "{}&container={container}",
            deepgram_speak_url(&voice, encoding)
        );
        if let Some(sr) = sample_rate {
            url.push_str(&format!("&sample_rate={sr}"));
        }
        let auth = format!("{DEEPGRAM_AUTH_SCHEME} {key}");
        let body = deepgram_speak_body(&req.text);
        let bytes = post_json(&url, &[("authorization", auth.as_str())], &body, "TTS")?;
        if bytes.is_empty() {
            return Err(verr(
                "TTS_EMPTY",
                "deepgram produced no audio".to_string(),
                true,
                "check the voice model",
            ));
        }
        Ok(TtsResult {
            bytes,
            format: req.format,
            provider: self.name().to_string(),
        })
    }
}

// --- Gemini TTS: `generateContent` with audio modality, base64 raw PCM ---
// --- in the response, wrapped in a WAV header. `x-goog-api-key` header. ---
// --- Source: Gemini docs (TTS). Preview model ids churn — ---
// --- verify the current id if synthesis 404s. ---
// NOTE: built from Gemini docs, not live-tested.
//
// Gemini TTS always returns 16-bit PCM @ 24 kHz, so the backend always
// produces WAV; non-WAV requests fail explicitly rather than silently
// returning the wrong container.
pub struct GeminiTts {
    pub model: String,
    pub api_key: Option<SecretValue>,
}

impl GeminiTts {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            model: options_get(options, "model")
                .unwrap_or("gemini-2.5-flash-preview-tts")
                .to_string(),
            api_key,
        })
    }
}

impl TtsProvider for GeminiTts {
    fn name(&self) -> &str {
        "gemini"
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        let voice = req.voice.clone().unwrap_or_else(|| "Kore".to_string());
        let key = require_key(&self.api_key, "gemini", "GEMINI_API_KEY")?;
        let url = gemini_tts_url(&self.model);
        let body = gemini_tts_body(&req.text, &voice);
        let bytes = post_json(&url, &[(GEMINI_API_KEY_HEADER, key.as_str())], &body, "TTS")?;
        let body_str = String::from_utf8_lossy(&bytes);
        let pcm = gemini_extract_pcm(&body_str)?;
        Ok(TtsResult {
            bytes: gemini_pcm_to_wav(&pcm),
            format: AudioFormat::Wav,
            provider: self.name().to_string(),
        })
    }
}

// --- FishAudio TTS: backend model in the `model` HTTP header, ---
// --- `Authorization: Bearer` auth. Source: FishAudio docs (TTS). ---
// NOTE: built from FishAudio docs, not live-tested.
//
// `reference_id` is optional in the FishAudio API (the request then uses
// the default voice); the model header defaults to the API's production
// recommendation.
pub struct FishAudioTts {
    pub backend_model: String,
    pub reference_id: Option<String>,
    pub api_key: Option<SecretValue>,
}

impl FishAudioTts {
    pub fn from_options(
        options: &HashMap<String, String>,
        api_key: Option<SecretValue>,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            backend_model: options_get(options, "model")
                .unwrap_or("s2.1-pro")
                .to_string(),
            reference_id: options_get(options, "voice").map(str::to_string),
            api_key,
        })
    }
}

impl TtsProvider for FishAudioTts {
    fn name(&self) -> &str {
        "fishaudio"
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        let reference_id = req
            .voice
            .clone()
            .or_else(|| self.reference_id.clone())
            .unwrap_or_default();
        let key = require_key(&self.api_key, "fishaudio", "FISH_API_KEY")?;
        let auth = format!("Bearer {key}");
        let mut body = fishaudio_tts_body(&req.text, &reference_id);
        body["format"] = serde_json::json!(fishaudio_output_format(req.format));
        let bytes = post_json(
            FISHAUDIO_TTS_URL,
            &[
                ("authorization", auth.as_str()),
                (FISHAUDIO_MODEL_HEADER, self.backend_model.as_str()),
            ],
            &body,
            "TTS",
        )?;
        if bytes.is_empty() {
            return Err(verr(
                "TTS_EMPTY",
                "fishaudio produced no audio".to_string(),
                true,
                "check the reference id and model header",
            ));
        }
        Ok(TtsResult {
            bytes,
            format: req.format,
            provider: self.name().to_string(),
        })
    }
}

// --- Kokoro (local): community CLIs differ in arg shape, so `cmd` is ---
// --- the binary and `args` is an explicit args template ---
// --- (`{voice}`/`{format}` placeholders; text on stdin, audio WAV on ---
// --- stdout). Detection covers availability; the command is explicit. ---

pub struct KokoroTts {
    inner: CommandTts,
}

impl KokoroTts {
    pub fn from_options(options: &HashMap<String, String>) -> Result<Self, PantheonError> {
        let cmd = options_get(options, "cmd").map(str::to_string).or_else(|| {
            detect_kokoro()
                .and_then(|k| k.cli)
                .map(|p| p.to_string_lossy().into_owned())
        });
        let Some(cmd) = cmd else {
            return Err(verr(
                "VOICE_CONFIG",
                "kokoro-local: no kokoro CLI found on PATH and no `cmd` given".to_string(),
                false,
                "pip install kokoro-onnx soundfile (plus a kokoro CLI), or set cmd in [tts] options",
            ));
        };
        if options_get(options, "args").is_none() {
            return Err(verr(
                "VOICE_CONFIG",
                "kokoro-local: community kokoro CLIs differ in arg shape — set `args` explicitly".to_string(),
                false,
                "set args in [tts] options, e.g. the flags your kokoro CLI expects for text-on-stdin / wav-on-stdout",
            ));
        }
        let timeout_secs = options_get(options, "timeout_secs")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
        Ok(Self {
            inner: CommandTts {
                cmd,
                args: options_get(options, "args").unwrap_or("").to_string(),
                default_voice: options_get(options, "voice").map(String::from),
                name: "kokoro-local".to_string(),
                timeout: Duration::from_secs(timeout_secs),
            },
        })
    }
}

impl TtsProvider for KokoroTts {
    fn name(&self) -> &str {
        "kokoro-local"
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        self.inner.synthesize(req).map(|mut r| {
            r.provider = self.name().to_string();
            r
        })
    }
}

// --- Fish Speech (local, self-hosted) --------------------------------------
// Fish Speech is the open model behind FishAudio (fishaudio/fish-speech).
// GPU-class: ~24 GB VRAM recommended; CPU-only works but slowly. Local
// inference is a multi-step Python workflow (see the fish-speech docs —
// CLI flags move fast), so like kokoro the command is explicit and the
// runtime never guesses args. License caution: 1.5 was CC-BY-NC-SA 4.0 —
// verify the S2/S2-Pro license before any commercial use.

pub struct FishSpeechTts {
    inner: CommandTts,
}

impl FishSpeechTts {
    pub fn from_options(options: &HashMap<String, String>) -> Result<Self, PantheonError> {
        let cmd = options_get(options, "cmd")
            .map(str::to_string)
            .or_else(|| which_binary("fish_speech").map(|p| p.to_string_lossy().into_owned()));
        let Some(cmd) = cmd else {
            return Err(verr(
                "VOICE_CONFIG",
                "fishspeech-local: no `fish_speech` binary on PATH and no `cmd` given".to_string(),
                false,
                "pip install fish-speech (24 GB GPU recommended; check the S2 license), or set cmd in [tts] options",
            ));
        };
        if options_get(options, "args").is_none() {
            return Err(verr(
                "VOICE_CONFIG",
                "fishspeech-local: fish-speech CLI flags move fast — set `args` explicitly".to_string(),
                false,
                "set args in [tts] options, e.g. `infer --reference-audio ref.wav --reference-text \"...\"` for your install",
            ));
        }
        let timeout_secs = options_get(options, "timeout_secs")
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
        Ok(Self {
            inner: CommandTts {
                cmd,
                args: options_get(options, "args").unwrap_or("").to_string(),
                default_voice: options_get(options, "voice").map(String::from),
                name: "fishspeech-local".to_string(),
                timeout: Duration::from_secs(timeout_secs),
            },
        })
    }
}

impl TtsProvider for FishSpeechTts {
    fn name(&self) -> &str {
        "fishspeech-local"
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        self.inner.synthesize(req).map(|mut r| {
            r.provider = self.name().to_string();
            r
        })
    }
}

// ---------------------------------------------------------------------------
// Local binary detection (PATH probes; no shell)
//
// `open_*` uses these to fill in defaults (piper binary, kokoro CLI,
// fish_speech CLI) when no explicit command is configured; the setup
// wizard instead runs the registry's `detect_cmd` shell snippets through
// its own probe. Filesystem probes only — never keys.
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Find `name` on PATH without invoking a shell (`which` equivalent).
/// Rejects names containing path separators.
pub fn which_binary(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// A piper install: the binary plus any downloaded voice models.
pub struct PiperInstall {
    pub binary: PathBuf,
    /// `<voice>.onnx` files found in the conventional voice dirs.
    pub voices: Vec<PathBuf>,
}

/// Conventional piper voice-model dirs, in search order.
pub fn piper_voice_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("PANTHEON_PIPER_MODELS") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/piper/voices"));
        dirs.push(home.join(".cache/piper"));
        dirs.push(home.join("piper/voices"));
    }
    dirs.push(PathBuf::from("/usr/share/piper-voices"));
    dirs
}

/// Bounded recursive file walk: collects files under `dir` (descending at
/// most `depth` levels) for which `want` holds. One walker for the piper
/// voice-model search and the kokoro asset search, which were previously
/// two near-identical recursive functions differing only in the file
/// predicate.
fn collect_files<F>(dir: &Path, depth: u8, want: &F, out: &mut Vec<PathBuf>)
where
    F: Fn(&Path) -> bool,
{
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && want(&path) {
            out.push(path);
        } else if path.is_dir() && depth > 0 {
            collect_files(&path, depth - 1, want, out);
        }
    }
}

/// Resolve a piper voice (id like `en_US-lessac-high`, or a local `.onnx`
/// path) to a model file: the path itself when it exists, else a bounded
/// recursive search of `extra_dirs` then the conventional voice dirs.
pub fn resolve_piper_voice(voice: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
    let direct = PathBuf::from(voice);
    if direct.is_file() {
        return Some(direct);
    }
    let file_name = format!("{voice}.onnx");
    let mut dirs: Vec<PathBuf> = extra_dirs.to_vec();
    dirs.extend(piper_voice_dirs());
    let mut found = Vec::new();
    for dir in &dirs {
        collect_files(
            dir,
            4,
            &|p: &Path| p.file_name().is_some_and(|n| n == file_name.as_str()),
            &mut found,
        );
        if let Some(first) = found.first() {
            return Some(first.clone());
        }
    }
    None
}

/// Probe for a piper install: binary on PATH plus any voice assets in the
/// conventional dirs. `None` = no binary (voices alone are not enough).
pub fn detect_piper() -> Option<PiperInstall> {
    let binary = which_binary("piper")?;
    let mut voices = Vec::new();
    for dir in piper_voice_dirs() {
        collect_files(
            &dir,
            4,
            &|p: &Path| p.extension().is_some_and(|e| e == "onnx"),
            &mut voices,
        );
    }
    Some(PiperInstall { binary, voices })
}

/// A kokoro install: CLI on PATH and/or the ONNX model assets.
pub struct KokoroInstall {
    pub cli: Option<PathBuf>,
    /// `kokoro-v1.0.onnx`, when found in the conventional model dirs.
    pub model: Option<PathBuf>,
}

pub fn kokoro_model_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("PANTHEON_KOKORO_MODELS") {
        dirs.push(PathBuf::from(d));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/share/kokoro"));
        dirs.push(home.join(".cache/kokoro"));
    }
    dirs
}

/// Probe for kokoro: a community CLI on PATH and/or the model assets.
/// `None` = neither found.
pub fn detect_kokoro() -> Option<KokoroInstall> {
    let cli = which_binary("kokoro-onnx")
        .or_else(|| which_binary("kokoro-tts"))
        .or_else(|| which_binary("kokoro"));
    let mut model = None;
    for dir in kokoro_model_dirs() {
        let mut found = Vec::new();
        collect_files(
            &dir,
            2,
            &|p: &Path| p.file_name().is_some_and(|n| n == "kokoro-v1.0.onnx"),
            &mut found,
        );
        if let Some(first) = found.into_iter().next() {
            model = Some(first);
            break;
        }
    }
    match (cli, model) {
        (None, None) => None,
        (cli, model) => Some(KokoroInstall { cli, model }),
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Backend kind, mirroring `pantheon-memory`'s `BackendKind`, extended to
/// distinguish local vs cloud: the setup wizard shows install/detection
/// UX for local backends and an API-key prompt for cloud ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceBackendKind {
    /// Local binary run as a subprocess (whisper.cpp, piper, espeak-ng).
    /// No network egress, no key.
    Subprocess,
    /// HTTP service on the local machine (self-hosted; no external
    /// egress). Reserved for local API servers; no catalog entries yet.
    HttpLocal,
    /// Remote cloud HTTP API (network egress; usually needs a key).
    HttpCloud,
}

/// How a backend authenticates, for the setup wizard's key prompt.
/// The key itself is always resolved by the caller via the secrets
/// broker/env — this crate never reads it and never logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthRequirement {
    /// No key: local/offline backend.
    None,
    /// API key taken from this env var.
    ApiKey { env_var: &'static str },
}

/// Catalog row for one registered backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VoiceBackendInfo {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: VoiceBackendKind,
    /// The setup wizard lists recommended backends first.
    pub recommended: bool,
    /// Key requirement; `ApiKey` names the env var the wizard prompts for.
    pub auth: AuthRequirement,
    /// One-line setup hint for the wizard (voice selection, key source,
    /// wire quirks). Sticks to research-verified facts — no unverified
    /// pricing or free-tier claims.
    pub setup_note: &'static str,
    /// Local-install metadata for `Subprocess` backends: drives the
    /// wizard's detect → install-or-skip flow. `None` for cloud backends.
    pub local: Option<VoiceLocalInstall>,
    /// Extra `[stt]`/`[tts]` options the wizard collects, in order —
    /// voice selection, command invocations, model ids. The wizard asks
    /// these verbatim, so a new provider never needs a wizard change.
    pub setup_fields: Vec<VoiceSetupField>,
}

/// Local-install metadata for one `Subprocess` voice backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VoiceLocalInstall {
    /// Shell snippet the wizard runs to detect an existing install.
    pub detect_cmd: &'static str,
    /// What the wizard prints when nothing is detected.
    pub install_hint: &'static str,
    /// One-shot install command the wizard may run with approval
    /// (`None` = no verified one-liner across distros).
    pub install_cmd: Option<&'static str>,
}

/// One wizard-collected option for a voice backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VoiceSetupField {
    /// `[stt]`/`[tts]` option key.
    pub key: &'static str,
    /// Wizard prompt shown to the user.
    pub prompt: &'static str,
    /// Prefilled default (empty = no default).
    pub default: &'static str,
    pub required: bool,
}

/// Registered STT backends (for `doctor` / `providers` listings), ranked
/// with the OpenAI-wire family (groq, openai, mistral — one shared
/// implementation, base URL + key config) first, then the bespoke-wire
/// providers (implemented from public docs; not yet exercised against
/// the live APIs), then the local `command` backend.
pub fn stt_backends() -> Vec<VoiceBackendInfo> {
    vec![
        VoiceBackendInfo {
            name: "groq",
            label: "Groq Whisper (recommended)",
            kind: VoiceBackendKind::HttpCloud,
            recommended: true,
            auth: AuthRequirement::ApiKey {
                env_var: "GROQ_API_KEY",
            },
            setup_note: "Whisper via Groq, OpenAI-compatible wire (POST /openai/v1/audio/transcriptions). Free plan with quotas (20 RPM / 2,000 RPD). Default model whisper-large-v3-turbo.",
            local: None,
            setup_fields: Vec::new(),
        },
        VoiceBackendInfo {
            name: "openai",
            label: "OpenAI Whisper",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "OPENAI_API_KEY",
            },
            setup_note: "The reference Whisper API (POST /v1/audio/transcriptions). No free tier; cheapest quality pick is gpt-4o-mini-transcribe.",
            local: None,
            setup_fields: Vec::new(),
        },
        VoiceBackendInfo {
            name: "mistral",
            label: "Mistral Voxtral",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "MISTRAL_API_KEY",
            },
            setup_note: "Voxtral, OpenAI-compatible wire (POST /v1/audio/transcriptions). Batch diarization + word timestamps; default model voxtral-mini-latest.",
            local: None,
            setup_fields: Vec::new(),
        },
        VoiceBackendInfo {
            name: "deepgram",
            label: "Deepgram Nova",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "DEEPGRAM_API_KEY",
            },
            setup_note: "Nova-3 via bespoke wire: raw audio body + query params, `Authorization: Token` (not Bearer). Best streaming story. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: Vec::new(),
        },
        VoiceBackendInfo {
            name: "elevenlabs",
            label: "ElevenLabs Scribe",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "ELEVENLABS_API_KEY",
            },
            setup_note: "Scribe v2 (POST /v1/speech-to-text): bespoke multipart (`model_id`, `language_code`), `xi-api-key` header. Note: stores data by default. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: Vec::new(),
        },
        VoiceBackendInfo {
            name: "xai",
            label: "xAI Grok STT",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "XAI_API_KEY",
            },
            setup_note: "Grok STT at POST /v1/stt (not /audio/transcriptions): bespoke multipart, option fields before the file part. Default model grok-voice-transcribe-2.0. Not Groq. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: Vec::new(),
        },
        VoiceBackendInfo {
            name: "assemblyai",
            label: "AssemblyAI",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "ASSEMBLYAI_API_KEY",
            },
            setup_note: "Async 3-step flow (upload, transcript, poll). Auth is the raw key, no Bearer prefix. Sends the current `speech_models` plural array and forwards `language_code` when set. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "speech_model",
                    prompt: "Model override (empty = API default: universal-3-5-pro, fallback universal-2)",
                    default: "",
                    required: false,
                },
            ],
        },
        VoiceBackendInfo {
            name: "command",
            label: "Local STT binary (whisper.cpp, ...)",
            kind: VoiceBackendKind::Subprocess,
            recommended: false,
            auth: AuthRequirement::None,
            setup_note: "Local binary; stdout is the transcript. Takes an explicit `cmd`; nothing is auto-detected.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "cmd",
                    prompt: "STT command binary (e.g. `whisper-cli`; extra args go below)",
                    default: "",
                    required: true,
                },
                VoiceSetupField {
                    key: "args",
                    prompt: "Extra args (`{file}`, `{language}` placeholders; e.g. `-m model.bin -f {file} -l {language} --no-prints`)",
                    default: "",
                    required: false,
                },
            ],
        },
    ]
}

/// Registered TTS backends, ranked local first (piper recommended
/// default, kokoro quality upgrade), then the cloud providers (bespoke
/// wires implemented from public docs; not yet exercised against the
/// live APIs).
pub fn tts_backends() -> Vec<VoiceBackendInfo> {
    vec![
        VoiceBackendInfo {
            name: "piper-local",
            label: "Piper (local, recommended)",
            kind: VoiceBackendKind::Subprocess,
            recommended: true,
            auth: AuthRequirement::None,
            setup_note: "Free, offline, no key. Needs the piper binary + a voice (.onnx + .onnx.json); espeak-ng ships bundled as the phonemizer. Vetted default voice: en_US-lessac-high.",
            local: Some(VoiceLocalInstall {
                detect_cmd: "command -v piper",
                install_hint: "install piper (github.com/rhasspy/piper), then download a voice model (<voice>.onnx + <voice>.onnx.json) from huggingface.co/rhasspy/piper-voices — vetted default: en_US-lessac-high",
                // No verified one-liner across distros: the hint is the
                // install path.
                install_cmd: None,
            }),
            setup_fields: vec![
                VoiceSetupField {
                    key: "voice",
                    prompt: "Piper voice model name",
                    default: "en_US-lessac-high",
                    required: true,
                },
                VoiceSetupField {
                    key: "model_dir",
                    prompt: "Voice model directory (empty = auto-search)",
                    default: "",
                    required: false,
                },
            ],
        },
        VoiceBackendInfo {
            name: "kokoro-local",
            label: "Kokoro-82M (local quality upgrade)",
            kind: VoiceBackendKind::Subprocess,
            recommended: false,
            auth: AuthRequirement::None,
            setup_note: "Best-sounding local engine, but needs Python + onnxruntime and a ~340 MB download. Fixed voice presets (af_heart, ...). Takes an explicit `cmd`; community CLIs differ in arg shape.",
            local: Some(VoiceLocalInstall {
                detect_cmd: "command -v kokoro-onnx || command -v kokoro-tts || command -v kokoro",
                install_hint: "pip install kokoro-onnx soundfile (+ espeakng-loader); download kokoro-v1.0.onnx + voices-v1.0.bin",
                install_cmd: Some("pip install kokoro-onnx soundfile"),
            }),
            setup_fields: vec![
                VoiceSetupField {
                    key: "cmd",
                    prompt: "Kokoro command binary (args go in `args` below; text is piped on stdin)",
                    default: "",
                    required: true,
                },
                VoiceSetupField {
                    key: "args",
                    prompt: "Kokoro args (`{voice}`/`{format}` placeholders; text is piped on stdin, audio read from stdout)",
                    default: "",
                    required: true,
                },
            ],
        },
        VoiceBackendInfo {
            name: "fishspeech-local",
            label: "Fish Speech (local self-host)",
            kind: VoiceBackendKind::Subprocess,
            recommended: false,
            auth: AuthRequirement::None,
            setup_note: "The open model behind FishAudio (S2/S2-Pro, 80+ languages, zero-shot cloning). GPU-class (~24 GB VRAM recommended); CLI flags move fast, so takes an explicit `cmd`. License caution: 1.5 was CC-BY-NC-SA — verify before commercial use.",
            local: Some(VoiceLocalInstall {
                detect_cmd: "command -v fish_speech",
                install_hint: "pip install fish-speech, then huggingface-cli download fishaudio/s2-pro; needs portaudio19-dev libsox-dev ffmpeg; ~24 GB GPU recommended. License: verify the S2/S2-Pro terms (1.5 was CC-BY-NC-SA 4.0).",
                install_cmd: Some("pip install fish-speech"),
            }),
            setup_fields: vec![
                VoiceSetupField {
                    key: "cmd",
                    prompt: "Fish Speech command binary (args go in `args` below)",
                    default: "",
                    required: true,
                },
                VoiceSetupField {
                    key: "args",
                    prompt: "Fish Speech args (see the fish-speech inference docs; flags move fast)",
                    default: "",
                    required: true,
                },
            ],
        },
        VoiceBackendInfo {
            name: "openai",
            label: "OpenAI TTS",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "OPENAI_API_KEY",
            },
            setup_note: "The wire standard (POST /v1/audio/speech). Fixed voice slugs (alloy, nova, ...); 4,096 chars/request; no free tier.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "voice",
                    prompt: "Voice (alloy, ash, ballad, coral, echo, fable, nova, onyx, sage, shimmer)",
                    default: "alloy",
                    required: true,
                },
                VoiceSetupField {
                    key: "model",
                    prompt: "Model (gpt-4o-mini-tts, tts-1, tts-1-hd)",
                    default: "gpt-4o-mini-tts",
                    required: false,
                },
            ],
        },
        VoiceBackendInfo {
            name: "elevenlabs",
            label: "ElevenLabs TTS",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "ELEVENLABS_API_KEY",
            },
            setup_note: "Best cloud voices + cloning; `xi-api-key` header (not Bearer). Voices are UUIDs — pick live from GET /v1/voices. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "voice",
                    prompt: "ElevenLabs voice ID (UUID — pick from GET /v1/voices)",
                    default: "",
                    required: true,
                },
                VoiceSetupField {
                    key: "model",
                    prompt: "Model (empty = eleven_turbo_v2_5)",
                    default: "",
                    required: false,
                },
            ],
        },
        VoiceBackendInfo {
            name: "deepgram",
            label: "Deepgram Aura TTS",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "DEEPGRAM_API_KEY",
            },
            setup_note: "Aura-2: `Authorization: Token` (not Bearer); the voice is the `model` query param (e.g. aura-2-thalia-en); 2,000 chars/request. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "voice",
                    prompt: "Aura-2 voice model (empty = aura-2-thalia-en)",
                    default: "aura-2-thalia-en",
                    required: false,
                },
            ],
        },
        VoiceBackendInfo {
            name: "gemini",
            label: "Gemini TTS",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "GEMINI_API_KEY",
            },
            setup_note: "Shares your Gemini key (`x-goog-api-key`); returns base64 raw PCM (16-bit/24 kHz mono) — we wrap it as WAV. ~30 fixed voices (Kore, ...); preview model ids change. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "voice",
                    prompt: "Gemini voice name (empty = Kore)",
                    default: "Kore",
                    required: false,
                },
                VoiceSetupField {
                    key: "model",
                    prompt: "Model (empty = gemini-2.5-flash-preview-tts)",
                    default: "",
                    required: false,
                },
            ],
        },
        VoiceBackendInfo {
            name: "fishaudio",
            label: "FishAudio TTS",
            kind: VoiceBackendKind::HttpCloud,
            recommended: false,
            auth: AuthRequirement::ApiKey {
                env_var: "FISH_API_KEY",
            },
            setup_note: "Cloning-first; the backend model goes in the `model` HTTP header, not the body. Billed per UTF-8 byte — CJK costs 3-4x per character. Wire implemented from public docs; not yet exercised against the live API.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "voice",
                    prompt: "FishAudio voice/model ID (reference_id; empty = default voice)",
                    default: "",
                    required: false,
                },
            ],
        },
        VoiceBackendInfo {
            name: "command",
            label: "Local TTS binary (piper, espeak-ng)",
            kind: VoiceBackendKind::Subprocess,
            recommended: false,
            auth: AuthRequirement::None,
            setup_note: "Local binary: text on stdin, audio on stdout. Takes an explicit `cmd`; nothing is auto-detected.",
            local: None,
            setup_fields: vec![
                VoiceSetupField {
                    key: "cmd",
                    prompt: "TTS command binary (e.g. `piper`; extra args go below)",
                    default: "",
                    required: true,
                },
                VoiceSetupField {
                    key: "args",
                    prompt: "Extra args (`{voice}`, `{format}` placeholders; text is piped on stdin, audio read from stdout)",
                    default: "",
                    required: false,
                },
            ],
        },
    ]
}

/// Wizard-facing STT catalog: the registered backends, recommended first
/// (stable — the documented rank order is preserved within each tier).
pub fn stt_providers() -> Vec<VoiceBackendInfo> {
    let mut v = stt_backends();
    v.sort_by_key(|b| !b.recommended);
    v
}

/// Wizard-facing TTS catalog: the registered backends, recommended first
/// (stable).
pub fn tts_providers() -> Vec<VoiceBackendInfo> {
    let mut v = tts_backends();
    v.sort_by_key(|b| !b.recommended);
    v
}

/// Env var holding the API key for a named voice backend, from the
/// registry's `auth` metadata. `None` = keyless or unknown name.
pub fn voice_key_env(name: &str) -> Option<&'static str> {
    stt_providers()
        .into_iter()
        .chain(tts_providers())
        .find_map(|b| match (b.name == name, b.auth) {
            (true, AuthRequirement::ApiKey { env_var }) => Some(env_var),
            _ => None,
        })
}

/// Instantiate the configured STT backend. `Err` = misconfigured/unknown
/// backend; callers treat it as "STT unavailable", never as a model error.
///
/// Named cloud backends: `groq` and `mistral` share the OpenAI-compatible
/// implementation (base URL + key config); `deepgram`, `elevenlabs`,
/// `xai`, and `assemblyai` have bespoke live backends built from their
/// documented wire shapes (see the helpers above). None of the bespoke
/// wires has been exercised against the live API yet — no keys were
/// available — so treat first runs as verification runs.
pub fn open_stt(
    backend: &str,
    options: &HashMap<String, String>,
    api_key: Option<SecretValue>,
) -> Result<Box<dyn SttProvider>, PantheonError> {
    match backend {
        "command" => Ok(Box::new(CommandStt::from_options(options)?)),
        "openai" | "groq" | "mistral" => {
            // A named backend is its own provider id; the legacy "openai"
            // backend keeps reading options["provider"].
            let mut opts = options.clone();
            opts.entry("provider".to_string())
                .or_insert_with(|| backend.to_string());
            Ok(Box::new(HttpStt::from_options(&opts, api_key)?))
        }
        "deepgram" => Ok(Box::new(DeepgramStt::from_options(options, api_key)?)),
        "elevenlabs" => Ok(Box::new(ElevenLabsStt::from_options(options, api_key)?)),
        "xai" => Ok(Box::new(XaiStt::from_options(options, api_key)?)),
        "assemblyai" => Ok(Box::new(AssemblyAiStt::from_options(options, api_key)?)),
        other => Err(verr(
            "VOICE_BACKEND_UNKNOWN",
            format!(
                "unknown STT backend {other:?}; registered: {}",
                stt_backends()
                    .iter()
                    .map(|b| b.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            false,
            "check [stt].backend in config.toml",
        )),
    }
}

/// Build a `piper-local` TTS backend: the piper binary (explicit
/// `piper_binary`/`cmd` option, else PATH detection) plus a resolved
/// voice model, mapped onto the tested `CommandTts` machinery.
fn open_piper_tts(options: &HashMap<String, String>) -> Result<CommandTts, PantheonError> {
    let cmd = options_get(options, "piper_binary")
        .or_else(|| options_get(options, "cmd"))
        .map(str::to_string)
        .or_else(|| detect_piper().map(|p| p.binary.to_string_lossy().into_owned()))
        .ok_or_else(|| {
            verr(
                "VOICE_CONFIG",
                "piper-local: no piper binary found on PATH".to_string(),
                false,
                "install piper (github.com/rhasspy/piper) or set piper_binary in [tts] options",
            )
        })?;
    let voice = require_option(options, "voice")?;
    let mut extra_dirs = Vec::new();
    if let Some(d) = options_get(options, "model_dir") {
        extra_dirs.push(PathBuf::from(d));
    }
    let voice_path = resolve_piper_voice(voice, &extra_dirs).ok_or_else(|| {
        verr(
            "VOICE_CONFIG",
            format!("piper-local: voice model for {voice:?} not found"),
            false,
            "download <voice>.onnx + <voice>.onnx.json from https://huggingface.co/rhasspy/piper-voices (vetted default: en_US-lessac-high)",
        )
    })?;
    let timeout_secs = options_get(options, "timeout_secs")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS);
    Ok(CommandTts {
        cmd,
        args: options_get(options, "args")
            .unwrap_or("--model {voice} --output_file -")
            .to_string(),
        default_voice: Some(voice_path.to_string_lossy().into_owned()),
        name: "piper-local".to_string(),
        timeout: Duration::from_secs(timeout_secs),
    })
}

/// Instantiate the configured TTS backend.
///
/// `piper-local` (the recommended default) maps onto the `command`
/// machinery with piper detection; `openai` is the OpenAI-compatible HTTP
/// backend; `elevenlabs`, `deepgram`, `gemini`, and `fishaudio` are bespoke
/// live backends; `kokoro-local` and `fishspeech-local` shell out to an
/// explicit `cmd` (community CLI argument shapes were not reliably
/// verified). The bespoke cloud wires have not been exercised against
/// the live APIs yet — no keys were available — so treat first runs
/// as verification runs.
pub fn open_tts(
    backend: &str,
    options: &HashMap<String, String>,
    api_key: Option<SecretValue>,
) -> Result<Box<dyn TtsProvider>, PantheonError> {
    match backend {
        "command" => Ok(Box::new(CommandTts::from_options(options)?)),
        "piper-local" => Ok(Box::new(open_piper_tts(options)?)),
        "kokoro-local" => Ok(Box::new(KokoroTts::from_options(options)?)),
        "fishspeech-local" => Ok(Box::new(FishSpeechTts::from_options(options)?)),
        "openai" => Ok(Box::new(HttpTts::from_options(options, api_key)?)),
        "elevenlabs" => Ok(Box::new(ElevenLabsTts::from_options(options, api_key)?)),
        "deepgram" => Ok(Box::new(DeepgramTts::from_options(options, api_key)?)),
        "gemini" => Ok(Box::new(GeminiTts::from_options(options, api_key)?)),
        "fishaudio" => Ok(Box::new(FishAudioTts::from_options(options, api_key)?)),
        other => Err(verr(
            "VOICE_BACKEND_UNKNOWN",
            format!(
                "unknown TTS backend {other:?}; registered: {}",
                tts_backends()
                    .iter()
                    .map(|b| b.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            false,
            "check [tts].backend in config.toml",
        )),
    }
}

// ---------------------------------------------------------------------------
// Config-driven construction (the [stt] / [tts] sections)
// ---------------------------------------------------------------------------

/// Resolve the HTTP backend's API key through the secrets broker.
///
/// Precedence: `options["api_key_env"]` (explicit env name) → the voice
/// registry's `auth` env var (e.g. `GROQ_API_KEY` for `groq`) → the model
/// catalog row's `key_env` → `PANTHEON_KEY_<PROVIDER>`. The broker (durable
/// vaults, then allowlisted env) is consulted first; the process env is
/// the fallback so keys written by `pantheon model` (which persists them
/// to `<data_dir>/.env`, loaded into the process at startup) keep
/// working. `Ok(None)` = no key anywhere: valid for keyless local
/// endpoints, and the HTTP backends simply send no auth header then.
pub fn voice_api_key(
    secrets: &SecretsBroker,
    provider: &str,
    options: &HashMap<String, String>,
) -> Result<Option<SecretValue>, PantheonError> {
    let name = options_get(options, "api_key_env")
        .map(str::to_string)
        .or_else(|| voice_key_env(provider).map(str::to_string))
        .or_else(|| {
            catalog::provider(provider).map(|p| {
                if p.key_env.is_empty() {
                    format!("PANTHEON_KEY_{}", catalog::env_part(&p.id))
                } else {
                    p.key_env
                }
            })
        })
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", catalog::env_part(provider)));
    match secrets.resolve(&name) {
        Ok(Some(v)) => Ok(Some(v)),
        Ok(None) => Ok(std::env::var(&name)
            .ok()
            .map(SecretValue::new)
            .filter(|v| !v.expose().trim().is_empty())),
        Err(e) => Err(verr(
            "VOICE_SECRETS",
            format!("resolve {name}: {e}"),
            false,
            "check the [secrets] env allowlist for the voice key env var",
        )),
    }
}

/// Instantiate the STT backend from `[stt]`. `None` = section absent
/// (speech off). `Some(Err)` = the section exists but the backend cannot
/// be constructed — surfaced as "STT unavailable", never a model error.
///
/// The OpenAI-wire backends (`openai`, `groq`, `mistral`) resolve their
/// key through the secrets broker; a named backend is its own provider
/// id unless `options["provider"]` overrides it (legacy `openai` shape).
pub fn stt_from_config(
    section: Option<&VoiceSection>,
    secrets: &SecretsBroker,
) -> Option<Result<Box<dyn SttProvider>, PantheonError>> {
    let section = section?;
    Some(match section.backend.as_str() {
        "openai" | "groq" | "mistral" | "deepgram" | "elevenlabs" | "xai" | "assemblyai" => {
            let provider = section
                .options
                .get("provider")
                .map(String::as_str)
                .unwrap_or(&section.backend);
            match voice_api_key(secrets, provider, &section.options) {
                Ok(key) => open_stt(&section.backend, &section.options, key),
                Err(e) => Err(e),
            }
        }
        _ => open_stt(&section.backend, &section.options, None),
    })
}

/// Instantiate the TTS backend from `[tts]`. Same contract as
/// [`stt_from_config`]. Local backends (`command`, `piper-local`) need no
/// key; `openai` resolves it through the secrets broker.
pub fn tts_from_config(
    section: Option<&VoiceSection>,
    secrets: &SecretsBroker,
) -> Option<Result<Box<dyn TtsProvider>, PantheonError>> {
    let section = section?;
    Some(match section.backend.as_str() {
        "openai" | "elevenlabs" | "deepgram" | "gemini" | "fishaudio" => {
            let provider = section
                .options
                .get("provider")
                .map(String::as_str)
                .unwrap_or(&section.backend);
            match voice_api_key(secrets, provider, &section.options) {
                Ok(key) => open_tts(&section.backend, &section.options, key),
                Err(e) => Err(e),
            }
        }
        _ => open_tts(&section.backend, &section.options, None),
    })
}
