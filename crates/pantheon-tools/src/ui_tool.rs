//! The agent-facing `render_ui` tool: publish structured UI content.
//!
//! This tool is host-mediated like `ask_user`: it never executes in the
//! registry. The agent loop intercepts the call before gating, validates
//! the document against the UI protocol, and emits `Event::UiContent` (or
//! `Event::UiUpdated` for revisions). The handler below only validates, so
//! direct `execute("render_ui", …)` callers (tests, CLIs) get the same
//! validation the turn path applies.
//!
//! Registered on every agent session: rendering is not a privileged
//! operation (it is morally a fancier assistant message), so the tool
//! itself is policy-Allow. What the components DO (form submits, tool
//! invocations, approvals) goes through the normal capability gate at
//! action time - a component can never grant itself permissions.

use crate::tools::{parse_args, tool_err, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::Layer;
use pantheon_api::message::ToolSchema;
use pantheon_api::ui::{UiDocument, RENDER_UI_TOOL_NAME, UI_PROTOCOL_VERSION};

/// Register the `render_ui` tool on a registry.
pub fn register_ui_tool(reg: &mut ToolRegistry) {
    reg.register(
        ToolSchema {
            name: RENDER_UI_TOOL_NAME.into(),
            description: format!(
                "Render structured UI for the operator (protocol v{UI_PROTOCOL_VERSION}). \
                Use when a richer presentation materially improves the response: tables, \
                charts, validated forms, task lists, progress, diffs, approval cards. \
                Ordinary text stays Markdown - do NOT wrap every answer in UI. \
                Arguments: {{\"document\": {{<component>}}, \"ui_id\": \"optional id of a published document to revise\"}}. \
                A document is {{\"version\": 1, \"id\": \"<slug>\", \"root\": {{\"id\": \"<slug>\", \"type\": \"<table|chart|form|...>\", ...}}}}. \
                Component types: markdown, table, chart (line|bar|pie), metrics, list, code, refs, \
                form (validated fields + submit action), task_list, progress, details, diff, approval, \
                layout (stack|columns|tabs), custom (with fallback). \
                Interactive actions declare an action_id + label; the runtime executes them through \
                the normal permission gate. Publishing an \"approval\" card parks your turn until \
                the operator approves or denies."
            ),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "document": {
                        "type": "object",
                        "description": "The UiDocument to publish (version, id, root component tree)."
                    },
                    "ui_id": {
                        "type": "string",
                        "description": "Id of a previously published document to revise (emits UiUpdated instead of UiContent)."
                    }
                },
                "required": ["document"]
            }),
        },
        Capability::Other(RENDER_UI_TOOL_NAME.into()),
        move |args| {
            let v = parse_args(args)?;
            let doc_value = v.get("document").ok_or_else(|| {
                tool_err(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "missing 'document' object".into(),
                    "call render_ui with a document payload",
                )
            })?;
            // Lenient parse: unknown component tags degrade to a fallback
            // note rather than failing the turn.
            let (doc, unknown) = UiDocument::from_value_lenient(doc_value).map_err(|e| {
                tool_err(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    format!("invalid UI document: {e}"),
                    "fix the document and call render_ui again",
                )
            })?;
            if let Err(e) = doc.validate() {
                return Err(tool_err(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    format!("invalid UI document: {e}"),
                    "fix the document and call render_ui again",
                ));
            }
            // ui_id, when given, must be slug-shaped; the runtime decides
            // whether it names a live document (revision) or is rejected.
            if let Some(ui_id) = v.get("ui_id").and_then(|u| u.as_str()) {
                if !pantheon_api::ident::is_slug(ui_id) {
                    return Err(tool_err(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        format!("ui_id {ui_id:?} is not slug-shaped"),
                        "use the ui_id from a previous render_ui result",
                    ));
                }
            }
            let mut out = String::from("ui validated");
            if !unknown.is_empty() {
                out.push_str(&format!(" (unknown components degraded: {})", unknown.join(", ")));
            }
            Ok(out)
        },
    );
}
