//! Behavioral evals for the adversarial `Verify` auxiliary.
//!
//! Covers: `[verify]` is OFF unless configured (absent section = no
//! `Verify` auxiliary entry, never auto-verified), an explicit
//! `[verify]` pin produces a `Verify` entry with its own provider /
//! model / timeout, `PANTHEON_VERIFY_*` env overrides win over the pin,
//! and `VerifyClient::from_policy` mirrors the config (None when the
//! slot is absent, Some when pinned).
//! Run with `cargo test -p pantheon-eval`.

use pantheon_api::model::{AuxiliaryKind, DefaultModel, ModelPolicy};
use pantheon_providers::VerifyClient;
use pantheon_tui::config::{self, Config};

// `PANTHEON_VERIFY_*` is process-global: a test that sets it must hold
// this lock, and every test that builds a policy (which reads the env)
// must hold it too, or parallel test threads race and the "off unless
// configured" test sees another test's override.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn write_config(dir: &std::path::Path, toml: &str) -> Config {
    std::fs::write(dir.join("config.toml"), toml).unwrap();
    Config::load(dir).expect("config must parse")
}

fn tmpdir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-eval-verify-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn default_model() -> DefaultModel {
    DefaultModel {
        provider: "openai".into(),
        model: "chat-model".into(),
    }
}

fn policy_with(cfg: Option<&Config>) -> ModelPolicy {
    let default = default_model();
    ModelPolicy {
        default: default.clone(),
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: config::auxiliaries(cfg, &default),
        reasoning: pantheon_api::model::ReasoningLevel::default(),
        reasoning_budget: None,
    }
}

fn verify_entry(policy: &ModelPolicy) -> Option<&pantheon_api::model::AuxiliaryModel> {
    policy
        .auxiliaries
        .iter()
        .find(|a| a.kind == AuxiliaryKind::Verify)
}

#[test]
fn verify_is_off_unless_configured() {
    let _env = ENV_LOCK.lock().unwrap();
    let dir = tmpdir("off");
    let cfg = write_config(
        &dir,
        "[model]\nprovider = \"openai\"\nmodel = \"chat-model\"\n",
    );
    let policy = policy_with(Some(&cfg));
    assert!(
        verify_entry(&policy).is_none(),
        "absent [verify] must produce no Verify entry — verification is opt-in"
    );
    assert!(
        VerifyClient::from_policy(&policy, None).is_none(),
        "no client can exist without a Verify entry"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_pin_produces_dedicated_entry() {
    let _env = ENV_LOCK.lock().unwrap();
    let dir = tmpdir("pin");
    let cfg = write_config(
        &dir,
        "[model]\nprovider = \"openai\"\nmodel = \"chat-model\"\n\n\
         [verify]\nprovider = \"anthropic\"\nmodel = \"verify-cheap\"\ntimeout = 45\n",
    );
    let policy = policy_with(Some(&cfg));
    let entry = verify_entry(&policy).expect("[verify] pin must produce a Verify entry");
    assert_eq!(entry.provider, "anthropic");
    assert_eq!(entry.model, "verify-cheap");
    assert_eq!(entry.timeout_secs, 45);
    assert!(
        VerifyClient::from_policy(&policy, None).is_some(),
        "pinned Verify entry must yield a client"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_env_overrides_win_over_pin() {
    let _env = ENV_LOCK.lock().unwrap();
    let dir = tmpdir("env");
    let cfg = write_config(
        &dir,
        "[model]\nprovider = \"openai\"\nmodel = \"chat-model\"\n\n\
         [verify]\nprovider = \"anthropic\"\nmodel = \"verify-cheap\"\n",
    );
    std::env::set_var("PANTHEON_VERIFY_MODEL", "verify-env");
    let policy = policy_with(Some(&cfg));
    std::env::remove_var("PANTHEON_VERIFY_MODEL");
    let entry = verify_entry(&policy).expect("[verify] pin must produce a Verify entry");
    assert_eq!(
        entry.model, "verify-env",
        "PANTHEON_VERIFY_MODEL must win over the [verify] pin"
    );
    // Provider inherits the pin when only the model is overridden.
    assert_eq!(entry.provider, "anthropic");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verify_default_timeout_is_thirty_seconds() {
    let _env = ENV_LOCK.lock().unwrap();
    let dir = tmpdir("timeout");
    let cfg = write_config(
        &dir,
        "[model]\nprovider = \"openai\"\nmodel = \"chat-model\"\n\n[verify]\nmodel = \"verify-cheap\"\n",
    );
    let policy = policy_with(Some(&cfg));
    let entry = verify_entry(&policy).expect("[verify] pin must produce a Verify entry");
    assert_eq!(
        entry.timeout_secs, 30,
        "verify defaults to a 30s timeout when unset"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
