//! Live platform keychain verification.
//!
//! Separate process on purpose: the keyring store is installed once per
//! process, so this binary must not share one with the sample-store lib
//! tests. It talks to the host's real credential store — macOS Keychain,
//! Windows CredMan, or the Linux Secret Service — and skips loudly (with
//! the reason) when the host has no usable store: headless boxes often
//! have a session bus but no usable default collection, which is an
//! environment fact, not a vault bug.

use pantheon_secrets::{KeychainVault, SecretValue, SecretVault};

/// Store the platform actually accepts, or print why not and skip.
fn live_store_or_skip(vault: &KeychainVault, name: &str) -> bool {
    if let Err(e) = KeychainVault::platform_available() {
        eprintln!("SKIP live platform keychain — store unavailable: {e}");
        return false;
    }
    let _ = vault.delete(name);
    match vault.set(name, SecretValue::new("pantheon-probe")) {
        Ok(()) => {
            let _ = vault.delete(name);
            true
        }
        Err(e) => {
            eprintln!("SKIP live platform keychain — store not usable here: {e}");
            false
        }
    }
}

#[test]
fn real_platform_store_roundtrip() {
    let vault = KeychainVault::with_service("pantheon-live-test");
    let name = format!("live-{}", std::process::id());
    if !live_store_or_skip(&vault, &name) {
        return;
    }

    // The probe landed: from here on every op must genuinely work.
    vault
        .set(&name, SecretValue::new("sk-live"))
        .unwrap_or_else(|e| panic!("set must work after a successful probe: {e}"));
    let got = vault
        .get(&name)
        .expect("get")
        .expect("just written — must be found");
    assert_eq!(got.expose(), "sk-live");

    vault
        .set(&name, SecretValue::new("v2"))
        .expect("overwrite against the platform store");
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

/// The no-store-at-all side of the availability contract.
///
/// Only meaningful on a host where the platform store does not exist at
/// all (headless CI, container without a Secret Service): `get` must read
/// as absence so a broker chain falls through, `delete` as its no-op, and
/// `set` as "nowhere to write". On hosts with a store, the roundtrip test
/// above owns the contract and this one steps aside.
#[test]
fn absent_store_reads_as_absence_for_broker_fallthrough() {
    if KeychainVault::platform_available().is_ok() {
        return;
    }
    use pantheon_secrets::{SecretsError, SecretVault};
    let vault = KeychainVault::new();
    assert!(
        matches!(vault.get("whatever"), Ok(None)),
        "no store must read as absence, not an error"
    );
    assert!(
        matches!(vault.delete("whatever"), Ok(())),
        "no store means nothing to delete — the trait's no-op"
    );
    assert!(
        matches!(vault.set("k", SecretValue::new("v")), Err(SecretsError::Backend(_))),
        "no store means nowhere to write — loud"
    );
}
