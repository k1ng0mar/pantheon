//! Behavioral tests: the Recommended setup path through the shared
//! non-interactive setup path (`pantheon_tui::setup::run_setup`).
//! Run with `cargo test -p pantheon-eval`.
//!
//! The rules under test:
//! - the Recommended flow is provider + model, then the recommended
//!   provider rows for websearch/browser/memory/computer-use, with no
//!   Tools screen and STT/TTS skipped: `tools` = every [`ToolGroup`]
//!   except `Voice`;
//! - recommended provider ids always resolve from the owning registries
//!   via `recommended_provider` — never hardcoded, so a registry change
//!   updates the expectations here instead of silently testing stale ids;
//! - the `--yes` default for a local provider (missing binary) records
//!   `skipped` and never runs an installer; installs are interactive-only.

use pantheon_api::config::ToolGroup;
use pantheon_tui::setup::{run_setup, SetupAnswers};
use pantheon_tui::setup_providers::{self, ProviderAnswer, ProviderKind, ProviderMeta};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tempfile::tempdir;

/// Serializes the tests in this file: one of them swaps `PATH`
/// process-wide to tripwire installers, and a concurrent `run_setup`
/// must not observe the swapped value.
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
/// kind-driven defaults, no prompts, and — critically — no installs.
/// Detection shells out read-only (`sh -c <detect_cmd>`); the install
/// path (`offer_install`) is only reachable from the interactive flow
/// after a confirm, so it cannot fire here.
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

/// The `SetupAnswers` the Recommended TUI flow hands to `run_setup`:
/// provider + model from the pickers, the recommended provider answer
/// for every provider-backed group that stays on, STT/TTS dropped with
/// the Voice group, no Tools screen (`tools` = all groups but Voice).
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
        // From the model catalog (pantheon-providers): official Hermes 4 id.
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

fn expect_recommended_setup(value: &toml::Value) {
    let websearch_id = recommended_row(&setup_providers::websearch_providers()).id;
    let browser_id = recommended_row(&setup_providers::browser_providers()).id;
    let computer_id = recommended_row(&setup_providers::computer_providers()).id;

    assert_eq!(
        value["websearch"]["provider"].as_str(),
        Some(websearch_id.as_str()),
        "[websearch] must record the recommended provider"
    );
    assert_eq!(
        value["browser"]["backend"].as_str(),
        Some(browser_id.as_str()),
        "[browser] must record the recommended backend"
    );
    assert!(
        value.get("memory").is_none(),
        "native is the runtime default: no [memory] section"
    );
    assert_eq!(
        value["computer_use"]["driver"].as_str(),
        Some(computer_id.as_str()),
        "[computer_use] must record the recommended driver"
    );
    assert!(value.get("stt").is_none(), "Voice off: no [stt] section");
    assert!(value.get("tts").is_none(), "Voice off: no [tts] section");

    // `[tools]` records only deviations from the all-on default: voice
    // off, and nothing else.
    let tools = value.get("tools").expect("[tools] records voice off");
    assert_eq!(
        tools.get("voice").and_then(|v| v.as_bool()),
        Some(false),
        "[tools] must record voice = false"
    );
    let keys: Vec<&str> = tools
        .as_table()
        .expect("[tools] is a table")
        .keys()
        .map(|k| k.as_str())
        .collect();
    assert_eq!(
        keys,
        vec!["voice"],
        "[tools] must record voice off and nothing else"
    );
}

#[test]
fn recommended_setup_writes_recommended_providers_and_no_voice_sections() {
    let _guard = ENV_GUARD.lock().unwrap();
    let dir = tempdir().unwrap();
    let cfg = run_setup(dir.path(), recommended_answers(), true);

    let model = cfg.model.as_ref().expect("[model] written");
    assert_eq!(model.provider, "nous");
    assert_eq!(model.model, "Hermes-4-70B");

    expect_recommended_setup(&read_config(dir.path()));
}

#[test]
fn full_mode_still_writes_stt_and_tts() {
    // The Recommended omission of [stt]/[tts] is a tools-selection
    // effect, not a setup.rs regression: with the Voice group on, the
    // same path writes both sections.
    let _guard = ENV_GUARD.lock().unwrap();
    let dir = tempdir().unwrap();
    let stt = default_answer(&recommended_row(&setup_providers::stt_providers()));
    let tts = default_answer(&recommended_row(&setup_providers::tts_providers()));
    let (stt_id, tts_id) = (stt.id.clone(), tts.id.clone());

    let mut answers = recommended_answers();
    answers.tools = Some(ToolGroup::all().to_vec());
    answers.stt = Some(stt);
    answers.tts = Some(tts);
    run_setup(dir.path(), answers, true);

    let value = read_config(dir.path());
    assert_eq!(
        value["stt"]["backend"].as_str(),
        Some(stt_id.as_str()),
        "[stt] must record the chosen backend"
    );
    assert_eq!(
        value["tts"]["backend"].as_str(),
        Some(tts_id.as_str()),
        "[tts] must record the chosen backend"
    );
    assert!(
        value.get("tools").is_none(),
        "all groups on writes no [tools] section"
    );
}

#[test]
fn assume_defaults_resolves_recommended_without_installs() {
    // Let setup.rs resolve every provider answer itself (all `None`
    // under assume-defaults): this drives the private `default_answer`
    // for the local computer-use driver. The PATH below contains only
    // tripwire installers — if setup.rs ever ran an install_cmd, the
    // marker file would exist afterwards.
    let _guard = ENV_GUARD.lock().unwrap();
    let dir = tempdir().unwrap();
    let bindir = dir.path().join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let marker = dir.path().join("installer-ran");
    for tool in ["curl", "npm", "pip", "pip3"] {
        let script = bindir.join(tool);
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"install attempted: {tool}\" >> \"{}\"\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink("/bin/sh", bindir.join("sh")).unwrap();

    let old_path = std::env::var_os("PATH");
    std::env::set_var("PATH", &bindir);

    let mut answers = recommended_answers();
    answers.websearch = None;
    answers.browser = None;
    answers.memory = None;
    answers.computer = None;
    run_setup(dir.path(), answers, true);

    match old_path {
        Some(p) => std::env::set_var("PATH", p),
        None => std::env::remove_var("PATH"),
    }

    assert!(
        !marker.exists(),
        "assume-defaults must resolve local providers by detection only, never by installing"
    );
    expect_recommended_setup(&read_config(dir.path()));
}
