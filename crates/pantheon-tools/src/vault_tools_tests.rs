//! Tests for `pantheon_exec::vault_tools::tests` — sibling file so sources stay test-free.
use super::*;
use tempfile::tempdir;

#[test]
fn test_vault_archive_read_search_list() {
    let tmp = tempdir().unwrap();
    let vault_path = tmp.path().to_path_buf();
    let opts = VaultToolOptions {
        vault_dir: vault_path.clone(),
    };

    let mut reg = ToolRegistry::new();
    register_vault_tools(&mut reg, opts);

    // 1. Archive a note
    let res = reg.execute(
            "vault_archive",
            r#"{"category": "notes", "title": "my-research", "content": "Autonomous agents need durable memory.", "tags": ["agent", "runtime"]}"#,
        ).unwrap();
    assert!(res.contains("my-research.md"));

    // 2. Read the note back
    let read_res = reg
        .execute("vault_read", r#"{"path": "notes/my-research.md"}"#)
        .unwrap();
    assert!(read_res.contains("Autonomous agents need durable memory."));
    assert!(read_res.contains("tags:"));

    // 3. Search the vault
    let search_res = reg
        .execute("vault_search", r#"{"query": "durable memory"}"#)
        .unwrap();
    assert!(search_res.contains("notes/my-research.md"));

    // 4. List files
    let list_res = reg
        .execute("vault_list", r#"{"category": "notes"}"#)
        .unwrap();
    assert!(list_res.contains("notes/my-research.md"));
}

#[test]
fn test_vault_path_traversal_rejected() {
    let tmp = tempdir().unwrap();
    let opts = VaultToolOptions {
        vault_dir: tmp.path().to_path_buf(),
    };

    let mut reg = ToolRegistry::new();
    register_vault_tools(&mut reg, opts);

    let err = reg
        .execute(
            "vault_archive",
            r#"{"category": "../etc", "title": "bad", "content": "malicious"}"#,
        )
        .unwrap_err();
    assert_eq!(err.code, "VAULT_PATH_TRAVERSAL");
}

/// A symlink planted inside the vault must not become a read primitive for
/// the rest of the filesystem. The old check was `rel.contains("..")`, which
/// a symlink never contains, so `vault/escape -> /etc/passwd` resolved and
/// read cleanly.
#[cfg(unix)]
#[test]
fn a_symlink_inside_the_vault_cannot_escape_it() {
    use std::os::unix::fs::symlink;
    let tmp = tempdir().unwrap();
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    // A secret well outside the vault.
    let secret_dir = tmp.path().join("outside");
    std::fs::create_dir_all(&secret_dir).unwrap();
    std::fs::write(secret_dir.join("secret.md"), "TOP SECRET").unwrap();
    // The attack: a link inside the vault pointing out of it.
    symlink(&secret_dir, vault.join("escape")).unwrap();

    let err = resolve_safe_vault_path(&vault, "escape/secret.md").unwrap_err();
    assert_eq!(err.code, "VAULT_ESCAPE");
}

/// The same protection for a link that targets a file rather than a
/// directory, which is the shape actually used to read one known file.
#[cfg(unix)]
#[test]
fn a_symlink_to_a_single_outside_file_is_refused() {
    use std::os::unix::fs::symlink;
    let tmp = tempdir().unwrap();
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    let outside = tmp.path().join("passwd-copy");
    std::fs::write(&outside, "root:x:0:0").unwrap();
    symlink(&outside, vault.join("creds.md")).unwrap();

    assert!(resolve_safe_vault_path(&vault, "creds.md").is_err());
}

/// Ordinary in-vault paths must still resolve, including a path that does
/// not exist yet (writes create it) and a nested new directory.
#[test]
fn ordinary_vault_paths_still_resolve() {
    let tmp = tempdir().unwrap();
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(vault.join("notes")).unwrap();
    std::fs::write(vault.join("notes").join("a.md"), "hello").unwrap();

    assert!(resolve_safe_vault_path(&vault, "notes/a.md").is_ok());
    // Not on disk yet, but a legitimate write target.
    assert!(resolve_safe_vault_path(&vault, "notes/b.md").is_ok());
    assert!(resolve_safe_vault_path(&vault, "notes/deep/new/c.md").is_ok());
    // Leading slash and `.` segments are normalized, not rejected.
    assert!(resolve_safe_vault_path(&vault, "/notes/a.md").is_ok());
    assert!(resolve_safe_vault_path(&vault, "./notes/a.md").is_ok());
}

/// Traversal is still refused, and so is the vault root itself.
#[test]
fn traversal_and_the_vault_root_are_refused() {
    let tmp = tempdir().unwrap();
    let vault = tmp.path().join("vault");
    std::fs::create_dir_all(&vault).unwrap();
    assert_eq!(
        resolve_safe_vault_path(&vault, "../etc/passwd")
            .unwrap_err()
            .code,
        "VAULT_PATH_TRAVERSAL"
    );
    assert_eq!(
        resolve_safe_vault_path(&vault, "notes/../../etc")
            .unwrap_err()
            .code,
        "VAULT_PATH_TRAVERSAL"
    );
    assert!(resolve_safe_vault_path(&vault, "").is_err());
    assert!(resolve_safe_vault_path(&vault, ".").is_err());
}
