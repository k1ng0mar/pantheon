//! Desktop computer use through the CUA driver.
//!
//! When the ComputerUse tool group is enabled, the runtime treats the CUA
//! driver (`cua-driver`, trycua/cua) as an MCP server: `cua-driver mcp`
//! speaks the Model Context Protocol over stdio, and the existing
//! [`McpManager`](pantheon_mcp::manager::McpManager) machinery — tool
//! projection, content-hash approval pinning, reconnect handling —
//! carries it. MCP is the transport detail here; the driver answers to
//! the ComputerUse toggle, not the Plugins toggle, and the projected
//! tools carry the inward `Capability::ComputerUse` (desktop control),
//! not the outward plugin treatment.
//!
//! Desktop control parks for human approval under the default policy:
//! `Capability::ComputerUse` is Approval in `Policy::coder()`, and the
//! driver's MCP server itself must be operator-approved before its tools
//! register at all.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use pantheon_mcp::manager::{McpServerSpec, McpTransport};

/// MCP server name under which the CUA driver registers. The projected
/// tool names are `mcp_cua-driver_<tool>` (e.g. `mcp_cua-driver_click`).
pub const CUA_DRIVER_SERVER: &str = "cua-driver";

/// Driver ids the runtime knows. Today there is exactly one.
pub const CUA_DRIVER_ID: &str = "cua-driver";

/// Locate the `cua-driver` binary on PATH. `None` = not installed.
pub fn find_cua_driver() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let candidate = dir.join("cua-driver");
            if candidate.is_file() {
                Some(candidate)
            } else {
                None
            }
        })
    })
}

/// Build the MCP server spec for the CUA driver. `binary` pins an
/// explicit path (from `[computer_use].binary`); otherwise the driver is
/// resolved from PATH. Returns `None` when the driver id is unknown or
/// no binary could be resolved.
pub fn cua_driver_spec(driver: Option<&str>, binary: Option<&str>) -> Option<McpServerSpec> {
    if let Some(d) = driver {
        if !d.eq_ignore_ascii_case(CUA_DRIVER_ID) {
            log_warn!("computer-use: unknown driver '{d}' (only 'cua-driver' is supported)");
            return None;
        }
    }
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
        None => find_cua_driver()?,
    };
    Some(McpServerSpec {
        name: CUA_DRIVER_SERVER.to_string(),
        transport: McpTransport::Stdio,
        command: Some(command.to_string_lossy().into_owned()),
        args: vec!["mcp".to_string()],
        env: HashMap::new(),
        url: None,
        enabled: true,
        timeout: Duration::from_secs(60),
    })
}
