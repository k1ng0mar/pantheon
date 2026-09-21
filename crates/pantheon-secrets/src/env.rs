//! Environment compatibility vault.
//!
//! Lets 12-factor style deployments feed secrets through the process
//! environment without touching the runtime secret store:
//!
//! - Name `db.password` resolves to env var `PANTHEON_SECRET_DB_PASSWORD`
//!   (uppercase, `.`/`-` become `_`).
//! - Name `env:FOO` resolves to the literal env var `FOO`.
//!
//! The system vault is read-only; store in a durable vault instead.

use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

const PREFIX: &str = "PANTHEON_SECRET_";

/// Reads secrets from the process environment.
#[derive(Debug, Default)]
pub struct EnvVault {
    vars: Mutex<HashMap<String, SecretValue>>,
    /// `true` when this vault mirrors the real process environment
    /// (`system()`), which forbids `set`/`delete`.
    system: bool,
}

impl EnvVault {
    /// Snapshot the real process environment, keeping only
    /// `PANTHEON_SECRET_*` variables.
    pub fn system() -> Self {
        let vars = std::env::vars()
            .filter(|(k, _)| k.starts_with(PREFIX))
            .map(|(k, v)| (k, SecretValue::new(v)))
            .collect();
        Self {
            vars: Mutex::new(vars),
            system: true,
        }
    }

    /// Build a vault from an explicit map (tests, embedded runtimes).
    pub fn from_map<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            vars: Mutex::new(
                vars.into_iter()
                    .map(|(k, v)| (k.into(), SecretValue::new(v.into())))
                    .collect(),
            ),
            system: false,
        }
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
        self.vars.lock().expect("EnvVault mutex poisoned")
    }

    /// Map a requested name to the env var key it reads.
    pub fn env_key(name: &str) -> String {
        if let Some(literal) = name.strip_prefix("env:") {
            return literal.to_string();
        }
        let normalized = name
            .replace('.', "_")
            .replace('-', "_")
            .to_ascii_uppercase();
        format!("{PREFIX}{normalized}")
    }
}

impl SecretVault for EnvVault {
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        let key = Self::env_key(name);
        Ok(self.state().get(&key).cloned())
    }

    fn set(&self, _name: &str, _value: SecretValue) -> Result<(), SecretsError> {
        if self.system {
            return Err(SecretsError::Unsupported(
                "process environment is read-only; store in a durable vault".into(),
            ));
        }
        Err(SecretsError::Unsupported(
            "use MemoryVault or EncryptedFileVault to store; EnvVault is for reads only".into(),
        ))
    }

    fn delete(&self, _name: &str) -> Result<(), SecretsError> {
        if self.system {
            return Err(SecretsError::Unsupported(
                "process environment is read-only".into(),
            ));
        }
        Err(SecretsError::Unsupported(
            "EnvVault is for reads only".into(),
        ))
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        let mut names: Vec<String> = self
            .state()
            .keys()
            .filter(|k| k.starts_with(PREFIX))
            .map(|k| k.trim_start_matches(PREFIX).to_ascii_lowercase())
            .collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
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
}
