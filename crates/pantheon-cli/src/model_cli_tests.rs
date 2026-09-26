//! Tests for `crate::model_cli` (kept here so source files stay test-free).

use crate::config_doc::Config;
use crate::dotenv::test_support::TEST_ENV_LOCK;
use crate::model_cli::*;
use pantheon_core::catalog;

#[test]
fn mask_hides_all_but_hint() {
    assert_eq!(mask_key(""), "(none)");
    assert_eq!(mask_key("short"), "********");
    assert_eq!(mask_key("sk-abcdef123456"), "sk-…3456");
    // Stacked: hint at the first key only.
    assert_eq!(mask_key("sk-abcdef123456,second"), "sk-…3456");
}

#[test]
fn port_shorthand_expands_to_localhost() {
    assert_eq!(normalize_base_url(":8015"), "http://127.0.0.1:8015/v1");
    assert_eq!(normalize_base_url(" :1234/ "), "http://127.0.0.1:1234/v1");
    assert_eq!(normalize_base_url("http://x:9/v1/"), "http://x:9/v1");
    // Not a port: left alone (fetch reports the real problem).
    assert_eq!(normalize_base_url(":abc"), ":abc");
}

#[test]
fn template_vars_persist_and_gate() {
    use crate::dotenv as dotenv_mod;
    let dir = std::env::temp_dir().join(format!("pantheon-model-vars-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let pid = "testvarprov";
    let base = "https://{region}.example.com/{account_id}/v1";
    let region_env = catalog::config_env_name(pid, "region");
    let acct_env = catalog::config_env_name(pid, "account_id");
    std::env::remove_var(&region_env);
    std::env::remove_var(&acct_env);
    // Non-interactive without values: fail closed, naming the flag.
    let err = ensure_template_vars(&dir, pid, base, &[], false).unwrap_err();
    assert!(err.contains("--set region="), "got: {err}");
    // Preset pairs persist to .env and resolve.
    let out = ensure_template_vars(
        &dir,
        pid,
        base,
        &[
            ("region".to_string(), "us-east-1".to_string()),
            (acct_env.clone(), "abc".to_string()),
        ],
        false,
    )
    .unwrap();
    assert_eq!(out, "https://us-east-1.example.com/abc/v1");
    assert_eq!(
        dotenv_mod::read_dotenv_value(&dir, &region_env).as_deref(),
        Some("us-east-1")
    );
    // Stored values satisfy a later call with no preset.
    let out2 = ensure_template_vars(&dir, pid, base, &[], false).unwrap();
    assert_eq!(out2, out);
    std::env::remove_var(&region_env);
    std::env::remove_var(&acct_env);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_interactive_save_writes_config_and_env() {
    let _lock = TEST_ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("pantheon-model-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PANTHEON_DATA_DIR", &dir);
    let args = vec![
        "pantheon".to_string(),
        "model".to_string(),
        "--provider".to_string(),
        "router".to_string(),
        "--model".to_string(),
        "code".to_string(),
        "--key".to_string(),
        "k1,k2".to_string(),
    ];
    cmd_model(&args);
    let cfg = Config::load(&dir).unwrap();
    let m = cfg.model.unwrap();
    assert_eq!((m.provider.as_str(), m.model.as_str()), ("router", "code"));
    assert_eq!(m.api_key_env.as_deref(), Some("PANTHEON_KEY_ROUTER"));
    let env_text = std::fs::read_to_string(dir.join(".env")).unwrap();
    assert!(env_text.contains("PANTHEON_KEY_ROUTER=k1,k2"));
    // No custom row for a cataloged provider.
    assert!(cfg.custom_providers.is_empty());
    std::env::remove_var("PANTHEON_DATA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn non_interactive_custom_provider_persists_endpoint() {
    let _lock = TEST_ENV_LOCK.lock().unwrap();
    let dir = std::env::temp_dir().join(format!("pantheon-model-cust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PANTHEON_DATA_DIR", &dir);
    let args = vec![
        "pantheon".to_string(),
        "model".to_string(),
        "--provider".to_string(),
        "testcustom7".to_string(),
        "--model".to_string(),
        "m1".to_string(),
        "--base-url".to_string(),
        "http://127.0.0.1:8015/v1".to_string(),
        "--api-mode".to_string(),
        "openai".to_string(),
        "--auxiliary".to_string(),
        "decision".to_string(),
    ];
    cmd_model(&args);
    let cfg = Config::load(&dir).unwrap();
    let c = cfg.custom_providers.get("testcustom7").expect("custom row");
    assert_eq!(c.base_url, "http://127.0.0.1:8015/v1");
    let d = cfg.judge.unwrap();
    assert_eq!(d.provider, "testcustom7");
    assert_eq!(d.model, "m1");
    // Registered in-memory too: runtime resolution sees it.
    assert_eq!(
        catalog::base_url_for("testcustom7"),
        "http://127.0.0.1:8015/v1"
    );
    std::env::remove_var("PANTHEON_DATA_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}
