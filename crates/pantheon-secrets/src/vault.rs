//! Vault abstraction.
//!
//! One `SecretVault` = one storage backend. Backends in the skeleton:
//! - [`crate::env::EnvVault`] — process environment compat (always present).
//! - [`crate::filevault::EncryptedFileVault`] — AES-256-GCM sealed JSON file.
//! - [`MemoryVault`] — tests / ephemeral runtime.
//!
//! - [`crate::keychain::KeychainVault`] — macOS Keychain, Windows CredMan,
//!   Linux Secret Service (the preferred durable backend).
//!
//! All of these implement [`SecretVault`].

use crate::error::SecretsError;
use crate::value::SecretValue;
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

/// Storage backend for secrets.
///
/// Implementations guard their state with a [`Mutex`] and take `&self`, so
/// a broker may share vaults across the runtime. `get` must never block on
/// network or prompt — it is called on hot paths.
pub trait SecretVault: Send + Sync + std::fmt::Debug {
    /// Fetch a secret, or `None` when this backend has no such name.
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError>;
    /// Store or overwrite a secret.
    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError>;
    /// Delete a secret. Removing an unknown name is a no-op.
    fn delete(&self, name: &str) -> Result<(), SecretsError>;
    /// All names this backend holds.
    fn names(&self) -> Result<Vec<String>, SecretsError>;
}

/// In-memory vault — for tests and ephemeral runtime use. Contents are lost
/// on drop and are never persisted.
#[derive(Debug, Default)]
pub struct MemoryVault {
    secrets: Mutex<HashMap<String, SecretValue>>,
}

impl MemoryVault {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
        self.secrets.lock().expect("MemoryVault mutex poisoned")
    }
}

impl SecretVault for MemoryVault {
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        Ok(self.state().get(name).cloned())
    }

    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        self.state().insert(name.to_string(), value);
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        self.state().remove(name);
        Ok(())
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        let mut names: Vec<String> = self.state().keys().cloned().collect();
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
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
}
