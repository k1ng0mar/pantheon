//! The agent-facing `todo` tool: replace the whole session todo list in
//! one call (opencode/Claude-Code style).
//!
//! The tool owns the shared in-memory list (the session, `/todos`, and
//! the TUI card read the same state). Durability crosses into the run's
//! ledger through [`TodoToolSink`], which the runtime implements — the
//! tool crate never touches storage directly, mirroring `memory_tools`.

use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::Layer;
use pantheon_api::message::ToolSchema;
use pantheon_api::todo::{TodoItem, TodoList, TodoStatus, TODO_TOOL_NAME};
use std::sync::{Arc, Mutex};

/// Guidance appended to the system prompt so the model actually plans
/// with todos on multi-step work. Kept next to the tool so the prose
/// and the behavior cannot drift apart.
pub const TODO_SYSTEM_GUIDANCE: &str = "\
Todo planning: you have a `todo` tool that holds your task list for this \
session. Use it on any multi-step task (roughly: anything with 3+ steps, \
or work you might get interrupted doing):\n\
- Call `todo` FIRST with the full plan, every step `pending`.\n\
- Mark exactly one item `in_progress` while you work it (with `activeForm` \
in present tense, e.g. \"Reading files\"); mark it `completed` the moment \
it is done, before moving on.\n\
- Replacing the list is the update mechanism: pass the complete new list \
every time the plan changes. At most one item may be `in_progress`.\n\
- Skip the tool for single-step questions and pure conversation.";

/// What happened when the `todo` tool ran; forwarded to the sink so the
/// runtime can persist the snapshot and project it into the ledger.
#[derive(Debug, Clone)]
pub enum TodoToolEvent {
    /// The list was replaced; carries the new items.
    Updated { items: Vec<TodoItem> },
    /// The replacement was rejected (bad args or failed validation).
    Denied { code: String, cause: String },
}

/// Sink for [`TodoToolEvent`]. Implemented by the runtime against the
/// run's ledger; the tool layer only knows this trait.
pub trait TodoToolSink: Send + Sync {
    fn record(&self, event: TodoToolEvent);
}

/// Options for [`register_todo_tool`]. `state` is the session's shared
/// list; `sink` carries each accepted replacement to durable storage.
pub struct TodoToolOptions {
    pub state: Arc<Mutex<TodoList>>,
    pub sink: Arc<dyn TodoToolSink>,
}

/// Register the `todo` tool on a registry.
pub fn register_todo_tool(reg: &mut ToolRegistry, opts: TodoToolOptions) {
    let state = opts.state;
    let sink = opts.sink;
    reg.register(
        ToolSchema {
            name: TODO_TOOL_NAME.into(),
            description: "Replace the whole session todo list. Plan multi-step work here: write the steps first (all pending), mark exactly one in_progress while you work it, mark completed as you finish. Pass the complete new list every time the plan changes.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The complete new todo list (replaces the old one). An empty array clears the list.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "The task, imperative phrasing." },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                "activeForm": { "type": "string", "description": "Present-tense phrasing shown while in progress, e.g. \"Reading files\"." }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        },
        Capability::Other(TODO_TOOL_NAME.into()),
        move |args| {
            let v = parse_args(args)?;
            let arr = v
                .get("todos")
                .and_then(|t| t.as_array())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'todos' array".into(), "pass the complete new todo list"))?;
            let mut items = Vec::with_capacity(arr.len());
            for (i, raw) in arr.iter().enumerate() {
                let content = raw
                    .get("content")
                    .and_then(|c| c.as_str())
                    .ok_or_else(|| {
                        crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, format!("todos[{i}]: missing string 'content'"), "pass the complete new todo list")
                    })?;
                let status_raw = raw
                    .get("status")
                    .and_then(|s| s.as_str())
                    .ok_or_else(|| {
                        crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, format!("todos[{i}]: missing string 'status'"), "pass the complete new todo list")
                    })?;
                let status = TodoStatus::parse(status_raw).ok_or_else(|| {
                    crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, format!(
                            "todos[{i}]: unknown status {status_raw:?} (want pending|in_progress|completed)"
                        ), "pass the complete new todo list")
                })?;
                let active_form = raw
                    .get("activeForm")
                    .and_then(|a| a.as_str())
                    .map(|s| s.to_string());
                items.push(TodoItem {
                    content: content.to_string(),
                    status,
                    active_form,
                });
            }
            let mut list = TodoList::default();
            if let Err(e) = list.replace(items) {
                let cause = e.to_string();
                sink.record(TodoToolEvent::Denied {
                    code: "TOOL_TODO_STATE".into(),
                    cause: cause.clone(),
                });
                return Err(crate::tools::tool_err("TOOL_TODO_STATE", Layer::Execution, false, cause, "pass the complete new todo list"));
            }
            // Commit to the shared in-memory list, then hand durability
            // (ledger snapshot + transcript event) to the sink.
            {
                let mut g = state
                    .lock()
                    .map_err(|e| crate::tools::tool_err("TOOL_STATE", Layer::Execution, false, format!("todo state lock failed: {e}"), "pass the complete new todo list"))?;
                *g = list.clone();
            }
            sink.record(TodoToolEvent::Updated {
                items: list.items.clone(),
            });
            Ok(summarize(&list))
        },
    );
}

/// Compact confirmation the model sees as the tool result.
fn summarize(list: &TodoList) -> String {
    if list.is_empty() {
        return "todo list cleared".to_string();
    }
    let mut pending = 0;
    let mut done = 0;
    let mut active = 0;
    for i in &list.items {
        match i.status {
            TodoStatus::Pending => pending += 1,
            TodoStatus::InProgress => active += 1,
            TodoStatus::Completed => done += 1,
        }
    }
    let mut out = format!(
        "todo list updated: {} items ({} in progress, {} completed, {} pending)",
        list.len(),
        active,
        done,
        pending
    );
    if let Some(cur) = list.in_progress() {
        let label = cur.active_form.as_deref().unwrap_or(cur.content.as_str());
        out.push_str(&format!("\nnow: {label}"));
    }
    out
}
