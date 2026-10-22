//! Desktop computer use through an MCP driver.
//!
//! When the ComputerUse tool group is enabled, the runtime treats a
//! desktop-control driver as an MCP server: it speaks the Model Context
//! Protocol over stdio, and the existing
//! [`McpManager`](pantheon_mcp::manager::McpManager) machinery - tool
//! projection, content-hash approval pinning, reconnect handling -
//! carries it. MCP is the transport detail; the driver answers to the
//! ComputerUse toggle, not the Plugins toggle, and the projected tools
//! carry the inward `Capability::ComputerUse` (desktop control), not the
//! outward plugin treatment.
//!
//! Desktop control parks for human approval under the default policy:
//! `Capability::ComputerUse` is Approval in `Policy::coder()`, and the
//! driver's MCP server itself must be operator-approved before its tools
//! register at all.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use pantheon_mcp::manager::{McpAuth, McpServerSpec, McpTransport};

/// MCP server name under which the CUA driver registers. Projected tool
/// names are `mcp_cua-driver_<tool>` (e.g. `mcp_cua-driver_click`).
pub const CUA_DRIVER_SERVER: &str = "cua-driver";

/// Driver ids the runtime knows.
pub const CUA_DRIVER_ID: &str = "cua-driver";
pub const TAPP_DRIVER_ID: &str = "tapp";

/// Locate a binary named `name` on PATH. `None` = not installed.
fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join(name);
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

/// Locate the `cua-driver` binary on PATH. `None` = not installed.
pub fn find_cua_driver() -> Option<PathBuf> {
    find_on_path(CUA_DRIVER_ID)
}

/// Locate the `tapp-mcp` binary on PATH. `None` = not installed.
pub fn find_tapp_mcp() -> Option<PathBuf> {
    find_on_path("tapp-mcp")
}

/// Build the MCP server spec for the selected desktop driver.
///
/// `driver` names the id (`cua-driver` or `tapp`); `None` defaults to
/// `cua-driver`. `binary` pins an explicit path (from
/// `[computer_use].binary`); otherwise the driver's binary is resolved
/// from PATH. Returns `None` when the driver id is unknown or no binary
/// could be resolved.
pub fn computer_driver_spec(driver: Option<&str>, binary: Option<&str>) -> Option<McpServerSpec> {
    let id = driver.unwrap_or(CUA_DRIVER_ID);

    // Per-driver argv (the binary, plus its subcommand where it has one)
    // and the MCP server name the tools project under.
    let (binary_name, argv, server): (&str, Vec<&str>, &str) =
        if id.eq_ignore_ascii_case(CUA_DRIVER_ID) {
            (CUA_DRIVER_ID, vec!["mcp"], CUA_DRIVER_SERVER)
        } else if id.eq_ignore_ascii_case(TAPP_DRIVER_ID) {
            // tapp-mcp speaks MCP over stdio directly, no subcommand.
            ("tapp-mcp", Vec::new(), TAPP_DRIVER_ID)
        } else {
            log_warn!("computer-use: unknown driver '{id}' (supported: 'cua-driver', 'tapp')");
            return None;
        };

    let command = match binary {
        Some(b) => {
            let p = PathBuf::from(b);
            if !p.is_file() {
                log_warn!(
                    "computer-use: configured binary '{}' not found",
                    p.display()
                );
                return None;
            }
            p
        }
        None => find_on_path(binary_name)?,
    };

    Some(McpServerSpec {
        name: server.to_string(),
        transport: McpTransport::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: argv.into_iter().map(String::from).collect(),
        env: HashMap::new(),
        url: None,
        enabled: true,
        timeout: Duration::from_secs(60),
        headers: HashMap::new(),
        auth: McpAuth::None,
        scopes: Vec::new(),
        client_id: None,
    })
}

/// Backward-compatible alias: the CUA driver is the default, so the old
/// call shape still works unchanged.
pub fn cua_driver_spec(driver: Option<&str>, binary: Option<&str>) -> Option<McpServerSpec> {
    computer_driver_spec(driver, binary)
}
