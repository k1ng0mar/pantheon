//! [`BrowserError`] and its conversion to [`PantheonError`].
//!
//! Error codes (all [`pantheon_api::error::Layer::Execution`]):
//!
//! | Code                    | Meaning                                          | Retryable |
//! |-------------------------|--------------------------------------------------|-----------|
//! | `BROWSER_BINARY_MISSING`| backend binary not found / not executable        | no        |
//! | `BROWSER_FAILED`        | backend ran, command failed (stderr attached)    | no        |
//! | `BROWSER_TIMEOUT`       | child killed after the timeout                   | yes       |
//! | `BROWSER_BAD_OUTPUT`    | stdout was not the expected JSON                 | no        |
//! | `BROWSER_STALE_REF`     | ref version mismatch - re-snapshot, don't retry  | no        |
//! | `BROWSER_UNSUPPORTED_COMMAND` | backend doesn't implement this command      | no        |

use pantheon_api::error::{Layer, PantheonError};

/// Install instructions surfaced whenever the binary is missing. Never
/// crash or panic on a missing optional binary - tell the user how to get
/// it and let the run continue.
///
/// Verified 2026-09-29 against the real project (open-gsd/gsd-browser):
/// the npm package is `@opengsd/gsd-browser` (npm registry, latest 0.2.2
/// at verification time) and the repo is `github.com/open-gsd/gsd-browser`
/// (GitHub API). An earlier revision of this string named
/// `install.gsd.build` and the `gsd-build` org - both wrong.
pub const INSTALL_INSTRUCTIONS: &str = "gsd-browser is not installed or not executable. \
Install it with `npm install -g @opengsd/gsd-browser` \
or build from source: https://github.com/open-gsd/gsd-browser. \
Then make sure `gsd-browser` is on PATH, or point the browser config at it explicitly (`[browser] binary`).";

/// Failures from the browser backend. Kept as a plain enum (not a
/// `PantheonError`) so the trait seam in [`crate::backend`] stays
/// decoupled from the API crate's error type; the `From` impl below is
/// the single place the mapping lives.
#[derive(Debug)]
pub enum BrowserError {
    /// The binary is missing from PATH or not executable. Carries the
    /// install instructions so the tool layer can hand the user a fix,
    /// not just a failure.
    BinaryMissing { binary: String, detail: String },
    /// The binary ran but the command failed (non-zero exit).
    Failed {
        argv: Vec<String>,
        exit: Option<i32>,
        stderr: String,
    },
    /// The child was killed after `timeout_secs` without finishing.
    Timeout { argv: Vec<String>, secs: u64 },
    /// Stdout was not parseable as the expected JSON.
    BadOutput { argv: Vec<String>, detail: String },
    /// A ref (`@v1:e1`) was used after its page version expired. The
    /// message is passed through so the model re-snapshots; the tool
    /// layer must NOT silently retry the same ref.
    StaleRef { message: String },
    /// The active backend does not implement this canonical command
    /// (e.g. `act-instruction` on the raw-CDP backends, or any
    /// interactive command on the extraction-only Lightpanda backend).
    /// Not a failure of the page or session - pick a different command
    /// or backend.
    UnsupportedCommand { command: String, backend: String },
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrowserError::BinaryMissing { binary, detail } => {
                write!(f, "browser binary '{binary}' unavailable: {detail}")
            }
            BrowserError::Failed { argv, exit, stderr } => {
                write!(
                    f,
                    "browser command {:?} failed (exit {exit:?}): {stderr}",
                    argv
                )
            }
            BrowserError::Timeout { argv, secs } => {
                write!(
                    f,
                    "browser command {argv:?} timed out after {secs}s and was killed"
                )
            }
            BrowserError::BadOutput { argv, detail } => {
                write!(
                    f,
                    "browser command {argv:?} produced unparseable output: {detail}"
                )
            }
            BrowserError::StaleRef { message } => write!(f, "stale browser ref: {message}"),
            BrowserError::UnsupportedCommand { command, backend } => write!(
                f,
                "browser backend '{backend}' does not implement command '{command}'"
            ),
        }
    }
}

impl std::error::Error for BrowserError {}

fn perr(code: &str, cause: String, retryable: bool, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Execution, retryable, cause, remediation, "")
}

impl From<BrowserError> for PantheonError {
    fn from(e: BrowserError) -> Self {
        match e {
            // The gsd-specific install block only applies to the gsd
            // backend's binary. Other backends (e.g. camofox's Python
            // shim) carry their own install guidance in `detail` - the
            // gsd text would be actively misleading there.
            BrowserError::BinaryMissing { binary, detail } if binary.contains("gsd") => perr(
                "BROWSER_BINARY_MISSING",
                format!("{detail}\n{INSTALL_INSTRUCTIONS}"),
                false,
                &format!("install gsd-browser or fix the configured binary path (was: {binary})"),
            ),
            BrowserError::BinaryMissing { binary, detail } => perr(
                "BROWSER_BINARY_MISSING",
                detail,
                false,
                &format!(
                    "install the backend's dependency or fix the configured binary path (was: {binary})"
                ),
            ),
            BrowserError::Failed { argv, exit, stderr } => perr(
                "BROWSER_FAILED",
                format!("gsd-browser {:?} exited {exit:?}: {stderr}", argv),
                false,
                "check the command arguments; the browser session may be in an unexpected state - re-snapshot before interacting",
            ),
            BrowserError::Timeout { argv, secs } => perr(
                "BROWSER_TIMEOUT",
                format!("gsd-browser {argv:?} exceeded {secs}s and was killed"),
                true,
                "retry once; if it times out again the page or daemon is likely wedged - stop the session and start over",
            ),
            BrowserError::BadOutput { argv, detail } => perr(
                "BROWSER_BAD_OUTPUT",
                format!("gsd-browser {argv:?} output was not valid JSON: {detail}"),
                false,
                "the gsd-browser CLI may have changed its output format; check its version",
            ),
            // Stale refs are a *model* problem, not a retry problem: the
            // message goes through verbatim so the model knows to take a
            // fresh snapshot and pick new refs.
            BrowserError::StaleRef { message } => perr(
                "BROWSER_STALE_REF",
                message,
                false,
                "take a fresh browser_snapshot and use the new refs; never reuse refs across page changes",
            ),
            // Unsupported command is a *configuration* problem: the
            // active backend can't do this. The remediation names the
            // fix (different command or different backend).
            BrowserError::UnsupportedCommand { command, backend } => perr(
                "BROWSER_UNSUPPORTED_COMMAND",
                format!("backend '{backend}' does not implement '{command}'"),
                false,
                "use a command the active backend supports, or switch [browser] backend",
            ),
        }
    }
}
