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
pub mod oauth;
pub mod rea;
pub mod rea_audit;

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
    namespaced_tool_name, sanitize_segment, EnvResolver, McpAuth, McpManager, McpReport,
    McpServerSpec, McpTransport, PendingMcpServer, ServerHealth, ServerStatus,
};

use pantheon_api::capability::{Capability, Policy};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One resource advertised by `resources/list`.
///
/// The wire shape is camelCase; `McpResource` is the stable shape the
/// runtime and CLI consume. `size` is advisory (the server may omit it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// One parameterized resource template from `resources/templates/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceTemplate {
    pub uri_template: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// One content block returned by `resources/read`. Exactly one of `text` or
/// `blob` is normally set; both are optional so unusual servers still parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResourceContent {
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Base64-encoded bytes, per the MCP spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// One prompt advertised by `prompts/list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpPrompt {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub arguments: Vec<McpPromptArgument>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpPromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// One message returned by `prompts/get`. `content` stays as raw JSON
/// because MCP permits text, image, audio, embedded-resource, and future
/// block types; flattening it would silently drop data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptMessage {
    pub role: String,
    pub content: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpPromptResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub messages: Vec<McpPromptMessage>,
}

/// One live server connection, regardless of transport. `Send` so the
/// manager can hold it behind a mutex; requests are issued one at a time.
pub(crate) trait McpConn: Send {
    fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError>;
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError>;
    fn list_resources(&mut self) -> Result<Vec<McpResource>, McpError>;
    fn list_resource_templates(&mut self) -> Result<Vec<McpResourceTemplate>, McpError>;
    fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>, McpError>;
    fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpError>;
    fn get_prompt(&mut self, name: &str, args: &Value) -> Result<McpPromptResult, McpError>;
    /// Cheap liveness probe: no network/stdin traffic.
    fn alive(&mut self) -> bool;
    /// Release the connection (kill the child / drop the stream).
    fn shutdown(&mut self);
    fn negotiated_version(&self) -> &str;
    /// The server's self-reported version, when it sends one.
    fn server_version(&self) -> Option<&str>;
}

fn parse_list<T: DeserializeOwned>(
    result: &Value,
    key: &str,
    method: &str,
) -> Result<Vec<T>, McpError> {
    let arr = result
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Protocol(format!("{method} response has no '{key}' array")))?;
    arr.iter()
        .cloned()
        .map(|v| {
            serde_json::from_value(v).map_err(|e| {
                McpError::Protocol(format!("{method} returned a bad {key} entry: {e}"))
            })
        })
        .collect()
}

/// Cursor for the next page of a list result. An empty string is treated
/// as the end, matching servers that use `""` for "no more".
pub(crate) fn next_cursor(result: &Value) -> Option<String> {
    result
        .get("nextCursor")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub(crate) fn resources_from(result: &Value) -> Result<Vec<McpResource>, McpError> {
    parse_list(result, "resources", "resources/list")
}

pub(crate) fn resource_templates_from(
    result: &Value,
) -> Result<Vec<McpResourceTemplate>, McpError> {
    parse_list(result, "resourceTemplates", "resources/templates/list")
}

pub(crate) fn resource_contents_from(result: &Value) -> Result<Vec<McpResourceContent>, McpError> {
    parse_list(result, "contents", "resources/read")
}

pub(crate) fn prompts_from(result: &Value) -> Result<Vec<McpPrompt>, McpError> {
    parse_list(result, "prompts", "prompts/list")
}

pub(crate) fn prompt_result_from(result: &Value) -> Result<McpPromptResult, McpError> {
    serde_json::from_value(result.clone())
        .map_err(|e| McpError::Protocol(format!("prompts/get returned a bad result: {e}")))
}

/// Classify one decoded JSON-RPC value. Shared by the stdio and HTTP
/// clients so the pairing rules stay identical.
pub(crate) enum Wire {
    Request {
        id: Value,
        method: String,
        params: Value,
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
        (Some(id), Some(method)) => Wire::Request {
            id,
            method: method.to_string(),
            params: obj.get("params").cloned().unwrap_or(Value::Null),
        },
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

#[cfg(test)]
mod resource_wire_tests {
    use super::*;

    #[test]
    fn parses_resources_and_templates() {
        let resources = serde_json::json!({
            "resources": [{
                "uri": "file:///etc/hosts",
                "name": "hosts",
                "mimeType": "text/plain",
                "size": 12
            }]
        });
        let parsed = resources_from(&resources).expect("resources");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].uri, "file:///etc/hosts");
        assert_eq!(parsed[0].mime_type.as_deref(), Some("text/plain"));
        assert_eq!(parsed[0].size, Some(12));

        let templates = serde_json::json!({
            "resourceTemplates": [{
                "uriTemplate": "file:///{path}",
                "name": "files"
            }]
        });
        let parsed = resource_templates_from(&templates).expect("templates");
        assert_eq!(parsed[0].uri_template, "file:///{path}");
    }

    #[test]
    fn parses_resource_contents_and_prompts() {
        let contents = serde_json::json!({
            "contents": [
                {"uri": "file:///a", "mimeType": "text/plain", "text": "hello"},
                {"uri": "file:///b", "mimeType": "application/octet-stream", "blob": "AAEC"}
            ]
        });
        let parsed = resource_contents_from(&contents).expect("contents");
        assert_eq!(parsed[0].text.as_deref(), Some("hello"));
        assert_eq!(parsed[1].blob.as_deref(), Some("AAEC"));

        let prompts = serde_json::json!({
            "prompts": [{
                "name": "review",
                "description": "Review a diff",
                "arguments": [{"name": "diff", "required": true}]
            }]
        });
        let parsed = prompts_from(&prompts).expect("prompts");
        assert_eq!(parsed[0].name, "review");
        assert!(parsed[0].arguments[0].required);

        let prompt = serde_json::json!({
            "description": "Review a diff",
            "messages": [{
                "role": "user",
                "content": {"type": "text", "text": "look at this"}
            }]
        });
        let parsed = prompt_result_from(&prompt).expect("prompt");
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.messages[0].role, "user");
    }

    #[test]
    fn missing_arrays_are_protocol_errors() {
        let err = resources_from(&serde_json::json!({})).unwrap_err();
        assert!(matches!(err, McpError::Protocol(_)));
    }
}
