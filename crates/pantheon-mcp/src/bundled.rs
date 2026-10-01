//! Thin facade over the canonical bundled MCP catalog.
//!
//! The ONE recipe definition lives in
//! [`pantheon_api::mcp_catalog`] (the leaf both the launcher and the
//! agent's `enable_mcp` tool depend on — putting it here would force a
//! `tools -> mcp` dependency cycle). This module re-exports the
//! canonical type and adds the launcher-side pure helpers: membership
//! checks and the in-memory enable/disable helpers the tests and the
//! setup wizard drive. There is no second copy of the recipe data —
//! the old "two copies + drift test" arrangement is gone by
//! construction. Every recipe is inert until the operator enables it in
//! `[mcp.servers.<name>]`; nothing here launches anything.
//!
//! MCPs vs plugins: a bundled MCP server is an out-of-process
//! integration — Pantheon spawns or connects to it and speaks the MCP
//! protocol, never executing the server's code. Bundled MCPs are
//! first-party: they skip the third-party consent store
//! ([`pantheon_api::approval`]) but never skip enablement; the
//! `enabled` flag is their only gate.

use pantheon_api::config::McpSection;
pub use pantheon_api::mcp_catalog::BundledMcpServer;
use std::fmt;

/// All bundled server recipes, sorted by name. Delegates to the
/// canonical catalog — the same rows the dashboard and the agent's
/// `enable_mcp` tool see.
pub fn bundled_catalog() -> Vec<BundledMcpServer> {
    pantheon_api::mcp_catalog::bundled_catalog()
}

/// Look up one bundled recipe by its config table name.
pub fn find_bundled(name: &str) -> Option<BundledMcpServer> {
    pantheon_api::mcp_catalog::find(name)
}

/// True when `name` is one of the bundled recipes.
pub fn is_bundled(name: &str) -> bool {
    find_bundled(name).is_some()
}

/// Error for the bundled enable/disable helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundledError {
    /// `name` is not one of the bundled recipes.
    UnknownServer(String),
}

impl fmt::Display for BundledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BundledError::UnknownServer(name) => {
                write!(f, "not a bundled MCP server: {name}")
            }
        }
    }
}

impl std::error::Error for BundledError {}

/// Enable or disable a bundled server in the resolved config state.
/// Pure and total: if the server has no `[mcp.servers.<name>]` entry
/// yet, the canonical recipe is materialized first (disabled by
/// default), then the `enabled` flag is flipped to the requested value.
/// Returns [`BundledError::UnknownServer`] for names outside the
/// catalog — only catalog names are toggleable, which is what keeps
/// arbitrary commands off the command line.
///
/// Note: this only mutates the in-memory [`McpSection`]; persisting the
/// config file is the surfaces' (wizard/dashboard/app) job.
pub fn set_bundled_enabled(
    section: &mut McpSection,
    name: &str,
    enabled: bool,
) -> Result<(), BundledError> {
    let recipe = find_bundled(name).ok_or_else(|| BundledError::UnknownServer(name.to_string()))?;
    // Materialize the canonical recipe only when the entry is absent;
    // an existing entry keeps the operator's keys — this flips just the
    // flag.
    let entry = section
        .servers
        .entry(name.to_string())
        .or_insert_with(|| recipe.to_config_entry());
    entry.enabled = enabled;
    Ok(())
}

/// Current enablement of a bundled server from the resolved config.
/// `None` when `name` is not a bundled recipe; a bundled recipe with no
/// config table is reported as disabled (the catalog default).
pub fn is_bundled_enabled(section: &McpSection, name: &str) -> Option<bool> {
    find_bundled(name)?;
    Some(section.servers.get(name).is_some_and(|e| e.enabled))
}
