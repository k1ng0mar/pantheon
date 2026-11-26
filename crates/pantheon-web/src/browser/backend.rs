//! the seam between the tool layer and whatever actually drives the browser.
//! the tool layer builds canonical argv (`navigate`, `snapshot`, `click-ref`,
//! ...) and each backend translates it to its own transport.
//!
//!   gsd          default, gsd-browser subprocess, full command surface
//!                including `act --intent` (checked against 0.1.24)
//!   chromiumoxide  native CDP, no subprocess, pinned =0.9.1
//!   steel        REST sessions + raw CDP ws, hosted or self-hosted
//!   browserbase  POST /v1/sessions then raw CDP wss, no SDK
//!   playwright   playwright-cli, ref-based snapshots
//!   camofox      python shim over json-stdio, patched firefox, no CDP
//!
//! selection is `[browser] backend` in config. see `registry` for per-backend
//! settings and `tools::BrowserOptions` for how the surface adapts.
//!
//! we don't wrap other agent frameworks. they aren't browsers, and wrapping one
//! turns the design inside out - pantheon is the agent, the backend is infra.

use super::error::BrowserError;
use std::time::Duration;

/// one cap for every wait path, including the gsd `--timeout` passthrough. no
/// wait-for runs longer than this whatever the backend timeout says, so the
/// same call doesn't wait differently depending on which backend is active.
pub const WAIT_CAP: Duration = Duration::from_secs(30);

/// `argv` is one canonical command plus args. on success you get the parsed
/// json back.
///
/// ignore transport-level unknowns you can safely skip (unrecognised CDP event
/// types, the schema drifts). unknown *commands* in `argv` become
/// [`BrowserError::UnsupportedCommand`].
pub trait BrowserBackend: Send + Sync {
    /// run one command in `session`
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError>;

    /// tear down whatever backs `session`. best effort: Ok if it's already
    /// gone, and callers treat failures as non-fatal.
    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError>;
}
