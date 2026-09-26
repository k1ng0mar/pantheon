//! Tests for `pantheon_exec::safewrite::tests` — sibling file so sources stay test-free.
use super::*;
fn fresh_state(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-safe-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
fn write(p: &Path, s: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, s).unwrap();
}
fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn fingerprint_of_missing_file_reports_not_existed() {
    let dir = fresh_state("finger");
    let p = dir.join("nope.txt");
    let fp = fingerprint_of(&p).unwrap();
    assert!(!fp.existed);
    assert_eq!(fp.len, 0);
}

#[test]
fn preview_is_read_only() {
    let dir = fresh_state("preview");
    let p = dir.join("a.txt");
    write(&p, "hello\n");
    let before = std::fs::read(&p).unwrap();
    let _ = preview_edit(&p, b"world\n").unwrap();
    let after = std::fs::read(&p).unwrap();
    assert_eq!(before, after, "preview must not touch disk");
}

#[test]
fn atomic_write_publishes_bytes() {
    let dir = fresh_state("atomic");
    let p = dir.join("f.txt");
    atomic_write(&p, b"hi\n").unwrap();
    assert_eq!(read(&p), "hi\n");
}

#[test]
fn stale_expected_hash_is_rejected() {
    let dir = fresh_state("stale");
    let p = dir.join("f.txt");
    write(&p, "v1\n");
    let w = SafeWriter::new(dir.clone()).unwrap();
    let err = w
        .apply_edits(
            vec![FileEdit {
                path: p.clone(),
                new_content: b"v2\n".to_vec(),
                expected_hash: Some("deadbeef".into()),
            }],
            1,
        )
        .unwrap_err();
    assert_eq!(err.code, "SAFE_STALE");
    assert_eq!(read(&p), "v1\n", "stale rejection must not mutate");
}

#[test]
fn apply_then_rollback_restores_pre_image() {
    let dir = fresh_state("rollback");
    let p = dir.join("f.txt");
    write(&p, "before\n");
    let w = SafeWriter::new(dir.clone()).unwrap();
    let fp = fingerprint_of(&p).unwrap();
    let receipt = w
        .apply_edits(
            vec![FileEdit {
                path: p.clone(),
                new_content: b"after\n".to_vec(),
                expected_hash: Some(fp.hash.clone()),
            }],
            1,
        )
        .unwrap();
    assert_eq!(read(&p), "after\n");
    let restored = w.restore_checkpoint(&receipt.checkpoint_id).unwrap();
    assert!(restored.contains(&p));
    assert_eq!(read(&p), "before\n");
}

#[test]
fn rollback_to_seq_picks_latest_in_range() {
    let dir = fresh_state("seq");
    let p = dir.join("f.txt");
    write(&p, "v0\n");
    let w = SafeWriter::new(dir.clone()).unwrap();
    let mut seq = 0i64;
    for body in ["v1\n", "v2\n", "v3\n"] {
        let fp = fingerprint_of(&p).unwrap();
        seq += 1;
        w.apply_edits(
            vec![FileEdit {
                path: p.clone(),
                new_content: body.as_bytes().to_vec(),
                expected_hash: Some(fp.hash),
            }],
            seq,
        )
        .unwrap();
    }
    assert_eq!(read(&p), "v3\n");
    let (id, _) = w.rollback_to_seq(2).unwrap();
    assert!(w.read_manifest(&id).is_ok());
    // rollback_to_seq(2) restores the pre-image captured AT seq 2,
    // which was the on-disk state before seq 2's apply, i.e. v1.
    assert_eq!(read(&p), "v1\n");
}

#[test]
fn stage_then_apply_staged_commits() {
    let dir = fresh_state("staged");
    let p = dir.join("f.txt");
    write(&p, "old\n");
    let w = SafeWriter::new(dir.clone()).unwrap();
    let fp = fingerprint_of(&p).unwrap();
    let batch = w
        .stage_edits(vec![FileEdit {
            path: p.clone(),
            new_content: b"new\n".to_vec(),
            expected_hash: Some(fp.hash),
        }])
        .unwrap();
    assert_eq!(read(&p), "old\n", "stage must not publish");
    let receipt = w.apply_staged(&batch.id, 1).unwrap();
    assert_eq!(read(&p), "new\n");
    assert_eq!(receipt.stage_id.as_deref(), Some(batch.id.as_str()));
}

#[test]
fn recover_restores_uncommitted_begin() {
    let dir = fresh_state("recover");
    let p = dir.join("f.txt");
    write(&p, "stable\n");
    // Stage + apply via direct path: that path journal-commits, so we
    // simulate a torn apply by replaying the journal on a torn state:
    // a checkpoint exists, a `begin` entry exists, no `commit` follows.
    let w = SafeWriter::new(dir.clone()).unwrap();
    let cp = w.checkpoint(&[p.clone()], 1).unwrap();
    // Manually journal a `begin` for that checkpoint — no matching commit.
    w.journal_append_for_test("begin", "manual", Some(cp.id.clone()), 1, vec![p.clone()])
        .unwrap();
    // Mutate the file to simulate a partially-applied run.
    atomic_write(&p, b"torn\n").unwrap();
    assert_eq!(read(&p), "torn\n");
    // recover() must find the uncommitted begin and restore pre-image.
    let w2 = SafeWriter::new(dir.clone()).unwrap();
    let ids = w2.recover().unwrap();
    assert!(
        ids.contains(&cp.id),
        "expected checkpoint {} in recovered set {:?}",
        cp.id,
        ids
    );
    assert_eq!(read(&p), "stable\n");
}
