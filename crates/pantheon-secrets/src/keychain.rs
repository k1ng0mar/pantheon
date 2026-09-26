//! OS keychain vault (spec §13): the preferred durable backend.
//!
//! macOS Keychain Services, Windows Credential Manager, and — on Linux —
//! the freedesktop Secret Service (gnome-keyring, KWallet) all sit behind
//! the `keyring` crate's platform-default store. This is the "OS
//! keychain" the spec ranks above the encrypted local file; insert it
//! into a broker ahead of [`crate::filevault::EncryptedFileVault`] and
//! the environment.
//!
//! Contract notes:
//! - `get`/`set` are one local round-trip to the store (a D-Bus call on
//!   Linux, ~ms). A *locked* keyring surfaces as [`SecretsError::Backend`],
//!   bounded by the platform's own method timeout — never a modal prompt
//!   the runtime would block on forever (the `SecretVault::get` contract
//!   forbids that).
//! - `names` is unsupported: the platform APIs store and look up
//!   credentials but offer no enumeration through this interface. Keep
//!   the authoritative name list wherever the references live.
//! - Secrets cross into the store as one plain copy at the platform
//!   boundary (unavoidable at the FFI/D-Bus edge); everything on this
//!   side stays a zeroizing [`SecretValue`].
//!
//! Availability contract (the broker relies on it):
//! - Host with **no store at all**: `get` reads as absence (`Ok(None)`),
//!   `delete` as the no-op it would be, so a broker chain falls through
//!   to the next vault; `set` errors — there is nowhere to write.
//! - Host **with a store that cannot answer** (locked, D-Bus broken):
//!   every op surfaces [`SecretsError::Backend`] — the secret may exist,
//!   the store just couldn't reply. Callers such as
//!   [`crate::SecretsBroker`] decide whether to fall through or fail.
//!
//! Headless machines without a credential store keep working: the broker
//! falls through to the next vault, and [`KeychainVault::platform_available`]
//! lets callers check first.

use crate::error::{validate_name, SecretsError};
use crate::value::SecretValue;
use crate::vault::SecretVault;
use keyring_core::{Entry as KrEntry, Error as KrError};

/// Durable vault backed by the OS credential store.
#[derive(Debug)]
pub struct KeychainVault {
    service: String,
}

impl KeychainVault {
    /// Vault under the default `pantheon` service name.
    pub fn new() -> Self {
        Self::with_service("pantheon")
    }

    /// Vault under a custom service name (tests isolate under their own
    /// so cleanup never touches real credentials).
    pub fn with_service(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    /// Is the platform credential store usable in this process?
    ///
    /// Initializes the store on first call (once per process, cached by
    /// the keyring crate). `Err` = no store here: headless box, container
    /// without a Secret Service, unsupported platform.
    pub fn platform_available() -> Result<(), SecretsError> {
        ensure_store()
    }

    fn entry(&self, name: &str) -> Result<KrEntry, SecretsError> {
        validate_name(name)?;
        ensure_store()?;
        KrEntry::new(&self.service, name).map_err(|e| backend_err(&e))
    }
}

/// Make sure some credential store is installed, then report the status.
///
/// The first use in a process asks the `keyring` crate to build the
/// platform-default store (a Secret Service connection on Linux, the
/// Keychain on macOS, CredMan on Windows) and installs it. Once a store
/// is already installed — including a test-injected one — this is a
/// no-op, which is what makes the sample-store lib tests deterministic
/// in the same process.
fn ensure_store() -> Result<(), SecretsError> {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }
    match keyring::Entry::store_status() {
        Ok(()) => Ok(()),
        Err(e) => Err(backend_err(e)),
    }
}

impl Default for KeychainVault {
    fn default() -> Self {
        Self::new()
    }
}

fn backend_err(e: &KrError) -> SecretsError {
    SecretsError::Backend(format!("os keychain: {e}"))
}

impl SecretVault for KeychainVault {
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        validate_name(name)?;
        // No store on this host: this backend holds nothing under any
        // name — absence, so the broker chain falls through.
        // (A store that exists but cannot answer stays a loud Backend
        // error below: the secret may well be in there.)
        if ensure_store().is_err() {
            return Ok(None);
        }
        match self.entry(name)?.get_password() {
            Ok(v) => Ok(Some(SecretValue::new(v))),
            Err(KrError::NoEntry) => Ok(None),
            Err(e) => Err(backend_err(&e)),
        }
    }

    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        // One plain copy crosses into the store; our side stays zeroized.
        // (No store → `entry` reports Backend: nowhere to write.)
        self.entry(name)?
            .set_password(value.expose())
            .map_err(|e| backend_err(&e))
    }

    fn delete(&self, name: &str) -> Result<(), SecretsError> {
        // Validate before touching the platform, exactly like get/set. A
        // store-less host returned Ok(()) for every name, empty ones
        // included, so `delete("")` only failed where a keyring happened to
        // exist — the test passed locally and failed on a CI runner with
        // one. A rejected name is rejected everywhere.
        validate_name(name)?;
        // Nothing can be stored where there is no store: deleting every
        // conceivable name is a chain of no-ops, contract and all.
        if ensure_store().is_err() {
            return Ok(());
        }
        match self.entry(name)?.delete_credential() {
            Ok(()) => Ok(()),
            // Trait contract: removing an unknown name is a no-op.
            Err(KrError::NoEntry) => Ok(()),
            Err(e) => Err(backend_err(&e)),
        }
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        Err(SecretsError::Unsupported(
            "the OS keychain API cannot enumerate entries; track names \
             alongside their references"
                .into(),
        ))
    }
}

#[cfg(test)]
#[path = "keychain_tests.rs"]
mod tests;
