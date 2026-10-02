//! Shared child-process runner for the CLI backends (`gsd-browser`,
//! `playwright-cli`): spawn with piped stdio, wait with a kill-on-timeout,
//! and map spawn failures to [`BrowserError`].
//!
//! Env values are never included in any error string here.

use super::error::BrowserError;
use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Poll interval while waiting for a child to exit.
const WAIT_POLL: Duration = Duration::from_millis(5);

/// Everything [`run_with_timeout`] needs for one spawn.
pub(crate) struct SpawnSpec<'a> {
    /// Binary to spawn (bare name resolved via PATH, or explicit path).
    pub binary: &'a Path,
    /// Full argv after the binary.
    pub args: &'a [String],
    /// Extra environment for the child.
    pub extra_env: &'a HashMap<String, String>,
    /// Seconds before the child is killed.
    pub timeout_secs: u64,
    /// The canonical command echoed in errors (the tool-level command,
    /// not the transport's full argv).
    pub error_argv: &'a [String],
}

/// Map a spawn failure to either `BinaryMissing` (the actionable,
/// never-panic case) or a generic `Failed`.
pub(crate) fn spawn_error(binary: &Path, error_argv: &[String], e: io::Error) -> BrowserError {
    match e.kind() {
        io::ErrorKind::NotFound => BrowserError::BinaryMissing {
            binary: binary.display().to_string(),
            detail: format!("binary not found: {e}"),
        },
        io::ErrorKind::PermissionDenied => BrowserError::BinaryMissing {
            binary: binary.display().to_string(),
            detail: format!("binary is not executable: {e}"),
        },
        _ => BrowserError::Failed {
            argv: error_argv.to_vec(),
            exit: None,
            stderr: format!("failed to spawn browser binary: {e}"),
        },
    }
}

/// Spawn, wait with a timeout (killing on expiry), and return the
/// captured output.
pub(crate) fn run_with_timeout(spec: &SpawnSpec) -> Result<Output, BrowserError> {
    let mut cmd = Command::new(spec.binary);
    cmd.args(spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .envs(spec.extra_env);
    let mut child = cmd
        .spawn()
        .map_err(|e| spawn_error(spec.binary, spec.error_argv, e))?;
    let deadline = Instant::now() + Duration::from_secs(spec.timeout_secs.max(1));
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BrowserError::Timeout {
                        argv: spec.error_argv.to_vec(),
                        secs: spec.timeout_secs,
                    });
                }
                std::thread::sleep(WAIT_POLL);
            }
            Err(e) => {
                return Err(BrowserError::Failed {
                    argv: spec.error_argv.to_vec(),
                    exit: None,
                    stderr: format!("failed waiting on browser child: {e}"),
                })
            }
        }
    }
    child.wait_with_output().map_err(|e| BrowserError::Failed {
        argv: spec.error_argv.to_vec(),
        exit: None,
        stderr: format!("failed reading browser child output: {e}"),
    })
}

/// Truncate a long string for error detail (keeps errors bounded).
pub(crate) fn snip(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...[truncated {} chars]", &s[..max], s.len() - max)
    }
}
