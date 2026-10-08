//! Behavioral tests for the pre-state hashing transaction protocol.
//!
//! The contract: when a file-writing tool call parks for approval, the
//! runtime records the SHA-256 of the target file. On resume, it re-reads
//! and compares. A mismatch means the file changed between park and apply:
//! the grant is stale and the call is refused, not blindly applied.

use pantheon_api::events::Event;
use pantheon_exec::prestate::{file_write_target, hash_file, verify_pre_state};
use pantheon_runtime::Supervisor;
use std::fs;
use tempfile::tempdir;

#[test]
fn hash_file_returns_none_for_missing_file() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("does-not-exist.txt");
    assert_eq!(hash_file(&path), None);
}

#[test]
fn hash_file_is_deterministic_for_same_content() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"hello world").unwrap();
    let h1 = hash_file(&path).unwrap();
    let h2 = hash_file(&path).unwrap();
    assert_eq!(h1, h2);
}

#[test]
fn hash_file_changes_when_content_changes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"version 1").unwrap();
    let h1 = hash_file(&path).unwrap();
    fs::write(&path, b"version 2").unwrap();
    let h2 = hash_file(&path).unwrap();
    assert_ne!(h1, h2);
}

#[test]
fn hash_file_empty_string_for_missing_matches_verify() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("new.txt");
    // File did not exist at park time: recorded hash is empty string.
    // File still does not exist at resume: verify passes.
    assert!(verify_pre_state(&path, "").is_ok());
}

#[test]
fn hash_file_empty_string_mismatches_when_file_created() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("new.txt");
    // File did not exist at park time (empty hash).
    // User created it while the run was parked.
    fs::write(&path, b"user wrote this").unwrap();
    assert!(verify_pre_state(&path, "").is_err());
}

#[test]
fn verify_pre_state_ok_when_unchanged() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"stable content").unwrap();
    let recorded = hash_file(&path).unwrap();
    assert!(verify_pre_state(&path, &recorded).is_ok());
}

#[test]
fn verify_pre_state_err_when_changed() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"original").unwrap();
    let recorded = hash_file(&path).unwrap();
    fs::write(&path, b"modified while parked").unwrap();
    let err = verify_pre_state(&path, &recorded).unwrap_err();
    assert!(err.contains("changed between approval and apply"));
}

#[test]
fn verify_pre_state_err_when_file_deleted() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"will be deleted").unwrap();
    let recorded = hash_file(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let err = verify_pre_state(&path, &recorded).unwrap_err();
    assert!(err.contains("changed between approval and apply"));
}

#[test]
fn file_write_target_extracts_path_for_write_file() {
    let args = serde_json::json!({"path": "/tmp/test.rs", "content": "fn main() {}"});
    assert_eq!(
        file_write_target("write_file", &args),
        Some("/tmp/test.rs".to_string())
    );
}

#[test]
fn file_write_target_returns_none_for_read_tools() {
    let args = serde_json::json!({"path": "/tmp/test.rs"});
    assert_eq!(file_write_target("read", &args), None);
    assert_eq!(file_write_target("read_file", &args), None);
    assert_eq!(file_write_target("search", &args), None);
    assert_eq!(file_write_target("shell", &args), None);
}

#[test]
fn file_write_target_returns_none_without_path_arg() {
    let args = serde_json::json!({"content": "fn main() {}"});
    assert_eq!(file_write_target("write_file", &args), None);
}

#[test]
fn file_write_target_returns_none_for_unknown_tools() {
    // No `_write` escape hatch: only the real built-in write tool is
    // tracked. A hypothetical future file-writing tool must be added
    // here explicitly, not smuggled through args.
    let args = serde_json::json!({"path": "/tmp/test.rs", "_write": true});
    assert_eq!(file_write_target("custom_tool", &args), None);
}

#[test]
fn full_cycle_park_verify_pass() {
    // Simulate: agent proposes write -> parks -> user approves -> file
    // unchanged -> verify passes -> write applies.
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, b"[old]\nkey = 1\n").unwrap();

    // Park time: record hash
    let recorded = hash_file(&path).unwrap();

    // User approves without touching the file.

    // Resume time: verify
    assert!(verify_pre_state(&path, &recorded).is_ok());
}

#[test]
fn full_cycle_park_verify_fail() {
    // Simulate: agent proposes write -> parks -> user edits file manually
    // -> approves -> verify fails -> write is refused.
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.toml");
    fs::write(&path, b"[old]\nkey = 1\n").unwrap();

    // Park time: record hash
    let recorded = hash_file(&path).unwrap();

    // User edits the file while the run is parked.
    fs::write(&path, b"[old]\nkey = 2\n").unwrap();

    // User approves.

    // Resume time: verify catches the change
    assert!(verify_pre_state(&path, &recorded).is_err());
}

fn prestate_test_supervisor(tag: &str) -> Supervisor {
    let dir = std::env::temp_dir().join(format!("pantheon-prestate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("parked-run").unwrap();
    sup
}

#[test]
fn prestate_lookup_returns_latest_record_for_scope() {
    // The runtime records one PreStateRecorded event per parked
    // file-writing call; on resume it looks the hash back up by scope.
    let sup = prestate_test_supervisor("lookup");
    sup.emit(Event::ApprovalRequested {
        run_id: "parked-run".into(),
        scope: "call-1:write_file:{...}".into(),
    })
    .unwrap();
    sup.emit(Event::PreStateRecorded {
        run_id: "parked-run".into(),
        scope: "call-1:write_file:{...}".into(),
        path: "/tmp/a.toml".into(),
        sha256: "aaa".into(),
    })
    .unwrap();
    let found = sup
        .prestate_for_scope("parked-run", "call-1:write_file:{...}")
        .unwrap()
        .expect("recorded pre-state must be found");
    assert_eq!(found.path, "/tmp/a.toml");
    assert_eq!(found.sha256, "aaa");
    let _ = std::fs::remove_dir_all(sup.data_dir());
}

#[test]
fn prestate_lookup_returns_none_without_a_record() {
    // Read-only calls (and runs from before this feature) record no
    // pre-state: the resume path must allow them, not refuse them.
    let sup = prestate_test_supervisor("missing");
    assert!(sup
        .prestate_for_scope("parked-run", "call-9:shell:{...}")
        .unwrap()
        .is_none());
    let _ = std::fs::remove_dir_all(sup.data_dir());
}

#[test]
fn prestate_record_survives_ledger_replay() {
    // The record lives in the SQLite ledger, so a process restart
    // between park and resume does not lose it.
    let dir = std::env::temp_dir().join(format!("pantheon-prestate-replay-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let sup = Supervisor::open(dir.clone()).unwrap();
        sup.start_run("parked-run").unwrap();
        sup.emit(Event::PreStateRecorded {
            run_id: "parked-run".into(),
            scope: "call-2:write_file:{...}".into(),
            path: "/tmp/b.toml".into(),
            sha256: "bbb".into(),
        })
        .unwrap();
    }
    // Reopen against the same data dir, like a restarted process.
    let sup2 = Supervisor::open(dir.clone()).unwrap();
    let found = sup2
        .prestate_for_scope("parked-run", "call-2:write_file:{...}")
        .unwrap()
        .expect("pre-state must survive a ledger reopen");
    assert_eq!(found.sha256, "bbb");
    let _ = std::fs::remove_dir_all(&dir);
}
