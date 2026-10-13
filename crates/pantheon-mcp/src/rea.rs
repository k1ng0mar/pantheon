//! REA ("Reverse Engineer Anything", morluto/rea) tool classification.
//!
//! REA is a third-party MCP server that exposes binary reverse-engineering
//! tools to agents. Its tool catalogue spans two risk tiers:
//!
//! * **Static analysis** (read-only): decompile, inspect, and compare
//!   binaries without executing them. These tools read the target file and
//!   return analysis — no new privilege beyond what the agent already has
//!   via [`Capability::FilesystemRead`].
//! * **Dynamic tracing** (executes the target): trace execution, capture
//!   scenarios, observe runtime behavior. The analyzed program *runs*,
//!   potentially with the user's privileges in an unsandboxed MCP server
//!   process. These carry [`Capability::BinaryTrace`], which default
//!   policies mark Approval so the run parks for a human.
//!
//! Classification is an explicit allowlist audited against REA v6.4.0.
//! REA exposes no `readOnlyHint`/`destructiveHint` annotations, so the
//! catalogue is the source of truth. Anything not listed here —
//! including tools REA adds in future versions — falls back to the
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
/// Static analysis tools map to [`Capability::FilesystemRead`]
/// (read-only; no new privilege). Dynamic tracing and scenario-capture
/// tools map to [`Capability::BinaryTrace`] (parks for approval).
/// Tools absent from this map fall back to [`rea_server_capability`].
pub fn rea_tool_capabilities() -> HashMap<(String, String), Capability> {
    let mut m = HashMap::new();
    // Static analysis: read-only. Audited against REA v6.4.0's tool
    // catalogue (src/server/register*.ts).
    for tool in [
        // Core binary analysis (registerEnhancedTools.ts)
        "binary_overview",
        "analyze_function",
        "batch_decompile",
        "get_call_graph",
        "find_xrefs_to_name",
        "inspect_native_api",
        "inspect_native_dispatch_metadata",
        "get_objc_classes",
        "get_objc_protocols",
        "analyze_swift_types",
        // Android (registerAndroidTools.ts, registerApktoolTools.ts)
        "inspect_android_class",
        "inspect_android_method",
        "inspect_android_package",
        "search_android_classes",
        // Firmware (registerFirmwareTools.ts)
        "extract_firmware",
        "inspect_firmware_regions",
        // WASM / EVM / Go (registerWasmTools.ts, registerEvmTools.ts, registerGoTools.ts)
        "inspect_wasm_artifact",
        "inspect_evm_interface",
        "inspect_go_binary",
        // Web / JS bundles (registerBrowserTools.ts, registerElectronTools.ts)
        "analyze_web_bundle",
        "inspect_web_page",
        "analyze_javascript_application",
        "inspect_electron_page",
        "list_electron_targets",
        "list_browser_targets",
        "get_navigation_context",
        // .NET (registerManagedTools.ts)
        // Analysis views and comparisons (read-only derivations)
        "inspect_analysis_view",
        "inspect_binary_layout",
        "compare_artifacts",
        "compare_bundles",
        "compare_functions",
        "compare_process_captures",
        "compare_web_captures",
        "compare_web_screenshots",
        "extract_artifact",
    ] {
        m.insert(
            (REA_SERVER_NAME.to_string(), tool.to_string()),
            Capability::FilesystemRead,
        );
    }
    // Dynamic tracing and scenario capture: executes the target.
    // Explicitly listed for auditability; unlisted tools fall back to
    // the server-level BinaryTrace anyway.
    for tool in [
        "trace_feature",
        "trace_call_path",
        "trace_native_values",
        "trace_native_ui_action",
        "trace_android_references",
        "capture_process_scenario",
        "capture_electron_scenario",
        "capture_web_screenshot",
        "observe_web_session",
        "reconcile_javascript_runtime",
    ] {
        m.insert(
            (REA_SERVER_NAME.to_string(), tool.to_string()),
            Capability::BinaryTrace,
        );
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_tools_are_filesystem_read() {
        let m = rea_tool_capabilities();
        for tool in [
            "binary_overview",
            "analyze_function",
            "batch_decompile",
            "inspect_wasm_artifact",
            "extract_firmware",
        ] {
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
            "trace_feature",
            "trace_call_path",
            "capture_process_scenario",
            "observe_web_session",
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
        assert!(!m.contains_key(&("rea::x".to_string(), "binary_overview".to_string())));
    }
}
