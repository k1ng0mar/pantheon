//! The public CLI surface.
//!
//! Pantheon grew one top-level verb per feature. This module is the
//! compatibility router: the public surface is a small set of nouns, and
//! the historic verbs remain accepted as hidden aliases. Canonical routes
//! are rewritten to the legacy argv shape before the existing dispatcher
//! sees them, so command implementations do not move in this pass.

/// Commands shown by `pantheon --help`.
pub const PUBLIC_VERBS: &[&str] = &[
    "run",
    "session",
    "memory",
    "agent",
    "tools",
    "workspace",
    "tasks",
    "desktop",
    "serve",
    "config",
    "setup",
    "doctor",
    "maintenance",
];

/// One-line descriptions for `pantheon --help`, in the same order as
/// [`PUBLIC_VERBS`].
pub const PUBLIC_COMMANDS: &[(&str, &str)] = &[
    ("run", "execute one task or continue a session"),
    ("session", "runs, send, audit, stats, logs"),
    ("memory", "import/export/sync/recall, reflect, consolidate"),
    ("agent", "model, provider, provider catalog, fallback"),
    ("tools", "mcp, oauth, skills, plugins, extensions, hook"),
    ("workspace", "worktree, codebase"),
    ("tasks", "schedule, pipeline, swarm, nightly"),
    (
        "desktop",
        "build + launch the desktop app, register it in app menus",
    ),
    ("serve", "api, gateway, dashboard, install"),
    ("config", "set, get, edit, path"),
    ("setup", "first-run wizard, migrate"),
    ("doctor", "preflight check, repair"),
    ("maintenance", "backup, reset, update, uninstall"),
];

/// Historic top-level verbs. They still dispatch, but help no longer
/// advertises them. This list is deliberately kept beside the public one so
/// the known-verb guard cannot drift again.
pub const LEGACY_VERBS: &[&str] = &[
    "audit",
    "backup",
    "codebase",
    "consolidate",
    "dashboard",
    "extensions",
    "fallback",
    "gateway",
    "hook",
    "init",
    "logs",
    "mcp",
    "migrate",
    "model",
    "nightly",
    "oauth",
    "pipeline",
    "plugins",
    "provider",
    "providers",
    "reflect",
    "repair",
    "reset",
    "runs",
    "schedule",
    "send",
    "skills",
    "stats",
    "swarm",
    "uninstall",
    "update",
    "worktree",
];

/// Every accepted top-level verb: public names plus hidden aliases.
pub const ALL_VERBS: &[&str] = &[
    "agent",
    "audit",
    "backup",
    "codebase",
    "config",
    "consolidate",
    "dashboard",
    "desktop",
    "doctor",
    "extensions",
    "fallback",
    "gateway",
    "hook",
    "init",
    "logs",
    "maintenance",
    "mcp",
    "memory",
    "migrate",
    "model",
    "nightly",
    "oauth",
    "pipeline",
    "plugins",
    "provider",
    "providers",
    "reflect",
    "repair",
    "reset",
    "run",
    "runs",
    "schedule",
    "send",
    "serve",
    "session",
    "setup",
    "skills",
    "stats",
    "swarm",
    "tasks",
    "tools",
    "uninstall",
    "update",
    "workspace",
    "worktree",
];

/// Result of routing one argv before the legacy dispatcher runs.
#[derive(Debug, PartialEq, Eq)]
pub enum Invocation {
    /// Pass this argv to the existing dispatcher.
    Command(Vec<String>),
    /// Print this group's usage and stop.
    Help(&'static str),
    /// A namespace was used with a subcommand it does not own.
    Unknown { group: &'static str, sub: String },
}

/// Route canonical namespaces to their legacy implementations.
pub fn normalize(argv: &[String]) -> Invocation {
    if argv.len() < 2 {
        return Invocation::Command(argv.to_vec());
    }
    let verb = argv[1].as_str();
    let sub = argv.get(2).map(String::as_str);
    match verb {
        "session" => one_level(
            argv,
            "session",
            sub,
            SESSION_HELP,
            &[
                ("runs", "runs"),
                ("list", "runs"),
                ("send", "send"),
                ("audit", "audit"),
                ("stats", "stats"),
                ("logs", "logs"),
            ],
        ),
        "agent" => match sub {
            None | Some("--help") | Some("-h") | Some("help") => Invocation::Help(AGENT_HELP),
            Some("provider") => {
                if is_help(argv.get(3).map(String::as_str)) {
                    Invocation::Help(AGENT_HELP)
                } else {
                    match argv.get(3).map(String::as_str) {
                        Some("catalog") if is_help(argv.get(4).map(String::as_str)) => {
                            Invocation::Help(CATALOG_HELP)
                        }
                        Some("catalog") => rewritten(argv, "providers", 3),
                        _ => rewritten(argv, "provider", 2),
                    }
                }
            }
            Some("model") => rewritten(argv, "model", 2),
            Some("fallback") if is_help(argv.get(3).map(String::as_str)) => {
                Invocation::Help(AGENT_HELP)
            }
            Some("fallback") => rewritten(argv, "fallback", 2),
            Some(other) => Invocation::Unknown {
                group: "agent",
                sub: other.to_string(),
            },
        },
        "tools" => {
            if is_help(argv.get(3).map(String::as_str))
                && matches!(
                    sub,
                    Some("mcp" | "oauth" | "skills" | "extensions" | "hook")
                )
            {
                Invocation::Help(TOOLS_HELP)
            } else {
                one_level(
                    argv,
                    "tools",
                    sub,
                    TOOLS_HELP,
                    &[
                        ("mcp", "mcp"),
                        ("oauth", "oauth"),
                        ("skills", "skills"),
                        ("plugins", "plugins"),
                        ("extensions", "extensions"),
                        ("hook", "hook"),
                    ],
                )
            }
        }
        "workspace" => one_level(
            argv,
            "workspace",
            sub,
            WORKSPACE_HELP,
            &[("worktree", "worktree"), ("codebase", "codebase")],
        ),
        "tasks" => {
            if is_help(argv.get(3).map(String::as_str)) && matches!(sub, Some("schedule" | "swarm"))
            {
                Invocation::Help(TASKS_HELP)
            } else {
                one_level(
                    argv,
                    "tasks",
                    sub,
                    TASKS_HELP,
                    &[
                        ("schedule", "schedule"),
                        ("pipeline", "pipeline"),
                        ("swarm", "swarm"),
                        ("nightly", "nightly"),
                    ],
                )
            }
        }
        "maintenance" => {
            if is_help(argv.get(3).map(String::as_str)) && matches!(sub, Some("reset")) {
                Invocation::Help(MAINTENANCE_HELP)
            } else {
                one_level(
                    argv,
                    "maintenance",
                    sub,
                    MAINTENANCE_HELP,
                    &[
                        ("backup", "backup"),
                        ("reset", "reset"),
                        ("update", "update"),
                        ("uninstall", "uninstall"),
                    ],
                )
            }
        }
        "memory" => match sub {
            Some("reflect") if is_help(argv.get(3).map(String::as_str)) => {
                Invocation::Help(NIGHTLY_ALIAS_HELP)
            }
            Some("consolidate") if is_help(argv.get(3).map(String::as_str)) => {
                Invocation::Help(NIGHTLY_ALIAS_HELP)
            }
            Some("reflect") => rewritten(argv, "reflect", 2),
            Some("consolidate") => rewritten(argv, "consolidate", 2),
            _ => Invocation::Command(argv.to_vec()),
        },
        "serve" => match sub {
            Some("api") => rewritten(argv, "serve", 2),
            Some("gateway") => rewritten(argv, "gateway", 2),
            Some("dashboard") => rewritten(argv, "dashboard", 2),
            Some("install") => rewritten(argv, "init", 2),
            _ => Invocation::Command(argv.to_vec()),
        },
        "setup" => match sub {
            Some("migrate") => rewritten(argv, "migrate", 2),
            _ => Invocation::Command(argv.to_vec()),
        },
        "doctor" => match sub {
            Some("check") => rewritten(argv, "doctor", 2),
            Some("repair") => rewritten(argv, "repair", 2),
            _ => Invocation::Command(argv.to_vec()),
        },
        // `run` and `config` are already canonical; legacy verbs pass
        // through untouched.
        _ => Invocation::Command(argv.to_vec()),
    }
}

/// Help for the namespaces that did not exist before this pass. Existing
/// groups keep their richer command-specific help.
pub fn group_help(group: &str) -> Option<&'static str> {
    match group {
        "session" => Some(SESSION_HELP),
        "agent" => Some(AGENT_HELP),
        "tools" => Some(TOOLS_HELP),
        "workspace" => Some(WORKSPACE_HELP),
        "tasks" => Some(TASKS_HELP),
        "maintenance" => Some(MAINTENANCE_HELP),
        _ => None,
    }
}

fn is_help(value: Option<&str>) -> bool {
    matches!(value, Some("--help" | "-h" | "help"))
}

fn one_level(
    argv: &[String],
    group: &'static str,
    sub: Option<&str>,
    help: &'static str,
    routes: &[(&str, &str)],
) -> Invocation {
    match sub {
        None | Some("--help") | Some("-h") | Some("help") => Invocation::Help(help),
        Some(s) => match routes.iter().find(|(name, _)| *name == s) {
            Some((_, legacy)) => rewritten(argv, legacy, 2),
            None => Invocation::Unknown {
                group,
                sub: s.to_string(),
            },
        },
    }
}

/// Replace the public namespace/subcommand pair with one legacy verb,
/// preserving every argument after it.
fn rewritten(argv: &[String], legacy: &str, consumed_after_program: usize) -> Invocation {
    let mut out = Vec::with_capacity(argv.len());
    out.push(argv[0].clone());
    out.push(legacy.to_string());
    out.extend(argv.iter().skip(1 + consumed_after_program).cloned());
    Invocation::Command(out)
}

const SESSION_HELP: &str = "\
pantheon session <command> [args]

Commands:
  runs [--metrics] [<run_id>]     list runs or show one event trace
  list                            alias for runs
  send --to <surface> MESSAGE     push a message to a gateway surface
  audit <run_id> [OUT.jsonl]      export a sequence-validated trajectory
  stats [--week|--month] ...      usage by model, project, session, day
  logs [agent|errors|gateway] ... read log files

The legacy top-level spellings (runs, send, audit, stats, logs) still work.
";

const AGENT_HELP: &str = "\
pantheon agent <command> [args]

Commands:
  model [--list] [--auxiliary KIND]  provider picker; keys land in .env
  provider <add|list|remove>         custom-endpoint registry
  provider catalog                   list cataloged providers and models
  fallback <add|list|remove>         ordered provider/model fallback chain

The legacy top-level spellings (model, provider, providers, fallback) still work.
";

const CATALOG_HELP: &str = "\
pantheon agent provider catalog

List the providers and models selectable in this build, including each
provider's API shape and base URL. Custom endpoints use --provider <base-url>
directly and speak the OpenAI shape.
";

const TOOLS_HELP: &str = "\
pantheon tools <command> [args]

Commands:
  mcp <list|status|approve|resources|templates|read|prompts|prompt>
  oauth <login|status|logout> [app]
  skills <list|import|doctor>
  plugins <list|install|enable|disable>
  extensions [approve|revoke|pending|list]
  hook <name> [--session S] [--platform P]

The legacy top-level spellings (mcp, oauth, skills, plugins, extensions, hook)
still work.
";

const WORKSPACE_HELP: &str = "\
pantheon workspace <command> [args]

Commands:
  worktree <list|create|remove|path|enter|root>  isolated git worktrees
  codebase <index|search|callers|callees|overview> native symbol/call graph

The legacy top-level spellings (worktree, codebase) still work.
";

const TASKS_HELP: &str = "\
pantheon tasks <command> [args]

Commands:
  schedule <task> --30m | list|pause|resume|cancel|run <id>
  pipeline <sub>                 durable 6-stage workflow with human gates
  swarm <N> TASK [--roles a,b]   parallel role-based work
  nightly [--dry-run] ...        nightly repair, memory, and task pass

The legacy top-level spellings (schedule, pipeline, swarm, nightly) still work.
";

const NIGHTLY_ALIAS_HELP: &str = "\
pantheon memory reflect|consolidate [--dry-run] [status|log|pending] [--approve ID] [--deny ID]

`reflect` and `consolidate` are compatibility aliases for the unified
nightly pass. See `pantheon tasks nightly --help` for the full command.
";

const MAINTENANCE_HELP: &str = "\
pantheon maintenance <command> [args]

Commands:
  backup [--list] [--restore DIR --yes]  atomic SQLite snapshots
  reset [--config|--state|--everything] [--yes]
  update [--check] [--version TAG]       replace this binary
  uninstall [--yes] [--include-secrets]  remove config + data

The legacy top-level spellings (backup, reset, update, uninstall) still work.
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn argv(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_string).collect()
    }

    /// Normalizer input always starts with argv[0], as it does in `run()`.
    fn input(text: &str) -> Vec<String> {
        let mut out = vec!["pantheon".to_string()];
        out.extend(argv(text));
        out
    }

    fn command(text: &str) -> Vec<String> {
        match normalize(&input(text)) {
            Invocation::Command(v) => v,
            other => panic!("expected command for {text:?}, got {other:?}"),
        }
    }

    #[test]
    fn public_surface_is_small_and_consistent() {
        assert_eq!(PUBLIC_VERBS.len(), 13);
        let from_help: Vec<&str> = PUBLIC_COMMANDS.iter().map(|(name, _)| *name).collect();
        assert_eq!(from_help.as_slice(), PUBLIC_VERBS);
        assert_eq!(PUBLIC_VERBS.len() + LEGACY_VERBS.len(), ALL_VERBS.len());

        let mut all = HashSet::new();
        for verb in PUBLIC_VERBS.iter().chain(LEGACY_VERBS.iter()) {
            assert!(all.insert(*verb), "duplicate verb {verb}");
            assert!(ALL_VERBS.contains(verb), "missing from ALL_VERBS: {verb}");
        }
        for verb in ALL_VERBS {
            assert!(
                PUBLIC_VERBS.contains(verb) || LEGACY_VERBS.contains(verb),
                "unclassified verb: {verb}"
            );
        }
    }

    #[test]
    fn canonical_routes_rewrite_to_legacy_argv() {
        assert_eq!(
            command("session runs --metrics"),
            argv("pantheon runs --metrics")
        );
        assert_eq!(command("session list"), argv("pantheon runs"));
        assert_eq!(
            command("session send --to telegram hello"),
            argv("pantheon send --to telegram hello")
        );
        assert_eq!(
            command("memory reflect --dry-run"),
            argv("pantheon reflect --dry-run")
        );
        assert_eq!(
            command("agent provider catalog"),
            argv("pantheon providers")
        );
        assert_eq!(
            command("agent provider add --name local"),
            argv("pantheon provider add --name local")
        );
        assert_eq!(command("agent model --list"), argv("pantheon model --list"));
        assert_eq!(
            command("tools mcp resources"),
            argv("pantheon mcp resources")
        );
        assert_eq!(command("tools oauth status"), argv("pantheon oauth status"));
        assert_eq!(
            command("workspace worktree create review"),
            argv("pantheon worktree create review")
        );
        assert_eq!(
            command("workspace codebase search WorktreeManager"),
            argv("pantheon codebase search WorktreeManager")
        );
        assert_eq!(
            command("tasks schedule list"),
            argv("pantheon schedule list")
        );
        assert_eq!(
            command("serve api --port 8080"),
            argv("pantheon serve --port 8080")
        );
        assert_eq!(command("serve install"), argv("pantheon init"));
        assert_eq!(
            command("setup migrate detect hermes"),
            argv("pantheon migrate detect hermes")
        );
        assert_eq!(
            command("doctor repair --dry-run"),
            argv("pantheon repair --dry-run")
        );
        assert_eq!(
            command("doctor check --ping"),
            argv("pantheon doctor --ping")
        );
        assert_eq!(
            command("maintenance backup --list"),
            argv("pantheon backup --list")
        );
    }

    #[test]
    fn legacy_verbs_are_unchanged() {
        assert_eq!(command("mcp list"), argv("pantheon mcp list"));
        assert_eq!(command("worktree list"), argv("pantheon worktree list"));
        assert_eq!(command("runs --metrics"), argv("pantheon runs --metrics"));
        assert_eq!(command("config path"), argv("pantheon config path"));
    }

    #[test]
    fn new_groups_have_help_and_reject_unknown_subcommands() {
        for group in [
            "session",
            "agent",
            "tools",
            "workspace",
            "tasks",
            "maintenance",
        ] {
            assert!(
                matches!(normalize(&input(group)), Invocation::Help(_)),
                "missing help for {group}"
            );
            assert!(
                matches!(
                    normalize(&input(&format!("{group} --help"))),
                    Invocation::Help(_)
                ),
                "missing --help for {group}"
            );
            assert!(
                matches!(
                    normalize(&input(&format!("{group} definitely-not-a-command"))),
                    Invocation::Unknown { .. }
                ),
                "unknown subcommand accepted by {group}"
            );
        }

        for alias in [
            "memory reflect --help",
            "memory consolidate --help",
            "agent provider --help",
            "agent provider catalog --help",
            "agent fallback --help",
            "tools mcp --help",
            "tools oauth --help",
            "tools skills --help",
            "tools extensions --help",
            "tools hook --help",
            "tasks schedule --help",
            "tasks swarm --help",
            "maintenance reset --help",
        ] {
            assert!(
                matches!(normalize(&input(alias)), Invocation::Help(_)),
                "missing help for {alias}"
            );
        }
    }

    #[test]
    fn usage_shows_only_the_public_surface() {
        let usage = crate::terminal::usage();
        for (verb, _) in PUBLIC_COMMANDS {
            assert!(
                usage.contains(&format!("  {verb:<12} ")),
                "usage is missing public verb {verb}"
            );
        }
        for verb in LEGACY_VERBS {
            assert!(
                !usage.contains(&format!("  {verb:<12} ")),
                "usage still advertises legacy verb {verb}"
            );
        }
    }
}
