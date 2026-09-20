//! MCP adapter (spec section 15): external interop boundary, not internal
//! architecture. Internal code speaks the native Capability API; MCP servers
//! are projected into it, and every projected tool is capability-gated.
use pantheon_core::capability::{Capability, Policy};
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
pub fn capability_from_token(token: &str) -> Capability {
    match token.trim() {
        "filesystem.read" => Capability::FilesystemRead,
        "filesystem.write" => Capability::FilesystemWrite,
        "shell.execute" => Capability::ShellExecute,
        "git.read" => Capability::GitRead,
        "git.write" => Capability::GitWrite,
        "git.push" => Capability::GitPush,
        "network.outbound" => Capability::NetworkOutbound,
        "browser" => Capability::Browser,
        "memory.read" => Capability::MemoryRead,
        "memory.write" => Capability::MemoryWrite,
        "secrets.use" => Capability::SecretsUse,
        "agent.spawn" => Capability::AgentSpawn,
        other => Capability::Other(other.to_string()),
    }
}

/// Project a server's tools through a policy. A tool whose capability is not
/// allowed is still listed but marked `allowed: false` so `/explain` can
/// show why it never ran.
pub fn project(server: &str, tools: &[McpTool], policy: &Policy) -> Vec<ProjectedTool> {
    tools.iter().map(|t| {
        let capability = capability_from_token(&t.requires);
        let allowed = matches!(policy.check(&capability),
            pantheon_core::capability::Decision::Allow);
        ProjectedTool { server: server.into(), name: t.name.clone(), capability, allowed }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tokens_map_and_unknowns_stay_gated() {
        assert_eq!(capability_from_token("shell.execute"), Capability::ShellExecute);
        match capability_from_token("weird.thing") {
            Capability::Other(n) => assert_eq!(n, "weird.thing"),
            other => panic!("expected Other, got {other:?}"),
        }
    }
    #[test]
    fn listing_shows_denied_tools_as_denied() {
        let tools = vec![
            McpTool { name: "read".into(), description: String::new(), requires: "filesystem.read".into() },
            McpTool { name: "browse".into(), description: String::new(), requires: "browser".into() },
        ];
        let projected = project("demo", &tools, &Policy::coder());
        assert!(projected[0].allowed);
        assert!(!projected[1].allowed);
    }
}
