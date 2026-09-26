//! Tests for `pantheon_secrets::keychain::tests` — sibling file so sources stay test-free.
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
        let store = keyring_core::sample::Store::new().expect("sample store must build");
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
    vault.set(&name, SecretValue::new("v2")).expect("overwrite");
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

    let broker =
        crate::SecretsBroker::new().with_vault(Box::new(KeychainVault::with_service(service)));
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
    assert!(matches!(
        vault.get("").unwrap_err(),
        SecretsError::Invalid(_)
    ));
    assert!(matches!(
        vault.delete("").unwrap_err(),
        SecretsError::Invalid(_)
    ));
    let long = "n".repeat(300);
    assert!(matches!(
        vault.get(&long).unwrap_err(),
        SecretsError::Invalid(_)
    ));
    assert!(matches!(
        vault.set(&long, SecretValue::new("x")).unwrap_err(),
        SecretsError::Invalid(_)
    ));
    assert!(matches!(
        vault.delete(&long).unwrap_err(),
        SecretsError::Invalid(_)
    ));
}
