//! Owner-only permissions for on-disk state.
//!
//! SQLite creates a database with the process umask, so a default `022` leaves
//! `ledger.db` at `0644`: every transcript, tool result and memory row is
//! readable by any other local user, while the secret stores beside it are
//! `0600`. Files created later by SQLite (the `-wal` and `-shm` siblings, and
//! a `VACUUM INTO` backup) inherit the main file's mode, so tightening the
//! main file once on open covers them too.
//!
//! This is deliberately best-effort: a database on a read-only or
//! permission-preserving-agnostic filesystem (a mounted backup volume, a FAT
//! stick) must still open. `pantheon doctor`'s `security.data_dir_permissions`
//! check reports the mode when it is loose, so a skip is visible, not silent.

use std::path::Path;

/// Tighten one database file to `0600`. No-op on non-Unix and on `:memory:`.
pub fn tighten_database(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // `Connection::path()` returns an empty path for in-memory databases,
        // and SQLite's own temporary databases are not ours to chmod.
        if name.is_empty() || path.as_os_str() == ":memory:" {
            return;
        }
        // `metadata` on a file that does not exist yet is an error, not a
        // panic; the next open (when SQLite has created it) tightens it.
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let mode = meta.permissions().mode() & 0o777;
        // Keep owner rw, drop everything for group/other. Only chmod when it
        // is actually loose, so a normal startup costs one stat, not one chmod.
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// The mode bits that matter for the check below (owner/group/other rwx).
#[cfg(all(unix, test))]
pub fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_loose_database_is_tightened_and_a_tight_one_is_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pantheon-privacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("ledger.db");
        std::fs::write(&db, b"x").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();

        tighten_database(&db);
        assert_eq!(mode_of(&db), Some(0o600));

        // Idempotent: a second pass keeps it owner-only.
        tighten_database(&db);
        assert_eq!(mode_of(&db), Some(0o600));

        // Missing files are a no-op, not a panic.
        tighten_database(&dir.join("does-not-exist.db"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
