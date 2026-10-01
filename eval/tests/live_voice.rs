//! Behavioral tests for live voice mode (`docs/live-voice-mode.md`).
//!
//! Worker A's gateway handler (`pantheon-gateway/src/live_voice.rs`) was
//! not present when these were written, so the tests drive a minimal
//! **protocol-contract harness** (`LiveSession` below) instead of the real
//! handler. The harness implements the wire protocol from the design doc
//! verbatim — frame types, JSON shapes, the VAD safety net, the double
//! gate, busy-drop, approval surfacing, and temp-file cleanup — using
//! FAKE STT/TTS backends (canned responses, never network).
//!
//! When worker A's handler lands, this harness should be retired in favor
//! of driving the real handler; the fake backends and most assertions
//! (event order, JSON shapes, cleanup) transfer directly.
//!
//! Honest verification limits (per the design doc): live provider
//! streaming, real mic capture, and actual end-to-end latency are NOT
//! verifiable here. Timing is modeled with a virtual clock (`tick(ms)`),
//! so the VAD/silence/cap logic is deterministic, not wall-clock.

use pantheon_api::config::{LiveVoiceSection, VoiceSection};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_providers::voice::{
    stt_from_config, SttProvider, SttRequest, SttResult, TtsProvider, TtsRequest, TtsResult,
};
use pantheon_secrets::SecretsBroker;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tempfile::TempDir;

// ── constants ─────────────────────────────────────────────────────────

/// 16 kHz mono 16-bit PCM: 32000 bytes per second.
const BYTES_PER_SEC: usize = 32_000;
/// One ~100 ms client chunk.
const CHUNK_BYTES: usize = 3_200;
/// Energy-VAD threshold: RMS below this counts as silence.
const SILENCE_RMS: f64 = 1_000.0;
/// Chunk size the harness splits TTS audio into (wire frames).
const TTS_FRAME_BYTES: usize = 3_200;

/// Prefix marking a transcript as live-voice-sourced so the agent knows
/// it came from speech. Harness-local choice — the real handler must pick
/// its own marking and document it.
const LIVE_TRANSCRIPT_PREFIX: &str = "[live voice] ";

// ── fake backends ─────────────────────────────────────────────────────

fn verr(code: &str) -> PantheonError {
    PantheonError::new(code, Layer::Provider, false, code, "fake backend", "")
}

/// Fake STT: returns a canned transcript. Asserts the harness staged the
/// utterance to a real temp file first (mirrors whisper-style backends
/// that need audio on disk), and records every path it was handed so
/// tests can check nothing was transcribed twice.
struct FakeStt {
    transcript: String,
    seen: Mutex<Vec<PathBuf>>,
}

impl SttProvider for FakeStt {
    fn name(&self) -> &str {
        "fake-stt"
    }
    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        assert!(
            req.path.exists(),
            "harness must stage utterance audio before STT"
        );
        let bytes = std::fs::read(&req.path).map_err(|_| verr("FAKE_STT_READ"))?;
        assert!(!bytes.is_empty(), "staged utterance must be non-empty");
        self.seen.lock().unwrap().push(req.path.clone());
        Ok(SttResult {
            text: self.transcript.clone(),
            language: None,
            duration_secs: None,
            provider: "fake-stt".into(),
        })
    }
}

/// Fake TTS: returns fixed PCM bytes for any non-empty text, recording
/// what it was asked to speak. Voice selection must come from the `[tts]`
/// options map at construction — the request must not override it.
struct FakeTts {
    audio: Vec<u8>,
    spoken: Mutex<Vec<String>>,
}

impl TtsProvider for FakeTts {
    fn name(&self) -> &str {
        "fake-tts"
    }
    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        assert!(!req.text.trim().is_empty(), "tts needs text");
        assert!(
            req.voice.is_none(),
            "live voice must not override the configured tts voice"
        );
        self.spoken.lock().unwrap().push(req.text.clone());
        Ok(TtsResult {
            bytes: self.audio.clone(),
            format: req.format,
            provider: "fake-tts".into(),
        })
    }
}

// ── fake agent ────────────────────────────────────────────────────────

enum AgentAction {
    Reply(String),
    NeedApproval(String),
}

/// Stand-in for the dispatcher path: records the (marked) transcript it
/// received and either replies or parks an approval.
struct FakeAgent {
    action: AgentAction,
    heard: Mutex<Vec<String>>,
}

impl FakeAgent {
    fn reply(text: &str) -> Self {
        Self {
            action: AgentAction::Reply(text.into()),
            heard: Mutex::new(vec![]),
        }
    }
    fn needs_approval(text: &str) -> Self {
        Self {
            action: AgentAction::NeedApproval(text.into()),
            heard: Mutex::new(vec![]),
        }
    }
    fn act(&self, marked_transcript: &str) -> AgentAction {
        self.heard
            .lock()
            .unwrap()
            .push(marked_transcript.to_string());
        match &self.action {
            AgentAction::Reply(t) => AgentAction::Reply(t.clone()),
            AgentAction::NeedApproval(t) => AgentAction::NeedApproval(t.clone()),
        }
    }
}

// ── the double gate ───────────────────────────────────────────────────

/// The four refusal conditions from the design doc, in doc order:
/// `[tools] voice` on, `[stt]` present + constructible, `[tts]` present +
/// constructible, `[voice] live_enabled`.
struct LiveGate {
    tools_voice_on: bool,
    stt_ready: bool,
    tts_ready: bool,
    live_enabled: bool,
}

impl LiveGate {
    fn open() -> Self {
        Self {
            tools_voice_on: true,
            stt_ready: true,
            tts_ready: true,
            live_enabled: true,
        }
    }
    /// Machine-readable refusal code, or `None` when the session may open.
    fn refusal(&self) -> Option<&'static str> {
        if !self.tools_voice_on {
            return Some("live_voice_disabled");
        }
        if !self.stt_ready {
            return Some("live_stt_unavailable");
        }
        if !self.tts_ready {
            return Some("live_tts_unavailable");
        }
        if !self.live_enabled {
            return Some("live_not_enabled");
        }
        None
    }
}

// ── wire frames ───────────────────────────────────────────────────────

/// What the session emitted toward the client: text JSON frames and
/// binary PCM frames, in order.
#[derive(Debug, Clone)]
enum OutFrame {
    Text(String),
    Binary(Vec<u8>),
}

// ── the harness session ───────────────────────────────────────────────

/// Minimal stand-in for worker A's `live_voice.rs` handler: implements the
/// client→server and server→client protocol from `docs/live-voice-mode.md`
/// against the fake backends. Virtual clock only — no wall time, no
/// threads, no network.
struct LiveSession {
    cfg: LiveVoiceSection,
    stt: Box<dyn SttProvider>,
    tts: Box<dyn TtsProvider>,
    agent: FakeAgent,
    tmp: TempDir,
    out: Vec<OutFrame>,
    now_ms: u64,
    session_start_ms: u64,
    // utterance state
    utter_open: bool,
    utter_start_ms: u64,
    utter_bytes: usize,
    speech_seen: bool,
    last_loud_ms: u64,
    staged: Option<PathBuf>,
    /// Test seam: stands in for the real race window where a turn
    /// (STT → agent → TTS) is in flight while audio arrives. The real
    /// handler sets this around its pipeline; tests set it directly.
    turn_in_flight: bool,
    paused_for_approval: bool,
    closed: bool,
    last_closed_bytes: usize,
}

impl LiveSession {
    fn open(
        gate: &LiveGate,
        cfg: LiveVoiceSection,
        stt: Box<dyn SttProvider>,
        tts: Box<dyn TtsProvider>,
        agent: FakeAgent,
        tmp: TempDir,
    ) -> Result<Self, (Vec<OutFrame>, &'static str)> {
        let mut s = Self {
            cfg,
            stt,
            tts,
            agent,
            tmp,
            out: vec![],
            now_ms: 0,
            session_start_ms: 0,
            utter_open: false,
            utter_start_ms: 0,
            utter_bytes: 0,
            speech_seen: false,
            last_loud_ms: 0,
            staged: None,
            turn_in_flight: false,
            paused_for_approval: false,
            closed: false,
            last_closed_bytes: 0,
        };
        if let Some(code) = gate.refusal() {
            s.emit_text(json!({"type": "error", "code": code}));
            return Err((std::mem::take(&mut s.out), code));
        }
        s.emit_text(json!({"type": "ready"}));
        Ok(s)
    }

    fn emit_text(&mut self, v: Value) {
        self.out.push(OutFrame::Text(v.to_string()));
    }

    /// Advance the virtual clock; runs the VAD safety net and the session
    /// cap. Deterministic: no wall-clock involved.
    fn tick(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        if self.closed {
            return;
        }
        if self.utter_open {
            let elapsed = now_ms.saturating_sub(self.utter_start_ms);
            // Max-utterance force-close wins over silence.
            if elapsed >= self.cfg.live_max_utterance_secs * 1000 {
                self.close_utterance("force-close: max utterance");
                return;
            }
            // Silence auto-close: only after speech was detected.
            if self.speech_seen
                && now_ms.saturating_sub(self.last_loud_ms) >= self.cfg.live_silence_timeout_ms
            {
                self.close_utterance("auto-close: silence timeout");
                return;
            }
        }
        if now_ms.saturating_sub(self.session_start_ms) >= self.cfg.live_max_session_secs * 1000 {
            self.emit_text(json!({"type": "end"}));
            self.cleanup_staged();
            self.closed = true;
        }
    }

    /// `{"type":"start"}` — begin an utterance, staging audio to a temp
    /// file the way streaming backends do.
    fn client_start(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        if self.closed || self.utter_open {
            return;
        }
        let path = self.tmp.path().join(format!("live-utter-{}.pcm", now_ms));
        std::fs::write(&path, []).expect("stage temp file");
        self.staged = Some(path);
        self.utter_open = true;
        self.utter_start_ms = now_ms;
        self.utter_bytes = 0;
        self.speech_seen = false;
        self.last_loud_ms = now_ms;
    }

    /// Binary PCM chunk (part of the open utterance).
    fn client_audio(&mut self, pcm: &[u8], now_ms: u64) {
        self.now_ms = now_ms;
        if self.closed {
            return;
        }
        // v1: no barge-in — audio during an in-flight (or approval-parked)
        // turn is dropped with a `busy` event, never queued.
        if self.turn_in_flight || self.paused_for_approval {
            self.emit_text(json!({"type": "busy"}));
            return;
        }
        if !self.utter_open {
            return;
        }
        let cap = (self.cfg.live_max_utterance_secs as usize) * BYTES_PER_SEC;
        let room = cap.saturating_sub(self.utter_bytes);
        let take = pcm.len().min(room);
        if take > 0 {
            if let Some(path) = &self.staged {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .expect("append staged audio");
                f.write_all(&pcm[..take]).expect("write staged audio");
            }
            self.utter_bytes += take;
        }
        if rms(&pcm[..take]) >= SILENCE_RMS {
            self.speech_seen = true;
            self.last_loud_ms = now_ms;
        }
    }

    /// `{"type":"end"}` — end utterance, transcribe now.
    fn client_end(&mut self) {
        if self.closed {
            return;
        }
        self.close_utterance("client end");
    }

    /// `{"type":"stop"}` — end the live session.
    fn client_stop(&mut self) {
        if self.closed {
            return;
        }
        self.cleanup_staged();
        self.utter_open = false;
        self.emit_text(json!({"type": "end"}));
        self.closed = true;
    }

    /// Client vanished: clean temp files on the abort path too.
    fn disconnect(&mut self) {
        self.cleanup_staged();
        self.utter_open = false;
        self.turn_in_flight = false;
        self.closed = true;
    }

    fn close_utterance(&mut self, _why: &str) {
        if !self.utter_open {
            return;
        }
        self.utter_open = false;
        self.last_closed_bytes = self.utter_bytes;
        let staged = self.staged.take();
        // Empty utterance: no turn, just clean up.
        if self.utter_bytes == 0 {
            self.cleanup_staged_path(&staged);
            return;
        }
        let path = staged.expect("open utterance always has a staged file");
        // Transcribe, then delete the staged file on EVERY path (errors
        // included) — temp hygiene is the whole point of this block.
        let transcript = self.stt.transcribe(&SttRequest::new(&path));
        let _ = std::fs::remove_file(&path);
        let text = match transcript {
            Ok(r) => r.text,
            Err(_) => {
                self.emit_text(json!({"type": "error", "code": "stt_failed"}));
                return;
            }
        };
        self.emit_text(json!({"type": "transcript", "text": text, "final": true}));
        let marked = format!("{LIVE_TRANSCRIPT_PREFIX}{text}");
        self.turn_in_flight = true;
        let action = self.agent.act(&marked);
        match action {
            AgentAction::NeedApproval(ask) => {
                // Approvals are NEVER auto-approved by voice: surface the
                // ask and park the turn until the normal (text) approval
                // path resolves it.
                self.emit_text(json!({"type": "approval_needed", "text": ask}));
                self.paused_for_approval = true;
                self.turn_in_flight = false;
            }
            AgentAction::Reply(reply) => {
                self.emit_text(json!({"type": "reply_text", "text": reply}));
                match self.tts.synthesize(&TtsRequest::new(&reply)) {
                    Ok(r) => {
                        for chunk in r.bytes.chunks(TTS_FRAME_BYTES) {
                            self.out.push(OutFrame::Binary(chunk.to_vec()));
                        }
                        self.emit_text(json!({"type": "audio_end"}));
                    }
                    Err(_) => {
                        self.emit_text(json!({"type": "error", "code": "tts_failed"}));
                    }
                }
                self.turn_in_flight = false;
            }
        }
    }

    fn cleanup_staged(&mut self) {
        let staged = self.staged.take();
        self.cleanup_staged_path(&staged);
    }

    fn cleanup_staged_path(&self, staged: &Option<PathBuf>) {
        if let Some(p) = staged {
            let _ = std::fs::remove_file(p);
        }
    }

    // ── test introspection ──
    fn texts(&self) -> Vec<Value> {
        self.out
            .iter()
            .filter_map(|f| match f {
                OutFrame::Text(t) => Some(serde_json::from_str(t).expect("valid json frame")),
                OutFrame::Binary(_) => None,
            })
            .collect()
    }
    fn binaries(&self) -> Vec<&[u8]> {
        self.out
            .iter()
            .filter_map(|f| match f {
                OutFrame::Binary(b) => Some(b.as_slice()),
                OutFrame::Text(_) => None,
            })
            .collect()
    }
    fn staged_exists(&self) -> bool {
        self.staged.as_ref().is_some_and(|p| p.exists())
    }
    fn tmp_empty(&self) -> bool {
        std::fs::read_dir(self.tmp.path())
            .map(|mut d| d.next().is_none())
            .unwrap_or(true)
    }
    fn tmp_path(&self) -> &Path {
        self.tmp.path()
    }
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        // Belt and suspenders: no staged audio may survive the session.
        self.cleanup_staged();
    }
}

// ── synthetic PCM ─────────────────────────────────────────────────────

/// RMS of 16-bit LE mono PCM.
fn rms(pcm: &[u8]) -> f64 {
    let n = pcm.len() / 2;
    if n == 0 {
        return 0.0;
    }
    let mut sum = 0f64;
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[2 * i], pcm[2 * i + 1]]) as f64;
        sum += s * s;
    }
    (sum / n as f64).sqrt()
}

/// Loud chunk: ±12000 square wave → RMS 12000, well above the VAD
/// threshold. Even byte length.
fn loud_pcm(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes);
    let mut neg = false;
    for _ in 0..bytes / 2 {
        neg = !neg;
        out.extend_from_slice(&(if neg { -12_000i16 } else { 12_000i16 }).to_le_bytes());
    }
    out
}

fn silent_pcm(bytes: usize) -> Vec<u8> {
    vec![0u8; bytes]
}

// ── builders ──────────────────────────────────────────────────────────

fn live_cfg() -> LiveVoiceSection {
    LiveVoiceSection {
        live_enabled: true,
        ..Default::default()
    }
}

fn open_session(
    gate: &LiveGate,
    cfg: LiveVoiceSection,
    agent: FakeAgent,
) -> Result<LiveSession, (Vec<OutFrame>, &'static str)> {
    let tmp = TempDir::new().expect("temp dir");
    let stt: Box<dyn SttProvider> = Box::new(FakeStt {
        transcript: "hello from the fake mic".into(),
        seen: Mutex::new(vec![]),
    });
    let tts: Box<dyn TtsProvider> = Box::new(FakeTts {
        audio: vec![0xABu8; 6_400], // 200 ms of fake reply audio
        spoken: Mutex::new(vec![]),
    });
    LiveSession::open(gate, cfg, stt, tts, agent, tmp)
}

// ── tests ─────────────────────────────────────────────────────────────

#[test]
fn live_turn_round_trip() {
    // (a) Full live turn: PCM in → transcript → agent reply → reply_text
    // + audio frames out, wire shapes exactly per the design doc.
    let mut s = open_session(
        &LiveGate::open(),
        live_cfg(),
        FakeAgent::reply("fake agent reply"),
    )
    .expect("gate open");
    assert_eq!(s.texts(), vec![json!({"type": "ready"})]);

    s.client_start(0);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 0);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 100);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 200);
    assert!(s.staged_exists(), "utterance audio must be staged");
    s.client_end();

    let texts = s.texts();
    assert_eq!(
        texts,
        vec![
            json!({"type": "ready"}),
            json!({"type": "transcript", "text": "hello from the fake mic", "final": true}),
            json!({"type": "reply_text", "text": "fake agent reply"}),
            json!({"type": "audio_end"}),
        ],
        "event order must be transcript → reply_text → audio frames → audio_end"
    );
    // 6400 fake audio bytes → two 3200-byte binary frames between
    // reply_text and audio_end.
    let bins = s.binaries();
    assert_eq!(bins.len(), 2);
    assert!(bins.iter().all(|b| b.len() == TTS_FRAME_BYTES));
    assert_eq!([bins[0], bins[1]].concat(), vec![0xABu8; 6_400]);

    // The agent heard the transcript marked as live-transcribed.
    let heard = s.agent.heard.lock().unwrap();
    assert_eq!(heard.as_slice(), ["[live voice] hello from the fake mic"]);

    // Temp hygiene: staged file gone after the turn.
    assert!(s.tmp_empty(), "no stray temp files after a turn");
}

#[test]
fn vad_silence_auto_closes_utterance() {
    // (b) Silence auto-close: speech then silence past
    // `live_silence_timeout_ms` closes the utterance without `end`.
    let cfg = LiveVoiceSection {
        live_silence_timeout_ms: 200,
        ..live_cfg()
    };
    let mut s = open_session(&LiveGate::open(), cfg, FakeAgent::reply("r")).expect("gate open");
    s.client_start(0);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 0); // speech detected
    s.client_audio(&silent_pcm(CHUNK_BYTES), 100); // 100 ms silent: not yet
    s.tick(100);
    assert!(
        s.texts().iter().all(|t| t["type"] != "transcript"),
        "no auto-close before the silence timeout"
    );
    s.client_audio(&silent_pcm(CHUNK_BYTES), 250); // 250 ms silent: past 200
    s.tick(250);
    let texts = s.texts();
    assert!(
        texts.iter().any(|t| t["type"] == "transcript"),
        "silence past the timeout must auto-close the utterance"
    );
    assert!(!s.utter_open);
    assert!(s.tmp_empty());
}

#[test]
fn vad_silence_before_speech_does_not_close() {
    // Negative case: pure silence with no speech seen must NOT auto-close.
    let cfg = LiveVoiceSection {
        live_silence_timeout_ms: 200,
        ..live_cfg()
    };
    let mut s = open_session(&LiveGate::open(), cfg, FakeAgent::reply("r")).expect("gate open");
    s.client_start(0);
    for t in [100, 200, 300, 500, 1000] {
        s.client_audio(&silent_pcm(CHUNK_BYTES), t);
        s.tick(t);
    }
    assert!(s.utter_open, "silence alone must not close the utterance");
    assert!(s.texts().iter().all(|t| t["type"] != "transcript"));
    // Closing a silence-only utterance still transcribes (it has bytes —
    // STT decides it was silence), but nothing auto-closed it.
    s.client_stop();
    assert_eq!(s.texts().last().unwrap(), &json!({"type": "end"}));
    assert!(s.tmp_empty());
}

#[test]
fn empty_utterance_produces_no_turn() {
    // `start` immediately followed by `end` with zero audio: no STT call,
    // no transcript, staged file cleaned.
    let mut s =
        open_session(&LiveGate::open(), live_cfg(), FakeAgent::reply("r")).expect("gate open");
    s.client_start(0);
    s.client_end();
    assert!(s.texts().iter().all(|t| t["type"] != "transcript"));
    assert!(s.tmp_empty());
}

#[test]
fn vad_max_utterance_force_closes() {
    // (b) Max-utterance force-close on runaway speech, with the byte cap.
    let cfg = LiveVoiceSection {
        live_max_utterance_secs: 1,
        ..live_cfg()
    };
    let mut s = open_session(&LiveGate::open(), cfg, FakeAgent::reply("r")).expect("gate open");
    s.client_start(0);
    for i in 0..12 {
        s.client_audio(&loud_pcm(CHUNK_BYTES), (i * 100) as u64);
        s.tick((i * 100) as u64);
    }
    assert!(
        s.texts().iter().any(|t| t["type"] == "transcript"),
        "runaway utterance must be force-closed at the cap"
    );
    assert!(
        s.last_closed_bytes <= BYTES_PER_SEC,
        "utterance audio capped at max_utterance_secs of PCM"
    );
    // Audio after the force-close lands outside any utterance: ignored.
    let n = s.texts().len();
    s.client_audio(&loud_pcm(CHUNK_BYTES), 1300);
    s.tick(1300);
    assert_eq!(s.texts().len(), n, "no second turn from post-close audio");
    assert!(s.tmp_empty());
}

#[test]
fn disconnect_mid_turn_cleans_temp_files() {
    // (c) Client disconnect with an open utterance: staged audio must not
    // survive.
    let mut s =
        open_session(&LiveGate::open(), live_cfg(), FakeAgent::reply("r")).expect("gate open");
    s.client_start(0);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 0);
    assert!(s.staged_exists());
    let dir = s.tmp_path().to_path_buf();
    s.disconnect();
    let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
    assert!(
        left.is_empty(),
        "disconnect must clean staged audio: {left:?}"
    );
}

#[test]
fn double_gate_refusals() {
    // (d) Each missing gate refuses the session with a machine-readable
    // code and no `ready` frame.
    let cases: &[(&str, LiveGate, &str)] = &[
        (
            "tools voice off",
            LiveGate {
                tools_voice_on: false,
                ..LiveGate::open()
            },
            "live_voice_disabled",
        ),
        (
            "stt missing",
            LiveGate {
                stt_ready: false,
                ..LiveGate::open()
            },
            "live_stt_unavailable",
        ),
        (
            "tts missing",
            LiveGate {
                tts_ready: false,
                ..LiveGate::open()
            },
            "live_tts_unavailable",
        ),
        (
            "live_enabled=false",
            LiveGate {
                live_enabled: false,
                ..LiveGate::open()
            },
            "live_not_enabled",
        ),
    ];
    for (name, gate, code) in cases {
        let (frames, err) = match open_session(gate, live_cfg(), FakeAgent::reply("r")) {
            Ok(_) => panic!("{name}: refused session must not open"),
            Err(e) => e,
        };
        assert_eq!(err, *code, "{name}: refusal code");
        // The refusal is visible on the wire as a machine-readable error,
        // and `ready` is never emitted.
        assert_eq!(frames.len(), 1, "{name}: exactly one wire frame");
        match &frames[0] {
            OutFrame::Text(t) => assert_eq!(
                serde_json::from_str::<Value>(t).unwrap(),
                json!({"type": "error", "code": *code}),
                "{name}: wire error frame"
            ),
            OutFrame::Binary(_) => panic!("{name}: refusal must be a text frame"),
        }
    }

    // An unconstructible backend is also a refusal, not a crash: drive the
    // real constructor with a bogus backend name.
    let bogus = VoiceSection {
        backend: "bogus".into(),
        options: Default::default(),
    };
    let stt = stt_from_config(Some(&bogus), &SecretsBroker::new());
    assert!(
        matches!(stt, Some(Err(_))),
        "bogus stt backend must fail construction"
    );
    let gate = LiveGate {
        stt_ready: false, // what from_config's Err maps to
        ..LiveGate::open()
    };
    let (frames, err) = match open_session(&gate, live_cfg(), FakeAgent::reply("r")) {
        Ok(_) => panic!("unconstructible stt: refused session must not open"),
        Err(e) => e,
    };
    assert_eq!(err, "live_stt_unavailable");
    assert_eq!(frames.len(), 1);
}

#[test]
fn busy_drop_during_in_flight_turn() {
    // (e) Audio arriving while a turn is in flight gets `busy` and is
    // dropped — never queued into the utterance.
    let mut s =
        open_session(&LiveGate::open(), live_cfg(), FakeAgent::reply("r")).expect("gate open");
    s.client_start(0);
    // Simulate the race window: a turn is in flight (STT → agent → TTS).
    s.turn_in_flight = true;
    s.client_audio(&loud_pcm(CHUNK_BYTES), 100);
    let texts = s.texts();
    assert_eq!(
        texts.last().unwrap(),
        &json!({"type": "busy"}),
        "in-flight audio must draw a busy event"
    );
    assert_eq!(s.utter_bytes, 0, "busy audio must not be buffered");
    // After the turn lands, the session works normally again.
    s.turn_in_flight = false;
    s.client_audio(&loud_pcm(CHUNK_BYTES), 200);
    s.client_end();
    assert!(s.texts().iter().any(|t| t["type"] == "transcript"));
}

#[test]
fn approval_needed_pauses_turn_no_auto_approve() {
    // (f) The agent parking an approval surfaces `approval_needed`; the
    // turn pauses — no reply audio, no auto-approve, ever.
    let mut s = open_session(
        &LiveGate::open(),
        live_cfg(),
        FakeAgent::needs_approval("run `deploy.sh`?"),
    )
    .expect("gate open");
    s.client_start(0);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 0);
    s.client_end();

    let texts = s.texts();
    assert_eq!(
        texts,
        vec![
            json!({"type": "ready"}),
            json!({"type": "transcript", "text": "hello from the fake mic", "final": true}),
            json!({"type": "approval_needed", "text": "run `deploy.sh`?"}),
        ]
    );
    assert!(s.binaries().is_empty(), "no reply audio while parked");
    assert!(s.paused_for_approval);
    // Time passing must not auto-approve: the turn stays parked.
    s.tick(60_000);
    assert_eq!(s.texts().len(), 3, "no auto-approve on tick");
    assert!(s.binaries().is_empty());
    assert!(s.tmp_empty());
}

#[test]
fn session_cap_ends_session() {
    // Hard session cap: the server closes with `end` and ignores later
    // audio.
    let cfg = LiveVoiceSection {
        live_max_session_secs: 1,
        ..live_cfg()
    };
    let mut s = open_session(&LiveGate::open(), cfg, FakeAgent::reply("r")).expect("gate open");
    s.tick(1000);
    let texts = s.texts();
    assert_eq!(texts.last().unwrap(), &json!({"type": "end"}));
    assert!(s.closed);
    s.client_start(2000);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 2000);
    assert!(!s.utter_open, "no new utterance after the cap");
}

#[test]
fn client_stop_ends_session_cleanly() {
    // `{"type":"stop"}` → server `end`, staged audio cleaned.
    let mut s =
        open_session(&LiveGate::open(), live_cfg(), FakeAgent::reply("r")).expect("gate open");
    s.client_start(0);
    s.client_audio(&loud_pcm(CHUNK_BYTES), 0);
    assert!(s.staged_exists());
    s.client_stop();
    assert_eq!(s.texts().last().unwrap(), &json!({"type": "end"}));
    assert!(s.tmp_empty());
    assert!(s.closed);
}
