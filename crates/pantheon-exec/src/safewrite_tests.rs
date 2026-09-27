//! In-crate tests for `safewrite` that need private/test-only seams
//! (`journal_append_for_test`, `open_tmp_nofollow`). Small and deterministic,
//! so they stay beside the code per the test-hygiene policy.
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
