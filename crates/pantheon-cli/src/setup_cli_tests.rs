//! Tests for `pantheon_cli::setup_cli::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn flag_only_setup_writes_a_complete_config_without_stdin() {
    let dir = std::env::temp_dir().join(format!("pantheon-setup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = run_setup(
        &dir,
        SetupAnswers {
            profile: Some("dev".into()),
            provider: Some("router".into()),
            model: Some("big-model".into()),
            api_key_env: Some("PANTHEON_API_KEY".into()),
            policy: Some(PolicyPreset::CoderMemory),
            memory_backend: Some("native".into()),
            ..Default::default()
        },
        true,
    );
    // Everything the wizard was asked for landed in the file.
    let loaded = Config::load(&dir).unwrap();
    assert_eq!(loaded, cfg);
    assert_eq!(loaded.model.as_ref().unwrap().provider, "router");
    assert_eq!(loaded.policy, Some(PolicyPreset::CoderMemory),);
    // Backend selection file was synced.
    let sel = load_backend_selection(&dir);
    assert_eq!(sel.name, "native");
    // No raw secrets anywhere.
    let text = std::fs::read_to_string(Config::path(&dir)).unwrap();
    assert!(!text.contains("sk-"));
}
