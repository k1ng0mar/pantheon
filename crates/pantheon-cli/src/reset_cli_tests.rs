//! Tests for `pantheon_cli::reset_cli::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn scopes_pick_the_right_files() {
    let dir = std::env::temp_dir().join(format!("pantheon-reset-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), b"x").unwrap();
    std::fs::write(dir.join("ledger.db"), b"x").unwrap();
    let config_files = target_files(&dir, "config");
    assert_eq!(config_files, vec![dir.join("config.toml")]);
    let state_files = target_files(&dir, "state");
    assert_eq!(state_files, vec![dir.join("ledger.db")]);
    let everything = target_files(&dir, "everything");
    assert_eq!(everything.len(), 2);
    assert!(!active_lease_exists(&dir));
}

#[test]
fn no_lease_in_fresh_dir() {
    let dir = std::env::temp_dir().join(format!("pantheon-reset2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!active_lease_exists(&dir));
}
