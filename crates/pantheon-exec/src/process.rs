//! Process-group lifecycle helpers.
//!
//! Cancellation is deliberately a two-step action: persist `canceling` in
//! the operation, then TERM the owned process group, wait briefly, and KILL
//! only that group.  PID numbers alone are never treated as ownership.

use std::time::Duration;

#[cfg(unix)]
use std::time::Instant;

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// An owned process group.  Constructing this from a persisted group row is
/// safe because the row is tied to a live run lease by the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessGroup {
    pub pgid: i32,
}

impl ProcessGroup {
    pub fn new(pgid: i32) -> Option<Self> {
        (pgid > 1).then_some(Self { pgid })
    }

    /// TERM → grace period → KILL.  The guard rejects PID 1 and our own PID.
    pub fn terminate(&self, grace: Duration) {
        let Some(group) = Self::new(self.pgid) else {
            return;
        };
        if group.pgid == std::process::id() as i32 {
            return;
        }
        #[cfg(unix)]
        {
            unsafe {
                // Ignore ESRCH: the desired terminal state is already true.
                libc::killpg(group.pgid, SIGTERM);
            }
            let deadline = Instant::now() + grace;
            while Instant::now() < deadline {
                // A group is considered gone when killpg returns ESRCH.  We
                // do not use `kill(0, ...)` because that would probe the
                // caller's own group.
                #[cfg(target_os = "linux")]
                unsafe {
                    if libc::killpg(group.pgid, 0) < 0 {
                        let e = std::io::Error::last_os_error();
                        if e.raw_os_error() == Some(libc::ESRCH) {
                            return;
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            unsafe {
                libc::killpg(group.pgid, SIGKILL);
            }
        }
        #[cfg(not(unix))]
        {
            // Non-Unix implementations use the process id as the group id.
            // The caller still has to provide a valid child pid; no shell is
            // spawned here.
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &group.pgid.to_string()])
                .status();
        }
    }
}

