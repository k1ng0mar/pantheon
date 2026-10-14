//! Single-flight rendezvous for `gateway run`: one gateway per data dir.
//!
//! Two gateway processes would double-fire the scheduler and double-deliver
//! webhook-driven channels (Telegram's persisted offset cursor survives a
//! replay, but the scheduler and Discord webhooks do not). The rendezvous is
//! an exclusive file lock on `<data_dir>/gateway.lock`, held for the whole
//! process lifetime. A second `gateway run` sees the contention and exits
//! instead of starting.
//!
//! The lock is OS-held: if the holder dies, the kernel releases it, so
//! there is no stale state to clean up. The holder's pid is written into
//! the file after locking (readable without the lock) purely so the
//! "already running" message can name it.

use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// The rendezvous file inside a data dir.
pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway.lock")
}

/// A held gateway singleton lock. Keep it alive for the process lifetime;
/// dropping it releases the lock.
pub struct GatewayLock {
    _file: File,
}

/// Outcome of a non-blocking acquire attempt.
pub enum AcquireOutcome {
    /// This process now holds the lock.
    Acquired(GatewayLock),
    /// Another process holds the lock. `pid` is the holder's recorded pid,
    /// when it could be read and parsed.
    HeldBy { pid: Option<u32> },
}

/// Try to take the singleton lock without blocking.
pub fn try_acquire(data_dir: &Path) -> std::io::Result<AcquireOutcome> {
    let path = lock_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            // Record our pid for the contention message. A contender reads
            // this without needing the lock.
            file.set_len(0)?;
            write!(file, "{}", std::process::id())?;
            file.sync_all().ok();
            Ok(AcquireOutcome::Acquired(GatewayLock { _file: file }))
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            let pid = File::open(&path)
                .and_then(|mut f| {
                    let mut text = String::new();
                    f.read_to_string(&mut text)?;
                    Ok::<_, std::io::Error>(text)
                })
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            Ok(AcquireOutcome::HeldBy { pid })
        }
        Err(e) => Err(e),
    }
}

/// True when another process currently holds the singleton lock.
pub fn is_running(data_dir: &Path) -> bool {
    matches!(try_acquire(data_dir), Ok(AcquireOutcome::HeldBy { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-rendezvous-test-{}-{}",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn second_acquire_reports_holder_pid() {
        let dir = test_dir();
        let _first = try_acquire(&dir).expect("first acquire succeeds");
        match try_acquire(&dir).expect("contention probe succeeds") {
            AcquireOutcome::HeldBy { pid } => {
                assert_eq!(pid, Some(std::process::id()), "holder pid is reported")
            }
            AcquireOutcome::Acquired(_) => panic!("second acquire must not succeed"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_releases_on_drop() {
        let dir = test_dir();
        {
            let _held = try_acquire(&dir).expect("acquire succeeds");
            assert!(is_running(&dir), "held lock reports running");
        }
        assert!(!is_running(&dir), "dropped lock reports not running");
        // Re-acquire after release works: no stale state.
        let _again = try_acquire(&dir).expect("re-acquire succeeds");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lock_path_is_inside_data_dir() {
        let dir = test_dir();
        assert_eq!(lock_path(&dir), dir.join("gateway.lock"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
