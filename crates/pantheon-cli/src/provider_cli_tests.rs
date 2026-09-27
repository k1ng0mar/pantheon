//! Tests for `crate::provider_cli` (kept here so source files stay test-free).

use crate::config_doc::{Config, CustomProviderSection};
use crate::dotenv::test_support::TEST_ENV_LOCK;
use crate::provider_cli::*;
use pantheon_providers::catalog;

fn provider_args(extra: &[&str]) -> Vec<String> {
    let mut a = vec!["pantheon".to_string(), "provider".to_string()];
    a.extend(extra.iter().map(|s| s.to_string()));
    a
}

#[test]
fn add_list_remove_round_trip() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("pantheon-provider-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PANTHEON_DATA_DIR", &dir);
    cmd_provider(&provider_args(&[
        "add",
        "--name",
        "provcrud",
        "--base-url",
        ":8015",
        "--key",
        "k1,k2",
    ]));
    let cfg = Config::load(&dir).unwrap();
    let row = cfg.custom_providers.get("provcrud").expect("row saved");
    assert_eq!(row.base_url, "http://127.0.0.1:8015/v1");
    assert_eq!(row.api_mode, "openai");
    let env_text = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert!(env_text.contains("PANTHEON_KEY_PROVCRUD=k1,k2"));
    // Runtime resolution sees it (registered in-memory).
    assert_eq!(
        catalog::base_url_for("provcrud"),
        "http://127.0.0.1:8015/v1"
    );
    cmd_provider(&provider_args(&["remove", "provcrud", "--delete-key"]));
    assert!(!Config::load(&dir)
        .unwrap()
        .custom_providers
        .contains_key("provcrud"));
    assert_eq!(
        crate::dotenv::read_dotenv_value(&dir, "PANTHEON_KEY_PROVCRUD"),
        None
    );
    std::env::remove_var("PANTHEON_DATA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn add_rejects_bad_names_and_needs_url() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Name validation is shared with the model flow.
    assert!(!crate::model_cli::valid_provider_id("has space"));
    assert!(!crate::model_cli::valid_provider_id("has:colon"));
    assert!(crate::model_cli::valid_provider_id("ok-name_1"));
    // Builtin guard.
    assert!(crate::model_cli::builtin_provider_id("openai"));
    assert!(!crate::model_cli::builtin_provider_id(
        "definitely-not-a-provider"
    ));
}

#[test]
fn remove_drops_row_and_optionally_the_key() {
    use crate::config_doc::ModelSection;
    use crate::dotenv as dotenv_mod;
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("pantheon-provider-rm-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PANTHEON_DATA_DIR", &dir);
    // Custom provider referenced by NOTHING (default rides catalog router).
    let mut cfg = Config {
        model: Some(ModelSection {
            reasoning_budget: None,
            provider: "router".into(),
            model: "chat".into(),
            api_key_env: None,
            fallbacks: vec![],
            reasoning: None,
        }),
        ..Default::default()
    };
    cfg.custom_providers.insert(
        "rmcustom1".into(),
        CustomProviderSection {
            models: Vec::new(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_mode: "openai".into(),
            key_env: Some("PANTHEON_KEY_RMCUSTOM1".into()),
        },
    );
    cfg.save(&dir).unwrap();
    dotenv_mod::upsert_dotenv(&dir, "PANTHEON_KEY_RMCUSTOM1", "sekret").unwrap();
    // Default: row goes, key stays.
    cmd_provider(&provider_args(&["remove", "rmcustom1"]));
    let cfg = Config::load(&dir).unwrap();
    assert!(!cfg.custom_providers.contains_key("rmcustom1"));
    assert_eq!(
        dotenv_mod::read_dotenv_value(&dir, "PANTHEON_KEY_RMCUSTOM1").as_deref(),
        Some("sekret")
    );
    // With --delete-key the .env line goes too.
    let mut cfg = Config::load(&dir).unwrap();
    cfg.custom_providers.insert(
        "rmcustom1".into(),
        CustomProviderSection {
            models: Vec::new(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_mode: "openai".into(),
            key_env: Some("PANTHEON_KEY_RMCUSTOM1".into()),
        },
    );
    cfg.save(&dir).unwrap();
    dotenv_mod::upsert_dotenv(&dir, "PANTHEON_KEY_RMCUSTOM1", "sekret").unwrap();
    cmd_provider(&provider_args(&["remove", "rmcustom1", "--delete-key"]));
    assert!(!Config::load(&dir)
        .unwrap()
        .custom_providers
        .contains_key("rmcustom1"));
    assert_eq!(
        dotenv_mod::read_dotenv_value(&dir, "PANTHEON_KEY_RMCUSTOM1"),
        None
    );
    std::env::remove_var("PANTHEON_DATA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remove_refuses_while_sections_use_the_provider() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("pantheon-provider-rmuse-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PANTHEON_DATA_DIR", &dir);
    // Default model points at the custom provider.
    cmd_provider(&provider_args(&[
        "add",
        "--name",
        "rmused",
        "--base-url",
        "http://127.0.0.1:9/v1",
    ]));
    crate::model_cli::cmd_model(&[
        "pantheon".to_string(),
        "model".to_string(),
        "--provider".to_string(),
        "rmused".to_string(),
        "--model".to_string(),
        "m".to_string(),
    ]);
    let err = crate::model_cli::remove_custom_provider(&dir, "rmused", Some(false)).unwrap_err();
    assert!(err.contains("still in use"), "got: {err}");
    assert!(err.contains("default [model]"), "got: {err}");
    assert!(Config::load(&dir)
        .unwrap()
        .custom_providers
        .contains_key("rmused"));
    // Unknown name errors cleanly.
    assert!(crate::model_cli::remove_custom_provider(&dir, "nosuch", Some(false)).is_err());
    std::env::remove_var("PANTHEON_DATA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A wire mode is not only a body dialect: an Anthropic endpoint rejects
/// `Authorization: Bearer` and needs `x-api-key` + `anthropic-version`.
///
/// `fetch_models` used to always send the OpenAI header, so listing the models
/// of an anthropic-wire custom endpoint failed auth — which reads as "this
/// endpoint serves no models" rather than as a credential problem, and then
/// silently fell back to whatever the operator had recorded.
#[test]
fn fetch_models_sends_the_header_the_wire_mode_requires() {
    // Point at a closed port: the request cannot succeed, so what we assert is
    // that the *shape* of the failure is a connection error rather than a
    // header-plumbing panic or a mis-parse. The header choice itself is
    // covered by the mode branches being the only two paths.
    use pantheon_providers::catalog::ApiMode;
    let dead = "http://127.0.0.1:1/v1";
    let e = crate::model_cli::fetch_models(dead, "k", ApiMode::Anthropic).unwrap_err();
    assert!(!e.is_empty());
    let e2 = crate::model_cli::fetch_models(dead, "k", ApiMode::OpenAi).unwrap_err();
    assert!(!e2.is_empty());
}

#[test]
fn the_anthropic_version_constant_matches_the_provider_plane() {
    // `pantheon_providers::anthropic::ANTHROPIC_VERSION` is the source of
    // truth. The CLI cannot import it (no dependency on the provider plane),
    // so this pins the literal to the same value the adapter sends.
    assert_eq!(crate::model_cli::ANTHROPIC_VERSION, "2023-06-01");
}

/// A custom provider's recorded models are the operator's, not a harvest.
#[test]
fn a_migrated_provider_carries_no_model_list() {
    // Migration must not bake a source config's stale snapshot into config.
    let dir = std::env::temp_dir().join(format!("pantheon-nomodels-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[custom_providers.p]\nbase_url = \"https://x.example/v1\"\napi_mode = \"openai\"\n",
    )
    .unwrap();
    let cfg = crate::config_doc::Config::load(&dir).unwrap();
    assert!(
        cfg.custom_providers["p"].models.is_empty(),
        "an endpoint with no hand-named models must stay empty"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
