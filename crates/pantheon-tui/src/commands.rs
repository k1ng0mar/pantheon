//! The command catalog: metadata for every slash command the session dispatches.
//!
//! This table does not dispatch anything. `handle_slash_inner` in
//! `session.rs` owns the dispatch chain, and the `/` palette reads from
//! `session::COMMANDS`. This table is the catalog behind two narrower
//! jobs: `is_builtin` (the dynamic `/<skill>` dispatch consults it so
//! built-in names always win over skill names) and `complete` (the
//! "did you mean" suggestion source). It must list exactly the commands
//! the session dispatches - a built-in missing here is invisible to
//! completion and to the built-in-vs-skill check.

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

/// True when `name` is a built-in slash command (without the leading
/// slash). The dynamic `/<skill>` dispatch consults this so built-ins
/// always win over skill names.
pub fn is_builtin(name: &str) -> bool {
    registry().contains_key(name)
}

/// Every command the dispatch chain in `handle_slash_inner` serves,
/// grouped - except the one it serves but keeps off the advertised
/// surface: /name (silent alias of /title).
///
/// Also absent on purpose: /cost (the header carries tokens and
/// elapsed), /memory (removed: /remember covers it), /debug,
/// /provenance and /events (removed: runs/audit/logs cover them).
/// /<skill> is not a built-in: it is the dynamic skill invocation, and
/// the dispatch chain handles it after every entry here.
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
            name: "rewind",
            desc: "roll back the last turn (confirm; ledger kept)",
            category: "session",
        },
        CommandMeta {
            name: "undo",
            desc: "undo the last response (same as /rewind)",
            category: "session",
        },
        CommandMeta {
            name: "checkpoint",
            desc: "save a named snapshot of the current turn",
            category: "session",
        },
        CommandMeta {
            name: "checkpoints",
            desc: "list saved checkpoints",
            category: "session",
        },
        CommandMeta {
            name: "restore",
            desc: "rewind back to a checkpoint (ledger kept)",
            category: "session",
        },
        CommandMeta {
            name: "fork",
            desc: "branch this conversation at a turn into a new run",
            category: "session",
        },
        CommandMeta {
            name: "yank",
            desc: "copy last answer (or its Nth code block) to clipboard",
            category: "session",
        },
        CommandMeta {
            name: "steer",
            desc: "redirect the running turn mid-flight (normal message when idle)",
            category: "session",
        },
        CommandMeta {
            name: "remember",
            desc: "store a memory record",
            category: "agent",
        },
        CommandMeta {
            name: "learn",
            desc: "save a behavioral lesson for future sessions",
            category: "agent",
        },
        CommandMeta {
            name: "skills",
            desc: "browse skills",
            category: "agent",
        },
        CommandMeta {
            name: "agent",
            desc: "current agent profile, or switch (/agent new <name> to create)",
            category: "agent",
        },
        CommandMeta {
            name: "agents",
            desc: "declared agent profiles (/agents create <name> to add one)",
            category: "agent",
        },
        CommandMeta {
            name: "soul",
            desc: "print the active profile's SOUL.md (/soul set <text> writes it)",
            category: "agent",
        },
        CommandMeta {
            name: "userfile",
            desc: "print the active profile's USER.md (/userfile set <text> writes it)",
            category: "agent",
        },
        CommandMeta {
            name: "agentsfile",
            desc: "print the active profile's AGENTS.md (/agentsfile set <text> writes it)",
            category: "agent",
        },
        CommandMeta {
            name: "collab",
            desc: "deprecated - multi-profile tasks moved to /swarm",
            category: "agent",
        },
        CommandMeta {
            name: "tasks",
            desc: "that agent's open tasks",
            category: "agent",
        },
        CommandMeta {
            name: "plugins",
            desc:
                "import a plugin by URL or clawhub slug; /plugins search <q> browses the registry",
            category: "agent",
        },
        CommandMeta {
            name: "plugin",
            desc: "alias of /plugins",
            category: "agent",
        },
        CommandMeta {
            name: "team",
            desc: "team of experts: list teams, /team <id> roster, /team <id> <task> launches",
            category: "agent",
        },
        CommandMeta {
            name: "swarm",
            desc: "remote swarms: list / new / inspect / retry (/swarm tree = delegation tree)",
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
            name: "title",
            desc: "show or rename this conversation",
            category: "session",
        },
        CommandMeta {
            name: "reset",
            desc: "reset turn state, keeping the session",
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
            name: "btw",
            desc: "run a task in the background (result lands here)",
            category: "runtime",
        },
        CommandMeta {
            name: "bg",
            desc: "list background tasks, or show one's output",
            category: "runtime",
        },
        CommandMeta {
            name: "reflect",
            desc: "run a reflection pass now, or toggle the self-improvement loop",
            category: "runtime",
        },
        CommandMeta {
            name: "consolidate",
            desc: "run a memory consolidation pass (dry run changes nothing)",
            category: "runtime",
        },
        CommandMeta {
            name: "mcp",
            desc: "MCP server declarations",
            category: "runtime",
        },
        CommandMeta {
            name: "tools",
            desc: "rebuild the tool registry in place",
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
            name: "voice",
            desc: "speech backends: [stt]/[tts] status",
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
            name: "send",
            desc: "push a message to a surface: /send <telegram|discord|mobile|home> <message>",
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
            name: "nightly",
            desc: "run the nightly maintenance pass now; on/off/status manage the loop",
            category: "runtime",
        },
        CommandMeta {
            name: "stats",
            desc: "today's usage from the ledger",
            category: "runtime",
        },
        CommandMeta {
            name: "goal",
            desc: "set or show the session goal (iteration-limited)",
            category: "agent",
        },
        CommandMeta {
            name: "todos",
            desc: "show the session todo list",
            category: "agent",
        },
        CommandMeta {
            name: "tokens",
            desc: "show or set the per-request output cap (default: uncapped)",
            category: "agent",
        },
        CommandMeta {
            name: "set",
            desc: "show or set session budget values",
            category: "agent",
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
            name: "theme",
            desc: "switch theme (pantheon, dark, light)",
            category: "system",
        },
        CommandMeta {
            name: "design",
            desc: "design systems: show the bound system, bind one, or verify artifacts",
            category: "system",
        },
        CommandMeta {
            name: "vim",
            desc: "modal vim editing for the composer (v1: Normal/Insert only)",
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

/// Commands matching a filter, as `/name` strings. The "did you mean"
/// suggestion source on an unknown command. The exact typed word is
/// never suggested: "did you mean /tasks?" in answer to `/tasks` is a
/// tautology, not a suggestion.
pub fn complete(prefix: &str) -> Vec<String> {
    let p = prefix.trim_start_matches('/').to_lowercase();
    registry()
        .into_iter()
        .filter(|(name, _)| name.starts_with(&p) && *name != p)
        .map(|(_, c)| c.slash())
        .collect()
}

/// A key the palette should react to while the composer is empty.
pub fn is_palette_key(k: Key) -> bool {
    matches!(k, Key::CtrlK)
}
