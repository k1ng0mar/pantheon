//! Tests for `pantheon_secrets::env::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn resolves_namespaced_env_var() {
    let vault = EnvVault::from_map([("PANTHEON_SECRET_API_KEY", "sk-live-1")]);
    assert_eq!(
        vault
            .get("api.key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("sk-live-1".into())
    );
    assert_eq!(
        vault
            .get("api-key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("sk-live-1".into())
    );
}

#[test]
fn resolves_literal_env_reference() {
    let vault = EnvVault::from_map([("FOO", "bar")]);
    assert_eq!(
        vault
            .get("env:FOO")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("bar".into())
    );
}

#[test]
fn missing_env_var_is_none_not_error() {
    let vault = EnvVault::from_map([("PANTHEON_SECRET_A", "1")]);
    assert_eq!(vault.get("nope").unwrap(), None);
}

#[test]
fn system_vault_is_read_only() {
    let vault = EnvVault::system();
    assert!(matches!(
        vault.set("x", SecretValue::new("y")),
        Err(SecretsError::Unsupported(_))
    ));
    assert!(matches!(
        vault.delete("x"),
        Err(SecretsError::Unsupported(_))
    ));
}

#[test]
fn env_key_mapping() {
    assert_eq!(EnvVault::env_key("api.key"), "PANTHEON_SECRET_API_KEY");
    assert_eq!(
        EnvVault::env_key("db-password"),
        "PANTHEON_SECRET_DB_PASSWORD"
    );
    assert_eq!(EnvVault::env_key("env:RAW_NAME"), "RAW_NAME");
}
