//! OS keychain vault (spec §13): the preferred durable backend.
//!
//! macOS Keychain Services, Windows Credential Manager, and - on Linux
//! the freedesktop Secret Service (gnome-keyring, KWallet) all sit behind
//! the `keyring` crate's platform-default store. This is the "OS
//! keychain" the spec ranks above the encrypted local file; insert it
//! into a broker ahead of [`crate::filevault::EncryptedFileVault`] and
//! the environment.
//!
//! Contract notes:
//! - `get`/`set` are one local round-trip to the store (a D-Bus call on
//!   Linux, ~ms). A *locked* keyring is the one case that can block: the
//!   Secret Service store prompts to unlock, and that prompt's
//!   `Completed`-signal wait has no timeout of its own. Every operation is
//!   therefore bounded by `KEYCHAIN_TIMEOUT`; on timeout the operation is
//!   abandoned and the store surfaces as [`SecretsError::Backend`] - which
//!   the broker skips - so the runtime never hangs on a modal prompt.
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
//!   to the next vault; `set` errors - there is nowhere to write.
//! - Host **with a store that cannot answer** (locked, D-Bus broken):
//!   every op surfaces [`SecretsError::Backend`] - the secret may exist,
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

/// Probe names for [`KeychainVault::usable`]. Never hold a real secret:
/// the probe only builds an entry handle, which is what forces the
/// platform store to open the collection.
const PROBE_SERVICE: &str = "pantheon-probe";
const PROBE_USER: &str = "probe";
/// Throwaway value written and immediately deleted by the probe. Not a
/// secret; it exists only to prove the credential store answers.
const PROBE_VALUE: &str = "pantheon-probe-value";

/// How long a keychain operation may block before we give up and report the
/// store as unavailable.
///
/// The Secret Service store prompts to unlock a locked keyring, and
/// `secret-service`'s `exec_prompt` waits for the unlock dialog's
/// `Completed` signal with no timeout of its own - so an unanswered prompt
/// would block the runtime forever (the `SecretVault::get` contract forbids
/// that). This bound turns an unbounded wait into a [`SecretsError::Backend`],
/// which the broker skips.
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Set once a keychain operation has timed out. The platform store is a
/// process-wide singleton and its mutex is still held by the abandoned thread
/// while the prompt is up, so every later operation would block on that
/// mutex. Latching here makes later calls fail fast instead of hanging: the
/// keychain is treated as unusable until the process restarts, and the broker
/// falls through to the next durable vault.
static KEYCHAIN_TIMED_OUT: AtomicBool = AtomicBool::new(false);

/// The error a timed-out (or latched-off) keychain operation reports.
fn timeout_err() -> SecretsError {
    SecretsError::Backend("os keychain: operation timed out".into())
}

/// Run one blocking keychain operation under [`KEYCHAIN_TIMEOUT`].
///
/// The operation runs on a throwaway thread so the wait can be bounded; the
/// outer `Err` is a timeout/latch, the inner `Result` is the platform's own
/// answer (so callers can still distinguish `NoEntry` from a real failure).
fn run_bounded<T>(
    op: impl FnOnce() -> Result<T, KrError> + Send + 'static,
) -> Result<Result<T, KrError>, SecretsError>
where
    T: Send + 'static,
{
    run_bounded_inner(KEYCHAIN_TIMEOUT, op)
}

/// [`run_bounded`] with an explicit timeout, for tests.
fn run_bounded_inner<T>(
    timeout: Duration,
    op: impl FnOnce() -> Result<T, KrError> + Send + 'static,
) -> Result<Result<T, KrError>, SecretsError>
where
    T: Send + 'static,
{
    if KEYCHAIN_TIMED_OUT.load(Ordering::SeqCst) {
        return Err(timeout_err());
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(op());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => Ok(result),
        Err(RecvTimeoutError::Timeout) => {
            KEYCHAIN_TIMED_OUT.store(true, Ordering::SeqCst);
            Err(timeout_err())
        }
        // The operation thread died without answering (panic). Treat it like
        // a timeout so the store is not retried into a poisoned mutex.
        Err(RecvTimeoutError::Disconnected) => {
            KEYCHAIN_TIMED_OUT.store(true, Ordering::SeqCst);
            Err(timeout_err())
        }
    }
}

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

    /// Can the keychain actually store a credential right now?
    ///
    /// [`Self::platform_available`] only proves a store object could be
    /// constructed. On a headless Linux host a D-Bus session is often
    /// present while the `login` collection is not, so a real entry probe
    /// fails where `store_status` succeeded. Callers that are about to
    /// commit to the keychain (rather than merely report availability)
    /// must use this one.
    pub fn usable() -> Result<(), SecretsError> {
        ensure_store()?;
        // Building an entry is lazy on the Secret Service backend: the
        // handle is created but no D-Bus call happens until a credential
        // is read or written. So the probe has to actually round-trip a
        // value, otherwise it reports a usable keychain on a host whose
        // `login` collection does not exist. Each round-trip is bounded
        // (see `run_bounded`) so a locked keyring cannot hang the probe.
        let entry = Arc::new(KrEntry::new(PROBE_SERVICE, PROBE_USER).map_err(|e| backend_err(&e))?);
        run_bounded({
            let entry = Arc::clone(&entry);
            move || entry.set_password(PROBE_VALUE)
        })?
        .map_err(|e| backend_err(&e))?;
        let read_back = run_bounded({
            let entry = Arc::clone(&entry);
            move || entry.get_password()
        })?
        .map_err(|e| backend_err(&e))?;
        // Best-effort cleanup: a failed delete must not fail the probe.
        let _ = run_bounded({
            let entry = Arc::clone(&entry);
            move || entry.delete_credential()
        });
        if read_back != PROBE_VALUE {
            return Err(SecretsError::Backend(
                "os keychain: probe value did not round-trip".into(),
            ));
        }
        Ok(())
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
/// is already installed - including a test-injected one - this is a
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
        // name - absence, so the broker chain falls through.
        // (A store that exists but cannot answer stays a loud Backend
        // error below: the secret may well be in there.)
        if ensure_store().is_err() {
            return Ok(None);
        }
        let entry = Arc::new(self.entry(name)?);
        match run_bounded(move || entry.get_password())? {
            Ok(v) => Ok(Some(SecretValue::new(v))),
            Err(KrError::NoEntry) => Ok(None),
            Err(e) => Err(backend_err(&e)),
        }
    }

    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        // One plain copy crosses into the store; our side stays zeroized.
        // (No store → `entry` reports Backend: nowhere to write.)
        let entry = Arc::new(self.entry(name)?);
        run_bounded(move || entry.set_password(value.expose()))?.map_err(|e| backend_err(&e))
    }

    fn delete(&self, name: &str) -> Result<(), SecretsError> {
        // Validate before touching the platform, exactly like get/set. A
        // store-less host returned Ok(()) for every name, empty ones
        // included, so `delete("")` only failed where a keyring happened to
        // exist - the test passed locally and failed on a CI runner with
        // one. A rejected name is rejected everywhere.
        validate_name(name)?;
        // Nothing can be stored where there is no store: deleting every
        // conceivable name is a chain of no-ops, contract and all.
        if ensure_store().is_err() {
            return Ok(());
        }
        let entry = Arc::new(self.entry(name)?);
        match run_bounded(move || entry.delete_credential())? {
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

    /// The bound must turn the Secret Service's unbounded unlock-prompt wait
    /// into a `Backend` error (which the broker skips) and latch the store
    /// off, so a later call fails fast instead of blocking on the abandoned
    /// thread's process-wide mutex.
    #[test]
    fn a_blocking_operation_is_bounded_and_latches_the_store_off() {
        KEYCHAIN_TIMED_OUT.store(false, Ordering::SeqCst);

        // A fast operation passes the platform answer straight through.
        let fast = run_bounded_inner(Duration::from_secs(5), || {
            Ok::<_, KrError>("value".to_string())
        })
        .expect("fast op is not a timeout")
        .expect("op succeeded");
        assert_eq!(fast, "value");
        assert!(!KEYCHAIN_TIMED_OUT.load(Ordering::SeqCst));

        // An operation that never returns (the unanswered prompt) is cut off.
        let blocked = run_bounded_inner(Duration::from_millis(50), || {
            std::thread::sleep(Duration::from_secs(3600));
            Ok(())
        });
        assert!(
            matches!(blocked, Err(SecretsError::Backend(_))),
            "a blocked operation must report Backend, got {blocked:?}"
        );
        assert!(KEYCHAIN_TIMED_OUT.load(Ordering::SeqCst));

        // Latched: the next call fails fast rather than blocking.
        let latched = run_bounded_inner(Duration::from_secs(5), || Ok(()));
        assert!(matches!(latched, Err(SecretsError::Backend(_))));

        KEYCHAIN_TIMED_OUT.store(false, Ordering::SeqCst);
    }
}
