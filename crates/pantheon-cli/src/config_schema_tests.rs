//! Tests for `pantheon_cli::config_schema::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn secret_ref_rejects_non_env_sources() {
    assert!(SecretRef {
        source: "raw".into(),
        name: "sk-123".into()
    }
    .validate()
    .is_err());
    assert!(SecretRef::from_env("PANTHEON_KEY").validate().is_ok());
    assert!(SecretRef::from_env("").validate().is_err());
    assert!(SecretRef::from_env("bad-name").validate().is_err());
}

#[test]
fn policy_preset_parses_the_config_toml_spellings() {
    // Literal strings, not `as_str`, so the test fails if a spelling in
    // config.toml drifts from what the parser accepts.
    assert_eq!(PolicyPreset::from_str("reader"), Some(PolicyPreset::Reader));
    assert_eq!(PolicyPreset::from_str("coder"), Some(PolicyPreset::Coder));
    assert_eq!(
        PolicyPreset::from_str("coder_memory"),
        Some(PolicyPreset::CoderMemory)
    );
    assert_eq!(PolicyPreset::from_str("nope"), None);
}

#[test]
fn every_preset_maps_to_the_policy_it_names() {
    // `reader` used to resolve to Policy::coder(), so a user who set
    // policy = "reader" got shell and file writes.
    use pantheon_api::capability::{Capability, Decision};

    let reader = PolicyPreset::Reader.to_policy();
    for denied in [
        Capability::ShellExecute,
        Capability::FilesystemWrite,
        Capability::GitPush,
        Capability::MemoryWrite,
    ] {
        assert_eq!(
            reader.check(&denied),
            Decision::Deny,
            "reader must not grant {denied:?}"
        );
    }
    assert_eq!(reader.check(&Capability::FilesystemRead), Decision::Allow);
    assert_eq!(reader.check(&Capability::MemoryRead), Decision::Allow);

    assert_eq!(
        PolicyPreset::Coder
            .to_policy()
            .check(&Capability::ShellExecute),
        Decision::Allow
    );
    assert_eq!(
        PolicyPreset::Coder
            .to_policy()
            .check(&Capability::MemoryWrite),
        Decision::Deny
    );
    assert_eq!(
        PolicyPreset::CoderMemory
            .to_policy()
            .check(&Capability::MemoryWrite),
        Decision::Allow
    );
}

#[test]
fn config_policy_wins_over_the_environment() {
    let cfg = crate::config_doc::Config {
        policy: Some(PolicyPreset::Reader),
        ..Default::default()
    };
    let policy = policy_for_config(&Some(cfg));
    assert_eq!(
        policy.check(&pantheon_api::capability::Capability::ShellExecute),
        pantheon_api::capability::Decision::Deny
    );
}
