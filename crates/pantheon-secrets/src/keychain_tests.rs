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
