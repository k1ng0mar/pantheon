//! MCP adapter (spec section 15): external interop boundary, not internal
//! architecture. Internal code speaks the native Capability API; MCP servers
//! are projected into it, and every projected tool is capability-gated.
//!
//! Three halves: [`client`] is the real stdio MCP client (`initialize`,
//! `tools/list`, `tools/call`) with call-time capability enforcement;
//! [`http`] is the same protocol over the two remote transports (legacy
//! SSE and streamable HTTP); [`manager`] is the production wiring
//! lifecycle, reconnect backoff, operator approval, and `ToolRegistry`
//! projection. [`bundled`] is the curated catalog of opt-in recipes
//! (packages, commands, secret names) the setup wizard and surfaces
//! enable/disable in `[mcp.servers.<name>]`; every recipe is disabled by
//! default. The projection helpers below map a server's advertised
//! tools through a [`Policy`](pantheon_api::capability::Policy).

pub mod bundled;
pub mod client;
pub mod framed;
pub mod http;
pub mod manager;

pub use bundled::{
    bundled_catalog, find_bundled, is_bundled, is_bundled_enabled, set_bundled_enabled,
    BundledError, BundledMcpServer,
};

pub use client::{
    CapabilityGate, McpClient, McpError, McpServerConfig, McpToolDef, DEFAULT_MAX_MESSAGE_BYTES,
    DEFAULT_REQUEST_TIMEOUT, MCP_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS,
};
pub use http::{HttpMcpClient, HttpTransport};
pub use manager::{
    namespaced_tool_name, sanitize_segment, EnvResolver, McpManager, McpReport, McpServerSpec,
    McpTransport, PendingMcpServer, ServerHealth, ServerStatus,
};

use pantheon_api::capability::{Capability, Policy};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One live server connection, regardless of transport. `Send` so the
/// manager can hold it behind a mutex; requests are issued one at a time.
pub(crate) trait McpConn: Send {
    fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError>;
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError>;
    /// Cheap liveness probe: no network/stdin traffic.
    fn alive(&mut self) -> bool;
    /// Release the connection (kill the child / drop the stream).
    fn shutdown(&mut self);
    fn negotiated_version(&self) -> &str;
    /// The server's self-reported version, when it sends one.
    fn server_version(&self) -> Option<&str>;
}

/// Classify one decoded JSON-RPC value. Shared by the stdio and HTTP
/// clients so the pairing rules stay identical.
pub(crate) enum Wire {
    Request {
        id: Value,
    },
    Response {
        id: Value,
        result: Option<Value>,
        error: Option<(i64, String)>,
    },
    Notification,
    Invalid,
}

pub(crate) fn classify(v: &Value) -> Wire {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return Wire::Invalid,
    };
    if obj.get("jsonrpc").and_then(|j| j.as_str()) != Some("2.0") {
        return Wire::Invalid;
    }
    let id = obj.get("id").cloned();
    let method = obj
        .get("method")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    match (id, method) {
        (Some(id), Some(_)) => Wire::Request { id },
        (Some(id), None) => {
            if let Some(err) = obj.get("error") {
                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                Wire::Response {
                    id,
                    result: None,
                    error: Some((code, message)),
                }
            } else {
                Wire::Response {
                    id,
                    result: obj.get("result").cloned(),
                    error: None,
                }
            }
        }
        (None, Some(_)) => Wire::Notification,
        (None, None) => Wire::Invalid,
    }
}

/// Pull the human-readable text out of a `tools/call` result's `content`
/// blocks. This is what a projected `mcp_*` tool returns to the agent
/// loop; when the server reports `isError: true` the caller maps it to
/// [`McpError::ServerToolError`] first, so this never sees error results.
pub fn result_text(result: &Value) -> String {
    let mut out = String::new();
    if let Some(blocks) = result.get("content").and_then(|c| c.as_array()) {
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
    }
    if out.is_empty() {
        result.to_string()
    } else {
        out
    }
}

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
/// Delegates to [`Capability::from_token`] - the single token table.
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
