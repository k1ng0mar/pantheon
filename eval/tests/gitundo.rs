//! Git-based undo: repo-level snapshots + restore, backed by a real git
//! repo in a temp dir. These tests actually run `git`, so they exercise
//! the real snapshot/patch/reverse-apply path.
use pantheon_exec::gitundo::{repo_root, GitUndo};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn tempdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon_gitundo_{}_{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q"]);
    git(dir, &["config", "user.email", "t@pantheon.local"]);
    git(dir, &["config", "user.name", "pantheon"]);
    // A first commit so HEAD exists (undo diffs against HEAD).
    std::fs::write(dir.join("README.md"), "# root\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

#[test]
fn repo_root_finds_git_repo() {
    let d = tempdir();
    let _ = std::fs::write(d.join("x"), "1");
    init_repo(&d);
    let root = repo_root(&d).expect("should find a repo");
    assert!(root.exists());

    // A plain directory with no repo returns None.
    let empty = tempdir();
    let _ = std::fs::write(empty.join("y"), "1");
    std::thread::sleep(Duration::from_millis(5));
    assert!(repo_root(&empty).is_none(), "non-repo should be None");
}

#[test]
fn snapshot_then_restore_roundtrip() {
    let d = tempdir();
    std::fs::write(d.join("base.txt"), "original\n").unwrap();
    std::fs::write(d.join("old.txt"), "stay\n").unwrap();
    init_repo(&d);

    let state = d.join("state");
    let undo = GitUndo::new(state.clone(), d.clone()).expect("init");

    // 1. Pre-batch the tree already has pending work (uncommitted). The
    //    snapshot captures this exact dirty state so it can be restored.
    std::fs::write(d.join("base.txt"), "original\nwork-so-far\n").unwrap();
    let point = undo.snapshot("before batch").expect("snapshot");
    assert!(
        point.changed_files.iter().any(|f| f == "base.txt"),
        "snapshot records the pending change, got {:?}",
        point.changed_files
    );

    // 2. The agent batch then runs: more changes + a deletion + a new file.
    std::fs::write(d.join("base.txt"), "original\nwork-so-far\nmore\n").unwrap();
    let _ = std::fs::remove_file(d.join("old.txt"));
    std::fs::write(d.join("new.txt"), "brand new\n").unwrap();

    // 3. Undo the batch: tree returns to the snapshot state. The batch
    //    extended the snapshot content, so the stored patch cannot apply
    //    and the faithful snapshot checkout is used.
    let res = undo.restore(&point).expect("restore");
    assert!(
        !res.via_patch,
        "diverged tree must use the checkout path, got via_patch=true"
    );
    assert_eq!(
        std::fs::read_to_string(d.join("base.txt")).unwrap(),
        "original\nwork-so-far\n",
        "base.txt should revert to the snapshot content"
    );
    assert!(
        d.join("old.txt").exists(),
        "deleted file should be restored"
    );
    assert!(!d.join("new.txt").exists(), "added file should be removed");
}

#[test]
fn list_and_delete_undo_points() {
    let d = tempdir();
    std::fs::write(d.join("a"), "1\n").unwrap();
    init_repo(&d);

    let state = d.join("state");
    let undo = GitUndo::new(state.clone(), d.clone()).expect("init");
    std::fs::write(d.join("a"), "1\n2\n").unwrap();
    let p1 = undo.snapshot("one").expect("snapshot 1");
    std::thread::sleep(Duration::from_millis(10));
    std::fs::write(d.join("a"), "1\n2\n3\n").unwrap();
    let p2 = undo.snapshot("two").expect("snapshot 2");

    let all = undo.list().expect("list");
    assert!(all.len() >= 2, "both snapshots listed, got {}", all.len());
    // Newest first.
    assert!(all[0].created_ms >= all[1].created_ms);

    assert_ne!(p1.id, p2.id);
    undo.delete(&p1).expect("delete p1");
    let after = undo.list().expect("list after delete");
    assert!(
        !after.iter().any(|p| p.id == p1.id),
        "deleted point should be gone"
    );
}

#[test]
fn corrupt_patch_is_rejected() {
    let d = tempdir();
    std::fs::write(d.join("a"), "1\n").unwrap();
    init_repo(&d);

    let state = d.join("state");
    let undo = GitUndo::new(state.clone(), d.clone()).expect("init");
    std::fs::write(d.join("a"), "1\n2\n").unwrap();
    let point = undo.snapshot("x").expect("snapshot");

    // Tamper with the stored patch on disk.
    let patch_file = state.join("git_undo").join(&point.patch_path);
    std::fs::write(&patch_file, "garbage not a real patch\n").unwrap();

    let res = undo.restore(&point);
    let err = res.expect_err("tampered patch must fail integrity");
    assert_eq!(err.code, "UNDO_CORRUPT");
}
