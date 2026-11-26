//! Browser automation and web search, kept as separate tool surfaces.
//! `browser_*` acts on live sites; `web_search` looks things up. Both are
//! registered by `pantheon-runtime`, where their config and capability gates
//! are applied.
pub mod browser;
pub mod websearch;
