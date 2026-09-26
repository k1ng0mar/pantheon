//! Tests for `pantheon_secrets::vault::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn get_set_delete_roundtrip() {
    let vault = MemoryVault::new();
    assert_eq!(vault.get("api.key").unwrap(), None);
    vault.set("api.key", SecretValue::new("v")).unwrap();
    assert_eq!(
        vault
            .get("api.key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("v".into())
    );
    vault.delete("api.key").unwrap();
    assert_eq!(vault.get("api.key").unwrap(), None);
    assert_eq!(vault.names().unwrap(), Vec::<String>::new());
}

#[test]
fn empty_name_rejected() {
    let vault = MemoryVault::new();
    assert!(matches!(vault.get(""), Err(SecretsError::Invalid(_))));
    assert!(matches!(
        vault.set("", SecretValue::new("x")),
        Err(SecretsError::Invalid(_))
    ));
}
