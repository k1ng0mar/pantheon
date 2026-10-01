//! Config document behavior as seen through the public API.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Everything here goes through `Config::load` against a real
//! `config.toml` on disk — no struct-literal construction, no
//! internals. Run with `cargo test -p pantheon-eval --test api_config`.
use pantheon_api::config::{
    nightly_enabled, nightly_enabled_reason, nightly_model_pin_present, Config,
};

/// Write `text` to `<dir>/config.toml` (the path `Config::load` reads)
/// and load it. `name` must be unique per test — the eval binary runs
/// tests in parallel.
fn load_config(name: &str, text: &str) -> Config {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-api-config-{}-{}",
        std::process::id(),
        name
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), text).unwrap();
    let cfg = Config::load(&dir).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    cfg
}

/// Voice/STT problems reported by `Config::validate` — the user-visible
/// config-doctor signal for `[stt]` / `[tts]`.
fn voice_problems(cfg: &Config) -> Vec<String> {
    cfg.validate()
        .into_iter()
        .filter(|p| p.starts_with("stt") || p.starts_with("tts"))
        .collect()
}

// --- `[nightly]` enable rule ------------------------------------------------
// Nightly is off by default. It turns on when `[nightly.model]` pins a
// model (or the PANTHEON_NIGHTLY_* env overrides do), and an explicit
// `enabled = false` always wins.

#[test]
fn nightly_explicit_on_enables_the_pass() {
    let cfg = load_config("nightly-on", "[nightly]\nenabled = true\n");
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    assert_eq!(n.enabled, Some(true));
    assert!(nightly_enabled(n));
    assert_eq!(nightly_enabled_reason(n), "explicit flag on");
}

#[test]
fn nightly_explicit_off_disables_the_pass() {
    let cfg = load_config("nightly-off", "[nightly]\nenabled = false\n");
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    assert_eq!(n.enabled, Some(false));
    assert!(!nightly_enabled(n));
    assert_eq!(nightly_enabled_reason(n), "explicit flag off");
}

#[test]
fn nightly_absent_flag_is_off_by_default() {
    // The env overrides count as a pin, so clear them: off-by-default
    // must not depend on the ambient environment.
    std::env::remove_var("PANTHEON_NIGHTLY_PROVIDER");
    std::env::remove_var("PANTHEON_NIGHTLY_MODEL");
    let cfg = load_config("nightly-default", "[nightly]\n");
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    assert_eq!(n.enabled, None, "absent flag is None, not false");
    assert!(!nightly_enabled(n));
    assert_eq!(nightly_enabled_reason(n), "off (no flag, no model pin)");
}

#[test]
fn nightly_model_pin_enables_the_pass() {
    let cfg = load_config(
        "nightly-pin",
        "[nightly]\n[nightly.model]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\n",
    );
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    let pin = n.model.as_ref().expect("[nightly.model] must parse");
    assert_eq!(pin.provider, "openai");
    assert_eq!(pin.model, "gpt-4o-mini");
    assert!(nightly_model_pin_present(n));
    // Pin present + absent flag = on.
    assert!(nightly_enabled(n));
}

#[test]
fn nightly_empty_pin_table_does_not_enable() {
    std::env::remove_var("PANTHEON_NIGHTLY_PROVIDER");
    std::env::remove_var("PANTHEON_NIGHTLY_MODEL");
    let cfg = load_config("nightly-empty-pin", "[nightly]\n[nightly.model]\n");
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    assert!(!nightly_model_pin_present(n));
    assert!(!nightly_enabled(n));
}

#[test]
fn nightly_explicit_off_wins_over_model_pin() {
    let cfg = load_config(
        "nightly-off-wins",
        "[nightly]\nenabled = false\n[nightly.model]\nprovider = \"openai\"\nmodel = \"gpt-4o-mini\"\n",
    );
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    assert!(nightly_model_pin_present(n));
    assert!(!nightly_enabled(n));
    assert_eq!(nightly_enabled_reason(n), "explicit flag off");
}

#[test]
fn nightly_default_provider_pin_counts_as_a_pin() {
    // provider = "default" is an explicit pin to the default model —
    // presence of the pin = enabled.
    let cfg = load_config(
        "nightly-default-pin",
        "[nightly]\n[nightly.model]\nprovider = \"default\"\n",
    );
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    assert!(nightly_model_pin_present(n));
    assert!(nightly_enabled(n));
}

#[test]
fn nightly_section_knobs_parse() {
    let cfg = load_config(
        "nightly-knobs",
        "[nightly]\nenabled = true\nauto_turns = 30\nmin_sessions = 5\nmax_age_days = 14\ncron = \"1 2 * * *\"\n",
    );
    let n = cfg.nightly.as_ref().expect("[nightly] must parse");
    assert_eq!(n.auto_turns, 30);
    assert_eq!(n.min_sessions, 5);
    assert_eq!(n.max_age_days, 14);
    assert_eq!(n.cron, "1 2 * * *");
}

// --- `[stt]` / `[tts]` validation ------------------------------------------
// `Config::validate` is the config-doctor signal: bad voice backends or
// options are reported as problems rather than failing the load.

#[test]
fn absent_voice_sections_have_no_problems() {
    let cfg = load_config("voice-absent", "");
    assert!(voice_problems(&cfg).is_empty());
}

#[test]
fn valid_command_stt_and_openai_tts_pass_validation() {
    let cfg = load_config(
        "voice-valid",
        "[stt]\nbackend = \"command\"\n[stt.options]\ncmd = \"whisper\"\ntimeout_secs = \"60\"\n\
         [tts]\nbackend = \"openai\"\n[tts.options]\nprovider = \"openai\"\nmodel = \"tts-1\"\napi_key_env = \"OPENAI_API_KEY\"\n",
    );
    assert!(voice_problems(&cfg).is_empty());
}

#[test]
fn unknown_voice_backend_is_rejected() {
    let cfg = load_config(
        "voice-unknown",
        "[stt]\nbackend = \"bogus\"\n[tts]\nbackend = \"bogus\"\n",
    );
    let ps = voice_problems(&cfg);
    assert_eq!(ps.len(), 2);
    assert!(ps.iter().all(|p| p.contains("is unknown")));
}

#[test]
fn blank_cmd_for_command_backend_is_rejected() {
    let cfg = load_config(
        "voice-blank-cmd",
        "[stt]\nbackend = \"command\"\n[stt.options]\ncmd = \"  \"\n",
    );
    let ps = voice_problems(&cfg);
    assert_eq!(ps.len(), 1);
    assert!(ps[0].contains("options.cmd is required"));
}

#[test]
fn blank_provider_for_openai_backend_is_rejected() {
    let cfg = load_config(
        "voice-blank-provider",
        "[tts]\nbackend = \"openai\"\n[tts.options]\nprovider = \"\"\n",
    );
    let ps = voice_problems(&cfg);
    assert_eq!(ps.len(), 1);
    assert!(ps[0].contains("options.provider is required"));
}

#[test]
fn non_positive_timeout_secs_is_rejected() {
    for t in ["0", "-5", "abc", ""] {
        let cfg = load_config(
            &format!("voice-timeout-{t}"),
            &format!("[stt]\nbackend = \"command\"\n[stt.options]\ncmd = \"x\"\ntimeout_secs = \"{t}\"\n"),
        );
        let ps = voice_problems(&cfg);
        assert_eq!(ps.len(), 1, "timeout_secs={t:?}");
        assert!(ps[0].contains("not a positive integer"));
    }
}

#[test]
fn blank_api_key_env_is_rejected() {
    let cfg = load_config(
        "voice-blank-key-env",
        "[tts]\nbackend = \"openai\"\n[tts.options]\nprovider = \"openai\"\napi_key_env = \"  \"\n",
    );
    let ps = voice_problems(&cfg);
    assert_eq!(ps.len(), 1);
    assert!(ps[0].contains("api_key_env is empty"));
}

#[test]
fn stt_and_tts_problems_report_independently() {
    let cfg = load_config(
        "voice-independent",
        "[stt]\nbackend = \"command\"\n[stt.options]\ncmd = \"whisper\"\n[tts]\nbackend = \"bogus\"\n",
    );
    let ps = voice_problems(&cfg);
    assert_eq!(ps.len(), 1);
    assert!(ps[0].starts_with("tts"));
}

// --- Legacy `[reflect]` / `[consolidation]` compatibility ------------------
// `[nightly]` is the single authoritative section, but configs written
// before it must keep loading: the legacy tables still parse and carry
// their values through, and the document layer never rejects the
// combination.

#[test]
fn legacy_reflect_section_still_parses_without_nightly() {
    let cfg = load_config(
        "legacy-reflect",
        "[reflect]\nenabled = true\nauto_turns = 7\nmax_proposals = 9\n",
    );
    let r = cfg.reflect.as_ref().expect("[reflect] must parse");
    assert!(r.enabled);
    assert_eq!(r.auto_turns, 7);
    assert_eq!(r.max_proposals, 9);
    assert!(cfg.nightly.is_none());
}

#[test]
fn legacy_consolidation_section_still_parses_without_nightly() {
    let cfg = load_config(
        "legacy-consolidation",
        "[consolidation]\nenabled = true\nmin_sessions = 6\ncron = \"1 2 * * *\"\n",
    );
    let c = cfg
        .consolidation
        .as_ref()
        .expect("[consolidation] must parse");
    assert!(c.enabled);
    assert_eq!(c.min_sessions, 6);
    // The migration fallback honors this field-by-field when [nightly]
    // is absent, so the value must survive the load.
    assert_eq!(c.cron, "1 2 * * *");
    assert!(cfg.nightly.is_none());
}

#[test]
fn nightly_and_legacy_sections_coexist_on_load() {
    // Precedence is decided by the consumer; the document layer must
    // never reject the combination.
    let cfg = load_config(
        "nightly-legacy-coexist",
        "[nightly]\nenabled = false\n[reflect]\nenabled = true\n[consolidation]\nenabled = true\n",
    );
    assert!(cfg.nightly.is_some());
    assert!(cfg.reflect.is_some());
    assert!(cfg.consolidation.is_some());
}
