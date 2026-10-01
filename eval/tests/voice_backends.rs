//! Behavioral tests: voice backends fail closed without keys.
//! Run with `cargo test -p pantheon-eval`.
//!
//! These touch the filesystem (a throwaway WAV fixture), so they live in
//! the eval harness per the repo's test-location rule: behavioral /
//! filesystem / network / subprocess / timing tests go to /eval, and only
//! small deterministic invariants stay in-crate.
use pantheon_providers::{open_stt, open_tts, SttRequest, TtsRequest};
use std::collections::HashMap;

fn opts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn bespoke_backends_require_keys_at_call_time() {
    // A backend without its key fails with VOICE_CONFIG naming the env var
    // (offline-deterministic: no network is touched).
    let dir = std::env::temp_dir();
    let audio = dir.join("pantheon-test-silence.wav");
    std::fs::write(&audio, [0u8; 44]).unwrap();
    let req = SttRequest::new(&audio);
    for (name, env) in [
        ("deepgram", "DEEPGRAM_API_KEY"),
        ("elevenlabs", "ELEVENLABS_API_KEY"),
        ("xai", "XAI_API_KEY"),
        ("assemblyai", "ASSEMBLYAI_API_KEY"),
    ] {
        let b = open_stt(name, &HashMap::new(), None).unwrap();
        let err = b.transcribe(&req).unwrap_err();
        assert_eq!(err.code, "VOICE_CONFIG", "{name}");
        assert!(
            err.cause.contains(env),
            "{name}: {cause}",
            cause = err.cause
        );
    }
    let treq = TtsRequest::new("hello");
    for (name, env) in [
        ("elevenlabs", "ELEVENLABS_API_KEY"),
        ("deepgram", "DEEPGRAM_API_KEY"),
        ("gemini", "GEMINI_API_KEY"),
    ] {
        let opts = opts(&[("voice", "test-voice")]);
        let b = open_tts(name, &opts, None).unwrap();
        let err = b.synthesize(&treq).unwrap_err();
        assert_eq!(err.code, "VOICE_CONFIG", "{name}");
        assert!(
            err.cause.contains(env),
            "{name}: {cause}",
            cause = err.cause
        );
    }
    std::fs::remove_file(&audio).ok();
}
