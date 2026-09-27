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

/// Built-in tool names a plugin may never claim (compared case-insensitively).
/// Enumerated from the tool modules in this crate: `builtins` (shell,
/// read_file, write_file, list_dir), `memory_tools` (memory_*),
/// `session_search_tools`, `skill_tools`, `vault_tools`, `safewrite_tools`.
/// Kept in sync manually with those modules; `register_plugin_tools` also
/// checks the live registry as belt-and-braces against tools registered
/// from other crates.
const BUILTIN_TOOL_NAMES: &[&str] = &[
    "shell",
    "read_file",
    "write_file",
    "list_dir",
    "memory_recall",
    "memory_list",
    "memory_propose",
    "memory_forget",
    "memory_confirm",
    "session_search",
    "skills_list",
    "skill_read",
    "vault_archive",
    "vault_read",
    "vault_search",
    "vault_list",
    "preview_file",
    "stage_files",
    "apply_files",
    "apply_staged",
    "checkpoint_files",
    "list_checkpoints",
    "rollback_checkpoint",
    "rollback_seq",
];

/// Namespace prefixes owned by the runtime (compared case-insensitively).
/// Plugins declaring under these can impersonate first-party tooling.
const RESERVED_TOOL_PREFIXES: &[&str] = &["pantheon.", "builtin.", "memory."];

fn name_err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Extension,
        false,
        cause,
        "rename the plugin tool or remove it from the plugin manifest",
        "",
    )
}

/// Validate a plugin tool name before registration. Rejects:
/// - empty names
/// - names containing whitespace or path separators (`/`, `\`)
/// - names that collide (case-insensitively) with a built-in tool —
///   a plugin must not be able to shadow (squat) `shell` and friends
/// - names under the reserved prefixes `pantheon.`, `builtin.`, `memory.`
///
/// Plugin capabilities are self-declared, so this runs at registration
/// time, before the name ever reaches the registry's `HashMap::insert`
/// (which would otherwise silently replace the builtin).
pub fn validate_plugin_tool_name(name: &str) -> Result<(), PantheonError> {
    if name.is_empty() {
        return Err(name_err(
            "PLUGIN_TOOL_NAME_INVALID",
            "plugin tool name must not be empty".to_string(),
        ));
    }
    if name.chars().any(|c| c.is_whitespace()) {
        return Err(name_err(
            "PLUGIN_TOOL_NAME_INVALID",
            format!("plugin tool name '{name}' must not contain whitespace"),
        ));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(name_err(
            "PLUGIN_TOOL_NAME_INVALID",
            format!("plugin tool name '{name}' must not contain path separators"),
        ));
    }
    let lower = name.to_lowercase();
    if let Some(builtin) = BUILTIN_TOOL_NAMES.iter().find(|b| **b == lower.as_str()) {
        return Err(name_err(
            "PLUGIN_TOOL_NAME_CONFLICT",
            format!(
                "plugin tool name '{name}' collides with built-in tool '{builtin}'; \
                 plugins cannot shadow built-in tools"
            ),
        ));
    }
    if let Some(prefix) = RESERVED_TOOL_PREFIXES
        .iter()
        .find(|p| lower.starts_with(**p))
    {
        return Err(name_err(
            "PLUGIN_TOOL_NAME_CONFLICT",
            format!(
                "plugin tool name '{name}' uses reserved prefix '{prefix}'; \
                 the '{prefix}' namespace belongs to the runtime"
            ),
        ));
    }
    Ok(())
}
/// Register every tool from a plugin manifest into the registry. Each closure
/// locks the shared supervisor, forwards the call, and returns the plugin's
/// (compacted) result string.
///
/// Names are validated up front (see `validate_plugin_tool_name`): the first
/// invalid name aborts registration and nothing from the manifest is
/// registered, so a squatting plugin cannot leave a half-registered set
/// behind. As belt-and-braces, each name is also checked against the live
/// registry so a plugin cannot replace a tool that is already present,
/// including built-ins registered from other crates.
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
) -> Result<(), PantheonError> {
    for cap in &manifest.capabilities {
        validate_plugin_tool_name(&cap.name)?;
        // Defense in depth: never replace a tool that is already
        // registered (a builtin, or another plugin's tool), even if it is
        // not in BUILTIN_TOOL_NAMES.
        let lower = cap.name.to_lowercase();
        if let Some(existing) = reg.names().into_iter().find(|n| n.to_lowercase() == lower) {
            return Err(name_err(
                "PLUGIN_TOOL_NAME_CONFLICT",
                format!(
                    "plugin tool name '{}' collides with already-registered tool '{existing}'; \
                     plugins cannot replace existing tools",
                    cap.name
                ),
            ));
        }
    }
    for cap in &manifest.capabilities {
        register_one_plugin_tool(reg, cap.clone(), sup.clone());
    }
    Ok(())
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
