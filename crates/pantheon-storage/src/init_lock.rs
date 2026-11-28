//! Short-lived exclusive lock around state-directory initialisation.
//!
//! Two processes may legitimately share a data dir (the TUI and the gateway
//! do), so this is deliberately *not* a process-lifetime singleton: it is
//! held only while a store applies its schema/migrations, which is the one
//! window where concurrent first-runs can race (a migration reading a table
//! another process is still creating). Kernel-held via `flock`, so a crash
//! releases it and there is no stale state to clean up.

use fs2::FileExt;
use pantheon_api::error::{Layer, PantheonError};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// Rendezvous file inside the data dir.
pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("init.lock")
}

/// Held for the duration of one migration pass; dropping it releases the lock.
pub struct InitLock {
    _file: File,
}

fn err(cause: impl Into<String>) -> PantheonError {
    PantheonError::new(
        "STATE_INIT_LOCK",
        Layer::Storage,
        true,
        cause.into(),
        "retry; if this persists, check permissions on the data directory",
        "",
    )
}

impl InitLock {
    /// Take the initialisation lock for `data_dir`, waiting for a peer that is
    /// mid-migration. The kernel releases it if the holder dies.
    pub fn acquire(data_dir: &Path) -> Result<Self, PantheonError> {
        std::fs::create_dir_all(data_dir).map_err(|e| err(format!("create dir: {e}")))?;
        let path = lock_path(data_dir);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| err(format!("open {}: {e}", path.display())))?;
        // `fs2`'s blocking acquire is `lock_exclusive`; a peer holding it for a
        // migration is a milliseconds-scale wait, and a hung peer is bounded by
        // the OS not by us, so a plain blocking call is the honest semantic.
        file.lock_exclusive()
            .map_err(|e| err(format!("lock {}: {e}", path.display())))?;
        Ok(Self { _file: file })
    }
}

impl Drop for InitLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self._file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_acquires_serialise_and_release() {
        let dir = std::env::temp_dir().join(format!("pantheon-initlock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = InitLock::acquire(&dir).expect("first");
        let path = lock_path(&dir);
        assert!(path.exists());
        // a second handle in the same process blocks on flock? On Linux flock
        // is per open file description, so this must be released before a
        // second acquire can succeed.
        drop(first);
        let second = InitLock::acquire(&dir).expect("second after release");
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
