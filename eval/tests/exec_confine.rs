//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral — SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem — lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Tests for `pantheon_exec::confine` — sibling file so sources stay test-free.
use pantheon_exec::confine::*;
use pantheon_api::error::PantheonError;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

fn fresh(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-confine-{}-{}-{}",
        tag,
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

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Sets HOME/PANTHEON_DATA_DIR for one test, restoring both on drop.
/// Caller must run serially w.r.t. other env-touching tests in this binary.
struct EnvGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    prev_home: Option<OsString>,
    prev_data: Option<OsString>,
}
impl EnvGuard {
    fn new(home: Option<&Path>, data_dir: Option<&Path>) -> Self {
        let lock = env_lock();
        let prev_home = std::env::var_os("HOME");
        let prev_data = std::env::var_os("PANTHEON_DATA_DIR");
        match home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        match data_dir {
            Some(d) => std::env::set_var("PANTHEON_DATA_DIR", d),
            None => std::env::remove_var("PANTHEON_DATA_DIR"),
        }
        Self {
            _lock: lock,
            prev_home,
            prev_data,
        }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match &self.prev_data {
            Some(v) => std::env::set_var("PANTHEON_DATA_DIR", v),
            None => std::env::remove_var("PANTHEON_DATA_DIR"),
        }
    }
}

fn code_of(err: &PantheonError) -> &str {
    &err.code
}

#[test]
fn dotdot_escapes_rejected() {
    let base = fresh("escape");
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    // `../.ssh/id_rsa` style: climbs out of the workspace.
    let err = confine(&work.join("../outside.txt"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_ESCAPE", "{err}");
    let err = confine(&work.join("sub/../../outside2.txt"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_ESCAPE", "{err}");
    // Absolute path outside the workspace is not "inside" either.
    let err = confine(&base.join("outside3.txt"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_ESCAPE", "{err}");
}

#[test]
#[cfg(unix)]
fn symlink_pointing_outside_rejected() {
    let base = fresh("symlink");
    let work = base.join("work");
    let outside = base.join("outside");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, work.join("link")).unwrap();
    let err = confine(&work.join("link/secret.txt"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_ESCAPE", "{err}");
    // Symlink to a single outside file, not just a dir.
    let target = outside.join("f.txt");
    std::fs::write(&target, "x").unwrap();
    std::os::unix::fs::symlink(&target, work.join("filelink")).unwrap();
    let err = confine(&work.join("filelink"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_ESCAPE", "{err}");
}

#[test]
fn ssh_and_etc_denied_by_glob() {
    let home = fresh("home");
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let _g = EnvGuard::new(Some(&home), None);

    // `~`-prefixed input is denied before any fs touch or capability check.
    let err = confine(Path::new("~/.ssh/authorized_keys"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_DENIED", "{err}");
    // Absolute form under the real home is denied too.
    let err = confine(&home.join(".ssh/authorized_keys"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_DENIED", "{err}");
    // Exact-file (non-`/**`) patterns.
    for dot in [".bashrc", ".bash_profile", ".profile", ".gitconfig"] {
        let err = confine(Path::new(&format!("~/{dot}")), &work).unwrap_err();
        assert_eq!(code_of(&err), "CONFINE_DENIED", "{dot}: {err}");
    }
    // /etc/** regardless of workspace.
    let err = confine(Path::new("/etc/passwd"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_DENIED", "{err}");
}

#[test]
#[cfg(unix)]
fn symlink_into_denied_dir_rejected_as_denied_not_escape() {
    // A symlink inside the workspace pointing at ~/.ssh must hit the deny
    // glob (canonical form), not merely the containment check.
    let home = fresh("home2");
    let base = fresh("symlinkdeny");
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::os::unix::fs::symlink(home.join(".ssh"), work.join("slink")).unwrap();
    let _g = EnvGuard::new(Some(&home), None);
    let err = confine(&work.join("slink/id_rsa"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_DENIED", "{err}");
}

#[test]
fn dotdot_into_home_ssh_denied_by_glob() {
    // `work/../.ssh/id_rsa` canonicalizes into the deny glob even when the
    // workspace itself sits under $HOME.
    let home = fresh("home3");
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let _g = EnvGuard::new(Some(&home), None);
    let err = confine(&work.join("../.ssh/id_rsa"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_DENIED", "{err}");
}

#[test]
fn data_dir_secrets_denied_by_glob() {
    let base = fresh("datadir");
    let work = base.join("work");
    let data = base.join("data");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(data.join("secrets")).unwrap();
    let _g = EnvGuard::new(None, Some(&data));
    let err = confine(&data.join("secrets/token.json"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_DENIED", "{err}");
}

#[test]
fn in_workspace_paths_allowed() {
    let base = fresh("allowed");
    let work = base.join("work");
    std::fs::create_dir_all(work.join("sub")).unwrap();
    let existing = work.join("sub/a.txt");
    std::fs::write(&existing, "hi").unwrap();

    // Existing file -> canonical path.
    let got = confine(&existing, &work).unwrap();
    assert_eq!(got, std::fs::canonicalize(&existing).unwrap());

    // Non-existent file in a non-existent subdir -> parent canonicalized.
    let got = confine(&work.join("newdir/b.txt"), &work).unwrap();
    assert_eq!(got, std::fs::canonicalize(&work).unwrap().join("newdir/b.txt"));

    // `..` that stays inside is fine.
    let got = confine(&work.join("sub/../c.txt"), &work).unwrap();
    assert_eq!(got, std::fs::canonicalize(&work).unwrap().join("c.txt"));

    // Relative paths join onto the workspace root.
    let got = confine(Path::new("rel.txt"), &work).unwrap();
    assert_eq!(got, std::fs::canonicalize(&work).unwrap().join("rel.txt"));

    // The root itself is allowed.
    let got = confine(&work, &work).unwrap();
    assert_eq!(got, std::fs::canonicalize(&work).unwrap());
}

#[test]
fn empty_path_rejected() {
    let work = fresh("empty");
    let err = confine(Path::new(""), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_BAD_PATH", "{err}");
}

#[test]
fn missing_workspace_root_fails_closed() {
    let work = fresh("noroot").join("does-not-exist");
    let err = confine(Path::new("a.txt"), &work).unwrap_err();
    assert_eq!(code_of(&err), "CONFINE_BAD_PATH", "{err}");
}
