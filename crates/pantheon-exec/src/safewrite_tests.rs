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
    let cp = w.checkpoint(std::slice::from_ref(&p), 1).unwrap();
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

#[test]
fn safewriter_confines_target_paths() {
    let base = fresh_state("confine");
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let state = base.join("state");
    let w = SafeWriter::new(state).unwrap().with_workspace_root(work.clone());

    // apply_edits outside the workspace is rejected before any write.
    let outside = base.join("evil.txt");
    let err = w
        .apply_edits(
            vec![FileEdit {
                path: outside.clone(),
                new_content: b"x".to_vec(),
                expected_hash: None,
            }],
            0,
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");
    assert!(!outside.exists(), "rejected write must not touch disk");

    // stage_edits is confined too.
    let err = w
        .stage_edits(vec![FileEdit {
            path: base.join("evil2.txt"),
            new_content: b"x".to_vec(),
            expected_hash: None,
        }])
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");

    // checkpoint() reads pre-images: confined as well.
    let err = w.checkpoint(&[base.join("evil3.txt")], 0).unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");

    // In-workspace writes still work through the confined writer.
    let p = work.join("ok.txt");
    let r = w
        .apply_edits(
            vec![FileEdit {
                path: p.clone(),
                new_content: b"v\n".to_vec(),
                expected_hash: None,
            }],
            0,
        )
        .unwrap();
    assert_eq!(r.files.len(), 1);
    assert_eq!(read(&p), "v\n");
}

#[test]
fn stage_and_checkpoint_ids_reject_traversal() {
    let dir = fresh_state("ids");
    let w = SafeWriter::new(dir).unwrap();
    let err = w.restore_checkpoint("../../nope").unwrap_err();
    assert_eq!(err.code, "SAFE_BAD_ID", "{err}");
    let err = w.apply_staged("../nope", 0).unwrap_err();
    assert_eq!(err.code, "SAFE_BAD_ID", "{err}");
    let err = w.restore_checkpoint("has space").unwrap_err();
    assert_eq!(err.code, "SAFE_BAD_ID", "{err}");
}

#[test]
#[cfg(unix)]
fn tmp_create_does_not_follow_symlinks() {
    // A planted symlink at the tmp name must fail the open, not redirect
    // the write to the link target.
    let dir = fresh_state("nofollow");
    let target = dir.join("real.txt");
    std::fs::write(&target, "original\n").unwrap();
    let link = dir.join("planted.tmp");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let err = open_tmp_nofollow(&link).unwrap_err();
    assert!(
        err.raw_os_error() == Some(libc::EEXIST) || err.kind() == std::io::ErrorKind::AlreadyExists,
        "symlink at tmp name must fail, got {err:?}"
    );
    assert_eq!(read(&target), "original\n", "target must be untouched");
    // A genuinely fresh name still opens.
    let fresh = dir.join("genuine.tmp");
    let mut f = open_tmp_nofollow(&fresh).unwrap();
    use std::io::Write;
    f.write_all(b"ok").unwrap();
    drop(f);
    assert_eq!(read(&fresh), "ok");
}
