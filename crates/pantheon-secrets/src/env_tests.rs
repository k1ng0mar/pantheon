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
    let vault = EnvVault::from_map([("FOO", "bar")]).with_env_allowlist(vec!["FOO".into()]);
    assert_eq!(
        vault
            .get("env:FOO")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("bar".into())
    );
}

#[test]
fn env_literal_fails_closed_without_allowlist() {
    // Same map as above, no allowlist: the read must NOT leak the var.
    let vault = EnvVault::from_map([("FOO", "bar")]);
    assert_eq!(vault.get("env:FOO").unwrap(), None);
}

#[test]
fn env_literal_allowlist_prefix_match() {
    let vault = EnvVault::from_map([
        ("PANTHEON_API_KEY", "k1"),
        ("AWS_SECRET_ACCESS_KEY", "leak-me"),
    ])
    .with_env_allowlist(vec!["PANTHEON_*".into()]);
    assert_eq!(
        vault
            .get("env:PANTHEON_API_KEY")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("k1".into())
    );
    // Non-allowlisted var fails closed even though it is in the map.
    assert_eq!(vault.get("env:AWS_SECRET_ACCESS_KEY").unwrap(), None);
}

#[test]
fn allowlist_matcher_semantics() {
    let list = vec!["FOO".into(), "PANTHEON_*".into(), "  ".into()];
    assert!(env_var_allowed(&list, "FOO"));
    assert!(!env_var_allowed(&list, "FOOBAR"));
    assert!(env_var_allowed(&list, "PANTHEON_SECRET_X"));
    assert!(!env_var_allowed(&list, "AWS_SECRET"));
    // "*" alone is the explicit allow-all escape hatch.
    assert!(env_var_allowed(&["*".into()], "ANYTHING"));
    // Empty allowlist denies everything.
    assert!(!env_var_allowed(&[], "FOO"));
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
