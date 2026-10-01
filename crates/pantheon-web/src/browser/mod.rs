//! Browser automation for Pantheon: agent-facing `browser_*` tools backed
//! by a seven-backend lineup behind the [`BrowserBackend`] trait seam.
//!
//! ## The lineup
//!
//! * GSD Browser (`gsd-browser` CLI subprocess) — default interactive
//!   backend: versioned refs, `act --intent`, vault, stealth.
//! * Raw CDP via exactly-pinned `chromiumoxide` 0.9.1 — local fallback,
//!   no CLI, no daemon.
//! * Steel — REST + CDP websocket through the official `steel-rs` SDK;
//!   cloud and self-host share one implementation (configurable base URL).
//! * Browserbase — cloud fallback: session REST API, raw CDP websocket
//!   per session.
//! * Lightpanda — extraction/fetch only, never interactive (allowlisted
//!   command surface enforced before any connection).
//! * Playwright — official stateful `@playwright/cli` subprocess fallback.
//! * Camoufox — anti-detect fallback: patched Firefox through the
//!   official Python launcher (Juggler protocol, no CDP), driven by a
//!   long-lived JSON-over-stdio shim; fingerprint config under
//!   `[browser.camofox]`.
//!
//! [`registry::BackendKind`] is the stable `[browser] backend` config id;
//! [`registry::build_backend`] constructs the selected backend lazily
//! (no network, no browser launch until first `invoke`).
//!
//! ## Deliberate: browser backends are NOT sandboxed
//!
//! The `shell` tool routes through `pantheon_exec::sandbox::runner::run_sandboxed`
//! (bwrap, dropped caps, no-new-privs). The browser backends deliberately
//! do **not**: they need real network egress to drive live sites, and
//! sandboxing would give the illusion of containment while a daemon or
//! cloud session still dials out. The authorization boundary here is the
//! capability gate (`Capability::Browser`, default-deny; `BrowserAct`
//! approval-gated), not the sandbox — and the binaries are user-installed
//! trusted software, not arbitrary model-supplied commands. Gating the
//! *decision to browse* is the honest control.
//!
//! ## Verified CLI behavior (gsd-browser 0.1.24, live 2026-09-29)
//!
//! Verified against the real binary (`--help` plus a live daemon attached
//! to Chrome via `--cdp-url`):
//!
//! * Invocation: `gsd-browser --session <name> --json <command>...` —
//!   global flags precede the subcommand. `--browser-path` points at a
//!   Chrome binary; `--cdp-url` attaches to an already-running Chrome
//!   (useful where the daemon cannot launch Chrome itself).
//! * `daemon stop` exists (`daemon {start,stop,health}`); GC uses it
//!   best-effort (see [`crate::lifecycle`]). A stale session in
//!   `starting`/`healthy` state refuses replacement — `daemon stop`
//!   first, then retry.
//! * `snapshot` returns `{"version": N, "refs": {"e1": {...}, ...},
//!   "metadata": {...}, "count": N}` — ref *keys* are `e1`-style; the
//!   `@vN:eM` form is the *input* spelling (`click-ref @v2:e2` echoes
//!   back `"ref": "@v2:e2"`).
//! * `extract` **requires** `--schema <json>` where the schema is
//!   `{"properties": {"title": {"_selector": "h1", ...}, ...}}` — a bare
//!   property map without `properties` is rejected. `--selector` scopes,
//!   `--multiple` enables container mode. Output: `{"data": {...},
//!   "fieldCount": N, "multiple": false, "scope": null}`.
//! * `wait-for` takes `--condition <enum> [--value <v>] [--timeout <ms>]`
//!   where the condition is one of `selector_visible`, `selector_hidden`,
//!   `url_contains`, `network_idle`, `delay`, `text_visible`,
//!   `text_hidden`, `request_completed`, `console_message`,
//!   `element_count`, `region_stable`. Success output:
//!   `{"condition": ..., "elapsed_ms": N, "met": true, ...}`.
//! * `act` takes `--intent <enum>` — one of `submit_form`,
//!   `close_dialog`, `primary_cta`, `search_field`, `next_step`,
//!   `dismiss`, `auth_action`, `back_navigation` (optional `--scope`).
//!   There is **no** natural-language `act-instruction` command in
//!   0.1.24; `browser_act` maps to `act --intent`.
//! * `screenshot` takes `--output <path> --format <jpeg|png>`
//!   (default jpeg), plus `--selector` / `--full-page` / `--quality`.
//! * Failures come back as `{"error": {"code": -32603, "message": ...,
//!   "data": {"retryHint": ...}}}` on stdout with exit 0 in some paths
//!   (e.g. navigation errors) — and as non-zero exits with JSON errors
//!   in others; both are surfaced as [`error::BrowserError::Failed`],
//!   never a panic.
//! * `@playwright/cli` (verified live via `npx -y @playwright/cli
//!   --help`, 2026-09-29): `playwright-cli -s=<session> --json
//!   <command>...`; `click`/`fill`/`hover` take a snapshot ref (`e15`)
//!   or a unique selector; `type <text>` has no selector slot;
//!   `eval <func> [target]` takes an arrow function; `snapshot`
//!   captures the page to obtain element refs.
//! * Refs are backend-specific and invalidated on page navigation; a
//!   version-mismatch message becomes [`error::BrowserError::StaleRef`]
//!   and is passed through to the model so it re-snapshots, never
//!   silently retried.

pub mod backend;
pub mod browserbase;
pub mod camofox;
pub mod cdp;
pub mod error;
pub mod fill;
pub mod lifecycle;
pub mod lightpanda;
pub mod native;
pub mod playwright;
pub mod proc;
pub mod registry;
pub mod remote;
pub mod steel;
pub mod subprocess;
pub mod tools;

pub use backend::BrowserBackend;
pub use browserbase::{BrowserbaseBackend, BrowserbaseConfig, BROWSERBASE_API_KEY_ENV};
pub use camofox::{CamofoxBackend, CamofoxConfig, CAMOFOX_INSTALL_INSTRUCTIONS};
pub use error::{BrowserError, INSTALL_INSTRUCTIONS};
pub use lifecycle::{sanitize_session_name, SessionManager};
pub use lightpanda::{
    LightpandaBackend, LightpandaConfig, ALLOWED_COMMANDS as LIGHTPANDA_COMMANDS,
};
pub use native::{NativeBackend, NativeConfig};
pub use playwright::{PlaywrightBackend, PlaywrightConfig};
pub use registry::{all_backends, build_backend, BackendConfig, BackendInfo, BackendKind};
pub use steel::{SteelBackend, SteelConfig, STEEL_API_KEY_ENV};
pub use subprocess::SubprocessBackend;
pub use tools::{register_browser_tools, BrowserOptions};
