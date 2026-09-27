//! STT/TTS provider plane (ARCHITECTURE §14). These are *services*, not
//! models: a whisper binary or a cloud transcription endpoint is a
//! swappable capability, never an entry in the model policy.
//!
//! Shape mirrors `pantheon-memory`'s backend registry: traits + a small
//! catalog of named backends + `open_*` constructors driven by config
//! (`[stt]` / `[tts]`, each `{ backend, options }`). Two backend kinds:
//!
//! - `command` (Subprocess): local binaries — whisper.cpp, piper,
//!   espeak-ng. STT reads text from stdout (`{file}`/`{language}`
//!   placeholders); TTS pipes text in on stdin and takes audio from
//!   stdout. Every run is wall-clock bounded.
//! - `openai` (Http): OpenAI-compatible audio endpoints
//!   (`/audio/transcriptions`, `/audio/speech`) resolved through the
//!   core catalog like every other provider.
//!
//! Callers gate access upstream (capability policy on the gateway/CLI
//! seam); backends only move bytes. Nothing here ever enters model
//! context except the transcription text the caller passes on.

use crate::catalog;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_secrets::SecretValue;
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
            timeout: Duration::from_secs(timeout_secs),
        })
    }
}

impl TtsProvider for CommandTts {
    fn name(&self) -> &str {
        "command"
    }

    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        let args = expand_args(
            &self.args,
            None,
            None,
            req.voice.as_deref(),
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
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// JSON body for `POST {base}/audio/speech`. Pure.
pub fn speech_payload(req: &TtsRequest, model: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "voice": req.voice.clone().unwrap_or_else(|| "alloy".into()),
        "input": req.text,
        "response_format": req.format.as_str(),
    })
}

/// OpenAI-compatible STT over HTTP.
///
/// Options: `provider` (catalog id or base URL, required), `model`
/// (default `whisper-1`).
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
        Ok(Self {
            provider: require_option(options, "provider")?.to_string(),
            model: options_get(options, "model")
                .unwrap_or("whisper-1")
                .to_string(),
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
        "openai"
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
            ureq::Error::Status(code, _) => verr(
                "STT_HTTP",
                format!("transcription endpoint: HTTP {code}"),
                !(400..500).contains(&code),
                "check the [stt] provider, model name, and API key",
            ),
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
        Ok(SttResult {
            text: v
                .get("text")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
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
            model: options_get(options, "model").unwrap_or("tts-1").to_string(),
            default_voice: options_get(options, "voice").map(String::from),
            api_key,
        })
    }
}

impl TtsProvider for HttpTts {
    fn name(&self) -> &str {
        "openai"
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
                ureq::Error::Status(code, _) => verr(
                    "TTS_HTTP",
                    format!("speech endpoint: HTTP {code}"),
                    !(400..500).contains(&code),
                    "check the [tts] provider, model name, and API key",
                ),
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
// Registry
// ---------------------------------------------------------------------------

/// Backend kind, mirroring `pantheon-memory`'s `BackendKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoiceBackendKind {
    Subprocess,
    Http,
}

/// Catalog row for one registered backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceBackendInfo {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: VoiceBackendKind,
    pub capabilities: Vec<&'static str>,
}

/// Registered STT backends (for `doctor` / `providers` listings).
pub fn stt_backends() -> Vec<VoiceBackendInfo> {
    vec![
        VoiceBackendInfo {
            name: "command",
            label: "Local STT binary (whisper.cpp, ...)",
            kind: VoiceBackendKind::Subprocess,
            capabilities: vec!["transcribe"],
        },
        VoiceBackendInfo {
            name: "openai",
            label: "OpenAI-compatible /audio/transcriptions",
            kind: VoiceBackendKind::Http,
            capabilities: vec!["transcribe"],
        },
    ]
}

/// Registered TTS backends.
pub fn tts_backends() -> Vec<VoiceBackendInfo> {
    vec![
        VoiceBackendInfo {
            name: "command",
            label: "Local TTS binary (piper, espeak-ng)",
            kind: VoiceBackendKind::Subprocess,
            capabilities: vec!["synthesize"],
        },
        VoiceBackendInfo {
            name: "openai",
            label: "OpenAI-compatible /audio/speech",
            kind: VoiceBackendKind::Http,
            capabilities: vec!["synthesize"],
        },
    ]
}

/// Instantiate the configured STT backend. `Err` = misconfigured/unknown
/// backend; callers treat it as "STT unavailable", never as a model error.
pub fn open_stt(
    backend: &str,
    options: &HashMap<String, String>,
    api_key: Option<SecretValue>,
) -> Result<Box<dyn SttProvider>, PantheonError> {
    match backend {
        "command" => Ok(Box::new(CommandStt::from_options(options)?)),
        "openai" => Ok(Box::new(HttpStt::from_options(options, api_key)?)),
        other => Err(verr(
            "VOICE_BACKEND_UNKNOWN",
            format!("unknown STT backend {other:?}; registered: command, openai"),
            false,
            "check [stt].backend in config.toml",
        )),
    }
}

/// Instantiate the configured TTS backend.
pub fn open_tts(
    backend: &str,
    options: &HashMap<String, String>,
    api_key: Option<SecretValue>,
) -> Result<Box<dyn TtsProvider>, PantheonError> {
    match backend {
        "command" => Ok(Box::new(CommandTts::from_options(options)?)),
        "openai" => Ok(Box::new(HttpTts::from_options(options, api_key)?)),
        other => Err(verr(
            "VOICE_BACKEND_UNKNOWN",
            format!("unknown TTS backend {other:?}; registered: command, openai"),
            false,
            "check [tts].backend in config.toml",
        )),
    }
}

#[cfg(test)]
#[path = "voice_tests.rs"]
mod tests;
