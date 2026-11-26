//! keep on-disk state owner-only.
//!
//! sqlite creates dbs with the process umask, so `022` leaves `ledger.db` at
//! `0644`: transcripts, tool results, memory rows, all readable by any local
//! user, while the secret stores next to them are `0600`. the `-wal`/`-shm`
//! siblings and `VACUUM INTO` backups inherit the main file's mode, so fixing
//! that one file on open covers them.
//!
//! best-effort only: a db on a mount that won't chmod (backup volume, FAT stick)
//! still has to open. `pantheon doctor`'s `security.data_dir_permissions` check
//! reports loose modes, so a skip isn't silent.

use std::path::Path;

/// chmod one database file to `0600`. no-op on non-Unix and on `:memory:`.
pub fn tighten_database(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        // `Connection::path()` is empty for :memory:, and sqlite's own temp
        // dbs aren't ours to chmod.
        if name.is_empty() || path.as_os_str() == ":memory:" {
            return;
        }
        // missing file -> try again on the next open, once sqlite made it.
        let Ok(meta) = std::fs::metadata(path) else {
            return;
        };
        let mode = meta.permissions().mode() & 0o777;
        // only chmod if it's actually loose: one stat, not one chmod, on a
        // normal startup.
        if mode & 0o077 != 0 {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// mode bits the check below looks at (owner/group/other rwx).
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

        // second pass is a no-op.
        tighten_database(&db);
        assert_eq!(mode_of(&db), Some(0o600));

        // missing file shouldn't panic.
        tighten_database(&dir.join("does-not-exist.db"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
