//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_secrets::{EncryptedFileVault, EnvVault, SecretValue, SecretsBroker};
use tempfile::tempdir;

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
