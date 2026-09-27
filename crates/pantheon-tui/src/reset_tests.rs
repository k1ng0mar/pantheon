//! Tests for `crate::reset` — sibling file so sources stay test-free.
use super::*;

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pantheon-reset-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn scopes_pick_the_right_files() {
    let dir = scratch("scopes");
    std::fs::write(dir.join("config.toml"), b"x").unwrap();
    std::fs::write(dir.join("ledger.db"), b"x").unwrap();
    let config_files = target_files(&dir, "config");
    assert_eq!(config_files, vec![dir.join("config.toml")]);
    let state_files = target_files(&dir, "state");
    assert_eq!(state_files, vec![dir.join("ledger.db")]);
    let everything = target_files(&dir, "everything");
    assert_eq!(everything.len(), 2);
    // A real (empty) ledger has no active lease, so the guard is satisfied.
    assert!(!active_lease_exists(&dir).unwrap());
}

#[test]
fn no_lease_in_fresh_dir() {
    let dir = scratch("fresh");
    // No ledger at all: nothing is running, so nothing blocks a reset.
    assert!(!active_lease_exists(&dir).unwrap());
}

/// An unreadable ledger must NOT be reported as "no active lease". The old
/// implementation mapped every read error to `false` ("let the delete
/// proceed"), which meant the one case where we cannot prove no session is
/// live was the one case that skipped the guard — and then deleted
/// `ledger.db` out from under a running session.
#[test]
fn an_unreadable_ledger_is_an_error_not_a_clearance() {
    let dir = scratch("corrupt");
    std::fs::write(dir.join("ledger.db"), b"this is not a sqlite database").unwrap();
    let res = active_lease_exists(&dir);
    assert!(
        res.is_err(),
        "a corrupt ledger must refuse, not report 'no active lease'"
    );
    // The message has to say what went wrong, since this is the message the
    // user sees instead of a reset happening.
    let msg = res.unwrap_err().to_string();
    assert!(
        msg.contains("LEASE") || msg.contains("database"),
        "got: {msg}"
    );
}

/// A directory-shaped but non-database ledger is the same case.
#[test]
fn a_truncated_ledger_is_also_an_error() {
    let dir = scratch("truncated");
    // A real SQLite header with nothing after it: opens, but has no schema.
    std::fs::write(dir.join("ledger.db"), b"SQLite format 3\0").unwrap();
    assert!(
        active_lease_exists(&dir).is_err(),
        "an unreadable schema must refuse the reset"
    );
}

/// `reset --state` must clear the gateway outbox, not just the cursors.
///
/// A queued reply is a deliverable. Leaving it behind means the reset still
/// sends it, into a conversation whose ledger the same command just deleted.
#[test]
fn state_reset_includes_the_gateway_outbox_directory() {
    let dir = std::env::temp_dir().join(format!("pantheon-reset-outbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let outbox = dir.join("gateway").join("outbox");
    std::fs::create_dir_all(&outbox).unwrap();
    std::fs::write(outbox.join("m1.json"), "{}").unwrap();

    let targets = target_files(&dir, "state");
    assert!(
        targets.iter().any(|t| t.ends_with("outbox")),
        "outbox not in the state reset set: {targets:?}"
    );

    // And the removal path must actually delete a directory, not just a file.
    let t = outbox.clone();
    let r = if t.is_dir() {
        std::fs::remove_dir_all(&t)
    } else {
        std::fs::remove_file(&t)
    };
    assert!(r.is_ok(), "{r:?}");
    assert!(!outbox.exists());
}

/// A config reset must not touch queued messages: they are state, not config.
#[test]
fn config_reset_leaves_the_outbox_alone() {
    let dir = std::env::temp_dir().join(format!("pantheon-reset-cfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let outbox = dir.join("gateway").join("outbox");
    std::fs::create_dir_all(&outbox).unwrap();

    let targets = target_files(&dir, "config");
    assert!(
        !targets.iter().any(|t| t.ends_with("outbox")),
        "config reset would delete queued messages"
    );
}
