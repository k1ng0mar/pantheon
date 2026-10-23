//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem - lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Tests for `pantheon_secrets::logins` - the login metadata store.
//!
//! The security property under test is the one that matters: credentials
//! must land on disk owner-only, and the metadata file must never contain
//! the password itself. Both were untested; `write_meta` also created its
//! temp file at the umask default and ignored a failed chmod.

use pantheon_secrets::logins::{LoginStore, LOGINS_META_FILE};
use std::os::unix::fs::PermissionsExt;

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode()
}

#[test]
fn created_login_metadata_is_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let store = LoginStore::open(dir.path());
    store
        .create("github.com", "user@example.com", "hunter2-secret")
        .unwrap();

    let meta = dir.path().join(LOGINS_META_FILE);
    let m = mode_of(&meta);
    assert_eq!(
        m & 0o077,
        0,
        "logins metadata must not be group/world readable (mode {:#o})",
        m
    );
    assert_eq!(m & 0o700, 0o600, "expected exactly 0600, got {:#o}", m);
}

#[test]
fn password_never_lands_in_the_metadata_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = LoginStore::open(dir.path());
    store
        .create("github.com", "user@example.com", "SUPER-SECRET-PASSWORD")
        .unwrap();

    let raw = std::fs::read_to_string(dir.path().join(LOGINS_META_FILE)).unwrap();
    assert!(
        !raw.contains("SUPER-SECRET-PASSWORD"),
        "the metadata file must reference the password, never contain it:\n{raw}"
    );
    assert!(
        raw.contains("user@example.com"),
        "the username belongs in the metadata"
    );
}

#[test]
fn update_rewrite_keeps_the_private_mode() {
    // The temp-file + rename path must not quietly widen permissions on a
    // rewrite. This pins the end state, not the failure mode: on a host with
    // a permissive umask the old `let _ = set_permissions` also produced
    // 0600, so this assertion passes either way. What it does pin is that the
    // rewrite keeps the private mode, and that no credentials survive in the
    // metadata across the rewrite.
    let dir = tempfile::tempdir().unwrap();
    let store = LoginStore::open(dir.path());
    let cred = store
        .create("github.com", "u@example.com", "first-password")
        .unwrap();
    store
        .update(&cred.id, None, None, Some("second-password"))
        .unwrap();

    let meta = dir.path().join(LOGINS_META_FILE);
    assert_eq!(
        mode_of(&meta) & 0o777,
        0o600,
        "mode must stay 0600 after an update rewrite"
    );
    let raw = std::fs::read_to_string(&meta).unwrap();
    assert!(!raw.contains("second-password"));
    assert!(!raw.contains("first-password"));
}

#[test]
fn no_temp_file_is_left_behind_after_a_write() {
    // The atomic-ish rename must clean up its temp, or a stale temp holding
    // credentials lingers in the data dir.
    let dir = tempfile::tempdir().unwrap();
    let store = LoginStore::open(dir.path());
    store.create("github.com", "u@example.com", "pw").unwrap();
    store.create("x.com", "u@example.com", "pw2").unwrap();

    let leftover = dir.path().join("logins.json.tmp");
    assert!(
        !leftover.exists(),
        "temp file should be renamed away, not left behind"
    );
}
