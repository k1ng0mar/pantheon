//! The secrets broker: resolution + injection at the execution boundary.
//!
//! Guarantees:
//! - Resolution order is deterministic: durable vaults in insertion order,
//!   then the environment. First hit wins.
//! - Secrets travel only as [`SecretValue`]; `describe`/`Debug` never leak
//!   contents, so events, `/explain`, and logs stay clean.
//! - `inject` returns `None` for absent secrets rather than erroring, so
//!   callers degrade gracefully when a secret isn't configured.

use crate::env::EnvVault;
use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;

/// Resolves and injects secrets for a run.
#[derive(Debug, Default)]
pub struct SecretsBroker {
    durable: Vec<Box<dyn SecretVault>>,
    env: EnvVault,
}

impl SecretsBroker {
    /// Broker with no durable vaults; falls back to the environment only.
    pub fn new() -> Self {
        Self::default()
    }

    /// Broker over the real process environment (`PANTHEON_SECRET_*`).
    pub fn from_system_env() -> Self {
        Self {
            durable: Vec::new(),
            env: EnvVault::system(),
        }
    }

    /// Add a durable vault (OS keychain, encrypted local, memory). Durable
    /// vaults are consulted in insertion order before the environment.
    pub fn with_vault(mut self, vault: Box<dyn SecretVault>) -> Self {
        self.durable.push(vault);
        self
    }

    /// Replace the environment source (tests use this).
    pub fn with_env(mut self, env: EnvVault) -> Self {
        self.env = env;
        self
    }

    /// Resolve a secret: durable vaults first, then the environment.
    pub fn resolve(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        for vault in &self.durable {
            if let Some(value) = vault.get(name)? {
                return Ok(Some(value));
            }
        }
        self.env.get(name)
    }

    /// Execution-boundary injection. Same as [`resolve`], named for the call
    /// site where a run's tool/process/session credentials are handed over.
    ///
    /// The returned value must be passed to the execution boundary directly;
    /// it must never be logged or embedded into model context.
    pub fn inject(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        self.resolve(name)
    }

    /// Store into the first durable vault (error when none configured).
    pub fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        let vault = self
            .durable
            .first()
            .ok_or_else(|| SecretsError::Unsupported("no durable vault configured".into()))?;
        vault.set(name, value)
    }

    /// Redacted one-line description for logs/events: presence, never value.
    pub fn describe(&self, name: &str) -> String {
        match self.resolve(name) {
            Ok(Some(v)) => format!("secret:{name} present ({} bytes)", v.len()),
            Ok(None) => format!("secret:{name} absent"),
            Err(e) => format!("secret:{name} error={e}"),
        }
    }
}

#[cfg(test)]
mod tests {
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
                EncryptedFileVault::open(dir.path().join("v.json"), dir.path().join("v.key"))
                    .unwrap(),
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
}
