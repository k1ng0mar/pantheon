//! Plugin tools: manifest tools wired to a shared `PluginSupervisor`.
//!
//! Moved out of `pantheon-exec::supervisor` in the tool-layer split
//! (capability ≠ tool): process supervision and plugin hosting stay in
//! `pantheon-exec`; registering the manifest's tools on a registry is a
//! tool-layer concern.

use crate::tools::ToolRegistry;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::plugins::{PluginManifest, ToolCapability};
use pantheon_exec::supervisor::PluginSupervisor;
use std::sync::{Arc, Mutex};
/// Register every tool from a plugin manifest into the registry. Each closure
/// locks the shared supervisor, forwards the call, and returns the plugin's
/// (compacted) result string.
///
/// The manifest's `ToolCapability` entries declare the tool name, the
/// `Capability` the session must grant, and the JSON schema shown to the
/// model. That declared capability is what the registry hands the agent
/// loop's gate, so a plugin tool is gated exactly like a builtin.
///
/// The closure itself does not re-check: it has no `Policy` to check
/// against, and a second check would either duplicate the loop's decision
/// or silently disagree with it. Callers outside the loop go through
/// `ToolRegistry::execute_gated`, which is the single place the gate lives.
pub fn register_plugin_tools(
    reg: &mut ToolRegistry,
    manifest: &PluginManifest,
    sup: Arc<Mutex<PluginSupervisor>>,
) {
    for cap in &manifest.capabilities {
        register_one_plugin_tool(reg, cap.clone(), sup.clone());
    }
}

fn register_one_plugin_tool(
    reg: &mut ToolRegistry,
    cap: ToolCapability,
    sup: Arc<Mutex<PluginSupervisor>>,
) {
    reg.register(
        ToolSchema {
            name: cap.name.clone(),
            description: if cap.description.is_empty() {
                format!(
                    "Plugin tool '{}' (capability: {:?})",
                    cap.name, cap.capability
                )
            } else {
                cap.description.clone()
            },
            parameters: cap.parameters.clone(),
        },
        cap.capability.clone(),
        move |args| {
            let v: serde_json::Value = if args.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(args).map_err(|e| {
                    PantheonError::new(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        format!("invalid JSON args: {e}"),
                        "check tool name and arguments",
                        "",
                    )
                })?
            };
            let mut guard = sup.lock().map_err(|_| {
                PantheonError::new(
                    "PLUGIN_LOCK",
                    Layer::Execution,
                    false,
                    "plugin supervisor lock poisoned".to_string(),
                    "respawn the plugin",
                    "",
                )
            })?;
            guard.call(&cap.name, v)
        },
    );
}
