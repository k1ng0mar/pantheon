//! Tests for `pantheon_providers::voice::tests` — sibling file so sources stay test-free.
use super::*;

fn opts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn command_stt_reads_stdout_of_the_template_command() {
    // `cat {file}` echoes the audio file's bytes — a stand-in for any
    // whisper-style binary that prints the transcript to stdout.
    let dir = std::env::temp_dir().join(format!("pantheon-stt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let audio = dir.join("clip.txt");
    std::fs::write(&audio, "faked transcript audio\n").unwrap();

    let stt = CommandStt::from_options(&opts(&[
        ("cmd", "cat"),
        ("args", "{file}"),
        ("language", "en"),
    ]))
    .unwrap();
    let mut req = SttRequest::new(&audio);
    req.prompt = None;
    let out = stt.transcribe(&req).unwrap();
    assert_eq!(out.text, "faked transcript audio");
    assert_eq!(out.provider, "command");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn command_tts_pipes_text_through_stdout() {
    // `cat` with no args: stdin text comes back as "audio" bytes.
    let tts = CommandTts::from_options(&opts(&[("cmd", "cat")])).unwrap();
    let out = tts.synthesize(&TtsRequest::new("hello out loud")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.bytes), "hello out loud");
    assert_eq!(out.format, AudioFormat::Wav);
}

#[test]
fn command_tts_substitutes_voice_and_format_placeholders() {
    let tts = CommandTts::from_options(&opts(&[
        ("cmd", "echo"),
        ("args", "voice={voice} fmt={format}"),
    ]))
    .unwrap();
    let req = TtsRequest::new("ignored by echo").with_voice("alice");
    let out = tts.synthesize(&req).unwrap();
    let line = String::from_utf8_lossy(&out.bytes);
    assert!(line.contains("voice=alice"), "got: {line}");
    assert!(line.contains("fmt=wav"), "got: {line}");
}

#[test]
fn command_failure_is_structured_not_empty_success() {
    let stt = CommandStt::from_options(&opts(&[("cmd", "false")])).unwrap();
    let dir = std::env::temp_dir().join(format!("pantheon-stt2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let audio = dir.join("clip.txt");
    std::fs::write(&audio, "x").unwrap();
    let err = stt.transcribe(&SttRequest::new(&audio)).unwrap_err();
    assert_eq!(err.code, "STT_EXIT");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn missing_audio_file_is_an_input_error() {
    let stt = CommandStt::from_options(&opts(&[("cmd", "cat")])).unwrap();
    let err = stt
        .transcribe(&SttRequest::new("/nonexistent/audio.wav"))
        .unwrap_err();
    assert_eq!(err.code, "STT_INPUT");
}

#[test]
fn subprocess_run_is_wall_clock_bounded() {
    let stt = CommandStt::from_options(&opts(&[
        ("cmd", "sleep"),
        ("args", "5"),
        ("timeout_secs", "1"),
    ]))
    .unwrap();
    let dir = std::env::temp_dir().join(format!("pantheon-stt3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let audio = dir.join("clip.txt");
    std::fs::write(&audio, "x").unwrap();
    let started = std::time::Instant::now();
    let err = stt.transcribe(&SttRequest::new(&audio)).unwrap_err();
    assert_eq!(err.code, "VOICE_TIMEOUT");
    assert!(started.elapsed() < Duration::from_secs(3));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multipart_body_carries_model_language_and_file() {
    let (content_type, body) = stt_multipart(
        "whisper-1",
        Some("en"),
        Some("pantheon"),
        "clip.ogg",
        b"RIFF",
    );
    assert!(content_type.starts_with("multipart/form-data; boundary="));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("name=\"model\""));
    assert!(text.contains("whisper-1"));
    assert!(text.contains("name=\"language\""));
    assert!(text.contains("name=\"prompt\""));
    assert!(text.contains("filename=\"clip.ogg\""));
    assert!(text.contains("RIFF"));
    assert!(text.trim_end().ends_with("--"));
}

#[test]
fn speech_payload_is_openai_shaped() {
    let v = speech_payload(&TtsRequest::new("hi").with_voice("nova"), "tts-1");
    assert_eq!(v["model"], "tts-1");
    assert_eq!(v["voice"], "nova");
    assert_eq!(v["input"], "hi");
    assert_eq!(v["response_format"], "wav");
}

#[test]
fn registry_lists_both_kinds_and_rejects_unknown() {
    assert_eq!(stt_backends().len(), 2);
    assert_eq!(tts_backends().len(), 2);
    assert_eq!(stt_backends()[0].kind, VoiceBackendKind::Subprocess);
    assert_eq!(stt_backends()[1].kind, VoiceBackendKind::Http);
    let err = match open_stt("bogus", &HashMap::new(), None) {
        Err(e) => e,
        Ok(_) => panic!("unknown backend must error"),
    };
    assert_eq!(err.code, "VOICE_BACKEND_UNKNOWN");
    // Missing required option is a config error, not a panic.
    let err = match open_tts("openai", &HashMap::new(), None) {
        Err(e) => e,
        Ok(_) => panic!("missing provider option must error"),
    };
    assert_eq!(err.code, "VOICE_CONFIG");
    // Happy path: registered backends construct.
    assert!(open_stt("command", &opts(&[("cmd", "cat")]), None).is_ok());
    assert!(open_tts("openai", &opts(&[("provider", "openai")]), None).is_ok());
}

#[test]
fn unknown_option_paths_default_safely() {
    // No args template = no placeholders, still runs.
    let stt = CommandStt::from_options(&opts(&[("cmd", "cat")])).unwrap();
    assert!(stt.args.is_empty());
    assert_eq!(stt.fallback_language, "auto");
    // Garbage timeout falls back to the default bound.
    let stt = CommandStt::from_options(&opts(&[("cmd", "cat"), ("timeout_secs", "not-a-number")]))
        .unwrap();
    assert_eq!(
        stt.timeout,
        Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS)
    );
}
