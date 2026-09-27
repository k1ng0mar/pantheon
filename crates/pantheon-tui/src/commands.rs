//! The one command registry.
//!
//! Placeholder for L0 step 4. Every slash command in the product resolves
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
    matches!(name, "model" | "resume" | "name" | "memory" | "search")
}

/// Every command, grouped. `/cost` is deliberately absent: the header shows
/// tokens and elapsed, and a cost figure is not worth a command.
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
            desc: "browse conversations",
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
            name: "runs",
            desc: "recent runs and their status",
            category: "session",
        },
        CommandMeta {
            name: "memory",
            desc: "search agent memory",
            category: "agent",
        },
        CommandMeta {
            name: "remember",
            desc: "store a memory record",
            category: "agent",
        },
        CommandMeta {
            name: "tools",
            desc: "manage available tools",
            category: "agent",
        },
        CommandMeta {
            name: "skills",
            desc: "browse skills",
            category: "agent",
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
