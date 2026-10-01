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

/// Read-only view over another vault.
///
/// An env-var mirror (see
/// [`crate::broker::SecretsBroker::from_system_env_with_api_key`]) must win
/// reads — an exported rotation beats a stale stored value — but must never
/// absorb writes: a write landing in a process-memory mirror dies with the
/// process instead of reaching durable storage. `set`/`delete` fail with
/// [`SecretsError::Backend`] so the broker treats this vault like a
/// degraded platform store and falls through to the next durable vault,
/// exactly as it does for a locked keychain.
#[derive(Debug)]
pub struct ReadOnlyVault<V: SecretVault> {
    inner: V,
}

impl<V: SecretVault> ReadOnlyVault<V> {
    /// Wrap `inner`; reads and listings pass through, writes fail closed.
    pub fn new(inner: V) -> Self {
        Self { inner }
    }

    /// Unwrap back to the inner vault.
    pub fn into_inner(self) -> V {
        self.inner
    }
}

impl<V: SecretVault> SecretVault for ReadOnlyVault<V> {
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        self.inner.get(name)
    }

    fn set(&self, _name: &str, _value: SecretValue) -> Result<(), SecretsError> {
        Err(SecretsError::Backend(
            "read-only vault: writes fall through to the next durable vault".into(),
        ))
    }

    fn delete(&self, _name: &str) -> Result<(), SecretsError> {
        Err(SecretsError::Backend(
            "read-only vault: deletes fall through to the next durable vault".into(),
        ))
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        self.inner.names()
    }
}
