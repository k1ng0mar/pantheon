//! Pantheon web access: browser automation and web search as one crate,
//! two distinct tool surfaces.
//!
//! * [`browser`] — agent-facing `browser_*` tools backed by the
//!   `gsd-browser` binary (trait seam for a future native CDP client).
//!   For *doing things on live sites*.
//! * [`websearch`] — the `web_search` tool (Tavily provider behind a
//!   provider trait). For *looking things up*.
//!
//! The split is deliberate: the model looks things up with `web_search`
//! and acts on live sites with `browser_*`. Both are consumed only by
//! `pantheon-runtime` (tool registration behind the session's
//! `[browser]`/`[websearch]` config); the TUI's config keys are unchanged.
pub mod browser;
pub mod websearch;
