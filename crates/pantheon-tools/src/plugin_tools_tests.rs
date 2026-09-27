//! Tests for `pantheon_tools::plugin_tools` — the registration half that
//! split out of `pantheon-exec::supervisor_tests` (capability ≠ tool).
use crate::plugin_tools::{register_plugin_tools, validate_plugin_tool_name};
use crate::tools::ToolRegistry;
use pantheon_api::capability::Capability;
use pantheon_exec::plugins::{PluginManifest, ToolCapability};
use pantheon_exec::supervisor::PluginSupervisor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Registry wiring: plugin tools execute through the shared supervisor
/// and respect direct-execute gating.

fn err_code(err: &pantheon_api::error::PantheonError) -> &str {
    err.code.as_str()
}

#[test]
fn plugin_tool_name_validation() {
    assert!(validate_plugin_tool_name("shell").is_err());
    assert!(validate_plugin_tool_name("read_file").is_err());
    assert!(validate_plugin_tool_name("my_plugin_tool").is_ok());
    assert!(validate_plugin_tool_name("greet").is_ok());
}

/// Built-in collision is case-insensitive: `Memory_Recall` must fail too.

#[test]
fn plugin_tool_name_case_insensitive_collision() {
    let err = validate_plugin_tool_name("Memory_Recall").unwrap_err();
    assert_eq!(err_code(&err), "PLUGIN_TOOL_NAME_CONFLICT");
    assert!(
        err.to_string().contains("memory_recall"),
        "error should name the collided builtin, got: {err}"
    );
}

/// Reserved namespaces `pantheon.`, `builtin.`, `memory.` are off-limits.

#[test]
fn plugin_tool_name_reserved_prefixes() {
    for name in [
        "pantheon.admin",
        "builtin.shell",
        "memory.recall",
        "Pantheon.Debug", // case-insensitive
    ] {
        let err = validate_plugin_tool_name(name).unwrap_err();
        assert_eq!(
            err_code(&err),
            "PLUGIN_TOOL_NAME_CONFLICT",
            "name '{name}' should be rejected"
        );
    }
}

/// Malformed names — empty, whitespace, path separators — are rejected.

#[test]
fn plugin_tool_name_bad_shapes() {
    for name in ["", "my tool", "a/b", "a\\b", "tab\tname"] {
        let err = validate_plugin_tool_name(name).unwrap_err();
        assert_eq!(
            err_code(&err),
            "PLUGIN_TOOL_NAME_INVALID",
            "name '{name}' should be invalid"
        );
    }
}
