//! Wire-format fixture tests for the local on-device STT backends
//! (Cactus/Needle, Moondream Photon).
//!
//! The engines are reached through a Python bridge, and the bridge
//! passes each engine's own result object through untouched, so the
//! parsed shapes are the engines' real ones: Whisper/Photon report the
//! transcript under `text`, the Cactus engine under `response`.
//!
//! No engine is installed here and no model is downloaded: the tests
//! pin the parsing and the argv the bridge is called with, and one
//! end-to-end test drives the bridge with a stub `python3` on PATH.
use pantheon_providers::voice::SttRequest;
use pantheon_providers::voice_local::{
    extract_bridge_result, local_stt_backends, parse_engine_result, LocalBackend, LocalEngine,
};
use std::collections::HashMap;

#[test]
fn local_engine_result_shapes_are_read_per_engine() {
    // Whistle and Photon report the transcript under `text`.
    let (text, lang) = parse_engine_result(
        r#"{"text":"turn off the kitchen lights","language":"en","ttft_ms":11.0,"decode_tps":1319.0}"#,
    )
    .unwrap();
    assert_eq!(text, "turn off the kitchen lights");
    assert_eq!(lang.as_deref(), Some("en"));

    // The Cactus engine reports it under `response`.
    let (text, lang) = parse_engine_result(
        r#"{"response":"the rain in spain","segments":[],"decode_tps":96.0,"confidence":0.9}"#,
    )
    .unwrap();
    assert_eq!(text, "the rain in spain");
    assert_eq!(lang, None);

    // Parakeet detects its own language and reports none.
    let (_, lang) = parse_engine_result(r#"{"response":"ok","language":null}"#).unwrap();
    assert_eq!(lang, None);

    // An empty transcript is a real transcript, not a parse failure.
    let (text, _) = parse_engine_result(r#"{"response":"","segments":[]}"#).unwrap();
    assert_eq!(text, "");
}

#[test]
fn malformed_local_engine_result_is_a_parse_error_not_a_silent_empty() {
    // A result without a transcript field is a broken engine, and must
    // not read as an empty transcript.
    let err = parse_engine_result(r#"{"segments":[]}"#).unwrap_err();
    assert_eq!(err.code, "STT_PARSE");
    // Not JSON at all.
    let err = parse_engine_result("not json").unwrap_err();
    assert_eq!(err.code, "STT_PARSE");
}

#[test]
fn bridge_result_is_found_past_library_chatter() {
    // The engine's own line wins; download logs and progress bars on
    // stdout are ignored.
    let stdout = b"Fetching weights...\n__PANTHEON_STT__ {\"text\":\"hello\"}\n";
    assert_eq!(
        extract_bridge_result(stdout).as_deref(),
        Some(r#"{"text":"hello"}"#)
    );
    // No marker: nothing to parse.
    assert_eq!(extract_bridge_result(b"just chatter\n"), None);
    // The last marked line wins when an engine prints more than one.
    let stdout = b"__PANTHEON_STT__ {\"text\":\"first\"}\n__PANTHEON_STT__ {\"text\":\"second\"}\n";
    assert_eq!(
        extract_bridge_result(stdout).as_deref(),
        Some(r#"{"text":"second"}"#)
    );
}

#[test]
fn every_local_backend_is_registered_with_its_engine_and_model() {
    let rows = local_stt_backends();
    assert_eq!(rows.len(), LocalBackend::all().len());
    for b in LocalBackend::all() {
        let row = rows
            .iter()
            .find(|r| r.name == b.id())
            .unwrap_or_else(|| panic!("{} row", b.id()));
        // Local, keyless, and detected before install.
        assert!(!row.recommended, "{} must not displace the default", b.id());
        assert_eq!(row.kind, pantheon_providers::voice::VoiceBackendKind::Subprocess);
        assert_eq!(row.auth, pantheon_providers::voice::AuthRequirement::None);
        let local = row.local.as_ref().expect("local install metadata");
        assert!(!local.detect_cmd.is_empty());
        assert!(!local.install_hint.is_empty());
        // The row asks for the model its engine defaults to, and the
        // device field only where the engine has devices.
        let model_field = row
            .setup_fields
            .iter()
            .find(|f| f.key == "model")
            .map(|f| f.prompt)
            .unwrap_or_default();
        if *b == LocalBackend::Whistle {
            // Whistle's weights are a local file, not a hub id.
            assert_eq!(b.default_model(), "");
            assert!(row.setup_fields.iter().any(|f| f.key == "weights"));
        } else {
            assert!(
                model_field.contains(b.default_model()),
                "{} must offer its default model",
                b.id()
            );
        }
        let has_device = row.setup_fields.iter().any(|f| f.key == "device");
        assert_eq!(has_device, b.engine() == LocalEngine::Photon);
        // Every backend names the interpreter that has its engine.
        assert!(row.setup_fields.iter().any(|f| f.key == "python"));
    }
    // Each engine backs its own backends: Whistle is the Needle
    // engine, Parakeet the Cactus one, the other two Photon.
    assert_eq!(b_engine(LocalBackend::Whistle), LocalEngine::Whistle);
    assert_eq!(b_engine(LocalBackend::Parakeet), LocalEngine::Cactus);
    assert_eq!(b_engine(LocalBackend::Photon), LocalEngine::Photon);
    assert_eq!(b_engine(LocalBackend::ParakeetRedux), LocalEngine::Photon);
}

fn b_engine(b: LocalBackend) -> LocalEngine {
    b.engine()
}

#[test]
fn local_backends_construct_without_a_key() {
    // Construction is config-only: no engine, no weights, no key.
    for b in LocalBackend::all() {
        let stt = pantheon_providers::voice::open_stt(b.id(), &HashMap::new(), None)
            .unwrap_or_else(|e| panic!("{} constructs: {}", b.id(), e.cause));
        assert_eq!(stt.name(), b.id());
    }
}

#[test]
fn local_backend_reports_a_missing_audio_file_as_an_input_error() {
    let stt = pantheon_providers::voice::open_stt("whistle", &HashMap::new(), None).unwrap();
    let err = stt
        .transcribe(&SttRequest::new("/nonexistent/clip.wav"))
        .unwrap_err();
    assert_eq!(err.code, "STT_INPUT");
}

/// End-to-end through the bridge: a stub `python3` on PATH stands in for
/// the engine, so the argv, the stdin-piped bridge and the result
/// parsing are all exercised without an engine or a model.
#[test]
fn local_backend_drives_the_bridge_and_parses_its_result() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("pantheon-voice-local-{}", std::process::id()));
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    // A stub interpreter: ignores its arguments, and answers on stdout
    // the way the real bridge does.
    let stub = bin.join("python3");
    std::fs::write(
        &stub,
        "#!/bin/sh\ncat > /dev/null\necho '__PANTHEON_STT__ {\"text\":\"stub transcript\",\"language\":\"en\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

    // 16 kHz mono WAV: the shape the engines take, so no ffmpeg
    // conversion is needed.
    let audio = dir.join("clip.wav");
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&36u32.to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&0u32.to_le_bytes());
    std::fs::write(&audio, &wav).unwrap();

    let prev_path = std::env::var_os("PATH");
    let mut new_path = bin.to_string_lossy().into_owned();
    if let Some(p) = &prev_path {
        new_path.push(':');
        new_path.push_str(&p.to_string_lossy());
    }
    std::env::set_var("PATH", &new_path);

    let stt = pantheon_providers::voice::open_stt("whistle", &HashMap::new(), None).unwrap();
    let out = stt.transcribe(&SttRequest::new(&audio)).unwrap();
    assert_eq!(out.text, "stub transcript");
    assert_eq!(out.language.as_deref(), Some("en"));
    assert_eq!(out.provider, "whistle");

    match prev_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
