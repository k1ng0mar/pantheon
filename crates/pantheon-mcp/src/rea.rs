//! REA ("Reverse Engineer Anything", morluto/rea) tool classification.
//!
//! REA is a third-party MCP server that exposes binary reverse-engineering
//! tools to agents. Its tool catalogue spans two risk tiers:
//!
//! * **Static analysis** (read-only): decompile, inspect, and compare
//!   binaries without executing the target. These map to
//!   [`Capability::FilesystemRead`] only if the tool also avoids spawning
//!   processes and accessing the network.
//! * **External or mutating effects**: process launches, network access,
//!   writes, and dynamic tracing map to [`Capability::BinaryTrace`], which
//!   default policies mark Approval so the run parks for a human.
//!
//! Classification is generated from the REA 6.5.0 tool effect audit
//! embedded in [`crate::rea_audit`]: 110 tools, each storing its full
//! effect vector and the tier derived from it. Anything not in the map
//! (including tools REA adds in future versions) falls back to the
//! server-level [`Capability::BinaryTrace`], which parks for approval.
//! That is the fail-closed direction: a new tool can never silently
//! inherit a permissive capability.
//!
//! The classification keys on the configured server name `"rea"` (the
//! documented setup). Tool identity uses exact `(server, tool)` tuple
//! keys built from the authoritative MCP handshake pair — never string
//! splitting — so a tool cannot accidentally match another server.

use pantheon_api::capability::Capability;
use std::collections::HashMap;

/// Configured server name the REA classification applies to.
pub const REA_SERVER_NAME: &str = "rea";

/// Restrictive server-level fallback for REA: any REA tool without an
/// explicit per-tool classification parks for human approval.
pub fn rea_server_capability() -> Capability {
    Capability::BinaryTrace
}

/// Explicit per-tool classification for REA, keyed by
/// `(server_name, tool_name)` from the MCP handshake.
///
/// Delegates to the generated 6.5.0 inventory in [`crate::rea_audit`],
/// which embeds the full REA tool effect audit. Static analysis tools map to
/// [`Capability::FilesystemRead`] only when they neither launch a process nor
/// access the network; those external effects, mutations, and execution map to
/// [`Capability::BinaryTrace`] (parks for approval). Tools absent from the map
/// fall back to [`rea_server_capability`].
pub fn rea_tool_capabilities() -> HashMap<(String, String), Capability> {
    crate::rea_audit::rea_tool_capabilities()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_tools_are_filesystem_read() {
        let m = rea_tool_capabilities();
        for tool in ["goto_address", "list_unknowns"] {
            assert_eq!(
                m.get(&(REA_SERVER_NAME.to_string(), tool.to_string())),
                Some(&Capability::FilesystemRead),
                "tool {tool}"
            );
        }
    }

    #[test]
    fn tracing_tools_are_binary_trace() {
        let m = rea_tool_capabilities();
        for tool in [
            "capture_process_scenario",
            "gdb_console",
            "instrument_with_frida",
        ] {
            assert_eq!(
                m.get(&(REA_SERVER_NAME.to_string(), tool.to_string())),
                Some(&Capability::BinaryTrace),
                "tool {tool}"
            );
        }
    }

    #[test]
    fn server_fallback_is_restrictive() {
        // Unknown future REA tools must park, not inherit permissiveness.
        assert_eq!(rea_server_capability(), Capability::BinaryTrace);
    }

    #[test]
    fn unknown_tool_has_no_per_tool_entry() {
        let m = rea_tool_capabilities();
        assert!(!m.contains_key(&(REA_SERVER_NAME.to_string(), "some_future_tool".to_string())));
    }

    #[test]
    fn tuple_keys_cannot_cross_match() {
        // (server, tool) tuples never collide across servers: a tool named
        // "rea" on another server, or a server named "rea::x", cannot hit
        // REA's entries because no string splitting is involved.
        let m = rea_tool_capabilities();
        assert!(!m.contains_key(&("other".to_string(), "rea".to_string())));
        assert!(!m.contains_key(&("rea::x".to_string(), "goto_address".to_string())));
    }
}
