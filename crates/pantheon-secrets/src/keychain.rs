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
use keyring_core::{Error as KrError, Entry as KrEntry};

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
mod tests {
    use super::*;

    /// Install the deterministic in-memory sample store and probe it.
    ///
    /// The lib-test process must never depend on the host's Secret
    /// Service (headless boxes have no usable default collection), so
    /// roundtrips run against `keyring-core`'s sample store — the real
    /// store trait with zero environment. Installed exactly once for
    /// the whole test process: a second install would swap the backing
    /// store out from under a concurrent test's credentials.
    /// `tests/keychain_live.rs` covers the actual platform store in its
    /// own process.
    ///
    /// Failure to store at all is an environment skip (reason printed,
    /// never silent); once the probe lands, every later op in the test
    /// must genuinely work — no skipping past errors.
    fn sample_store_or_skip(test: &str, vault: &KeychainVault, name: &str) -> bool {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let store =
                keyring_core::sample::Store::new().expect("sample store must build");
            keyring_core::set_default_store(store);
        });
        let _ = vault.delete(name);
        match vault.set(name, SecretValue::new("pantheon-probe")) {
            Ok(()) => {
                let _ = vault.delete(name);
                true
            }
            Err(e) => {
                eprintln!("SKIP {test} — credential store not usable here: {e}");
                false
            }
        }
    }

    #[test]
    fn roundtrip_set_get_overwrite_delete() {
        let vault = KeychainVault::with_service("pantheon-test-keychain");
        let name = format!("probe-{}", std::process::id());
        if !sample_store_or_skip("keychain roundtrip", &vault, &name) {
            return;
        }

        vault
            .set(&name, SecretValue::new("sk-roundtrip"))
            .unwrap_or_else(|e| panic!("set must work after a successful probe: {e}"));
        let got = vault
            .get(&name)
            .expect("get")
            .expect("just written — must be found");
        assert_eq!(got.expose(), "sk-roundtrip");

        // Overwrite is the documented behavior of the platform store.
        vault
            .set(&name, SecretValue::new("v2"))
            .expect("overwrite");
        assert_eq!(
            vault.get(&name).expect("get").expect("present").expose(),
            "v2"
        );

        vault.delete(&name).expect("delete");
        assert!(
            vault.get(&name).expect("get after delete").is_none(),
            "deleted credential must be gone"
        );
    }

    #[test]
    fn broker_resolves_from_the_keychain_when_inserted() {
        let service = "pantheon-test-keychain-broker";
        let cleanup = KeychainVault::with_service(service);
        let name = format!("broker-probe-{}", std::process::id());
        if !sample_store_or_skip("broker keychain resolve", &cleanup, &name) {
            return;
        }

        let broker = crate::SecretsBroker::new()
            .with_vault(Box::new(KeychainVault::with_service(service)));
        broker
            .set(&name, SecretValue::new("from-keychain"))
            .unwrap_or_else(|e| panic!("set through broker must work after probe: {e}"));
        let v = broker
            .resolve(&name)
            .expect("resolve")
            .expect("just stored through the broker");
        assert_eq!(v.expose(), "from-keychain");
        let _ = cleanup.delete(&name);
    }

    #[test]
    fn names_is_unsupported_and_says_why() {
        // No store needed: enumeration is refused structurally.
        let err = KeychainVault::new().names().unwrap_err();
        assert!(matches!(err, SecretsError::Unsupported(_)));
        assert!(err.to_string().contains("enumerate"));
    }

    #[test]
    fn bad_names_are_rejected_before_touching_the_store() {
        // Deterministic everywhere — validation happens before any
        // platform interaction, so even a store-less machine asserts.
        let vault = KeychainVault::new();
        let err = vault.set("", SecretValue::new("x")).unwrap_err();
        assert!(matches!(err, SecretsError::Invalid(_)));
        assert!(matches!(vault.get("").unwrap_err(), SecretsError::Invalid(_)));
        assert!(matches!(vault.delete("").unwrap_err(), SecretsError::Invalid(_)));
        let long = "n".repeat(300);
        assert!(matches!(
            vault.get(&long).unwrap_err(),
            SecretsError::Invalid(_)
        ));
    }
}
