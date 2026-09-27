//! MCP adapter (spec section 15): external interop boundary, not internal
//! architecture. Internal code speaks the native Capability API; MCP servers
//! are projected into it, and every projected tool is capability-gated.
//!
//! Two halves: [`client`] is the real stdio MCP client (`initialize`,
//! `tools/list`, `tools/call`) with call-time capability enforcement;
//! the projection helpers below map a server's advertised tools through a
//! [`Policy`](pantheon_api::capability::Policy).
pub mod client;
pub mod framed;

pub use client::{
    CapabilityGate, McpClient, McpError, McpServerConfig, McpToolDef,
    DEFAULT_MAX_MESSAGE_BYTES, DEFAULT_REQUEST_TIMEOUT, MCP_PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS,
};

use pantheon_api::capability::{Capability, Policy};
use serde::{Deserialize, Serialize};

/// One tool as an MCP server advertises it (subset of the wire shape).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    /// Capability this tool needs, as a policy token like `shell.execute`.
    pub requires: String,
}

/// A tool projected into the native plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectedTool {
    pub server: String,
    pub name: String,
    pub capability: Capability,
    pub allowed: bool,
}

/// Map a policy token string to a capability. Unknown tokens become
/// `Other(name)` so policy can still gate them explicitly.
/// Delegates to [`Capability::from_token`] — the single token table.
pub fn capability_from_token(token: &str) -> Capability {
    Capability::from_token(token)
}

/// Project a server's tools through a policy. A tool whose capability is not
/// allowed is still listed but marked `allowed: false` so `pantheon logs` can
/// show why it never ran.
pub fn project(server: &str, tools: &[McpTool], policy: &Policy) -> Vec<ProjectedTool> {
    tools
        .iter()
        .map(|t| {
            let capability = capability_from_token(&t.requires);
            let allowed = matches!(
                policy.check(&capability),
                pantheon_api::capability::Decision::Allow
            );
            ProjectedTool {
                server: server.into(),
                name: t.name.clone(),
                capability,
                allowed,
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
