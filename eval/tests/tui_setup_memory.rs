//! Behavioral tests: the memory screen through the shared
//! non-interactive setup path (`pantheon_tui::setup::run_setup`).
//! Run with `cargo test -p pantheon-eval`.
//!
//! Umar's Screen 8 directive: native memory is zero friction. Picking
//! the recommended (native) backend asks no follow-up questions and
//! leaves a working state: no `[memory]` section is written (native is
//! the runtime default) and the `memory-backend.toml` selection file
//! records the pick, so the runtime instantiates it on next start.

use pantheon_api::config::ToolGroup;
use pantheon_tui::setup::{run_setup, SetupAnswers};
use pantheon_tui::setup_providers::{self, ProviderAnswer, ProviderMeta};
use std::sync::Mutex;
use tempfile::tempdir;

/// Serializes the tests in this file: `run_setup` touches
/// process-global state (PATH probing) and must not overlap itself.
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// The recommended row for a provider group, with the same
/// first-row fallback `run_setup` uses when nothing is marked.
fn recommended_row(metas: &[ProviderMeta]) -> ProviderMeta {
    setup_providers::recommended_provider(metas)
        .or_else(|| metas.first())
        .expect("provider group must offer at least one row")
        .clone()
}

#[test]
fn native_memory_needs_no_followups_and_leaves_working_state() {
    let _guard = ENV_GUARD.lock().unwrap();
    let dir = tempdir().unwrap();
    // The native backend id, resolved from the registry - never
    // hardcoded, so a registry rename updates the expectation here.
    let native_id = recommended_row(&setup_providers::memory_providers()).id;

    // The zero-friction answer: the id alone, no key env, no URL, no
    // options - the shape a Screen 8 "Pantheon Native" pick produces.
    let answers = SetupAnswers {
        provider: Some("nous".to_string()),
        model: Some("Hermes-4-70B".to_string()),
        api_key_env: None,
        fallback_provider: None,
        fallback_model: None,
        policy: None,
        tools: Some(vec![ToolGroup::Memory]),
        websearch: None,
        browser: None,
        stt: None,
        tts: None,
        memory: Some(ProviderAnswer {
            id: native_id.clone(),
            ..Default::default()
        }),
        computer: None,
        key: None,
        custom_provider: None,
        skipped_tools: Vec::new(),
        skipped_stt: false,
        skipped_tts: false,
        mcp_servers: Vec::new(),
        skill_deps_skipped: Vec::new(),
    };
    // assume_defaults = no stdin: a native pick must not prompt.
    let cfg = run_setup(dir.path(), answers, true);

    // Native is the runtime default: no `[memory]` section is written.
    assert!(
        cfg.memory.is_none(),
        "native memory must not write a [memory] section"
    );
    let config_text = std::fs::read_to_string(dir.path().join("config.toml"))
        .expect("run_setup must write config.toml");
    let value: toml::Value = config_text.parse().expect("config.toml must parse");
    assert!(
        value.get("memory").is_none(),
        "native memory must not write a [memory] section"
    );

    // The backend selection file is what the runtime instantiates
    // from: it must record the native pick.
    let selection_text = std::fs::read_to_string(dir.path().join("memory-backend.toml"))
        .expect("run_setup must write memory-backend.toml");
    let selection: toml::Value = selection_text
        .parse()
        .expect("memory-backend.toml must parse");
    assert_eq!(
        selection.get("name").and_then(|n| n.as_str()),
        Some(native_id.as_str()),
        "memory-backend.toml must record the native backend"
    );
}
