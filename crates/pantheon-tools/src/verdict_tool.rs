//! The reviewer-facing `verdict` tool: emit a structured review verdict.
//!
//! Registered ONLY on reviewer runs in staged review stages (see
//! `pantheon_runtime::swarm_exec`): the reviewer calls `verdict` once at
//! the end of its review, and the orchestrator extracts the verdict from
//! the call's structured args instead of parsing the reviewer's prose.
//! The tool itself is stateless — the run's ledger `ToolStarted` event
//! persists the args, which is what the orchestrator reads back after
//! the stage settles.

use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::{Capability, VERDICT_TOOL_NAME};
use pantheon_api::error::Layer;
use pantheon_api::message::ToolSchema;

/// Register the `verdict` tool on a registry.
pub fn register_verdict_tool(reg: &mut ToolRegistry) {
    reg.register(
        ToolSchema {
            name: VERDICT_TOOL_NAME.into(),
            description: "Emit your review verdict. Call exactly once at the end of your review: pass=true when the reviewed stage output satisfies every contract, pass=false with every failed item listed otherwise.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pass": {
                        "type": "boolean",
                        "description": "Whether the reviewed stage output satisfies every contract."
                    },
                    "failed_items": {
                        "type": "array",
                        "description": "Every failed item; empty when pass is true.",
                        "items": { "type": "string" }
                    },
                    "notes": {
                        "type": "string",
                        "description": "Optional notes for the lead."
                    }
                },
                "required": ["pass", "failed_items"]
            }),
        },
        Capability::Other(VERDICT_TOOL_NAME.into()),
        move |args| {
            let v = parse_args(args)?;
            let pass = v.get("pass").and_then(|p| p.as_bool()).ok_or_else(|| {
                crate::tools::tool_err(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "missing boolean 'pass'".into(),
                    "call verdict with pass, failed_items, and notes",
                )
            })?;
            let failed_items = v
                .get("failed_items")
                .and_then(|a| a.as_array())
                .ok_or_else(|| {
                    crate::tools::tool_err(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        "missing 'failed_items' array".into(),
                        "call verdict with pass, failed_items, and notes",
                    )
                })?;
            for (i, item) in failed_items.iter().enumerate() {
                if item.as_str().is_none() {
                    return Err(crate::tools::tool_err(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        format!("failed_items[{i}] is not a string"),
                        "call verdict with pass, failed_items, and notes",
                    ));
                }
            }
            // `notes` is optional; the orchestrator only reads pass/failed_items.
            let _ = v.get("notes").and_then(|n| n.as_str()).unwrap_or("");
            let _ = pass;
            Ok("verdict recorded".to_string())
        },
    );
}
