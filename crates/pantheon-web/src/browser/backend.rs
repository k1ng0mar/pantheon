//! Backend trait: the seam between the tool layer and whatever drives the
//! browser. Every backend implements [`BrowserBackend`]; the tool layer in
//! [`crate::tools`] builds backend-agnostic *canonical* argv (the
//! gsd-browser command vocabulary: `navigate`, `snapshot`, `click-ref`,
//! ...) and each backend translates it to its own transport.
//!
//! ## The lineup
//!
//! | Backend (`BackendKind`) | Transport | Role |
//! |-------------------------|-----------|------|
//! | `Gsd` (default) | `gsd-browser` CLI subprocess | Default interactive backend: full command surface incl. semantic `act --intent` (verified live against 0.1.24) |
//! | `ChromiumOxide` | Native CDP via `chromiumoxide` (=0.9.1, pinned) | Local dependency-free fallback: no CLI/subprocess, single-binary friendly |
//! | `Steel` | REST session management + raw CDP websocket | Hosted or self-hosted sessions; self-host Docker exposes the same API as cloud, so one implementation with a configurable base URL |
//! | `Browserbase` | `POST /v1/sessions` → raw CDP `wss://` URL | Cloud fallback; driven over the websocket directly, no SDK |
//! | `Lightpanda` | CDP websocket to `lightpanda serve` | **Extraction only** - fetch/extract workloads; never the interactive backend. Its tool surface exposes fetch/extract, not interactive act |
//! | `Playwright` | `playwright-cli` (`@playwright/cli`) subprocess | Mature fallback: industry-standard engine, ref-based snapshots |
//! | `Camofox` | Long-lived Python shim over JSON-over-stdio (official `camoufox` launcher, Juggler - no CDP) | Anti-detect fallback: patched Firefox; fingerprint config in `[browser.camofox]` |
//!
//! Backend selection lives in config (`[browser] backend`, resolved to
//! [`crate::registry::BackendKind`]); see [`crate::registry`] for the
//! per-backend settings and [`crate::tools::BrowserOptions`] for how the
//! tool surface adapts (extraction-only surface for Lightpanda,
//! `browser_act` only on GSD).
//!
//! Deliberately NOT backends: competing agent frameworks and sidecar
//! transports are never wrapped - wrapping them would invert the
//! architecture (Pantheon is the agent; the backend is infrastructure).

use super::error::BrowserError;

/// Drives a browser session identified by a sanitized session name.
///
/// `argv` is one canonical browser command and its arguments
/// (e.g. `["navigate", "https://example.com"]`); each backend translates
/// the canonical vocabulary to its own transport (gsd-browser passes it
/// through with `--session`/`--json` global flags; the CDP backends map
/// it onto CDP domains; Playwright maps it onto `playwright-cli`).
/// On success returns the command's structured output as JSON.
///
/// Backends must NOT hard-fail on transport-level unknowns they can
/// safely ignore (e.g. unrecognized CDP event types - schema drift is a
/// known maintenance tax on the native path); unknown *commands* in
/// `argv` become [`BrowserError::UnsupportedCommand`].
pub trait BrowserBackend: Send + Sync {
    /// Invoke one canonical browser command in `session`, returning
    /// parsed JSON.
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError>;

    /// Stop the backend state backing `session` (gsd daemon, CDP page,
    /// remote session, playwright-cli session). Best-effort:
    /// implementations should return `Ok` when the state is already
    /// gone, and callers (see [`crate::lifecycle`]) must treat failures
    /// as non-fatal.
    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError>;
}
