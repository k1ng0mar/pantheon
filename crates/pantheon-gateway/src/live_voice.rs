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
//!   queued — no barge-in in v1); `approval_needed` when a tool approval
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

use crate::channel_voice::{VoicePipes, VoiceSlot};
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
    /// double gate.
    pub pipes: VoicePipes,
}

impl std::fmt::Debug for LiveVoiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // VoicePipes holds backend trait objects; summarize secret-free.
        f.debug_struct("LiveVoiceConfig")
            .field("limits", &self.limits)
            .field("pipes", &self.pipes.describe())
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
        Self {
            limits,
            pipes: VoicePipes::from_config(tools, stt, tts, secrets, false),
        }
    }

    /// Live voice unavailable: every session is refused up front.
    pub fn disabled() -> Self {
        Self {
            limits: LiveVoiceSection::default(),
            pipes: VoicePipes::disabled(),
        }
    }

    /// Up-front gate: the session is refused unless `[voice] live_enabled`
    /// is true AND both backends constructed. `Err` is the machine-readable
    /// `error` code sent before `end` — never key material.
    pub fn gate(&self) -> Result<(), &'static str> {
        if !self.limits.live_enabled {
            return Err("live_disabled");
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
        let size = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        let end = body.saturating_add(size).min(bytes.len());
        if id == b"fmt " {
            if end - body < 16 {
                return Err("wav_bad_fmt");
            }
            let audio_fmt = u16::from_le_bytes(bytes[body..body + 2].try_into().unwrap());
            if audio_fmt != 1 {
                return Err("wav_unsupported"); // PCM only, no float/ADPCM
            }
            channels = u16::from_le_bytes(bytes[body + 2..body + 4].try_into().unwrap());
            rate = u32::from_le_bytes(bytes[body + 4..body + 8].try_into().unwrap());
            bits = u16::from_le_bytes(bytes[body + 14..body + 16].try_into().unwrap());
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
                i16::from_le_bytes(frame[off..off + 2].try_into().unwrap())
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

/// Staged utterance WAV. `Drop` removes the file on every path — STT
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
struct HeadReplay {
    head: Vec<u8>,
    pos: usize,
    stream: TcpStream,
}

impl HeadReplay {
    fn stream_mut(&mut self) -> &mut TcpStream {
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

fn json_text(v: &serde_json::Value) -> Message {
    Message::text(v.to_string())
}

/// Send with a short WouldBlock retry: the socket is nonblocking and a
/// full send buffer must not silently drop a reply audio chunk.
fn send_msg(ws: &mut WebSocket<HeadReplay>, msg: Message) {
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
                    self.send_json(&serde_json::json!({"type":"transcript","text":t,"final":true}));
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
