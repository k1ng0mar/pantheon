//! The secrets broker: resolution + injection at the execution boundary.
//!
//! Guarantees:
//! - Resolution order is deterministic: durable vaults in insertion order,
//!   then the environment. First hit wins.
//! - A durable vault whose platform store cannot answer
//!   ([`SecretsError::Backend`]) is skipped, not fatal: a broken keyring
//!   must never fail a resolve the next vault could satisfy. Deeper
//!   failures (tampered encrypted vault, I/O) still propagate loudly.
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

    /// Broker over the real process environment (`PANTHEON_SECRET_*`),
    /// plus the OS keychain first when this host has a usable platform
    /// credential store (spec §13 ranks it above every other durable
    /// backend).
    pub fn from_system_env() -> Self {
        let mut durable: Vec<Box<dyn SecretVault>> = Vec::new();
        if crate::keychain::KeychainVault::platform_available().is_ok() {
            durable.push(Box::new(crate::keychain::KeychainVault::new()));
        }
        Self {
            durable,
            env: EnvVault::system(),
        }
    }

    /// Like `from_system_env`, but also picks up the legacy `PANTHEON_API_KEY`
    /// env var (and any config-named env var passed via `key_env`) by injecting
    /// it into a memory vault. The mirror is ranked *with the environment*
    /// (in front of the keychain), so exporting a rotated key always beats a
    /// stale stored one; an explicit `--key` still beats both via
    /// [`Self::with_vault_front`].
    pub fn from_system_env_with_api_key(key_env: Option<&str>) -> Self {
        let mem = crate::vault::MemoryVault::new();
        let found = key_env
            .and_then(|env| std::env::var(env).ok())
            .or_else(|| std::env::var("PANTHEON_API_KEY").ok());
        if let Some(k) = found {
            let _ = mem.set("PANTHEON_API_KEY", SecretValue::new(k));
        }
        Self::from_system_env().with_vault_front(Box::new(mem))
    }

    /// Add a durable vault (OS keychain, encrypted local, memory). Durable
    /// vaults are consulted in insertion order before the environment.
    pub fn with_vault(mut self, vault: Box<dyn SecretVault>) -> Self {
        self.durable.push(vault);
        self
    }

    /// Add a vault consulted before every existing one. Explicit overrides
    /// (a `--key` flag, a per-call credential) must beat config- and
    /// env-seeded vaults, and durable vaults resolve in insertion order.
    pub fn with_vault_front(mut self, vault: Box<dyn SecretVault>) -> Self {
        self.durable.insert(0, vault);
        self
    }

    /// Replace the environment source (tests use this).
    pub fn with_env(mut self, env: EnvVault) -> Self {
        self.env = env;
        self
    }

    /// Resolve a secret: durable vaults first, then the environment.
    ///
    /// A vault whose platform store cannot answer ([`SecretsError::Backend`])
    /// is skipped — a broken keyring degrades to the next vault instead of
    /// failing the run. Any other error (tampered encrypted vault, bad
    /// request) propagates immediately.
    pub fn resolve(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        for vault in &self.durable {
            match vault.get(name) {
                Ok(Some(value)) => return Ok(Some(value)),
                Ok(None) => {}
                Err(SecretsError::Backend(_)) => continue,
                Err(e) => return Err(e),
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

    /// Store into the first durable vault that can accept it (error when
    /// none is configured).
    ///
    /// A higher-ranked vault whose platform store cannot answer falls
    /// through to the next one — a broken keyring must not block a write
    /// the encrypted file could take — while every other failure
    /// propagates.
    pub fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        if self.durable.is_empty() {
            return Err(SecretsError::Unsupported(
                "no durable vault configured".into(),
            ));
        }
        let mut first_backend: Option<SecretsError> = None;
        for vault in &self.durable {
            match vault.set(name, value.clone()) {
                Ok(()) => return Ok(()),
                Err(SecretsError::Backend(e)) => {
                    if first_backend.is_none() {
                        first_backend = Some(SecretsError::Backend(e));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        // Every vault failed; the highest-ranked failure is the real one.
        Err(first_backend
            .expect("durable is non-empty, so at least one attempt ran"))
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
            .with_env(EnvVault::from_map([(
                "PANTHEON_SECRET_TOKEN",
                "env-value",
            )]));

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
        broker
            .set("api.key", SecretValue::new("written"))
            .unwrap();
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
}
