//! Behavioral tests: the setup provider flow is metadata-driven.
//! Run with `cargo test -p pantheon-eval`.
//!
//! The rules under test:
//! - every provider row comes from the owning crate's registry (a new
//!   backend appears in setup with no wizard change);
//! - only backends the runtime can actually construct are offered;
//! - the follow-up flow branches on the provider *kind*, never on the
//!   provider id;
//! - cloud follow-ups ask for the env var NAME, never the value.
use pantheon_tui::setup_providers as sp;

#[test]
fn websearch_rows_come_from_the_registry_recommended_first() {
    let rows = sp::websearch_providers();
    let reg: Vec<String> = pantheon_web::websearch::all_providers()
        .into_iter()
        .map(|p| p.id.to_string())
        .collect();
    let row_ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(row_ids, reg.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    assert!(
        rows[0].recommended,
        "first row must be the recommended provider"
    );
    assert_eq!(rows[0].id, "tinyfish");
}

#[test]
fn browser_rows_cover_every_registry_backend() {
    let rows = sp::browser_providers();
    let reg: Vec<String> = pantheon_web::browser::all_backends()
        .into_iter()
        .map(|b| b.kind.id().to_string())
        .collect();
    let mut row_ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    let mut reg_ids: Vec<&str> = reg.iter().map(|s| s.as_str()).collect();
    row_ids.sort_unstable();
    reg_ids.sort_unstable();
    assert_eq!(
        row_ids, reg_ids,
        "wizard rows must match the browser registry"
    );
    let gsd = rows.iter().find(|r| r.id == "gsd").expect("gsd row");
    assert!(gsd.recommended, "gsd is the documented default backend");
}

#[test]
fn stt_rows_cover_every_registry_backend() {
    // Umar's approved STT roster: groq (recommended), openai, mistral,
    // xai, elevenlabs scribe, deepgram, assemblyai, plus the local
    // `command` backend. Every registry entry is offered and every one
    // constructs via `open_stt` (bespoke wires are implemented from
    // public docs; first live runs are verification runs).
    let rows = sp::stt_providers();
    let reg: Vec<String> = pantheon_providers::stt_providers()
        .into_iter()
        .map(|b| b.name.to_string())
        .collect();
    let mut row_ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    let mut reg_ids: Vec<&str> = reg.iter().map(|s| s.as_str()).collect();
    row_ids.sort_unstable();
    reg_ids.sort_unstable();
    assert_eq!(row_ids, reg_ids, "wizard rows must match the STT registry");
    for r in &rows {
        // The generic `command` backend needs its explicit invocation.
        let opts = if r.id == "command" {
            std::collections::HashMap::from([("cmd".to_string(), "true".to_string())])
        } else {
            std::collections::HashMap::new()
        };
        pantheon_providers::voice::open_stt(&r.id, &opts, None)
            .unwrap_or_else(|e| panic!("stt row {} must construct: {}", r.id, e.cause));
    }
    // Groq is the approved recommended default.
    assert!(sp::find_provider(&rows, "groq")
        .map(|r| r.recommended)
        .unwrap_or(false));
}

#[test]
fn tts_rows_cover_every_registry_backend() {
    // Umar's approved TTS roster: piper-local (recommended), kokoro-local,
    // openai, elevenlabs, deepgram, gemini, fishaudio, plus `command`.
    // Every registry entry is offered and constructs via `open_tts`;
    // elevenlabs/fishaudio need a voice option, kokoro-local needs an
    // explicit cmd - all fail closed with VOICE_CONFIG, never a hang.
    let rows = sp::tts_providers();
    let reg: Vec<String> = pantheon_providers::tts_providers()
        .into_iter()
        .map(|b| b.name.to_string())
        .collect();
    let mut row_ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
    let mut reg_ids: Vec<&str> = reg.iter().map(|s| s.as_str()).collect();
    row_ids.sort_unstable();
    reg_ids.sort_unstable();
    assert_eq!(row_ids, reg_ids, "wizard rows must match the TTS registry");
    for r in &rows {
        let mut opts = std::collections::HashMap::from([
            ("voice".to_string(), "test-voice".to_string()),
            ("cmd".to_string(), "true".to_string()),
            // Kokoro needs explicit args (community CLIs differ in shape).
            ("args".to_string(), "--voice {voice}".to_string()),
            // The legacy `openai` row reads its provider id from options.
            ("provider".to_string(), "openai".to_string()),
        ]);
        if r.id == "piper-local" {
            // Piper resolves the voice to a real file: point it at a
            // temp stub so construction (not synthesis) is exercised.
            let stub = std::env::temp_dir().join("pantheon-test-voice.onnx");
            std::fs::write(&stub, b"stub").unwrap();
            opts.insert("voice".to_string(), stub.to_string_lossy().into_owned());
        }
        pantheon_providers::voice::open_tts(&r.id, &opts, None)
            .unwrap_or_else(|e| panic!("tts row {} must construct: {}", r.id, e.cause));
    }
    // Piper exposes the vetted `en_US-lessac-high` default and explains
    // the `.onnx` plus `.onnx.json` assets.
    let piper = sp::find_provider(&rows, "piper-local").expect("piper row");
    assert!(piper.recommended, "piper is the approved local default");
    assert!(piper.extra.iter().any(|f| f.key == "voice" && f.required));
    // Kokoro asks for the explicit command (community CLIs differ).
    let kokoro = sp::find_provider(&rows, "kokoro-local").expect("kokoro row");
    assert!(kokoro.extra.iter().any(|f| f.key == "cmd" && f.required));
}

#[test]
fn memory_rows_exclude_the_fake_backend() {
    let rows = sp::memory_providers();
    assert!(!rows.iter().any(|r| r.id == "fake"), "test double offered");
    let native = rows.iter().find(|r| r.id == "native").expect("native row");
    assert!(native.recommended, "native is the runtime default");
}

#[test]
fn cloud_answer_records_env_name_never_value() {
    let rows = sp::stt_providers();
    let meta = sp::find_provider(&rows, "openai").expect("openai stt");
    let answer = sp::complete_answer(
        meta,
        &mut |_label, _prefill| Some("CUSTOM_STT_KEY".to_string()),
        &mut |_, _| Some(false),
    )
    .expect("answer");
    assert_eq!(answer.id, "openai");
    assert_eq!(answer.key_env.as_deref(), Some("CUSTOM_STT_KEY"));
    assert!(answer.options.is_empty());
}

#[test]
fn empty_cloud_answer_falls_back_to_provider_default() {
    let rows = sp::websearch_providers();
    let meta = sp::find_provider(&rows, "tinyfish").expect("tinyfish");
    let answer = sp::complete_answer(
        meta,
        &mut |_label, _prefill| Some(String::new()),
        &mut |_, _| Some(false),
    )
    .expect("answer");
    assert!(
        answer.key_env.is_some(),
        "empty answer must use the provider default"
    );
}

#[test]
fn required_extra_field_reprompts_on_empty() {
    let rows = sp::stt_providers();
    let meta = sp::find_provider(&rows, "command").expect("command stt");
    let mut calls = 0;
    let answer = sp::complete_answer(
        meta,
        &mut |_label, _prefill| {
            calls += 1;
            if calls == 1 {
                Some(String::new())
            } else {
                Some("whisper -f -".to_string())
            }
        },
        &mut |_, _| Some(false),
    )
    .expect("answer");
    assert_eq!(calls, 3, "required field must reprompt on empty");
    assert_eq!(
        answer.options,
        vec![
            ("cmd".to_string(), "whisper -f -".to_string()),
            ("args".to_string(), "whisper -f -".to_string()),
        ]
    );
}

#[test]
fn cancel_aborts_the_answer() {
    let rows = sp::websearch_providers();
    let answer = sp::complete_answer(&rows[0], &mut |_, _| None, &mut |_, _| None);
    assert!(answer.is_none());
}

#[test]
fn recommended_lookup_finds_the_default_row() {
    let rows = sp::websearch_providers();
    let rec = sp::recommended_provider(&rows).expect("a recommended row");
    assert_eq!(rec.id, "tinyfish");
    assert!(
        sp::find_provider(&rows, "TINYFISH").is_some(),
        "lookup is case-insensitive"
    );
    assert!(sp::find_provider(&rows, "nope").is_none());
}

#[test]
fn tools_screen_answer_round_trips_to_enablement() {
    use pantheon_api::config::{ToolGroup, ToolsSection};
    use pantheon_runtime::tool_config::ToolEnablement;
    // The wizard writes only deviations from all-on; the runtime
    // resolves them back to the same toggles.
    let groups: Vec<ToolGroup> = ToolGroup::all()
        .into_iter()
        .filter(|g| *g != ToolGroup::Browser)
        .collect();
    let section = ToolsSection::from_enabled(&groups).expect("one group off");
    let e = ToolEnablement::from_section(&section);
    assert!(!e.is_enabled(ToolGroup::Browser));
    assert!(e.is_enabled(ToolGroup::WebSearch));
    assert!(e.is_enabled(ToolGroup::Voice));
}

#[test]
fn recommended_mode_shows_fixed_provider_screens() {
    use pantheon_tui::setup_graph::{sections, Answers, Mode, Section};
    // Recommended mode: fixed provider screens, no Tools screen - the
    // recommended toolset drives the screens, not tool answers. Memory
    // is always present (its screen records silently; the native
    // backend is keyless).
    let a = Answers {
        mode: Some(Mode::Recommended),
        ..Default::default()
    };
    let s = sections(&a);
    for want in [
        Section::Provider,
        Section::Model,
        Section::WebSearch,
        Section::Browser,
        Section::Memory,
        Section::ComputerUse,
    ] {
        assert!(s.contains(&want), "recommended shows {want:?}");
    }
    assert!(
        !s.contains(&Section::Tools),
        "no Tools screen in recommended mode"
    );
}

#[test]
fn computer_use_provider_is_the_cua_driver_with_detect_and_install() {
    use pantheon_tui::setup_providers::ProviderKind;
    let rows = sp::computer_providers();
    assert_eq!(rows.len(), 1, "exactly one driver today: cua-driver");
    let row = &rows[0];
    assert_eq!(row.id, "cua-driver");
    assert!(row.recommended, "the only driver is the recommended pick");
    match &row.kind {
        ProviderKind::Local { local } => {
            assert!(
                local.detect_cmd.contains("cua-driver"),
                "detect must probe the driver binary"
            );
            assert!(
                local
                    .install_cmd
                    .as_deref()
                    .unwrap_or("")
                    .contains("cua.ai"),
                "install must be the verified official one-liner"
            );
            assert!(
                local.install_hint.contains("cua-driver serve"),
                "hint must mention the daemon requirement"
            );
        }
        other => panic!("cua-driver must be a local provider, got {other:?}"),
    }
    assert!(
        row.blurb.contains("X11") || row.blurb.contains("graphical"),
        "blurb must state the graphical-session requirement"
    );
}

#[test]
fn full_mode_gates_the_computer_use_screen_on_the_tool_answer() {
    use pantheon_tui::setup_graph::{sections, Answers, Mode, Section};
    let on = Answers {
        mode: Some(Mode::Full),
        computer_use_enabled: true,
        ..Default::default()
    };
    assert!(sections(&on).contains(&Section::ComputerUse));
    let off = Answers {
        mode: Some(Mode::Full),
        ..Default::default()
    };
    assert!(!sections(&off).contains(&Section::ComputerUse));
}
