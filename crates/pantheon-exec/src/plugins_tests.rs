//! Tests for `pantheon_exec::plugins::tests` — sibling file so sources stay test-free.
use super::*;
use tempfile::tempdir;

// ---- manifest runner path traversal regression tests ----

#[test]
fn plugin_install_dir_rejects_bad_name() {
    let d = tempdir().unwrap();
    let long = "x".repeat(65);
    for bad in ["../evil", "..", "/abs", "a/b", "", long.as_str()] {
        let err = plugin_install_dir(d.path(), bad).unwrap_err();
        assert_eq!(err.code, "PLUGIN_BAD_NAME", "{bad:?}");
    }
    assert!(
        !d.path().join("evil").exists(),
        "rejection must precede any write"
    );
}

#[test]
fn plugin_install_dir_stays_under_plugins_root() {
    let d = tempdir().unwrap();
    let dest = plugin_install_dir(d.path(), "good-name_2").unwrap();
    assert!(
        dest.ends_with("plugins/good-name_2"),
        "unexpected dest: {}",
        dest.display()
    );
}
