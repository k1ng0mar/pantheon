//! On-device STT backends: Cactus/Needle (Whistle, Parakeet) and
//! Moondream Photon (Parakeet Redux, Qwen3-ASR, Whisper large-v3).
//!
//! These are *services*, not models: a locally installed engine is a
//! swappable capability, exactly like the cloud providers and the
//! `command` backend. Each backend is a distinct engine+model pairing
//! the setup wizard offers by name:
//!
//! - `whistle` - Cactus Whistle, 16.9 MB, CPU only, 7 languages
//!   (en/de/fr/es/it/nl/pl). The smallest and fastest of the four.
//! - `parakeet` - NVIDIA Parakeet TDT 0.6B v3 on the Cactus engine.
//!   The same weights Photon serves, on a runtime that also runs LLMs.
//! - `photon` - Moondream Photon, the compiled inference engine.
//!   Serves Qwen3-ASR and Whisper large-v3-turbo alongside Parakeet.
//! - `parakeet-redux` - Moondream's 1.58-bit Parakeet on Photon:
//!   178 MB, 25 languages, and it runs on CPU (AVX-512 VNNI / NEON /
//!   Metal), so it is the one that works on a GPU-less machine.
//!
//! The engines differ in how they are reached, so this module owns
//! that difference: each backend drives its engine through a small
//! Python bridge (`stt_local.py`) and parses the engine's own JSON
//! result. The bridge is the only place that knows a given engine's
//! API, and its result object is passed through untouched so the wire
//! format stays the engine's, not ours.
//!
//! Audio is normalised to 16 kHz mono WAV before the engine sees it:
//! every one of these engines wants 16 kHz mono, and the gateway
//! stages whatever the channel delivered (ogg, opus, mp3, ...).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use pantheon_api::error::PantheonError;

use crate::voice::{
    run_bounded, verr, AuthRequirement, SttProvider, SttRequest, SttResult, VoiceBackendInfo,
    VoiceBackendKind, VoiceLocalInstall, VoiceSetupField,
};

/// The Python bridge that reaches the engines. It is fed to
/// `python3 -` on stdin, so nothing is installed on disk.
const BRIDGE: &str = include_str!("../stt_local.py");

/// Marker the bridge prefixes its JSON result with, so library chatter
/// on stdout is ignored.
const BRIDGE_MARKER: &str = "__PANTHEON_STT__";

/// Default bound for a local engine run. The engines are small, but a
/// first run also downloads weights, so this is looser than the
/// command backend's default.
pub const DEFAULT_LOCAL_TIMEOUT_SECS: u64 = 300;

/// Sample rate every one of these engines expects.
const ENGINE_SAMPLE_RATE: u32 = 16_000;

/// Which engine a local backend drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalEngine {
    /// Cactus/Needle: `needle.transcribe` (Whistle), from the
    /// `cactus-needle` package.
    Whistle,
    /// Cactus: `cactus.cactus_transcribe` (Parakeet), from the
    /// `cactus-compute` package.
    Cactus,
    /// Moondream Photon: `moondream.photon(...).transcribe(...)`.
    Photon,
}

/// The local STT backends, by name. Each is one engine+model pairing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalBackend {
    /// Cactus Whistle on the Cactus/Needle engine.
    Whistle,
    /// NVIDIA Parakeet TDT 0.6B v3 on the Cactus engine.
    Parakeet,
    /// Moondream Photon with Qwen3-ASR 0.6B.
    Photon,
    /// Moondream Parakeet Redux on Photon.
    ParakeetRedux,
}

impl LocalBackend {
    /// Every local backend, in registry order.
    pub fn all() -> &'static [LocalBackend] {
        &[
            LocalBackend::Whistle,
            LocalBackend::Parakeet,
            LocalBackend::Photon,
            LocalBackend::ParakeetRedux,
        ]
    }

    /// The backend id used in `[stt] backend = "..."`.
    pub fn id(self) -> &'static str {
        match self {
            LocalBackend::Whistle => "whistle",
            LocalBackend::Parakeet => "parakeet",
            LocalBackend::Photon => "photon",
            LocalBackend::ParakeetRedux => "parakeet-redux",
        }
    }

    /// The engine this backend drives.
    pub fn engine(self) -> LocalEngine {
        match self {
            LocalBackend::Whistle => LocalEngine::Whistle,
            LocalBackend::Parakeet => LocalEngine::Cactus,
            LocalBackend::Photon | LocalBackend::ParakeetRedux => LocalEngine::Photon,
        }
    }

    /// The model id the engine is asked for. Empty for Whistle, whose
    /// weights are a local `.cact` file rather than a hub id.
    pub fn default_model(self) -> &'static str {
        match self {
            LocalBackend::Whistle => "",
            LocalBackend::Parakeet => "nvidia/parakeet-tdt-0.6b-v3",
            LocalBackend::Photon => "Qwen/Qwen3-ASR-0.6B",
            LocalBackend::ParakeetRedux => "moondream/parakeet-redux",
        }
    }

    /// Human label for the setup wizard.
    pub fn label(self) -> &'static str {
        match self {
            LocalBackend::Whistle => "Whistle (Cactus, on-device)",
            LocalBackend::Parakeet => "NVIDIA Parakeet (Cactus, on-device)",
            LocalBackend::Photon => "Moondream Photon (on-device)",
            LocalBackend::ParakeetRedux => "Parakeet Redux (Moondream, on-device)",
        }
    }
}

/// A local, on-device STT backend.
pub struct LocalStt {
    backend: LocalBackend,
    engine: LocalEngine,
    /// Model id passed to the engine (empty for Whistle).
    model: String,
    /// Optional explicit Whistle weights path.
    weights: Option<PathBuf>,
    /// Forced language code, when the caller wants one.
    language: Option<String>,
    /// Context hint passed to the engine.
    prompt: Option<String>,
    /// Photon device hint (`cpu`, `cuda`, `mps`).
    device: Option<String>,
    /// Interpreter that has the engine package installed. Engines are
    /// Python packages, and they often live in a virtualenv, so the
    /// interpreter is configurable rather than assumed.
    python: String,
    timeout: Duration,
}

impl LocalStt {
    /// Build a local backend from `[stt.options]`.
    ///
    /// `model` overrides the backend's default checkpoint, `weights`
    /// points at a local `.cact` (Whistle), `language` forces a
    /// language, `prompt` supplies domain words, `device` picks the
    /// Photon device, `python` names the interpreter that has the
    /// engine package, and `timeout_secs` bounds the run.
    pub fn from_options(
        backend: LocalBackend,
        options: &HashMap<String, String>,
    ) -> Result<Self, PantheonError> {
        let timeout_secs = options
            .get("timeout_secs")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_LOCAL_TIMEOUT_SECS);
        let text = |key: &str| {
            options
                .get(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Ok(Self {
            backend,
            engine: backend.engine(),
            model: text("model").unwrap_or_else(|| backend.default_model().to_string()),
            weights: text("weights").map(PathBuf::from),
            language: text("language"),
            prompt: text("prompt"),
            device: text("device"),
            python: text("python").unwrap_or_else(|| "python3".to_string()),
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    /// Run the engine over an already-normalised WAV.
    fn run_engine(
        &self,
        wav: &Path,
        prompt: Option<&str>,
    ) -> Result<(Vec<u8>, Vec<u8>, i32), PantheonError> {
        let mut args: Vec<String> = vec![
            "-".to_string(),
            "--engine".to_string(),
            match self.engine {
                LocalEngine::Whistle => "whistle".to_string(),
                LocalEngine::Cactus => "cactus".to_string(),
                LocalEngine::Photon => "photon".to_string(),
            },
            "--audio".to_string(),
            wav.to_string_lossy().into_owned(),
        ];
        if !self.model.is_empty() {
            args.push("--model".to_string());
            args.push(self.model.clone());
        }
        if let Some(w) = &self.weights {
            args.push("--weights".to_string());
            args.push(w.to_string_lossy().into_owned());
        }
        if let Some(l) = &self.language {
            args.push("--language".to_string());
            args.push(l.clone());
        }
        if let Some(p) = prompt {
            args.push("--prompt".to_string());
            args.push(p.to_string());
        }
        if let Some(d) = &self.device {
            args.push("--device".to_string());
            args.push(d.clone());
        }
        // The bridge is piped in on stdin: nothing is written to disk.
        run_bounded(&self.python, &args, Some(BRIDGE), self.timeout)
    }
}

/// Decode the engine's JSON result into a transcript.
///
/// Every engine returns its own result object and the bridge passes it
/// through untouched, so each engine's fields are read here. The
/// Cactus engines put the transcript under `response`; Whistle and
/// Photon use `text`. A missing or malformed result is a parse
/// failure, never a silent empty transcript.
pub fn parse_engine_result(body: &str) -> Result<(String, Option<String>), PantheonError> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        verr(
            "STT_PARSE",
            format!("local engine returned invalid JSON: {e}"),
            false,
            "check the installed engine package version",
        )
    })?;
    let text = v
        .get("response")
        .or_else(|| v.get("text"))
        .and_then(|t| t.as_str())
        .ok_or_else(|| {
            verr(
                "STT_PARSE",
                "local engine result had no transcript field".to_string(),
                false,
                "reinstall the engine package; the result shape changed",
            )
        })?
        .to_string();
    // Parakeet detects its own language and reports none; the others
    // report the code they used.
    let language = v
        .get("language")
        .and_then(|l| l.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Ok((text, language))
}

/// Pull the engine's JSON result out of the bridge's stdout.
///
/// The bridge prefixes its result with a marker; everything else on
/// stdout is library chatter (progress bars, download logs) and is
/// ignored.
pub fn extract_bridge_result(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    text.lines()
        .rev()
        .find_map(|line| line.trim().strip_prefix(BRIDGE_MARKER))
        .map(|s| s.trim().to_string())
}

impl SttProvider for LocalStt {
    fn name(&self) -> &str {
        self.backend.id()
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
        let (wav, converted) = to_engine_wav(&req.path)?;
        // The configured prompt is the default; a per-request hint wins.
        let prompt = req.prompt.as_deref().or(self.prompt.as_deref());
        let result = self.run_engine(&wav, prompt);
        // The converted WAV is scratch: drop it whether or not the
        // engine succeeded. A file that was already in engine shape
        // belongs to the caller and is left alone.
        if converted {
            let _ = std::fs::remove_file(&wav);
        }
        let (stdout, stderr, code) = result?;
        if code != 0 {
            return Err(verr(
                "STT_EXIT",
                format!(
                    "{} engine exited {code}: {}",
                    self.backend.id(),
                    String::from_utf8_lossy(&stderr)
                        .chars()
                        .take(300)
                        .collect::<String>()
                ),
                false,
                "check the engine package is installed and the model id is valid",
            ));
        }
        let body = extract_bridge_result(&stdout).ok_or_else(|| {
            verr(
                "STT_PARSE",
                format!("{} engine produced no result", self.backend.id()),
                false,
                "check the installed engine package version",
            )
        })?;
        let (text, language) = parse_engine_result(&body)?;
        Ok(SttResult {
            text,
            language: language.or_else(|| req.language.clone()),
            duration_secs: None,
            provider: self.backend.id().to_string(),
        })
    }
}

/// Convert input audio to the 16 kHz mono WAV the engines want.
///
/// ffmpeg does the work when it is available. A file that is already
/// 16 kHz mono WAV is passed through untouched, and the second return
/// value says whether this call created the file.
fn to_engine_wav(input: &Path) -> Result<(PathBuf, bool), PantheonError> {
    if is_engine_wav(input) {
        return Ok((input.to_path_buf(), false));
    }
    let out = std::env::temp_dir().join(format!(
        "pantheon-stt-{}-{}.wav",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let status = Command::new(ffmpeg_bin())
        .args(["-v", "error", "-y", "-i"])
        .arg(input)
        .args([
            "-ac",
            "1",
            "-ar",
            &ENGINE_SAMPLE_RATE.to_string(),
            "-acodec",
            "pcm_s16le",
        ])
        .arg("-f")
        .arg("wav")
        .arg(&out)
        .status();
    match status {
        Ok(s) if s.success() && out.exists() => Ok((out, true)),
        Ok(s) => {
            let _ = std::fs::remove_file(&out);
            Err(verr(
                "STT_CONVERT",
                format!(
                    "ffmpeg could not convert {} to 16 kHz mono WAV (exit {})",
                    input.display(),
                    s.code().unwrap_or(-1)
                ),
                false,
                "install ffmpeg, or hand the backend a 16 kHz mono WAV file",
            ))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            Err(verr(
                "STT_CONVERT",
                format!(
                    "ffmpeg is required to convert {} to 16 kHz mono WAV: {e}",
                    input.display()
                ),
                false,
                "install ffmpeg (https://ffmpeg.org/download.html) and make sure it is on PATH",
            ))
        }
    }
}

/// ffmpeg binary under test can be overridden via env (test seam).
fn ffmpeg_bin() -> String {
    std::env::var("PANTHEON_TEST_FFMPEG").unwrap_or_else(|_| "ffmpeg".to_string())
}

/// True when the file is already a 16 kHz mono WAV, so it can be
/// handed to the engine untouched.
fn is_engine_wav(input: &Path) -> bool {
    let Ok(bytes) = std::fs::read(input) else {
        return false;
    };
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return false;
    }
    // Walk the RIFF chunks: `fmt ` carries the rate and channel count.
    let mut pos = 12usize;
    while pos + 8 <= bytes.len() {
        let id = [bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]];
        let size = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        if &id == b"fmt " && pos + 8 + 16 <= bytes.len() {
            let fmt = &bytes[pos + 8..pos + 8 + 16];
            let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            let rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            return channels == 1 && rate == ENGINE_SAMPLE_RATE;
        }
        pos += 8 + size + (size & 1);
    }
    false
}

/// Registry rows for the local backends, in wizard order.
pub fn local_stt_backends() -> Vec<VoiceBackendInfo> {
    LocalBackend::all()
        .iter()
        .map(|b| {
            let (note, detect, install, model_prompt) = match b {
                LocalBackend::Whistle => (
                    "Cactus Whistle: 16.9 MB on-device speech recognition, CPU only, no GPU needed. 7 languages (en/de/fr/es/it/nl/pl), language auto-detected. Weights download on first run. Needs the `cactus-needle` Python package.",
                    "python3 -c 'import needle'",
                    "pip install cactus-needle",
                    "",
                ),
                LocalBackend::Parakeet => (
                    "NVIDIA Parakeet TDT 0.6B v3 on the Cactus engine: non-autoregressive, fast and multilingual. Runs on CPU or Apple Silicon. Model weights download on first run. Needs the `cactus-compute` Python package.",
                    "python3 -c 'import cactus'",
                    "pip install cactus-compute",
                    "Model id (empty = nvidia/parakeet-tdt-0.6b-v3)",
                ),
                LocalBackend::Photon => (
                    "Moondream Photon: compiled on-device inference for NVIDIA GPUs (Ampere+) and Apple Silicon. Serves Qwen3-ASR and Whisper large-v3-turbo alongside Parakeet. Checkpoints download on first run. Needs the `moondream` Python package.",
                    "python3 -c 'import moondream'",
                    "pip install moondream",
                    "Model id (empty = Qwen/Qwen3-ASR-0.6B)",
                ),
                LocalBackend::ParakeetRedux => (
                    "Moondream's 1.58-bit Parakeet: 178 MB instead of 1.2 GB, 25 languages, and it runs on plain CPU (AVX-512 VNNI, NEON, Metal) - the one that works without an NVIDIA GPU. Beats the full-precision model on 25-language FLEURS and long-form audio. Needs the `moondream` Python package.",
                    "python3 -c 'import moondream'",
                    "pip install moondream",
                    "Model id (empty = moondream/parakeet-redux)",
                ),
            };
            let mut setup_fields = Vec::new();
            if *b == LocalBackend::Whistle {
                setup_fields.push(VoiceSetupField {
                    key: "weights",
                    prompt: "Whistle weights file (.cact; empty = the published weights, downloaded on first run)",
                    default: "",
                    required: false,
                });
            } else {
                setup_fields.push(VoiceSetupField {
                    key: "model",
                    prompt: model_prompt,
                    default: "",
                    required: false,
                });
            }
            if b.engine() == LocalEngine::Photon {
                setup_fields.push(VoiceSetupField {
                    key: "device",
                    prompt: "Device (empty = auto; cpu, cuda or mps)",
                    default: "",
                    required: false,
                });
            }
            // The engine is a Python package, and it is often installed
            // into a virtualenv, so the interpreter is asked for.
            setup_fields.push(VoiceSetupField {
                key: "python",
                prompt: "Python interpreter with the engine package (empty = python3)",
                default: "",
                required: false,
            });
            VoiceBackendInfo {
                name: b.id(),
                label: b.label(),
                kind: VoiceBackendKind::Subprocess,
                recommended: false,
                auth: AuthRequirement::None,
                setup_note: note,
                local: Some(VoiceLocalInstall {
                    detect_cmd: detect,
                    install_hint: install,
                    install_cmd: Some(install),
                }),
                setup_fields,
            }
        })
        .collect()
}
