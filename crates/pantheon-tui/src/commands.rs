//! The one command registry.
//!
//! Every slash command in the product resolves
//! through this table; there is no per-surface dispatch list.

use crate::widget::Key;
use std::collections::BTreeMap;

/// A command's identity and metadata, without its handler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandMeta {
    pub name: &'static str,
    pub desc: &'static str,
    pub category: &'static str,
}

impl CommandMeta {
    /// The slash form, e.g. `/models`.
    pub fn slash(&self) -> String {
        format!("/{}", self.name)
    }
}

/// Commands that take an argument and can complete it.
pub fn arg_completes(name: &str) -> bool {
    matches!(name, "model" | "resume" | "name")
}

/// Every command the session actually dispatches, grouped. Absent on
/// purpose: /cost (the header carries tokens and elapsed), /memory and
/// /tools (removed: /remember and /skills cover them), /debug,
/// /provenance and /events (removed: runs/audit/logs cover them).
pub fn registry() -> BTreeMap<&'static str, CommandMeta> {
    let mut m = BTreeMap::new();
    for c in [
        CommandMeta {
            name: "models",
            desc: "browse and switch model",
            category: "model",
        },
        CommandMeta {
            name: "model",
            desc: "switch the active model",
            category: "model",
        },
        CommandMeta {
            name: "reasoning",
            desc: "reasoning effort for this session",
            category: "model",
        },
        CommandMeta {
            name: "sessions",
            desc: "live sessions elsewhere",
            category: "session",
        },
        CommandMeta {
            name: "resume",
            desc: "resume a conversation",
            category: "session",
        },
        CommandMeta {
            name: "new",
            desc: "start a fresh conversation",
            category: "session",
        },
        CommandMeta {
            name: "compress",
            desc: "compress this conversation's context now",
            category: "session",
        },
        CommandMeta {
            name: "export",
            desc: "save this conversation to a file",
            category: "session",
        },
        CommandMeta {
            name: "runs",
            desc: "recent runs and their status",
            category: "session",
        },
        CommandMeta {
            name: "remember",
            desc: "store a memory record",
            category: "agent",
        },
        CommandMeta {
            name: "skills",
            desc: "browse skills",
            category: "agent",
        },
        CommandMeta {
            name: "agent",
            desc: "current agent profile, or switch",
            category: "agent",
        },
        CommandMeta {
            name: "agents",
            desc: "declared agent profiles",
            category: "agent",
        },
        CommandMeta {
            name: "collab",
            desc: "active collaborations and their tasks",
            category: "agent",
        },
        CommandMeta {
            name: "tasks",
            desc: "that agent's open tasks",
            category: "agent",
        },
        CommandMeta {
            name: "inbox",
            desc: "messages sent to this agent",
            category: "agent",
        },
        CommandMeta {
            name: "history",
            desc: "searchable run history",
            category: "session",
        },
        CommandMeta {
            name: "name",
            desc: "show or rename this conversation",
            category: "session",
        },
        CommandMeta {
            name: "approvals",
            desc: "pending approvals for this run",
            category: "runtime",
        },
        CommandMeta {
            name: "approve",
            desc: "approve a pending approval by index",
            category: "runtime",
        },
        CommandMeta {
            name: "deny",
            desc: "deny a pending approval by index",
            category: "runtime",
        },
        CommandMeta {
            name: "schedule",
            desc: "scheduled jobs",
            category: "runtime",
        },
        CommandMeta {
            name: "mcp",
            desc: "MCP server declarations",
            category: "runtime",
        },
        CommandMeta {
            name: "migrate",
            desc: "import from other harnesses (hermes, openclaw, omp, claude)",
            category: "runtime",
        },
        CommandMeta {
            name: "migrate",
            desc: "import from other harnesses (hermes, openclaw, omp, claude)",
            category: "runtime",
        },
        CommandMeta {
            name: "migrate",
            desc: "import from other harnesses (hermes, openclaw, omp, claude)",
            category: "runtime",
        },
        CommandMeta {
            name: "env",
            desc: "secret names and status (never values)",
            category: "runtime",
        },
        CommandMeta {
            name: "settings",
            desc: "configure pantheon",
            category: "runtime",
        },
        CommandMeta {
            name: "gateway",
            desc: "manage messaging gateways",
            category: "runtime",
        },
        CommandMeta {
            name: "doctor",
            desc: "diagnose this install",
            category: "runtime",
        },
        CommandMeta {
            name: "status",
            desc: "this run's status",
            category: "runtime",
        },
        CommandMeta {
            name: "help",
            desc: "list commands",
            category: "system",
        },
        CommandMeta {
            name: "clear",
            desc: "clear the visible transcript",
            category: "system",
        },
        CommandMeta {
            name: "quit",
            desc: "leave pantheon",
            category: "system",
        },
        CommandMeta {
            name: "exit",
            desc: "leave pantheon",
            category: "system",
        },
    ] {
        m.insert(c.name, c);
    }
    m
}

/// Commands matching a filter, as `/name` strings. The palette's completion
/// source; identical filtering rules to the widgets, so a user typing `/mo`
/// sees the same two rows whether they are in the composer or the palette.
pub fn complete(prefix: &str) -> Vec<String> {
    let p = prefix.trim_start_matches('/').to_lowercase();
    registry()
        .into_iter()
        .filter(|(name, _)| name.starts_with(&p))
        .map(|(_, c)| c.slash())
        .collect()
}

/// A key the palette should react to while the composer is empty.
pub fn is_palette_key(k: Key) -> bool {
    matches!(k, Key::CtrlK)
}

#[cfg(test)]
#[path = "commands_tests.rs"]
mod tests;
