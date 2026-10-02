//! Wire-format fixture tests for the bespoke STT/TTS providers: request
//! builders and response parsers are pure, so their documented shapes are
//! pinned here with fixtures. No network, no keys - live calls are NOT
//! performed (see docs/stt-providers.md, docs/tts-providers.md).
use pantheon_providers::voice::{
    assemblyai_transcript_body, assemblyai_transcript_status, b64decode, deepgram_listen_url,
    deepgram_speak_body, deepgram_speak_url, detect_kokoro, detect_piper, elevenlabs_stt_multipart,
    elevenlabs_tts_body, elevenlabs_tts_url, fishaudio_tts_body, gemini_extract_pcm,
    gemini_pcm_to_wav, gemini_tts_body, gemini_tts_url, open_tts, parse_assemblyai_transcript,
    parse_deepgram_transcript, parse_elevenlabs_transcript, resolve_piper_voice, which_binary,
    xai_stt_multipart, TtsRequest, ASSEMBLYAI_AUTH_SCHEME, DEEPGRAM_AUTH_SCHEME,
    ELEVENLABS_API_KEY_HEADER, ELEVENLABS_STT_URL, FISHAUDIO_MODEL_HEADER, FISHAUDIO_TTS_URL,
    GEMINI_API_KEY_HEADER, XAI_STT_URL,
};
use std::collections::HashMap;

fn opts(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn deepgram_stt_wire_is_query_params_not_multipart_fields() {
    // Options ride the query string; the body is raw audio bytes.
    let url = deepgram_listen_url("nova-3", Some("en"), true);
    assert!(url.starts_with("https://api.deepgram.com/v1/listen?"));
    assert!(url.contains("model=nova-3"));
    assert!(url.contains("smart_format=true"));
    assert!(url.contains("language=en"));
    assert!(url.contains("diarize=true"));
    let plain = deepgram_listen_url("nova-3", None, false);
    assert!(!plain.contains("language="));
    assert!(!plain.contains("diarize="));
    // Auth is `Authorization: Token`, never Bearer.
    assert_eq!(DEEPGRAM_AUTH_SCHEME, "Token");
}

#[test]
fn deepgram_transcript_comes_from_channels_alternatives() {
    let body = r#"{
        "results": {"channels": [{"alternatives": [
            {"transcript": "hello world", "confidence": 0.99},
            {"transcript": "hello whirl"}
        ]}]}
    }"#;
    assert_eq!(
        parse_deepgram_transcript(body).as_deref(),
        Some("hello world")
    );
    assert_eq!(parse_deepgram_transcript("{}"), None);
    assert_eq!(parse_deepgram_transcript("not json"), None);
}

#[test]
fn elevenlabs_stt_uses_bespoke_field_names() {
    let (ct, body) = elevenlabs_stt_multipart("scribe_v2", Some("en"), true, "clip.wav", b"RIFF");
    assert_eq!(
        ELEVENLABS_STT_URL,
        "https://api.elevenlabs.io/v1/speech-to-text"
    );
    assert_eq!(ELEVENLABS_API_KEY_HEADER, "xi-api-key");
    assert!(ct.starts_with("multipart/form-data; boundary="));
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("name=\"model_id\""));
    assert!(text.contains("scribe_v2"));
    assert!(text.contains("name=\"language_code\""));
    assert!(text.contains("name=\"diarize\""));
    // ...and NOT the OpenAI field names.
    assert!(!text.contains("name=\"model\""));
    assert!(text.contains("filename=\"clip.wav\""));
    assert!(text.contains("RIFF"));
}

#[test]
fn elevenlabs_transcript_comes_from_text_field() {
    let body = r#"{"text": "scribe heard this", "language_code": "en",
                   "words": [{"text": "scribe", "speaker_id": "spk0"}]}"#;
    assert_eq!(
        parse_elevenlabs_transcript(body).as_deref(),
        Some("scribe heard this")
    );
    assert_eq!(parse_elevenlabs_transcript("{}"), None);
}

#[test]
fn xai_stt_sends_options_before_the_file_part() {
    let (_, body) = xai_stt_multipart("grok-stt", Some("en"), "clip.wav", b"RIFF");
    assert_eq!(XAI_STT_URL, "https://api.x.ai/v1/stt");
    let text = String::from_utf8_lossy(&body);
    let model_at = text.find("name=\"model\"").expect("model field");
    let lang_at = text.find("name=\"language\"").expect("language field");
    let file_at = text.find("name=\"file\"").expect("file part");
    // The documented gotcha: option fields must precede the file part.
    assert!(model_at < file_at && lang_at < file_at);
}

#[test]
fn assemblyai_async_flow_shapes() {
    // Step 2 body: JSON with the upload URL.
    let v = assemblyai_transcript_body(
        "https://cdn.assemblyai.com/up/abc",
        Some(&["universal"]),
        None,
    );
    assert_eq!(v["audio_url"], "https://cdn.assemblyai.com/up/abc");
    assert_eq!(v["speech_models"][0], "universal");
    // Auth is the raw key - no Bearer prefix.
    assert_eq!(ASSEMBLYAI_AUTH_SCHEME, "raw-key");
    // Step 3: poll until completed.
    assert_eq!(
        assemblyai_transcript_status(r#"{"status": "processing"}"#).as_deref(),
        Some("processing")
    );
    assert_eq!(
        assemblyai_transcript_status(r#"{"status": "completed", "text": "done"}"#).as_deref(),
        Some("completed")
    );
    assert_eq!(
        parse_assemblyai_transcript(r#"{"status": "completed", "text": "done"}"#).as_deref(),
        Some("done")
    );
    assert_eq!(assemblyai_transcript_status("{}"), None);
}

#[test]
fn deepgram_tts_voice_is_a_query_param() {
    let url = deepgram_speak_url("aura-2-thalia-en", "mp3");
    assert_eq!(
        url,
        "https://api.deepgram.com/v1/speak?model=aura-2-thalia-en&encoding=mp3"
    );
    let v = deepgram_speak_body("hello");
    assert_eq!(v["text"], "hello");
    assert!(v.get("model").is_none(), "voice is not a body field");
}

#[test]
fn gemini_tts_request_shape_and_pcm_extraction() {
    assert_eq!(
        gemini_tts_url("gemini-2.5-flash-preview-tts"),
        "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash-preview-tts:generateContent"
    );
    assert_eq!(GEMINI_API_KEY_HEADER, "x-goog-api-key");
    let v = gemini_tts_body("Say cheerfully: hi", "Kore");
    assert_eq!(v["generationConfig"]["responseModalities"][0], "AUDIO");
    assert_eq!(
        v["generationConfig"]["speechConfig"]["voiceConfig"]["prebuiltVoiceConfig"]["voiceName"],
        "Kore"
    );
    // Response: base64 raw PCM at candidates[0].content.parts[0].inlineData.data.
    // "AQID" decodes to bytes [1, 2, 3].
    let body = r#"{"candidates": [{"content": {"parts": [
        {"inlineData": {"mimeType": "audio/pcm", "data": "AQID"}}
    ]}}]}"#;
    assert_eq!(gemini_extract_pcm(body).unwrap(), vec![1u8, 2, 3]);
    assert!(gemini_extract_pcm("{}").is_err());
    assert!(gemini_extract_pcm(r#"{"candidates": []}"#).is_err());
}

#[test]
fn gemini_pcm_wraps_in_a_valid_wav_header() {
    // Two 16-bit samples, mono, 24 kHz.
    let wav = gemini_pcm_to_wav(&[0x01, 0x02, 0x03, 0x04]);
    assert_eq!(wav.len(), 44 + 4);
    assert_eq!(&wav[0..4], b"RIFF");
    assert_eq!(&wav[8..12], b"WAVE");
    assert_eq!(&wav[12..16], b"fmt ");
    assert_eq!(u16::from_le_bytes(wav[20..22].try_into().unwrap()), 1); // PCM
    assert_eq!(u16::from_le_bytes(wav[22..24].try_into().unwrap()), 1); // mono
    assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 24_000);
    assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 48_000); // byte rate
    assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2); // block align
    assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16); // bits
    assert_eq!(&wav[36..40], b"data");
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
    assert_eq!(&wav[44..], &[0x01, 0x02, 0x03, 0x04]);
    assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 4);
    // Odd-length input pads to a whole sample.
    let wav = gemini_pcm_to_wav(&[0x01, 0x02, 0x03]);
    assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
    assert_eq!(&wav[44..], &[0x01, 0x02, 0x03, 0x00]);
}

#[test]
fn b64decode_handles_standard_urlsafe_and_padding() {
    assert_eq!(b64decode("TWFu").unwrap(), b"Man");
    assert_eq!(b64decode("TWE=").unwrap(), b"Ma");
    assert_eq!(b64decode("TQ==").unwrap(), b"M");
    assert_eq!(b64decode("TWFu").unwrap(), b64decode("TWFu").unwrap());
    // URL-safe alphabet decodes identically.
    assert_eq!(b64decode("++++").unwrap(), b64decode("----").unwrap());
    assert_eq!(b64decode("++++").unwrap(), vec![0xFB, 0xEF, 0xBE]);
    assert!(b64decode("!!!").is_err());
    assert!(b64decode("TWFuX").is_err(), "length % 4 == 1 is invalid");
}

#[test]
fn fishaudio_model_goes_in_a_header_and_bills_per_byte() {
    assert_eq!(FISHAUDIO_TTS_URL, "https://api.fish.audio/v1/tts");
    assert_eq!(FISHAUDIO_MODEL_HEADER, "model");
    let v = fishaudio_tts_body("hello", "ref-123");
    assert_eq!(v["text"], "hello");
    assert_eq!(v["reference_id"], "ref-123");
    assert_eq!(v["format"], "mp3");
    assert!(
        v.get("model").is_none(),
        "model is a header, not a body field"
    );
}

#[test]
fn elevenlabs_tts_url_carries_the_voice_id() {
    let url = elevenlabs_tts_url("voice-uuid-123");
    assert_eq!(
        url,
        "https://api.elevenlabs.io/v1/text-to-speech/voice-uuid-123"
    );
    let v = elevenlabs_tts_body("hello", "eleven_multilingual_v2");
    assert_eq!(v["model_id"], "eleven_multilingual_v2");
    assert_eq!(v["text"], "hello");
}

#[cfg(unix)]
#[test]
fn local_binary_detection_probes_path_and_piper_backend_runs() {
    use std::os::unix::fs::PermissionsExt;

    let dir = std::env::temp_dir().join(format!("pantheon-voice-detect-{}", std::process::id()));
    let bin = dir.join("bin");
    let voices = dir.join("voices");
    let models = dir.join("models");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&voices).unwrap();
    std::fs::create_dir_all(&models).unwrap();
    // Fake `piper`: ignores args, echoes stdin to stdout like `--output_file -`.
    for name in ["piper", "kokoro-onnx"] {
        let p = bin.join(name);
        std::fs::write(&p, "#!/bin/sh\ncat\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    std::fs::write(voices.join("en_US-lessac-high.onnx"), b"fake-onnx").unwrap();
    std::fs::write(voices.join("en_US-lessac-high.onnx.json"), b"{}").unwrap();
    std::fs::write(models.join("kokoro-v1.0.onnx"), b"fake-onnx").unwrap();

    let prev_path = std::env::var_os("PATH");
    let prev_piper_models = std::env::var_os("PANTHEON_PIPER_MODELS");
    let prev_kokoro_models = std::env::var_os("PANTHEON_KOKORO_MODELS");
    let mut new_path = bin.to_string_lossy().into_owned();
    if let Some(p) = &prev_path {
        new_path.push(':');
        new_path.push_str(&p.to_string_lossy());
    }
    std::env::set_var("PATH", &new_path);
    std::env::set_var("PANTHEON_PIPER_MODELS", &voices);
    std::env::set_var("PANTHEON_KOKORO_MODELS", &models);

    // which_binary: found, missing, and path-separator rejection.
    assert_eq!(which_binary("piper"), Some(bin.join("piper")));
    assert_eq!(which_binary("definitely-not-a-voice-binary"), None);
    assert_eq!(which_binary("a/b"), None);
    assert_eq!(which_binary(""), None);
    // piper install: binary + voice assets.
    let install = detect_piper().expect("fake piper on PATH");
    assert_eq!(install.binary, bin.join("piper"));
    assert!(install
        .voices
        .iter()
        .any(|v| v.ends_with("en_US-lessac-high.onnx")));
    // Voice resolution: by id, by direct path, and missing.
    assert_eq!(
        resolve_piper_voice("en_US-lessac-high", &[]),
        Some(voices.join("en_US-lessac-high.onnx"))
    );
    let direct = voices.join("en_US-lessac-high.onnx");
    assert_eq!(
        resolve_piper_voice(direct.to_str().unwrap(), &[]),
        Some(direct)
    );
    assert_eq!(resolve_piper_voice("no-such-voice", &[]), None);
    // Other local probes.
    let kokoro = detect_kokoro().expect("fake kokoro on PATH");
    assert_eq!(kokoro.cli, Some(bin.join("kokoro-onnx")));
    assert_eq!(kokoro.model, Some(models.join("kokoro-v1.0.onnx")));
    // piper-local constructs from detection and synthesizes.
    let tts = match open_tts(
        "piper-local",
        &opts(&[("voice", "en_US-lessac-high")]),
        None,
    ) {
        Ok(t) => t,
        Err(e) => panic!("piper-local from detected install: {e}"),
    };
    assert_eq!(tts.name(), "piper-local");
    let out = tts.synthesize(&TtsRequest::new("hello out loud")).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.bytes), "hello out loud");
    // Explicit binary + direct voice path also works.
    let tts = match open_tts(
        "piper-local",
        &opts(&[
            ("piper_binary", bin.join("piper").to_str().unwrap()),
            (
                "voice",
                voices.join("en_US-lessac-high.onnx").to_str().unwrap(),
            ),
        ]),
        None,
    ) {
        Ok(t) => t,
        Err(e) => panic!("piper-local from explicit options: {e}"),
    };
    let out = tts
        .synthesize(&TtsRequest::new("hi").with_voice("ignored"))
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.bytes), "hi");

    match prev_path {
        Some(v) => std::env::set_var("PATH", v),
        None => std::env::remove_var("PATH"),
    }
    match prev_piper_models {
        Some(v) => std::env::set_var("PANTHEON_PIPER_MODELS", v),
        None => std::env::remove_var("PANTHEON_PIPER_MODELS"),
    }
    match prev_kokoro_models {
        Some(v) => std::env::set_var("PANTHEON_KOKORO_MODELS", v),
        None => std::env::remove_var("PANTHEON_KOKORO_MODELS"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}
