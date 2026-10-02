//! Behavioral tests driving worker A's REAL live-voice handler
//! (`pantheon-gateway/src/live_voice.rs`) over a loopback TCP WebSocket,
//! with FAKE STT/TTS backends (canned responses, never network) and a
//! fake [`LiveTurnDriver`].
//!
//! Companion to `live_voice.rs`, which tests the same protocol contract
//! against a minimal harness with a virtual clock (deterministic VAD
//! timing). This file exercises the real session loop, the real
//! double-gate (`LiveVoiceConfig::gate`), the real worker-thread turn
//! pipeline, and the real `TempWav` cleanup. Timing-sensitive parts here
//! use generous margins; the deterministic VAD edge cases stay in the
//! harness file.
//!
//! Honest verification limits (per `docs/live-voice-mode.md`): live
//! provider streaming, real mic capture, and actual end-to-end latency
//! are NOT verifiable here.

use pantheon_api::config::LiveVoiceSection;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_gateway::channel_voice::{VoicePipes, VoiceSlot};
use pantheon_gateway::live_voice::{
    is_speech, pcm_rms, serve_live_session, wav_to_pcm_16k, wav_wrap, LiveTurnDriver,
    LiveVoiceConfig, TurnOutcome, LIVE_TRANSCRIPT_PREFIX, LIVE_VOICE_PATH, SAMPLE_RATE,
};
use pantheon_providers::voice::{
    AudioFormat, SttProvider, SttRequest, SttResult, TtsProvider, TtsRequest, TtsResult,
};
use pantheon_secrets::SecretsBroker;
use serde_json::{json, Value};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{connect, Message, WebSocket};

// ── fakes ─────────────────────────────────────────────────────────────

fn verr(code: &str) -> PantheonError {
    PantheonError::new(code, Layer::Provider, false, code, "fake backend", "")
}

/// Fake STT: canned transcript; asserts the handler staged a real WAV
/// file first (the real path sniffs the container).
struct FakeStt {
    transcript: String,
}

impl SttProvider for FakeStt {
    fn name(&self) -> &str {
        "fake-stt"
    }
    fn transcribe(&self, req: &SttRequest) -> Result<SttResult, PantheonError> {
        let bytes = std::fs::read(&req.path).map_err(|_| verr("FAKE_STT_READ"))?;
        assert!(!bytes.is_empty(), "staged utterance must be non-empty");
        assert!(
            bytes.starts_with(b"RIFF"),
            "handler must stage a WAV file, got {}",
            req.path.display()
        );
        Ok(SttResult {
            text: self.transcript.clone(),
            language: None,
            duration_secs: None,
            provider: "fake-stt".into(),
        })
    }
}

/// Fake TTS: returns the canned samples wrapped as WAV (the handler
/// requires `AudioFormat::Wav` and decodes it with `wav_to_pcm_16k`).
struct FakeTts {
    samples: Vec<i16>,
    spoken: Mutex<Vec<String>>,
}

impl TtsProvider for FakeTts {
    fn name(&self) -> &str {
        "fake-tts"
    }
    fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
        assert!(!req.text.trim().is_empty());
        assert!(
            req.voice.is_none(),
            "must not override the configured voice"
        );
        self.spoken.lock().unwrap().push(req.text.clone());
        Ok(TtsResult {
            bytes: wav_wrap(&self.samples),
            format: AudioFormat::Wav,
            provider: "fake-tts".into(),
        })
    }
}

enum DriverMode {
    Answer(String),
    Approval(String),
    /// Block inside `run_turn` until the test releases the gate: models
    /// an in-flight turn for the busy-drop test.
    Block {
        gate: Mutex<mpsc::Receiver<()>>,
        answer: String,
    },
}

struct FakeDriver {
    heard: Mutex<Vec<String>>,
    mode: DriverMode,
}

impl LiveTurnDriver for FakeDriver {
    fn run_turn(&self, transcript: &str) -> TurnOutcome {
        self.heard.lock().unwrap().push(transcript.to_string());
        match &self.mode {
            DriverMode::Answer(t) => TurnOutcome::answered(t.clone()),
            DriverMode::Approval(s) => TurnOutcome::approval_needed(s.clone()),
            DriverMode::Block { gate, answer } => {
                let _ = gate.lock().unwrap().recv();
                TurnOutcome::answered(answer.clone())
            }
        }
    }
}

// ── session scaffolding ───────────────────────────────────────────────

fn enabled_limits() -> LiveVoiceSection {
    LiveVoiceSection {
        live_enabled: true,
        ..Default::default()
    }
}

fn pipes_with(stt: Box<dyn SttProvider>, tts: Box<dyn TtsProvider>) -> VoicePipes {
    VoicePipes {
        stt: VoiceSlot::Ready(stt),
        tts: VoiceSlot::Ready(tts),
        voice_replies: false,
    }
}

fn test_pipes() -> (VoicePipes, Arc<FakeTts>) {
    let stt: Box<dyn SttProvider> = Box::new(FakeStt {
        transcript: "hello from the fake mic".into(),
    });
    let tts = Arc::new(FakeTts {
        // 3200 samples = 200 ms of fake reply audio → two 3200-byte frames.
        samples: vec![0x1234i16; 3200],
        spoken: Mutex::new(vec![]),
    });
    struct ArcTts(Arc<FakeTts>);
    impl TtsProvider for ArcTts {
        fn name(&self) -> &str {
            "fake-tts"
        }
        fn synthesize(&self, req: &TtsRequest) -> Result<TtsResult, PantheonError> {
            self.0.synthesize(req)
        }
    }
    (pipes_with(stt, Box::new(ArcTts(Arc::clone(&tts)))), tts)
}

struct Harness {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    session: Option<JoinHandle<()>>,
    driver: Arc<FakeDriver>,
}

fn spawn_session(cfg: LiveVoiceConfig, driver: FakeDriver) -> Harness {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let driver = Arc::new(driver);
    let session = {
        let cfg = Arc::new(cfg);
        let driver = Arc::clone(&driver);
        thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            serve_live_session(stream, vec![], cfg, driver);
        })
    };
    let (ws, _) = connect(format!("ws://{addr}{LIVE_VOICE_PATH}")).expect("ws connect");
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout"),
        _ => panic!("plain ws expected"),
    }
    Harness {
        ws,
        session: Some(session),
        driver,
    }
}

impl Harness {
    fn send_text(&mut self, t: &str) {
        self.ws.send(Message::Text(t.into())).expect("send text");
    }
    fn send_binary(&mut self, b: &[u8]) {
        self.ws
            .send(Message::Binary(b.to_vec().into()))
            .expect("send binary");
    }
    /// Next server frame, or `None` on read timeout.
    fn next(&mut self) -> Option<Message> {
        match self.ws.read() {
            Ok(m) => Some(m),
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                None
            }
            Err(e) => panic!("ws read failed: {e}"),
        }
    }
    fn next_text(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.next() {
                Some(Message::Text(t)) => {
                    return serde_json::from_str(&t).expect("server text is json")
                }
                Some(Message::Binary(_)) => continue, // skip audio while waiting
                Some(Message::Ping(_)) | Some(Message::Pong(_)) => continue,
                Some(m) => panic!("unexpected frame: {m:?}"),
                None => {
                    if Instant::now() > deadline {
                        panic!("timed out waiting for server text frame");
                    }
                }
            }
        }
    }
    /// Collect frames until `audio_end` (or timeout); returns (texts, binaries).
    fn collect_turn(&mut self) -> (Vec<Value>, Vec<Vec<u8>>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut texts = vec![];
        let mut binaries = vec![];
        loop {
            if Instant::now() > deadline {
                panic!("timed out waiting for audio_end; texts={texts:?}");
            }
            match self.next() {
                Some(Message::Text(t)) => {
                    let v: Value = serde_json::from_str(&t).expect("server text is json");
                    let done = v["type"] == "audio_end";
                    texts.push(v);
                    if done {
                        return (texts, binaries);
                    }
                }
                Some(Message::Binary(b)) => binaries.push(b.into()),
                Some(Message::Ping(_)) | Some(Message::Pong(_)) => {}
                Some(m) => panic!("unexpected frame: {m:?}"),
                None => {}
            }
        }
    }
    fn stop(mut self) {
        self.send_text(r#"{"type":"stop"}"#);
        let v = self.next_text();
        assert_eq!(v, json!({"type": "end"}));
        let _ = self.ws.close(None);
        if let Some(h) = self.session.take() {
            h.join().expect("session thread");
        }
    }
}

/// Loud PCM chunk: ±12000 square wave, well above the handler's 400 RMS
/// speech threshold.
fn loud_pcm(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes);
    let mut neg = false;
    for _ in 0..bytes / 2 {
        neg = !neg;
        out.extend_from_slice(&(if neg { -12_000i16 } else { 12_000i16 }).to_le_bytes());
    }
    out
}

/// `pantheon-live-*.wav` files currently in the shared temp dir.
fn live_wavs() -> Vec<std::path::PathBuf> {
    std::fs::read_dir(std::env::temp_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("pantheon-live-") && n.ends_with(".wav"))
        })
        .collect()
}

// ── tests ─────────────────────────────────────────────────────────────

#[test]
fn handler_full_turn_round_trip() {
    // (a) Full live turn through the REAL handler: PCM in → transcript →
    // agent reply → reply_text + audio frames out, per the doc's protocol.
    let (pipes, tts) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes,
    };
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Answer("fake agent reply".into()),
        },
    );
    assert_eq!(h.next_text(), json!({"type": "ready"}));

    h.send_text(r#"{"type":"start"}"#);
    for _ in 0..3 {
        h.send_binary(&loud_pcm(3200));
    }
    h.send_text(r#"{"type":"end"}"#);

    let (texts, binaries) = h.collect_turn();
    assert_eq!(
        texts,
        vec![
            json!({"type":"transcript","text":"hello from the fake mic","final":true}),
            json!({"type":"reply_text","text":"fake agent reply"}),
            json!({"type":"audio_end"}),
        ]
    );
    // 3200 samples → two 1600-sample (3200-byte) binary frames.
    assert_eq!(binaries.len(), 2);
    assert!(binaries.iter().all(|b| b.len() == 3200));
    let mut samples = vec![];
    for b in &binaries {
        for pair in b.chunks_exact(2) {
            samples.push(i16::from_le_bytes([pair[0], pair[1]]));
        }
    }
    assert_eq!(samples, vec![0x1234i16; 3200]);

    // The agent heard the transcript with the handler's live marking.
    let heard: Vec<String> = h.driver.heard.lock().unwrap().clone();
    assert_eq!(
        heard.as_slice(),
        [format!("{LIVE_TRANSCRIPT_PREFIX}hello from the fake mic")]
    );
    // TTS spoke the reply text (voice left to backend config).
    assert_eq!(tts.spoken.lock().unwrap().as_slice(), ["fake agent reply"]);

    h.stop();
}

#[test]
fn handler_gate_refusals() {
    // (d) The REAL double gate refuses up front: error + end, never ready.
    // live_enabled = false.
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: LiveVoiceSection::default(), // live_enabled = false
        pipes,
    };
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Answer("x".into()),
        },
    );
    assert_eq!(
        h.next_text(),
        json!({"type":"error","code":"live_disabled"})
    );
    assert_eq!(h.next_text(), json!({"type":"end"}));
    if let Some(s) = h.session.take() {
        s.join().expect("session thread");
    }

    // [stt]/[tts] missing entirely (pipes disabled).
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes: VoicePipes::disabled(),
    };
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Answer("x".into()),
        },
    );
    assert_eq!(
        h.next_text(),
        json!({"type":"error","code":"voice_not_configured"})
    );
    assert_eq!(h.next_text(), json!({"type":"end"}));
    if let Some(s) = h.session.take() {
        s.join().expect("session thread");
    }
}

#[test]
fn handler_gate_unit_conditions() {
    // `LiveVoiceConfig::gate` against every refusal condition, using the
    // real constructors (double gate = `[tools] voice` AND constructible
    // `[stt]`/`[tts]`, plus `[voice] live_enabled`).
    use pantheon_api::config::{ToolsSection, VoiceSection};
    let secrets = SecretsBroker::new();
    let bogus = || VoiceSection {
        backend: "bogus".into(),
        options: Default::default(),
    };

    // live_enabled = false refuses first, whatever the pipes say.
    let cfg = LiveVoiceConfig::from_config(None, None, None, LiveVoiceSection::default(), &secrets);
    assert_eq!(cfg.gate(), Err("live_disabled"));

    // [tools] voice off → both slots Disabled.
    let tools_off = ToolsSection {
        voice: Some(false),
        ..Default::default()
    };
    let cfg = LiveVoiceConfig::from_config(
        Some(&tools_off),
        Some(&bogus()),
        Some(&bogus()),
        enabled_limits(),
        &secrets,
    );
    assert_eq!(cfg.gate(), Err("voice_not_configured"));

    // [stt] missing → Disabled (stt is checked first).
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes: VoicePipes {
            stt: VoiceSlot::Disabled,
            tts: pipes.tts,
            voice_replies: false,
        },
    };
    assert_eq!(cfg.gate(), Err("voice_not_configured"));

    // [tts] missing → Disabled.
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes: VoicePipes {
            stt: pipes.stt,
            tts: VoiceSlot::Disabled,
            voice_replies: false,
        },
    };
    assert_eq!(cfg.gate(), Err("voice_not_configured"));

    // Present but unconstructible → Unavailable (distinct code).
    let cfg = LiveVoiceConfig::from_config(
        None,
        Some(&bogus()),
        Some(&bogus()),
        enabled_limits(),
        &secrets,
    );
    assert_eq!(cfg.gate(), Err("voice_backend_misconfigured"));

    // Gate open: both backends Ready.
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes,
    };
    assert_eq!(cfg.gate(), Ok(()));
}

#[test]
fn handler_busy_and_silent_drop_during_inflight_turn() {
    // (e) While a turn is in flight: `start` draws `busy`; binary audio is
    // dropped with a `busy` event (never queued). No barge-in in v1.
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes,
    };
    let (gate_tx, gate_rx) = mpsc::channel();
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Block {
                gate: Mutex::new(gate_rx),
                answer: "late reply".into(),
            },
        },
    );
    assert_eq!(h.next_text(), json!({"type": "ready"}));
    h.send_text(r#"{"type":"start"}"#);
    h.send_binary(&loud_pcm(3200));
    h.send_text(r#"{"type":"end"}"#);
    // The worker thread is now blocked inside run_turn: the turn is in
    // flight (transcript is emitted before the agent runs).
    assert_eq!(
        h.next_text(),
        json!({"type":"transcript","text":"hello from the fake mic","final":true})
    );
    h.send_text(r#"{"type":"start"}"#);
    assert_eq!(h.next_text(), json!({"type": "busy"}));
    // Binary during the in-flight turn: dropped, answered with `busy`.
    h.send_binary(&loud_pcm(3200));
    assert_eq!(
        h.next_text(),
        json!({"type": "busy"}),
        "in-flight binary must be dropped with a busy event"
    );
    // Release the turn: the parked turn completes normally.
    gate_tx.send(()).unwrap();
    let (texts, binaries) = h.collect_turn();
    assert_eq!(
        texts,
        vec![
            json!({"type":"reply_text","text":"late reply"}),
            json!({"type":"audio_end"}),
        ]
    );
    assert_eq!(binaries.len(), 2);
    // The busy `start` did not open a second utterance: one turn total.
    assert_eq!(h.driver.heard.lock().unwrap().len(), 1);
    h.stop();
}

#[test]
fn handler_approval_needed_parks_turn() {
    // (f) The agent parking an approval surfaces `approval_needed`; the
    // turn pauses - no reply audio, no auto-approve.
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes,
    };
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Approval("run deploy.sh?".into()),
        },
    );
    assert_eq!(h.next_text(), json!({"type": "ready"}));
    h.send_text(r#"{"type":"start"}"#);
    h.send_binary(&loud_pcm(3200));
    h.send_text(r#"{"type":"end"}"#);
    assert_eq!(
        h.next_text(),
        json!({"type":"transcript","text":"hello from the fake mic","final":true})
    );
    assert_eq!(
        h.next_text(),
        json!({"type":"approval_needed","text":"run deploy.sh?"})
    );
    // Nothing else follows: no audio_end, no binary, no auto-approve.
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        assert!(h.next().is_none(), "parked turn must emit nothing more");
    }
    h.stop();
}

#[test]
fn handler_silence_autoclose() {
    // (b) Server-side VAD safety net: speech then silence past
    // `live_silence_timeout_ms` auto-closes the utterance - no `end` sent.
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: LiveVoiceSection {
            live_silence_timeout_ms: 200,
            ..enabled_limits()
        },
        pipes,
    };
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Answer("r".into()),
        },
    );
    assert_eq!(h.next_text(), json!({"type": "ready"}));
    h.send_text(r#"{"type":"start"}"#);
    h.send_binary(&loud_pcm(3200)); // speech seen
                                    // No `end` - the silence timeout must close the utterance itself.
    assert_eq!(
        h.next_text(),
        json!({"type":"transcript","text":"hello from the fake mic","final":true}),
        "silence past the timeout must auto-close the utterance"
    );
    // The auto-closed utterance runs a normal turn afterwards.
    let (texts, _) = h.collect_turn();
    assert_eq!(
        texts,
        vec![
            json!({"type":"reply_text","text":"r"}),
            json!({"type":"audio_end"}),
        ]
    );
    h.stop();
}

#[test]
fn handler_disconnect_cleans_temp_files() {
    // (c) Client disconnect mid-turn: the staged WAV must not survive.
    // The handler stages `pantheon-live-*.wav` in the shared temp dir.
    let before = live_wavs();
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes,
    };
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Answer("r".into()),
        },
    );
    assert_eq!(h.next_text(), json!({"type": "ready"}));
    h.send_text(r#"{"type":"start"}"#);
    h.send_binary(&loud_pcm(3200));
    h.send_text(r#"{"type":"end"}"#); // turn starts: TempWav staged on worker
                                      // Vanish mid-turn: drop the socket without `stop`.
    drop(h.ws);
    if let Some(s) = h.session.take() {
        s.join().expect("session thread");
    }
    // Give the worker thread a beat to unwind, then assert cleanup.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = live_wavs();
        let stray: Vec<_> = now.iter().filter(|p| !before.contains(p)).collect();
        if stray.is_empty() || Instant::now() > deadline {
            assert!(
                stray.is_empty(),
                "disconnect must clean staged audio: {stray:?}"
            );
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn handler_bad_frame_is_a_coded_error() {
    // Malformed client text → machine-readable error, session survives.
    let (pipes, _) = test_pipes();
    let cfg = LiveVoiceConfig {
        limits: enabled_limits(),
        pipes,
    };
    let mut h = spawn_session(
        cfg,
        FakeDriver {
            heard: Mutex::new(vec![]),
            mode: DriverMode::Answer("r".into()),
        },
    );
    assert_eq!(h.next_text(), json!({"type": "ready"}));
    h.send_text("not json at all");
    assert_eq!(h.next_text(), json!({"type":"error","code":"bad_frame"}));
    h.send_text(r#"{"type":"wat"}"#);
    assert_eq!(
        h.next_text(),
        json!({"type":"error","code":"unknown_frame"})
    );
    h.stop();
}

#[test]
fn handler_vad_and_wav_helpers() {
    // Small deterministic pins on the handler's real DSP helpers.
    assert_eq!(SAMPLE_RATE, 16_000, "doc: 16 kHz mono 16-bit PCM");
    assert_eq!(LIVE_VOICE_PATH, "/agui/voice/live");
    assert_eq!(pcm_rms(&[]), 0.0);
    let loud: Vec<i16> = (0..1600)
        .map(|i| if i % 2 == 0 { 12_000 } else { -12_000 })
        .collect();
    assert!(is_speech(&loud), "±12000 square wave is speech");
    assert!(!is_speech(&vec![0i16; 1600]), "silence is not speech");
    // WAV wrap/parse round-trips the samples exactly.
    let back = wav_to_pcm_16k(&wav_wrap(&loud)).expect("wav must parse");
    assert_eq!(back, loud);
    // The live-transcript marking is present and non-empty.
    assert!(!LIVE_TRANSCRIPT_PREFIX.is_empty());
    assert!(LIVE_TRANSCRIPT_PREFIX.contains("live"));
}
