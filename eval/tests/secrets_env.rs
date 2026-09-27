//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_secrets::{EnvVault, SecretVault};

#[test]
fn env_literal_system_vault_reads_live_env_when_allowlisted() {
    std::env::set_var("PANTHEON_TEST_ALLOWLISTED", "yes");
    let vault = EnvVault::system().with_env_allowlist(vec!["PANTHEON_TEST_ALLOWLISTED".into()]);
    assert_eq!(
        vault
            .get("env:PANTHEON_TEST_ALLOWLISTED")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("yes".into())
    );
    std::env::remove_var("PANTHEON_TEST_ALLOWLISTED");
    // Same vault, no allowlist hit for an unrelated var: closed.
    std::env::set_var("PANTHEON_TEST_NOT_ALLOWLISTED", "no");
    assert_eq!(
        vault.get("env:PANTHEON_TEST_NOT_ALLOWLISTED").unwrap(),
        None
    );
    std::env::remove_var("PANTHEON_TEST_NOT_ALLOWLISTED");
}
