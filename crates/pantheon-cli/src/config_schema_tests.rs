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
fn policy_preset_round_trips() {
    for p in [
        PolicyPreset::Reader,
        PolicyPreset::Coder,
        PolicyPreset::CoderMemory,
    ] {
        assert_eq!(PolicyPreset::from_str(p.as_str()), Some(p));
    }
    assert_eq!(PolicyPreset::from_str("nope"), None);
}
