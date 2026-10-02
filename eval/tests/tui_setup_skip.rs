//! Behavioral tests: provider-screen Skip through the shared
//! non-interactive setup path (`pantheon_tui::setup::run_setup`).
//! Run with `cargo test -p pantheon-eval`.
//!
//! The rules under test:
//! - skipping a provider screen removes that tool group from the
//!   enabled-tools vec: no provider section is written and the runtime
//!   never registers the tool;
//! - STT/TTS skips are granular per backend: a skipped STT writes no
//!   `[stt]` while an answered TTS still writes `[tts]`; both skipped
//!   turns the Voice group off.
//!
//! The tests drive the wizard's own skip composition
//! (`apply_pick`/`apply_voice_picks` over `ProviderPick`) into
//! `run_setup`, so the covered path is the real one, not a hand-built
//! imitation of it.

use pantheon_api::config::ToolGroup;
use pantheon_tui::setup::{run_setup, SetupAnswers};
use pantheon_tui::setup_providers::{
    self, ProviderAnswer, ProviderKind, ProviderMeta, ProviderPick,
};
use std::path::Path;
use std::process::{Command, Stdio};
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

/// Mirror of the private `--yes` default inside `pantheon_tui::setup`:
/// kind-driven defaults, no prompts, no installs.
fn default_answer(meta: &ProviderMeta) -> ProviderAnswer {
    let mut answer = ProviderAnswer {
        id: meta.id.clone(),
        ..Default::default()
    };
    match &meta.kind {
        ProviderKind::Keyless => {}
        ProviderKind::Cloud { env_var } => {
            answer.key_env = Some(env_var.clone());
        }
        ProviderKind::SelfHosted { default_url, .. } => {
            let url = default_url.trim();
            if !url.is_empty() {
                answer.url = Some(url.to_string());
            }
        }
        ProviderKind::Local { local } => {
            if !detect_binary(&local.detect_cmd) {
                answer.skipped = true;
            }
        }
    }
    for f in &meta.extra {
        if !f.default.is_empty() {
            answer.options.push((f.key.clone(), f.default.clone()));
        }
    }
    answer
}

/// Same probe `setup.rs` uses: exit status 0 = binary present.
fn detect_binary(detect_cmd: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(detect_cmd)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Baseline answers: provider + model picked, recommended provider
/// answers for the groups that stay on, Voice off, nothing skipped.
fn recommended_answers() -> SetupAnswers {
    let websearch = default_answer(&recommended_row(&setup_providers::websearch_providers()));
    let browser = default_answer(&recommended_row(&setup_providers::browser_providers()));
    let memory = default_answer(&recommended_row(&setup_providers::memory_providers()));
    let computer = default_answer(&recommended_row(&setup_providers::computer_providers()));
    let tools: Vec<ToolGroup> = ToolGroup::all()
        .into_iter()
        .filter(|g| *g != ToolGroup::Voice)
        .collect();
    SetupAnswers {
        provider: Some("nous".to_string()),
        model: Some("Hermes-4-70B".to_string()),
        api_key_env: None,
        fallback_provider: None,
        fallback_model: None,
        policy: None,
        tools: Some(tools),
        websearch: Some(websearch),
        browser: Some(browser),
        stt: None,
        tts: None,
        memory: Some(memory),
        computer: Some(computer),
        key: None,
        custom_provider: None,
        skipped_tools: Vec::new(),
        skipped_stt: false,
        skipped_tts: false,
        mcp_servers: Vec::new(),
        skill_deps_skipped: Vec::new(),
    }
}

fn read_config(data_dir: &Path) -> toml::Value {
    let text = std::fs::read_to_string(data_dir.join("config.toml"))
        .expect("run_setup must write config.toml");
    text.parse::<toml::Value>().expect("config.toml must parse")
}

#[test]
fn skipped_browser_screen_removes_group_and_section() {
    // The Skip row on the browser screen removes Browser from the
    // enabled-tools vec: no `[browser]` section is written, the skip is
    // recorded in `[tools]`, and the runtime never registers the
    // browser tools — while unskipped groups still resolve.
    let _guard = ENV_GUARD.lock().unwrap();
    let dir = tempdir().unwrap();
    let mut answers = recommended_answers();
    let mut tools: Vec<ToolGroup> = answers.tools.clone().expect("tools preset");
    let mut skipped = Vec::new();
    let browser = setup_providers::apply_pick(
        &mut tools,
        &mut skipped,
        ToolGroup::Browser,
        &ProviderPick::Skipped,
    );
    assert!(browser.is_none(), "a skipped pick yields no answer");
    answers.tools = Some(tools);
    answers.skipped_tools = skipped;
    answers.browser = browser;
    let cfg = run_setup(dir.path(), answers, true);

    let value = read_config(dir.path());
    assert!(
        value.get("browser").is_none(),
        "skipped browser screen: no [browser] section"
    );
    assert_eq!(
        value["tools"]["browser"].as_bool(),
        Some(false),
        "[tools] records the browser skip"
    );
    assert_eq!(
        value["tools"]["voice"].as_bool(),
        Some(false),
        "[tools] still records voice off"
    );
    let enablement = pantheon_tui::config::tool_enablement(Some(&cfg));
    assert!(
        !enablement.is_enabled(ToolGroup::Browser),
        "the runtime must never register browser tools after a skip"
    );
    assert!(
        enablement.is_enabled(ToolGroup::WebSearch),
        "unskipped groups stay registered"
    );
    assert!(
        value.get("websearch").is_some(),
        "unskipped groups still resolve their recommended provider"
    );
    assert!(value.get("computer_use").is_some());
}

#[test]
fn skipped_voice_screens_turn_voice_off() {
    // Both speech screens skipped: the Voice group comes off, so no
    // `[stt]`, no `[tts]`, and the runtime registers no voice tools.
    let _guard = ENV_GUARD.lock().unwrap();
    let dir = tempdir().unwrap();
    let mut answers = recommended_answers();
    let mut tools = ToolGroup::all().to_vec();
    let mut skipped = Vec::new();
    let voice = setup_providers::apply_voice_picks(
        &mut tools,
        &mut skipped,
        &ProviderPick::Skipped,
        &ProviderPick::Skipped,
    );
    assert!(
        !tools.contains(&ToolGroup::Voice),
        "both backends skipped: Voice comes off the tools vec"
    );
    answers.tools = Some(tools);
    answers.skipped_tools = skipped;
    answers.stt = voice.stt;
    answers.tts = voice.tts;
    answers.skipped_stt = voice.skipped_stt;
    answers.skipped_tts = voice.skipped_tts;
    let cfg = run_setup(dir.path(), answers, true);

    let value = read_config(dir.path());
    assert!(value.get("stt").is_none(), "voice off: no [stt]");
    assert!(value.get("tts").is_none(), "voice off: no [tts]");
    assert!(
        !pantheon_tui::config::tool_enablement(Some(&cfg)).is_enabled(ToolGroup::Voice),
        "the runtime must never register voice tools after a skip"
    );
}

#[test]
fn skipped_stt_keeps_tts() {
    // Granular skip: the STT screen is skipped while TTS is answered
    // and Voice stays on. `[tts]` is written with the picked backend;
    // `[stt]` is absent — the recommended STT default must NOT fill
    // the gap the user explicitly declined.
    let _guard = ENV_GUARD.lock().unwrap();
    let dir = tempdir().unwrap();
    let tts_answer = default_answer(&recommended_row(&setup_providers::tts_providers()));
    let tts_id = tts_answer.id.clone();
    let mut answers = recommended_answers();
    let mut tools = ToolGroup::all().to_vec();
    let mut skipped = Vec::new();
    let voice = setup_providers::apply_voice_picks(
        &mut tools,
        &mut skipped,
        &ProviderPick::Skipped,
        &ProviderPick::Chosen(tts_answer),
    );
    assert!(
        tools.contains(&ToolGroup::Voice),
        "one backend answered: Voice stays on"
    );
    assert!(voice.skipped_stt && !voice.skipped_tts);
    answers.tools = Some(tools);
    answers.skipped_tools = skipped;
    answers.stt = voice.stt;
    answers.tts = voice.tts;
    answers.skipped_stt = voice.skipped_stt;
    answers.skipped_tts = voice.skipped_tts;
    let cfg = run_setup(dir.path(), answers, true);

    let value = read_config(dir.path());
    assert!(value.get("stt").is_none(), "skipped STT: no [stt] section");
    assert_eq!(
        value["tts"]["backend"].as_str(),
        Some(tts_id.as_str()),
        "answered TTS still writes [tts]"
    );
    assert!(
        pantheon_tui::config::tool_enablement(Some(&cfg)).is_enabled(ToolGroup::Voice),
        "Voice stays registered while one backend is configured"
    );
}
