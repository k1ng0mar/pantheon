//! [`SubprocessBackend`]: [`BrowserBackend`] implemented by spawning the
//! `gsd-browser` CLI.
//!
//! Deliberately plain `std::process::Command` - NOT
//! `pantheon_exec::sandbox::runner::run_sandboxed` (see the crate docs in
//! [`crate`] for why the browser daemon needs real network egress and its
//! Unix socket, and why the `Capability::Browser` gate is the authorization
//! boundary instead).
//!
//! ## gsd-browser CLI shape (verified live against 0.1.24, 2026-09-29)
//!
//! * Invocation: `gsd-browser --session <name> --json <cmd> [args]`
//!   global flags precede the subcommand. Verified via `--help` and a
//!   live daemon attached to Chrome over `--cdp-url`.
//! * `daemon stop` exists (`daemon {start,stop,health}`); GC uses it
//!   best-effort (see [`crate::lifecycle`]).
//! * `extract` **requires** `--schema <json>` shaped as
//!   `{"properties": {"title": {"_selector": "h1"}, ...}}` (properties
//!   carry `_selector`/`_attribute` hints); `--selector` only scopes.
//!   The tool layer builds this shape - see [`crate::tools`].
//! * `wait-for` takes `--condition <enum> [--value <v>] [--timeout
//!   <ms>]` where the condition is one of `selector_visible`,
//!   `selector_hidden`, `url_contains`, `network_idle`, `delay`,
//!   `text_visible`, `text_hidden`, `request_completed`,
//!   `console_message`, `element_count`, `region_stable`.
//! * `act` takes `--intent <enum>` (fixed intents: `submit_form`,
//!   `close_dialog`, `primary_cta`, `search_field`, `next_step`,
//!   `dismiss`, `auth_action`, `back_navigation`; optional `--scope`).
//!   0.1.24 has no natural-language `act-instruction` command
//!   `browser_act` maps to `act --intent`.
//! * Without Chrome on the host the daemon fails to start and the CLI
//!   exits 1 with a JSON `{"error": {"message": ...}}` object on stderr
//!   surfaced as [`BrowserError::Failed`], never a panic.

use super::backend::BrowserBackend;
use super::error::BrowserError;
use super::proc::{self, SpawnSpec};
use std::collections::HashMap;
use std::path::PathBuf;

/// Default per-command timeout (seconds) before the child is killed.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Substrings (lowercased) that mark a failed command as a stale-ref
/// version mismatch rather than a generic failure. Checked against both
/// stdout and stderr so the signal is found wherever the CLI prints it.
/// Defensive: the exact wording is gsd-version-dependent, so several
/// phrasings are matched.
const STALE_REF_MARKERS: &[&str] = &[
    "version mismatch",
    "stale ref",
    "invalid ref",
    "unknown ref",
    "ref expired",
    "ref not found",
    "ref version",
];

/// `BrowserBackend` over the `gsd-browser` binary on PATH (or at an
/// explicit path). `extra_env` is applied to every spawn - the tool layer
/// uses it to inject `GSD_BROWSER_VAULT_KEY` without ever logging the
/// value (env values are never included in any error string here).
pub struct SubprocessBackend {
    /// Binary to spawn: a bare name (`gsd-browser`) resolved via PATH,
    /// or an explicit path.
    pub binary: PathBuf,
    /// Extra environment for every child process.
    pub extra_env: HashMap<String, String>,
    /// Seconds before a child is killed. Defaults to
    /// [`DEFAULT_TIMEOUT_SECS`].
    pub timeout_secs: u64,
}

impl SubprocessBackend {
    pub fn new(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
            extra_env: HashMap::new(),
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout_secs = secs.max(1);
        self
    }

    /// True when the combined output indicates a ref version mismatch.
    fn is_stale_ref(text: &str) -> bool {
        let lower = text.to_lowercase();
        STALE_REF_MARKERS.iter().any(|m| lower.contains(m))
    }
}

impl BrowserBackend for SubprocessBackend {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        // Verified order: global flags first, then the subcommand.
        let mut args = vec![
            "--session".to_string(),
            session.to_string(),
            "--json".to_string(),
        ];
        args.extend(argv.iter().cloned());
        let out = proc::run_with_timeout(&SpawnSpec {
            binary: &self.binary,
            args: &args,
            extra_env: &self.extra_env,
            timeout_secs: self.timeout_secs,
            error_argv: argv,
        })?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);

        if !out.status.success() {
            let combined = format!("{stdout}\n{stderr}");
            if Self::is_stale_ref(&combined) {
                return Err(BrowserError::StaleRef {
                    message: combined.trim().to_string(),
                });
            }
            return Err(BrowserError::Failed {
                argv: argv.to_vec(),
                exit: out.status.code(),
                stderr: proc::snip(stderr.trim(), 2000),
            });
        }

        let text = stdout.trim();
        if text.is_empty() {
            return Err(BrowserError::BadOutput {
                argv: argv.to_vec(),
                detail: "command succeeded but stdout was empty; expected JSON".into(),
            });
        }
        let v: serde_json::Value =
            serde_json::from_str(text).map_err(|e| BrowserError::BadOutput {
                argv: argv.to_vec(),
                detail: format!("{e}; stdout began with {:?}", proc::snip(text, 300)),
            })?;
        // Verified live: some gsd-browser failures print a top-level
        // {"error": ...} object on stdout and still exit 0. Treat that
        // as a command failure, not success (no success shape carries a
        // top-level "error" key).
        if let Some(err) = v.get("error") {
            return Err(BrowserError::Failed {
                argv: argv.to_vec(),
                exit: Some(0),
                stderr: proc::snip(&format!("{err}"), 2000),
            });
        }
        Ok(v)
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        // Verified: `daemon stop` exists (gsd-browser 0.1.24 prints
        // "Daemon stopped." on stdout with exit 0).
        let argv = vec!["daemon".to_string(), "stop".to_string()];
        let args = vec![
            "--session".to_string(),
            session.to_string(),
            "daemon".to_string(),
            "stop".to_string(),
        ];
        let out = proc::run_with_timeout(&SpawnSpec {
            binary: &self.binary,
            args: &args,
            extra_env: &self.extra_env,
            timeout_secs: self.timeout_secs,
            error_argv: &argv,
        })?;
        if out.status.success() {
            Ok(())
        } else {
            Err(BrowserError::Failed {
                argv,
                exit: out.status.code(),
                stderr: proc::snip(String::from_utf8_lossy(&out.stderr).trim(), 500),
            })
        }
    }
}
