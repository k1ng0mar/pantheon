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
