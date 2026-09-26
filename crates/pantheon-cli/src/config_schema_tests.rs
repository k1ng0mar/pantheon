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
