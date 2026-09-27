//! Tests for `pantheon_providers::voice::tests` — sibling file so sources stay test-free.
use super::*;

fn opts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
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

