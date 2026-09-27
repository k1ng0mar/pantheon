//! Tests for `pantheon_secrets::broker::tests` — sibling file so sources stay test-free.
use super::*;
use crate::{EncryptedFileVault, MemoryVault};
use tempfile::tempdir;

#[test]
fn resolves_from_durable_then_env() {
    let broker = SecretsBroker::new()
        .with_vault(Box::new(MemoryVault::new()))
        .with_env(EnvVault::from_map([
            ("PANTHEON_SECRET_API_KEY", "from-env"),
            ("PANTHEON_SECRET_EXTRA_WEBHOOK", "from-env-extra"),
        ]));
    broker
        .set("api.key", SecretValue::new("from-vault"))
        .unwrap();
    assert_eq!(
        broker
            .inject("api.key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("from-vault".into())
    );
    // Not in the vault => environment fallback kicks in.
    assert_eq!(
        broker
            .inject("extra.webhook")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("from-env-extra".into())
    );
}

#[test]
fn env_compat_path_with_encrypted_vault() {
    let dir = tempdir().unwrap();
    let broker = SecretsBroker::new()
        .with_vault(Box::new(
            EncryptedFileVault::open(dir.path().join("v.json"), dir.path().join("v.key")).unwrap(),
        ))
        .with_env(EnvVault::from_map([(
            "PANTHEON_SECRET_DB_PASSWORD",
            "env-pw",
        )]));

    assert_eq!(
        broker
            .inject("db.password")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("env-pw".into())
    );
    broker
        .set("db.password", SecretValue::new("vault-pw"))
        .unwrap();
    // Durable vault now shadows the environment.
    assert_eq!(
        broker
            .inject("db.password")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("vault-pw".into())
    );
    // Stored value must be recoverable by a fresh broker too.
    let fresh = SecretsBroker::new().with_vault(Box::new(
        EncryptedFileVault::open(dir.path().join("v.json"), dir.path().join("v.key")).unwrap(),
    ));
    assert_eq!(
        fresh
            .inject("db.password")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("vault-pw".into())
    );
}

#[test]
fn absent_secret_is_none_not_error() {
    let broker = SecretsBroker::new().with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()));
    assert_eq!(broker.inject("missing").unwrap(), None);
}

#[test]
fn describe_never_leaks_value() {
    let broker = SecretsBroker::new()
        .with_vault(Box::new(MemoryVault::new()))
        .with_env(EnvVault::from_map([(
            "PANTHEON_SECRET_TOKEN",
            "dont-leak-me",
        )]));

    let present = broker.describe("token");
    assert!(present.contains("present"));
    assert!(!present.contains("dont-leak-me"));
    // Absent + error paths are also value-free.
    assert!(broker.describe("nope").contains("absent"));
}

#[test]
fn set_without_durable_is_unsupported() {
    let broker = SecretsBroker::new();
    assert!(matches!(
        broker.set("x", SecretValue::new("y")),
        Err(SecretsError::Unsupported(_))
    ));
}

#[test]
fn front_vault_beats_config_seeded_vaults() {
    // Config/env seeding uses with_vault (appended); an explicit
    // override like `--key` uses with_vault_front so it wins.
    let config_seed = MemoryVault::new();
    config_seed
        .set("PANTHEON_API_KEY", SecretValue::new("from-config"))
        .unwrap();
    let flag = MemoryVault::new();
    flag.set("PANTHEON_API_KEY", SecretValue::new("from-flag"))
        .unwrap();
    let broker = SecretsBroker::new()
        .with_vault(Box::new(config_seed))
        .with_vault_front(Box::new(flag));
    assert_eq!(
        broker
            .resolve("PANTHEON_API_KEY")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("from-flag".into()),
        "an explicit override must win over the config-seeded vault"
    );
}

/// A vault whose platform store exists but cannot answer — the exact
/// failure mode of a locked or D-Bus-broken OS keyring.
#[derive(Debug)]
struct BrokenVault;

impl SecretVault for BrokenVault {
    fn get(&self, _: &str) -> Result<Option<SecretValue>, SecretsError> {
        Err(SecretsError::Backend(
            "os keychain: store cannot answer".into(),
        ))
    }
    fn set(&self, _: &str, _: SecretValue) -> Result<(), SecretsError> {
        Err(SecretsError::Backend(
            "os keychain: store cannot answer".into(),
        ))
    }
    fn delete(&self, _: &str) -> Result<(), SecretsError> {
        Err(SecretsError::Backend(
            "os keychain: store cannot answer".into(),
        ))
    }
    fn names(&self) -> Result<Vec<String>, SecretsError> {
        Err(SecretsError::Backend(
            "os keychain: store cannot answer".into(),
        ))
    }
}

#[test]
fn broken_store_falls_through_instead_of_failing_the_chain() {
    let healthy = MemoryVault::new();
    healthy
        .set("api.key", SecretValue::new("from-the-fallback"))
        .unwrap();
    let broker = SecretsBroker::new()
        .with_vault(Box::new(BrokenVault))
        .with_vault(Box::new(healthy))
        .with_env(EnvVault::from_map([("PANTHEON_SECRET_TOKEN", "env-value")]));

    // Read: the broken higher-ranked vault must not abort resolution.
    assert_eq!(
        broker
            .resolve("api.key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("from-the-fallback".into())
    );
    // Present only in the environment: still resolves through it.
    assert_eq!(
        broker
            .resolve("token")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("env-value".into())
    );
    // Absent everywhere: graceful None, not the store error.
    assert_eq!(broker.resolve("nothing.here").unwrap(), None);
    // Write: falls through to the vault that CAN store.
    broker.set("api.key", SecretValue::new("written")).unwrap();
    assert_eq!(
        broker
            .resolve("api.key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("written".into())
    );
}

#[test]
fn set_surfaces_the_store_error_when_no_vault_can_take_it() {
    let broker = SecretsBroker::new().with_vault(Box::new(BrokenVault));
    assert!(matches!(
        broker.set("k", SecretValue::new("v")),
        Err(SecretsError::Backend(_))
    ));
}

#[test]
fn clone_keeps_the_durable_backends() {
    // Regression: clone() used to drop every durable vault and fall back
    // to memory-only, silently losing the platform stores.
    let broker = SecretsBroker::new()
        .with_vault(Box::new(MemoryVault::new()))
        .with_env(EnvVault::from_map([("PANTHEON_SECRET_E", "e")]));
    broker
        .set("api.key", SecretValue::new("durable-value"))
        .unwrap();

    let cloned = broker.clone();
    // The clone resolves through the SAME backend, not a fresh one.
    assert_eq!(
        cloned
            .resolve("api.key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("durable-value".into())
    );
    // Writes through the clone land in the shared backend...
    cloned
        .set("api.key", SecretValue::new("via-clone"))
        .unwrap();
    assert_eq!(
        broker
            .resolve("api.key")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("via-clone".into()),
        "clone must share the backends, not snapshot them"
    );
    // ...and policy travels with the clone too.
    let broker = SecretsBroker::new()
        .with_env_allowlist(vec!["A".into()])
        .with_plugin_env_allowlist(vec!["B".into()]);
    let cloned = broker.clone();
    assert_eq!(cloned.plugin_env_allowlist(), &["B".to_string()]);
    assert_eq!(cloned.env_allowlist(), &["A".to_string()]);
}
