//! The one resolver for Pantheon's data directory.
//!
//! Security decisions depend on this path (the file-tool deny list and
//! the shell sandbox's hidden paths cover it), so it must resolve the
//! same way everywhere. Before this module the deny list only knew the
//! data dir when `PANTHEON_DATA_DIR` was exported, which left the default
//! `~/.pantheon` (and its `.env`) readable and writable by the file tools
//! (audit SEC-02).

use std::path::{Path, PathBuf};

/// `PANTHEON_DATA_DIR` when set and non-empty, else `$HOME/.pantheon`
/// (`%USERPROFILE%` on Windows), else `$XDG_DATA_HOME/pantheon`, else a
/// per-user directory under the system temp dir.
pub fn resolve() -> PathBuf {
    if let Some(d) = std::env::var_os("PANTHEON_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(h) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(h).join(".pantheon");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(xdg).join("pantheon");
    }
    let user = std::env::var_os("USER")
        .or_else(|| std::env::var_os("USERNAME"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".into());
    std::env::temp_dir()
        .join("pantheon")
        .join(user)
        .join(".pantheon")
}

/// Create the data directory and make it owner-only.
///
/// SQLite (and every plain `File::create` in the tree) uses the process umask,
/// so a default `022` leaves the data dir at `0755` and the ledgers inside it
/// at `0644`: transcripts and memory readable by every local user, next to a
/// `0600` secret store (audit F2). Startup calls this before anything opens a
/// database. Best-effort on non-Unix and when the mode cannot be changed (a
/// read-only or mode-agnostic mount); `pantheon doctor` reports a loose mode.
///
/// Returns the resolved path so callers can use the result directly:
/// `let dir = data_dir::ensure_private();`
pub fn ensure_private() -> PathBuf {
    let dir = resolve();
    ensure_private_at(&dir);
    dir
}

/// The [`ensure_private`] body, taking the directory explicitly so callers
/// that resolved it themselves (and tests) use the same rule.
pub fn ensure_private_at(dir: &Path) {
    let _ = std::fs::create_dir_all(dir);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = std::fs::metadata(dir) else {
            return;
        };
        // Only chmod when it is actually loose, so a normal startup costs one
        // stat rather than one chmod.
        if meta.permissions().mode() & 0o077 != 0 {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn ensure_private_tightens_a_loose_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pantheon-data-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_at(&dir);
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "data dir must be owner-only, got {mode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
