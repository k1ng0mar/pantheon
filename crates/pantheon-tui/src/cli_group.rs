//! cli_group: args, cli_surface, consolidate_cli, entry, custom_commands, commands, prompt, mentions, acp, agui in one file.

pub mod args {
    //! Shared CLI argument helpers. One parser for every verb: `--flag value`
    //! and `--flag=value` both work, positional args are those that do not
    //! start with `--`.
    use std::collections::HashMap;

    /// Parsed arguments: positional values in order plus a flag map (last
    /// occurrence wins).
    pub struct Args {
        pub positionals: Vec<String>,
        flags: HashMap<String, String>,
    }

    impl Args {
        pub fn parse(raw: &[String]) -> Self {
            let mut positionals = Vec::new();
            let mut flags = HashMap::new();
            let mut i = 0;
            while i < raw.len() {
                let a = &raw[i];
                if let Some(stripped) = a.strip_prefix("--") {
                    if let Some((k, v)) = stripped.split_once('=') {
                        flags.insert(k.to_string(), v.to_string());
                    } else if let Some(v) = raw.get(i + 1) {
                        if v.starts_with("--") {
                            // A bare flag followed by another flag is a boolean
                            // switch. Without this the following flag is eaten as
                            // its value and silently disappears: `--yes
                            // --provider openai` lost the provider entirely.
                            flags.insert(stripped.to_string(), String::new());
                        } else {
                            flags.insert(stripped.to_string(), v.clone());
                            i += 1;
                        }
                    } else {
                        // Boolean flag at end of argv.
                        flags.insert(stripped.to_string(), String::new());
                    }
                } else {
                    positionals.push(a.clone());
                }
                i += 1;
            }
            Self { positionals, flags }
        }
        pub fn flag(&self, name: &str) -> Option<String> {
            self.flags.get(name).filter(|v| !v.is_empty()).cloned()
        }
        /// True when the flag appeared, even without a value (boolean flags).
        pub fn has(&self, name: &str) -> bool {
            self.flags.contains_key(name)
        }
        pub fn positional(&self, index: usize) -> Option<String> {
            self.positionals.get(index).cloned()
        }
    }
}

pub mod cli_surface {
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
        "auth",
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
        ("auth", "add/list/remove/priority/status provider keys"),
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
        "secrets",
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
        "secrets",
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
        "auth",
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
                if is_help(argv.get(3).map(String::as_str))
                    && matches!(sub, Some("schedule" | "swarm"))
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
      mcp <list|status|health|approve|enable|disable|resources|templates|read|prompts|prompt|login|logout|auth>
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
            assert_eq!(PUBLIC_VERBS.len(), 14);
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
}

pub mod consolidate_cli {
    //! `pantheon consolidate` - compatibility shim over the unified nightly pass.
    //!
    //! The old standalone consolidation pipeline is merged into
    //! [`crate::nightly_cli`]: `pantheon consolidate` now runs the full
    //! nightly pass (single ledger scan → memory promotion + proposals →
    //! gates → audit). Every function here keeps its old name and shape so
    //! the TUI, the scheduler, and scripts keep working; new code should
    //! call `nightly_cli` directly.

    use std::path::Path;

    /// Run one bounded pass. Backed by the unified nightly pipeline.
    pub fn run_one_pass(
        data_dir: &Path,
        dry_run: bool,
    ) -> Result<pantheon_nightly::PassResult, String> {
        crate::nightly_cli::run_one_pass(data_dir, dry_run)
    }

    /// Whether the operator enabled LLM-backed nightly steps in config.
    pub fn consolidation_enabled(data_dir: &Path) -> bool {
        crate::nightly_cli::nightly_enabled(data_dir)
    }

    /// Human-readable status: enabled state plus the last pass summary.
    pub fn status_line(data_dir: &Path) -> String {
        crate::nightly_cli::status_line(data_dir)
    }

    /// One-line summary of a finished pass, prefixed so transcript readers can
    /// tell a dry run from the real thing.
    pub fn summarize_pass(out: &pantheon_nightly::PassResult, dry_run: bool) -> String {
        if dry_run {
            format!("(dry run) {}", crate::nightly_cli::summarize_pass(out))
        } else {
            crate::nightly_cli::summarize_pass(out)
        }
    }

    /// `pantheon consolidate [status] [--dry-run]`.
    ///
    /// Compatibility alias for `pantheon nightly`: runs the unified pass.
    pub fn cmd_consolidate(args: &[String], data_dir: &Path) {
        eprintln!(
            "note: `pantheon consolidate` is now part of the unified `pantheon nightly` pass"
        );
        let mut nightly_args = args.to_vec();
        if nightly_args.len() > 1 {
            nightly_args[1] = "nightly".to_string();
        }
        crate::nightly_cli::cmd_nightly(&nightly_args, data_dir)
    }
}

pub mod entry {
    //! The single interactive entry point.
    //!
    //! Everything a person types goes through here. Two decisions live in this
    //! module and nowhere else: whether the install is configured enough to open
    //! a session, and how the terminal is restored when something fails.
    //!
    //! Setup runs inside the TUI when the config is incomplete. There is no
    //! separate wizard binary and no text prompt fallback, because a user who
    //! gets a different program depending on their terminal is a user who files
    //! two bug reports about two products.

    use std::io::IsTerminal;
    use std::path::Path;

    /// What the install looks like before we decide what to open.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Entry {
        /// A usable agent configuration. Open a session.
        Session { resume: Option<String> },
        /// Nothing configured yet. Run setup first, then a session.
        Setup { resume: Option<String> },
        /// A config file exists but does not parse. Show repair guidance and
        /// stop: never route this into setup (the wizard will not overwrite the
        /// file, so it would strand the user) and never silently discard it.
        Repair,
    }

    /// The three states a bare `pantheon` invocation can find the install in.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ConfigState {
        /// Parseable config with a model: open a session.
        Ready,
        /// No config file, or one with no model: first run, open setup.
        Missing,
        /// Config exists but does not parse: repair guidance, not setup.
        Broken,
    }

    impl Entry {
        /// The resolution itself, separated from the terminal so it is testable.
        ///
        /// Tri-state on purpose: a config with no model cannot produce an
        /// answer, so it is not a usable agent no matter what else it declares,
        /// and a config that does not parse is a repair job rather than a
        /// first run. Anything less than a model and a provider means setup.
        pub fn resolve_state(state: ConfigState, resume: Option<String>) -> Entry {
            match state {
                ConfigState::Ready => Entry::Session { resume },
                ConfigState::Missing => Entry::Setup { resume },
                ConfigState::Broken => Entry::Repair,
            }
        }
    }

    /// Whether the install can open a session right now.
    ///
    /// A missing config file, an unreadable one, or one without a model all mean
    /// the same thing to a user on first run: nothing is set up.
    ///
    /// This deliberately does not treat a *malformed* config as "not configured".
    /// Overwriting a file the user broke would destroy the evidence of what went
    /// wrong, and the entry point's job is to decide what to open, not to repair
    /// anything. A malformed config is reported by `doctor` and by the reader that
    /// owns that message; here it simply means "not a usable session", which routes
    /// the user to setup, which will not overwrite the file.
    pub fn is_configured(data_dir: &Path) -> bool {
        matches!(config_state(data_dir), ConfigState::Ready)
    }

    /// The full three-way read of the install state.
    ///
    /// `Config::load` distinguishes "no file" (`CONFIG_OPEN`) from "file does
    /// not parse" (`CONFIG_PARSE`); collapsing both into "not configured" sent
    /// users with a typo'd config into the setup wizard, which refuses to
    /// overwrite the file and left them stranded with no diagnosis.
    pub fn config_state(data_dir: &Path) -> ConfigState {
        // `load`, not `load_or_report`: the reporting variant exits the process on
        // a parse error, which makes it unusable here and untestable. The exit
        // path belongs to the verb that owns the message.
        match crate::config::Config::load(data_dir) {
            Ok(cfg) => {
                let usable = cfg
                    .model
                    .as_ref()
                    .map(|m| !m.model.trim().is_empty())
                    .unwrap_or(false);
                if usable {
                    ConfigState::Ready
                } else {
                    ConfigState::Missing
                }
            }
            Err(e) if e.code == "CONFIG_OPEN" => ConfigState::Missing,
            Err(_) => ConfigState::Broken,
        }
    }

    /// Open the terminal product.
    ///
    /// The ordering here is deliberate. Config is read *before* the terminal is
    /// taken, so a user with no config is not flashed a blank alternate screen
    /// before the wizard draws. Setup runs, and on success the same process falls
    /// into a session without the user retyping anything.
    pub fn run() {
        let data_dir = crate::terminal::data_dir();
        match Entry::resolve_state(config_state(&data_dir), None) {
            Entry::Session { resume } => launch(data_dir, resume, None),
            Entry::Setup { resume } => {
                // Setup owns the terminal for the whole wizard. It is a TUI
                // screen sequence, not a prompt loop, so there is no text mode
                // and no way to answer it without a terminal.
                crate::setup_wizard::run_setup_flow(&data_dir);
                // Re-read after setup: setup writes the config, and if the user
                // quit early there may be nothing usable. Falling into a session
                // with no model would look like a broken agent rather than an
                // unfinished install.
                if is_configured(&data_dir) {
                    launch(data_dir, resume, None);
                } else {
                    eprintln!("pantheon: setup did not finish, so there is no session to open.");
                    eprintln!("pantheon: run `pantheon setup` when you are ready.");
                    std::process::exit(1);
                }
            }
            Entry::Repair => {
                // The config exists but does not parse. Print the doctor's
                // findings in human-readable form and stop with a non-zero
                // exit. This is the "invalid config" arm of the bare-command
                // auto-pick: onboarding when fresh, repair guidance when
                // broken, a session when healthy.
                let (_healthy, findings) = crate::doctor::actionable_findings(&data_dir, false);
                eprintln!("pantheon: the config file is broken, so there is no session to open.");
                let mut shown = 0;
                for c in &findings {
                    shown += 1;
                    eprintln!("pantheon: [{}] {}: {}", c.status, c.section, c.detail);
                    if !c.fix.is_empty() {
                        eprintln!("pantheon:   fix: {}", c.fix);
                    }
                }
                if shown == 0 {
                    eprintln!("pantheon: the doctor found nothing actionable; see `pantheon doctor` for the full report.");
                } else {
                    eprintln!("pantheon: fix the TOML by hand, or move it aside and run `pantheon setup` to start fresh.");
                }
                std::process::exit(2);
            }
        }
    }

    /// `pantheon --resume [run_id]`. Same product, same terminal, same rules.
    /// A resumed session may also submit its first message up front, so the
    /// prompt travels with the resume instead of needing a second keystroke.
    pub fn run_with_resume_and_prompt(resume: Option<String>, prompt: Option<String>) {
        let data_dir = crate::terminal::data_dir();
        if !is_configured(&data_dir) {
            eprintln!("pantheon: nothing is configured yet, so there is no session to resume.");
            eprintln!("pantheon: run `pantheon setup` first.");
            std::process::exit(1);
        }
        launch(data_dir, resume, prompt);
    }

    /// `pantheon --profile <name>` (also `-p`, `--agent`): open the TUI as the
    /// named agent profile instead of the configured default.
    ///
    /// The profile is validated *before* the terminal is taken: an unknown
    /// name fails here with a clear error, because the session path folds
    /// resolution failures into the anonymous fallback and would otherwise
    /// open the wrong agent silently. The validated name is installed as the
    /// process-lifetime override, which `Config::resolve_profile` consults
    /// session construction needs no new parameter.
    pub fn run_with_profile(name: &str) {
        let data_dir = crate::terminal::data_dir();
        if !is_configured(&data_dir) {
            eprintln!("pantheon: nothing is configured yet, so there is no session to open.");
            eprintln!("pantheon: run `pantheon setup` first.");
            std::process::exit(1);
        }
        match crate::config::Config::load(&data_dir) {
            Ok(cfg) => {
                if let Err(e) = cfg.resolve_profile(Some(name)) {
                    eprintln!("pantheon: --profile {name:?}: {e}");
                    eprintln!("pantheon: fix: declare [agents.{name}] in config.toml");
                    std::process::exit(2);
                }
            }
            Err(e) => {
                eprintln!("pantheon: cannot read config: {e}");
                std::process::exit(1);
            }
        }
        pantheon_api::config::set_profile_override(Some(name.to_string()));
        run();
    }

    /// Fail fast with Ollama guidance when the configured provider is
    /// `local` and nothing answers at its base URL.
    ///
    /// The `local` provider is Ollama (install.sh installs it), and a bare
    /// `pantheon` with Ollama down used to open a session whose first
    /// message died with a bare PROVIDER_HTTP network error - no mention of
    /// Ollama, no `pantheon setup` hint. Probing before the TUI opens turns
    /// that into the fix: start Ollama, pull the model, or pick another
    /// provider.
    ///
    /// A fresh install lands here: the installer writes the `local`
    /// default non-interactively, and nothing on a clean box runs Ollama.
    /// Exiting with instructions made the first run a wall of text ending
    /// in "run `pantheon setup`" - so when the terminal is interactive the
    /// wizard opens directly instead, and the user is walked through a
    /// provider choice rather than told to go run a command. Only `local`
    /// is probed: a cloud endpoint hiccup at launch time must not refuse
    /// to open the session.
    fn check_local_provider(data_dir: &std::path::Path) {
        let model = match crate::config::Config::load(data_dir) {
            Ok(cfg) => match cfg.model {
                Some(m) => m,
                None => return,
            },
            // config_state already decided this is not a session; the
            // Setup/Repair arms own the message.
            Err(_) => return,
        };
        if model.provider != "local" {
            return;
        }
        let url = match pantheon_providers::catalog::resolve_base_url("local") {
            Ok(u) => u,
            Err(_) => return,
        };
        if crate::doctor::tcp_probe(&url, std::time::Duration::from_secs(3)) {
            return;
        }
        if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            eprintln!(
                "pantheon: the configured provider \"local\" (Ollama) is not reachable at {url}."
            );
            eprintln!("pantheon: opening setup so you can choose a provider.");
            crate::setup_wizard::run_setup_flow(data_dir);
            std::process::exit(0);
        }
        eprintln!(
            "pantheon: the configured provider \"local\" (Ollama) is not reachable at {url}."
        );
        eprintln!(
            "pantheon: if you use Ollama: start it with `ollama serve`, then `ollama pull {}`.",
            model.model
        );
        eprintln!("pantheon: or run `pantheon setup` to choose a different provider.");
        std::process::exit(1);
    }

    /// Hand off to the TUI and make sure a failure leaves the user's shell sane.
    ///
    /// A panic between entering raw mode and leaving the alternate screen leaves
    /// the shell unusable until `reset`, so the hook restores the terminal first
    /// and then hands the panic to the default hook for the backtrace. An error
    /// return already restores via the session's own cleanup; the hook only fires
    /// on a panic, which would otherwise skip it entirely.
    fn launch(data_dir: std::path::PathBuf, resume: Option<String>, prompt: Option<String>) -> ! {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            eprintln!("pantheon: the terminal interface needs a terminal on stdin and stdout.");
            std::process::exit(1);
        }
        check_local_provider(&data_dir);
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
            use std::io::IsTerminal;
            let _ = disable_raw_mode();
            if std::io::stdout().is_terminal() {
                use crossterm::ExecutableCommand;
                let _ = std::io::stdout().execute(LeaveAlternateScreen);
            }
            default_hook(info);
        }));
        let result = crate::session::run_tui_session_with(data_dir, resume, prompt);
        if let Err(e) = result {
            // The TUI should have restored the screen, but a restore that itself
            // failed is exactly the case where the user needs to be told rather
            // than left guessing why their terminal is broken.
            eprintln!("pantheon: terminal interface failed: {e}");
            eprintln!("pantheon: if your terminal looks wrong, run `reset`.");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
}

pub mod custom_commands {
    //! User-defined slash commands.
    //!
    //! Two on-disk forms are supported, matching the familiar Markdown-first
    //! workflow while keeping machine-generated command sets easy:
    //!
    //! * `.pantheon/commands/<name>.md` (project), `.opencode/commands/<name>.md`
    //!   (compatibility), `<data_dir>/commands/<name>.md` (global). The body is
    //!   the prompt; optional YAML-ish frontmatter supplies `description`,
    //!   `agent`, and `model`.
    //! * `commands.json` in any of those roots. It may be a map of names to
    //!   `{ "description", "template"|"prompt", "agent", "model" }`, a single
    //!   command object, or an array of command objects with `name`.
    //!
    //! `$ARGUMENTS` is replaced with the text after the command; `$1`..`$9` are
    //! replaced with whitespace-separated arguments. If neither placeholder is
    //! present, arguments are appended on a blank line. Built-in commands always
    //! win over a user command with the same name.

    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct CustomCommand {
        pub name: String,
        pub description: String,
        pub template: String,
        pub agent: Option<String>,
        pub model: Option<String>,
        pub source: PathBuf,
    }

    /// Discover commands from the project and global data roots. Project
    /// commands shadow global ones; within one root, later candidates do not
    /// replace the first definition. Invalid files are skipped rather than
    /// making every other command unavailable.
    pub fn discover(data_dir: &Path, project_root: &Path) -> Vec<CustomCommand> {
        let mut roots = vec![project_root.join(".pantheon/commands")];
        // OpenCode-compatible location. Keeping it read-only means a Pantheon
        // user never has to choose between the two ecosystems' command folders.
        roots.push(project_root.join(".opencode/commands"));
        roots.push(data_dir.join("commands"));
        if let Ok(extra) = std::env::var("PANTHEON_COMMANDS_DIR") {
            for entry in extra.split(':').filter(|s| !s.trim().is_empty()) {
                roots.push(PathBuf::from(entry));
            }
        }
        discover_in(&roots)
    }

    pub fn discover_in(roots: &[PathBuf]) -> Vec<CustomCommand> {
        let mut found: BTreeMap<String, CustomCommand> = BTreeMap::new();
        for root in roots {
            let Ok(read_dir) = std::fs::read_dir(root) else {
                continue;
            };
            let mut entries: Vec<PathBuf> = read_dir
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file())
                .collect();
            entries.sort();
            for path in entries {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let ext = path
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let parsed = match ext.as_str() {
                    "md" => {
                        let stem = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .to_string();
                        parse_markdown(&path, &stem, &text).into_iter().collect()
                    }
                    "json" => parse_json(&path, &text),
                    _ => Vec::new(),
                };
                for command in parsed {
                    if command.name.is_empty() || crate::commands::is_builtin(&command.name) {
                        continue;
                    }
                    found.entry(command.name.clone()).or_insert(command);
                }
            }
        }
        found.into_values().collect()
    }

    pub fn find<'a>(commands: &'a [CustomCommand], name: &str) -> Option<&'a CustomCommand> {
        commands.iter().find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Expand placeholders without invoking a shell. Arguments are data, never
    /// a command line, so a command template cannot be escalated by user input.
    pub fn expand(template: &str, arguments: &str) -> String {
        let args: Vec<&str> = arguments.split_whitespace().collect();
        let has_args_placeholder = template.contains("$ARGUMENTS");
        let mut positional = false;
        for i in 1..=9 {
            if template.contains(&format!("${i}")) || template.contains(&format!("${{{i}}}")) {
                positional = true;
                break;
            }
        }
        let mut out = template.replace("$ARGUMENTS", arguments.trim());
        for (i, arg) in args.iter().enumerate().take(9) {
            out = out.replace(&format!("${{{}}}", i + 1), arg);
            out = out.replace(&format!("${}", i + 1), arg);
        }
        for i in args.len() + 1..=9 {
            out = out.replace(&format!("${{{}}}", i), "");
            out = out.replace(&format!("${}", i), "");
        }
        if !has_args_placeholder && !positional && !arguments.trim().is_empty() {
            out.push_str("\n\n");
            out.push_str(arguments.trim());
        }
        out.trim().to_string()
    }

    fn parse_markdown(path: &Path, stem: &str, text: &str) -> Option<CustomCommand> {
        let (meta, body) = split_frontmatter(text);
        let body = body.trim();
        if body.is_empty() {
            return None;
        }
        let name = meta
            .get("name")
            .map(String::as_str)
            .unwrap_or(stem)
            .trim()
            .trim_start_matches('/')
            .to_string();
        let description = meta
            .get("description")
            .cloned()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| first_line(body));
        Some(CustomCommand {
            name,
            description,
            template: body.to_string(),
            agent: meta.get("agent").cloned().filter(|s| !s.is_empty()),
            model: meta.get("model").cloned().filter(|s| !s.is_empty()),
            source: path.to_path_buf(),
        })
    }

    fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, &str) {
        let mut meta = BTreeMap::new();
        let rest = if let Some(rest) = text.strip_prefix("---\n") {
            rest
        } else if let Some(rest) = text.strip_prefix("---\r\n") {
            rest
        } else {
            return (meta, text);
        };
        let mut offset = 0usize;
        for line in rest.split_inclusive('\n') {
            let clean = line.trim_end_matches(['\r', '\n']);
            if clean.trim() == "---" {
                let body = &rest[offset + line.len()..];
                return (meta, body);
            }
            if let Some((key, value)) = clean.split_once(':') {
                let key = key.trim().to_ascii_lowercase();
                if !key.is_empty() {
                    meta.insert(key, unquote(value.trim()));
                }
            }
            offset += line.len();
        }
        // No closing fence: treat the document as ordinary Markdown, not as a
        // command with an empty body.
        (BTreeMap::new(), text)
    }

    fn unquote(s: &str) -> String {
        let s = s.trim();
        if s.len() >= 2
            && ((s.starts_with('"') && s.ends_with('"'))
                || (s.starts_with('\'') && s.ends_with('\'')))
        {
            s[1..s.len() - 1].to_string()
        } else {
            s.to_string()
        }
    }

    fn first_line(body: &str) -> String {
        body.lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim().trim_start_matches('#').trim().to_string())
            .unwrap_or_default()
    }

    fn parse_json(path: &Path, text: &str) -> Vec<CustomCommand> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
            return Vec::new();
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("command");
        let mut out = Vec::new();
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    if let Some(command) = json_command(path, stem, None, &item) {
                        out.push(command);
                    }
                }
            }
            serde_json::Value::Object(map) => {
                if map.contains_key("template") || map.contains_key("prompt") {
                    if let Some(command) =
                        json_command(path, stem, None, &serde_json::Value::Object(map))
                    {
                        out.push(command);
                    }
                } else {
                    let nested = map.get("commands").and_then(|v| v.as_object());
                    let entries: Box<dyn Iterator<Item = (&String, &serde_json::Value)>> =
                        if let Some(commands) = nested {
                            Box::new(commands.iter())
                        } else {
                            Box::new(map.iter())
                        };
                    for (name, item) in entries {
                        if let Some(command) = json_command(path, stem, Some(name), item) {
                            out.push(command);
                        }
                    }
                }
            }
            _ => {}
        }
        out
    }

    fn json_command(
        path: &Path,
        stem: &str,
        key: Option<&str>,
        value: &serde_json::Value,
    ) -> Option<CustomCommand> {
        let (template, description, name, agent, model) = match value {
            serde_json::Value::String(template) => (
                template.clone(),
                String::new(),
                key.unwrap_or(stem).to_string(),
                None,
                None,
            ),
            serde_json::Value::Object(map) => {
                let template = map
                    .get("template")
                    .or_else(|| map.get("prompt"))
                    .and_then(|v| v.as_str())?
                    .to_string();
                let description = map
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = map
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or_else(|| key.unwrap_or(stem))
                    .trim_start_matches('/')
                    .to_string();
                (
                    template,
                    description,
                    name,
                    map.get("agent")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    map.get("model")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                )
            }
            _ => return None,
        };
        if template.trim().is_empty() || name.is_empty() {
            return None;
        }
        Some(CustomCommand {
            name,
            description: if description.trim().is_empty() {
                first_line(&template)
            } else {
                description
            },
            template,
            agent,
            model,
            source: path.to_path_buf(),
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn markdown_frontmatter_and_body_are_parsed() {
            let text =
                "---\ndescription: Review a diff\nagent: coder\n---\nReview this:\n$ARGUMENTS\n";
            let cmd = parse_markdown(Path::new("review.md"), "review", text).unwrap();
            assert_eq!(cmd.name, "review");
            assert_eq!(cmd.description, "Review a diff");
            assert_eq!(cmd.agent.as_deref(), Some("coder"));
            assert_eq!(cmd.template, "Review this:\n$ARGUMENTS");
        }

        #[test]
        fn no_closing_frontmatter_is_plain_markdown() {
            let cmd =
                parse_markdown(Path::new("x.md"), "x", "---\nnot: frontmatter\nhello").unwrap();
            assert!(cmd.template.contains("not: frontmatter"));
            assert_eq!(cmd.description, "---");
        }

        #[test]
        fn arguments_replace_placeholders_and_append_when_absent() {
            assert_eq!(expand("Review $ARGUMENTS", "a.rs"), "Review a.rs");
            assert_eq!(expand("Compare $1 with $2", "a b"), "Compare a with b");
            assert_eq!(expand("Do the thing", "now"), "Do the thing\n\nnow");
            assert_eq!(expand("Do it", ""), "Do it");
        }

        #[test]
        fn json_map_and_single_object_are_supported() {
            let map = r#"{"review":{"description":"Review","template":"Look at $ARGUMENTS"}}"#;
            let cmds = parse_json(Path::new("commands.json"), map);
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0].name, "review");
            assert_eq!(cmds[0].template, "Look at $ARGUMENTS");

            let one = r#"{"name":"explain","prompt":"Explain $1"}"#;
            let cmds = parse_json(Path::new("one.json"), one);
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0].name, "explain");
        }

        #[test]
        fn project_roots_shadow_global_roots() {
            let nonce = format!(
                "pantheon-commands-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            );
            let project = std::env::temp_dir().join(format!("{nonce}-project"));
            let global = std::env::temp_dir().join(format!("{nonce}-global"));
            std::fs::create_dir_all(project.join(".pantheon/commands")).unwrap();
            std::fs::create_dir_all(global.join("commands")).unwrap();
            std::fs::write(
                project.join(".pantheon/commands/review.md"),
                "project prompt",
            )
            .unwrap();
            std::fs::write(global.join("commands/review.md"), "global prompt").unwrap();
            let cmds = discover(&global, &project);
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0].template, "project prompt");
            let _ = std::fs::remove_dir_all(&project);
            let _ = std::fs::remove_dir_all(&global);
        }
    }
}

pub mod commands {
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
                name: "project",
                desc: "named project buckets for sessions",
                category: "session",
            },
            CommandMeta {
                name: "worktree",
                desc: "managed git worktrees; move re-roots this live session",
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
                name: "secrets",
                desc: "secure secret entry: masked form, list, rotate, delete (never values)",
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
                name: "oauth",
                desc: "connect hosted apps via local OAuth2: login/status/logout",
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn registry_covers_every_dispatched_command_including_project() {
            // /project is dispatched by handle_slash_inner but used to be
            // missing here, so completion never suggested it and the
            // built-in-vs-skill check did not treat it as built-in.
            let reg = registry();
            assert!(reg.contains_key("project"), "registry lacks /project");
            assert!(is_builtin("project"));
            assert_eq!(reg["project"].desc, "named project buckets for sessions");
        }

        #[test]
        fn complete_suggests_project() {
            let suggestions = complete("proj");
            assert!(
                suggestions.iter().any(|s| s == "/project"),
                "complete('proj') = {suggestions:?}"
            );
        }
    }
}

pub mod prompt {
    //! Running one component at a time, on the real terminal.
    //!
    //! The setup wizard is a sequence of these. Each call opens the alternate
    //! screen, runs the event loop until the widget submits or cancels, and
    //! returns the value. The widgets themselves are in `pantheon-tui` and know
    //! nothing about terminals; this is the only place that does.
    //!
    //! Cancelling returns `None` rather than a default. A wizard that invents an
    //! answer the user did not give writes a config they did not ask for, and
    //! the failure shows up later as an agent that talks to the wrong provider.

    use crate::app::{Screen, TuiApp};
    use crate::widget::{Confirm, Item, MultiSelect, Select, TextInput};

    /// Draw and run a `Select` until the user chooses. `None` means cancelled.
    pub fn pick_one(title: &str, subtitle: &str, items: Vec<Item>) -> Option<String> {
        let widget = Select::new(title, items).hint(format!(
            "up/down navigate  type to filter  enter select  esc back  ({subtitle})"
        ));
        let mut app = TuiApp::new();
        app.push(Screen::select(subtitle, widget));
        if app.run().is_err() {
            return None;
        }
        app.take_value().and_then(|s| s.one().map(String::from))
    }

    /// Draw and run a `TextInput`. `None` means cancelled.
    pub fn pick_text(title: &str, subtitle: &str, placeholder: &str) -> Option<String> {
        let widget = TextInput::new(title)
            .hint("enter confirm  esc back".to_string())
            .placeholder(placeholder.to_string());
        let mut app = TuiApp::new();
        app.push(Screen::text(subtitle, widget));
        if app.run().is_err() {
            return None;
        }
        app.take_value().and_then(|s| s.text().map(String::from))
    }

    /// A masked text input for secrets: the value is real, the display is
    /// dots. Used when the wizard collects an API key value to save to
    /// `.env`, so the key is not echoed across the terminal.
    pub fn pick_secret_text(title: &str, subtitle: &str, placeholder: &str) -> Option<String> {
        let widget = TextInput::new(title)
            .hint("enter confirm  esc back".to_string())
            .placeholder(placeholder.to_string())
            .secret();
        let mut app = TuiApp::new();
        app.push(Screen::text(subtitle, widget));
        if app.run().is_err() {
            return None;
        }
        app.take_value().and_then(|s| s.text().map(String::from))
    }

    /// Draw and run a `MultiSelect`. `None` means cancelled (esc); `Some`
    /// (possibly empty) is the confirmed check set, in list order.
    pub fn pick_many(
        title: &str,
        subtitle: &str,
        items: Vec<Item>,
        preselected: &[&str],
    ) -> Option<Vec<String>> {
        let widget = MultiSelect::new(title, items, preselected);
        let mut app = TuiApp::new();
        app.push(Screen::multi(subtitle, widget));
        if app.run().is_err() {
            return None;
        }
        app.take_value().and_then(|s| s.many().map(|v| v.to_vec()))
    }

    /// Draw and run a `Confirm`. `None` means the run errored; esc submits
    /// `false`, so cancelling is always the safe answer.
    pub fn pick_confirm(title: &str, question: &str, default_yes: bool) -> Option<bool> {
        let widget = Confirm::new(title, question).default_yes(default_yes);
        let mut app = TuiApp::new();
        app.push(Screen::confirm(question, widget));
        if app.run().is_err() {
            return None;
        }
        app.take_value().and_then(|s| s.flag())
    }
}

pub mod mentions {
    //! `@` file mentions: parse `@path` tokens from the prompt, offer a fuzzy
    //! file picker, and attach file contents as context on submit.
    //!
    //! The picker lists files under the working directory, respecting
    //! `.gitignore` and always skipping VCS/build noise (`.git`, `target`,
    //! `node_modules`, ...). On submit, `resolve_mentions` reads each mentioned
    //! file: text goes to the model inside a delimited block, binaries and
    //! unreadable files are reported and skipped, and total attached bytes are
    //! capped so one `@` cannot blow the context window.

    use std::path::{Path, PathBuf};

    /// Most candidates the picker ever shows.
    pub const MAX_RESULTS: usize = 100;
    /// Total bytes of file content attached to one submitted prompt.
    pub const MAX_ATTACH_BYTES: usize = 200 * 1024;
    /// A single file is never read past this (protects the picker too).
    pub const MAX_FILE_BYTES: usize = 1024 * 1024;
    /// Directories never descended into, even without a .gitignore.
    const ALWAYS_SKIP: &[&str] = &[
        ".git",
        ".hg",
        ".svn",
        "target",
        "node_modules",
        ".venv",
        "venv",
        "__pycache__",
        ".tox",
        "dist",
        "build",
        ".next",
        ".nuxt",
        ".cache",
    ];

    /// Extract `@path` mentions from prompt text, in order of appearance.
    ///
    /// A mention starts at `@` and runs to the next whitespace. A lone `@`
    /// (end of input, or followed by whitespace) is not a mention - it is the
    /// trigger that opens the picker.
    pub fn extract_mentions(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        let bytes = text.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'@' {
                let mut j = i + 1;
                while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j > i + 1 {
                    out.push(text[i + 1..j].to_string());
                }
                i = j;
            } else {
                i += 1;
            }
        }
        out
    }

    /// True when `path` looks binary: a NUL byte in the first 8 KiB.
    pub fn is_binary(data: &[u8]) -> bool {
        data.iter().take(8192).any(|&b| b == 0)
    }

    /// Minimal `.gitignore` matcher: `*` wildcards, trailing `/` for dirs,
    /// `!` negation is not supported (documented; rare in practice).
    #[derive(Debug, Default)]
    pub struct IgnoreRules {
        patterns: Vec<String>,
    }

    impl IgnoreRules {
        pub fn load(dir: &Path) -> Self {
            let mut rules = Self::default();
            if let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) {
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    rules
                        .patterns
                        .push(line.trim_start_matches('/').to_string());
                }
            }
            rules
        }

        fn matches(&self, rel: &str, is_dir: bool) -> bool {
            for pat in &self.patterns {
                let (pat, dir_only) = match pat.strip_suffix('/') {
                    Some(p) => (p, true),
                    None => (pat.as_str(), false),
                };
                if dir_only && !is_dir {
                    continue;
                }
                if glob_match(pat, rel) || rel.split('/').any(|seg| glob_match(pat, seg)) {
                    return true;
                }
            }
            false
        }
    }

    /// `*`/`?` glob over a single path segment (no `/` crossing).
    fn glob_match(pat: &str, name: &str) -> bool {
        let (p, n) = (pat.as_bytes(), name.as_bytes());
        // Recursive matcher; patterns are tiny.
        fn go(p: &[u8], n: &[u8]) -> bool {
            if p.is_empty() {
                return n.is_empty();
            }
            match p[0] {
                b'*' => {
                    // Collapse runs of stars.
                    let mut p = &p[1..];
                    while p.first() == Some(&b'*') {
                        p = &p[1..];
                    }
                    if p.is_empty() {
                        return true;
                    }
                    for i in 0..=n.len() {
                        if go(p, &n[i..]) {
                            return true;
                        }
                    }
                    false
                }
                b'?' => !n.is_empty() && go(&p[1..], &n[1..]),
                c => !n.is_empty() && n[0] == c && go(&p[1..], &n[1..]),
            }
        }
        go(p, n)
    }

    /// Case-insensitive subsequence (fuzzy) match. Lower score is better.
    fn fuzzy_score(query: &str, candidate: &str) -> Option<u32> {
        if query.is_empty() {
            return Some(0);
        }
        let q: Vec<char> = query.to_lowercase().chars().collect();
        let c: Vec<char> = candidate.to_lowercase().chars().collect();
        let (mut qi, mut score, mut consecutive) = (0usize, 0u32, 0u32);
        for (i, &ch) in c.iter().enumerate() {
            if qi < q.len() && ch == q[qi] {
                // Bonus for matches at segment starts or consecutive runs.
                let boundary = i == 0 || c[i - 1] == '/' || c[i - 1] == '_' || c[i - 1] == '-';
                score += i as u32 + if boundary { 0 } else { 5 } - consecutive.min(4);
                consecutive += 1;
                qi += 1;
            } else {
                consecutive = 0;
            }
        }
        if qi == q.len() {
            Some(score)
        } else {
            None
        }
    }

    /// List files under `dir` matching `query`, best matches first, capped at
    /// [`MAX_RESULTS`]. Respects `.gitignore` and [`ALWAYS_SKIP`].
    pub fn list_files(dir: &Path, query: &str) -> Vec<String> {
        let rules = IgnoreRules::load(dir);
        let mut found: Vec<(u32, String)> = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        // Bound the walk so giant trees cannot hang the picker.
        let mut visited = 0usize;
        while let Some(d) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for entry in entries.flatten() {
                visited += 1;
                if visited > 20_000 {
                    break;
                }
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().into_owned();
                let ft = entry.file_type().ok();
                let is_dir = ft.map(|t| t.is_dir()).unwrap_or(false);
                if is_dir && ALWAYS_SKIP.contains(&name.as_str()) {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(dir) else {
                    continue;
                };
                let rel = rel.to_string_lossy().replace('\\', "/");
                if rules.matches(&rel, is_dir) {
                    continue;
                }
                if is_dir {
                    stack.push(path);
                    continue;
                }
                if let Some(score) = fuzzy_score(query, &rel) {
                    found.push((score, rel));
                }
                if found.len() >= MAX_RESULTS * 4 {
                    // Enough raw candidates; ranking trims to MAX_RESULTS.
                    break;
                }
            }
        }
        found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.len().cmp(&b.1.len())));
        found.truncate(MAX_RESULTS);
        found.into_iter().map(|(_, p)| p).collect()
    }

    /// One attached file: its mention path and what happened.
    #[derive(Debug)]
    pub enum Attachment {
        /// Text content, possibly truncated to the byte budget.
        Text {
            path: String,
            content: String,
            truncated: bool,
        },
        /// Skipped with a human reason shown in the transcript.
        Skipped { path: String, reason: String },
    }

    /// Resolve `@` mentions in `text` against `cwd`.
    ///
    /// Returns `(display, model)`: `display` is what the transcript shows
    /// (mentions annotated with size/skip reason), `model` is what the agent
    /// receives (original text plus a delimited attachment section).
    pub fn resolve_mentions(text: &str, cwd: &Path) -> (String, String) {
        let mentions = extract_mentions(text);
        if mentions.is_empty() {
            return (text.to_string(), text.to_string());
        }
        let mut attachments: Vec<Attachment> = Vec::new();
        let mut budget = MAX_ATTACH_BYTES;
        let mut seen = std::collections::HashSet::new();
        // Confine every mention to the working directory: no `..` escapes,
        // no absolute paths outside the tree.
        let canon_cwd = cwd.canonicalize().ok();
        for m in mentions {
            if !seen.insert(m.clone()) {
                continue;
            }
            let disk = cwd.join(&m);
            let Some(root) = canon_cwd.as_deref() else {
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: "cannot resolve working directory".into(),
                });
                continue;
            };
            let target = match disk.canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    // Name the common case honestly: a mention that matches
                    // nothing is "no such file", not a sandbox violation.
                    let reason = if disk.symlink_metadata().is_err() {
                        "no such file"
                    } else {
                        "cannot resolve path"
                    };
                    attachments.push(Attachment::Skipped {
                        path: m,
                        reason: reason.into(),
                    });
                    continue;
                }
            };
            if !target.starts_with(root) {
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: "outside working directory".into(),
                });
                continue;
            }
            if target.is_dir() {
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: "is a directory".into(),
                });
                continue;
            }
            let data = match std::fs::read(&target) {
                Ok(d) => d,
                Err(e) => {
                    attachments.push(Attachment::Skipped {
                        path: m,
                        reason: format!("unreadable: {e}"),
                    });
                    continue;
                }
            };
            if data.len() > MAX_FILE_BYTES {
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: format!("over {} file limit", fmt_bytes(MAX_FILE_BYTES)),
                });
                continue;
            }
            if is_binary(&data) {
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: "binary file".into(),
                });
                continue;
            }
            let content = String::from_utf8_lossy(&data).into_owned();
            if budget == 0 {
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: format!("over {} attach budget", fmt_bytes(MAX_ATTACH_BYTES)),
                });
                continue;
            }
            let mut take = budget.min(content.len());
            // Never split a UTF-8 char: back up to the char boundary.
            while take > 0 && !content.is_char_boundary(take) {
                take -= 1;
            }
            let truncated = take < content.len();
            budget -= take;
            attachments.push(Attachment::Text {
                path: m,
                content: content[..take].to_string(),
                truncated,
            });
        }

        // Display: annotate each mention in place.
        let mut display = text.to_string();
        for a in &attachments {
            let (path, note) = match a {
                Attachment::Text {
                    path,
                    content,
                    truncated,
                } => (
                    path.clone(),
                    format!("attached, {}", fmt_bytes(content.len()))
                        + if *truncated { " (truncated)" } else { "" },
                ),
                Attachment::Skipped { path, reason } => {
                    (path.clone(), format!("skipped: {reason}"))
                }
            };
            display = display.replacen(&format!("@{path}"), &format!("@{path} [{note}]"), 1);
        }

        // Model: original text plus the attachment section.
        let mut model = text.to_string();
        let texts: Vec<&Attachment> = attachments
            .iter()
            .filter(|a| matches!(a, Attachment::Text { .. }))
            .collect();
        if !texts.is_empty() {
            model.push_str("\n\n<attached_files>\n");
            for a in texts {
                if let Attachment::Text {
                    path,
                    content,
                    truncated,
                } = a
                {
                    model.push_str(&format!("--- file: {path} ---\n{content}"));
                    if !content.ends_with('\n') {
                        model.push('\n');
                    }
                    if *truncated {
                        model.push_str("(truncated to attach budget)\n");
                    }
                }
            }
            model.push_str("</attached_files>");
        }
        (display, model)
    }

    fn fmt_bytes(n: usize) -> String {
        if n >= 1024 {
            format!("{:.1}KB", n as f64 / 1024.0)
        } else {
            format!("{n}B")
        }
    }

    /// Interactive `@` picker state, owned by the TUI loop. `anchor` is the
    /// byte offset in the prompt input where the triggering `@` sits, so
    /// selecting a file replaces `@<filter>` with `@<path> ` in place.
    #[derive(Debug)]
    pub struct MentionPicker {
        pub cwd: PathBuf,
        pub anchor: usize,
        pub input: String,
        pub files: Vec<String>,
        pub sel: usize,
    }

    impl MentionPicker {
        pub fn open(cwd: PathBuf, anchor: usize) -> Self {
            let mut p = Self {
                cwd,
                anchor,
                input: String::new(),
                files: Vec::new(),
                sel: 0,
            };
            p.requery();
            p
        }

        /// Re-run the file search for the current filter text.
        pub fn requery(&mut self) {
            self.files = list_files(&self.cwd, &self.input);
            self.sel = 0;
        }

        pub fn move_sel(&mut self, n: isize) {
            if self.files.is_empty() {
                self.sel = 0;
                return;
            }
            let len = self.files.len() as isize;
            self.sel = (self.sel as isize + n).clamp(0, len - 1) as usize;
        }

        pub fn selected(&self) -> Option<&str> {
            self.files.get(self.sel).map(String::as_str)
        }

        /// Splice the selected path into `input`, replacing `@<filter>`.
        /// Returns the new input string.
        pub fn apply_to(&self, input: &str) -> String {
            let path = match self.selected() {
                Some(p) => p,
                None => return input.to_string(),
            };
            // The mention runs from `anchor` to the end of the current
            // `@`-token (anchor + 1 + filter length), or to input end.
            let token_end = (self.anchor + 1 + self.input.len()).min(input.len());
            let mut out = String::with_capacity(input.len() + path.len());
            out.push_str(&input[..self.anchor.min(input.len())]);
            out.push('@');
            out.push_str(path);
            out.push(' ');
            out.push_str(&input[token_end.min(input.len())..]);
            out
        }
    }
}

pub mod acp {
    //! Agent Client Protocol (ACP) server over stdio.
    //!
    //! `pantheon serve acp` speaks newline-delimited JSON-RPC 2.0 on stdin/stdout,
    //! which is what Zed and other ACP clients expect. It maps one ACP session to
    //! one Pantheon run:
    //!
    //! * `initialize` advertises protocol v1 and the capabilities we implement;
    //! * `session/new` / `session/load` create or reopen a run and set the
    //!   session workspace to the client's `cwd`;
    //! * `session/prompt` streams `session/update` notifications while the model
    //!   and tools work, then returns an ACP `stopReason`;
    //! * approval-gated tool calls become `session/request_permission` requests;
    //! * UI approval cards (`render_ui` parks) become the same
    //!   `session/request_permission` requests, resolved through the runtime's
    //!   revision-pinned `submit_ui_action`; document content degrades to
    //!   markdown for every client, and the raw `UiDocument` rides the
    //!   `_pantheon/uiDocument` extension notification for clients that opt in;
    //!   `ask_user` questions become an assistant message and are answered by the
    //!   next `session/prompt` (the run is parked, so the text is routed through
    //!   `Supervisor::answer_input`, not treated as a fresh user turn).
    //!
    //! The server is deliberately one prompt at a time. `Session` already owns a
    //! single run lease and process-global working directory; pretending to run
    //! concurrent ACP prompts would let two clients race those resources.
    //!
    //! This module is transport and translation only. Tool gating, approvals,
    //! ledger persistence, and the actual turn loop remain in the runtime.

    use crate::config;
    use pantheon_api::events::Event;
    use pantheon_api::mode::AgentMode;
    use pantheon_runtime::session::Session;
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::io::{BufRead, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    const PROTOCOL_VERSION: u64 = 1;
    const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

    type PendingReply = mpsc::Sender<Value>;

    struct Shared {
        out: Mutex<std::io::Stdout>,
        pending: Mutex<HashMap<String, PendingReply>>,
        next_id: AtomicU64,
        /// ACP session id -> Pantheon run id.
        sessions: Mutex<HashMap<String, String>>,
        /// The run currently owned by a prompt worker, if any.
        active: Mutex<Option<String>>,
        /// A parked `ask_user` keyed by ACP session id: (call id, options).
        pending_input: Mutex<HashMap<String, (String, Vec<String>)>>,
    }

    impl Shared {
        fn new() -> Self {
            Self {
                out: Mutex::new(std::io::stdout()),
                pending: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
                sessions: Mutex::new(HashMap::new()),
                active: Mutex::new(None),
                pending_input: Mutex::new(HashMap::new()),
            }
        }

        fn send(&self, value: Value) {
            if let Ok(mut out) = self.out.lock() {
                let _ = writeln!(out, "{value}");
                let _ = out.flush();
            }
        }

        fn respond(&self, id: Value, result: Value) {
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
        }

        fn error(&self, id: Value, code: i64, message: impl Into<String>) {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message.into()}
            }));
        }

        fn notify(&self, method: &str, params: Value) {
            self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
        }

        fn update(&self, session_id: &str, update: Value) {
            self.notify(
                "session/update",
                json!({"sessionId": session_id, "update": update}),
            );
        }

        /// Rich-UI documents: markdown fallback for every ACP client (the
        /// protocol's own degradation rule), plus an extension notification with
        /// the raw `UiDocument` for clients that opt into native rendering.
        /// Extension delivery is additive: standard clients ignore the method.
        fn ui_document(
            &self,
            session_id: &str,
            ui_id: &str,
            revision: u64,
            document: &pantheon_api::ui::UiDocument,
        ) {
            let markdown = ui_document_markdown(document);
            if !markdown.is_empty() {
                self.text_chunk(session_id, &markdown);
            }
            if let Ok(doc) = serde_json::to_value(document) {
                self.notify(
                    "_pantheon/uiDocument",
                    json!({"sessionId": session_id, "uiId": ui_id, "revision": revision, "document": doc}),
                );
            }
        }

        fn tool_call(&self, session_id: &str, call_id: &str, tool: &str, args: &str, status: &str) {
            self.update(
                session_id,
                json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": call_id,
                    "title": tool,
                    "kind": tool_kind(tool),
                    "status": status,
                    "content": [{"type": "content", "content": {"type": "text", "text": args}}],
                    "rawInput": parse_raw_input(args),
                }),
            );
        }

        fn tool_update(&self, session_id: &str, call_id: &str, status: &str, text: Option<&str>) {
            let mut update = json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "status": status,
            });
            if let Some(text) = text {
                update["content"] = json!([{
                    "type": "content",
                    "content": {"type": "text", "text": text}
                }]);
            }
            self.update(session_id, update);
        }

        fn text_chunk(&self, session_id: &str, text: &str) {
            if text.is_empty() {
                return;
            }
            self.update(
                session_id,
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                }),
            );
        }

        /// Send a request to the client and wait for its response. The reader
        /// loop stays free while the prompt worker blocks here, so a permission
        /// answer (or a cancel) can always be read.
        fn request_client(&self, method: &str, params: Value) -> Option<Value> {
            let id = format!(
                "pantheon-{}-{}",
                method.replace('/', "-"),
                self.next_id.fetch_add(1, Ordering::SeqCst)
            );
            let (tx, rx) = mpsc::channel();
            if let Ok(mut pending) = self.pending.lock() {
                pending.insert(id.clone(), tx);
            }
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
            match rx.recv_timeout(PERMISSION_TIMEOUT) {
                Ok(response) => response.get("result").cloned(),
                Err(_) => {
                    if let Ok(mut pending) = self.pending.lock() {
                        pending.remove(&id);
                    }
                    None
                }
            }
        }

        fn resolve_response(&self, response: &Value) -> bool {
            let Some(id) = response.get("id") else {
                return false;
            };
            let key = match id {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                _ => return false,
            };
            let tx = self.pending.lock().ok().and_then(|mut p| p.remove(&key));
            if let Some(tx) = tx {
                let _ = tx.send(response.clone());
                true
            } else {
                false
            }
        }
    }

    /// Entry point for `pantheon serve acp`.
    pub fn run_stdio() {
        let data_dir = crate::terminal::data_dir();
        let session = match open_session(&data_dir) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                eprintln!("acp: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("pantheon acp: protocol v{PROTOCOL_VERSION} on stdio");
        let shared = Arc::new(Shared::new());
        let stdin = std::io::stdin();
        let mut line = String::new();
        loop {
            line.clear();
            match stdin.lock().read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) => {
                    eprintln!("acp: read stdin: {e}");
                    break;
                }
            }
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let message: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    shared.error(Value::Null, -32700, format!("parse error: {e}"));
                    continue;
                }
            };
            // Responses to requests we sent (permission prompts).
            if message.get("method").is_none()
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                shared.resolve_response(&message);
                continue;
            }
            let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
                continue;
            };
            let id = message.get("id").cloned();
            let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
            match method {
                "initialize" => {
                    if let Some(id) = id {
                        shared.respond(
                            id,
                            json!({
                                "protocolVersion": PROTOCOL_VERSION,
                                "agentCapabilities": {
                                    "loadSession": true,
                                    "promptCapabilities": {
                                        "image": false,
                                        "audio": false,
                                        "embeddedContext": true
                                    },
                                    "mcpCapabilities": {"http": false, "sse": false}
                                },
                                "authMethods": [],
                                "agentInfo": {
                                    "name": "pantheon",
                                    "version": env!("CARGO_PKG_VERSION")
                                }
                            }),
                        );
                    }
                }
                "authenticate" => {
                    if let Some(id) = id {
                        shared.respond(id, json!({}));
                    }
                }
                "session/new" => {
                    let Some(id) = id else { continue };
                    let cwd = params
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from)
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_else(|| PathBuf::from("."));
                    if let Err(e) = session.set_workspace_root(&cwd) {
                        shared.error(id, -32000, format!("set cwd: {e}"));
                        continue;
                    }
                    let run_id = pantheon_runtime::new_run_id();
                    if let Ok(mut sessions) = shared.sessions.lock() {
                        sessions.insert(run_id.clone(), run_id.clone());
                    }
                    shared.respond(
                        id,
                        json!({
                            "sessionId": run_id,
                            "modes": {
                                "currentModeId": agent_mode_id(session.mode()),
                                "availableModes": [
                                    {"id": "build", "name": "Build", "description": "Edit and run code."},
                                    {"id": "plan", "name": "Plan", "description": "Read-only planning."}
                                ]
                            }
                        }),
                    );
                }
                "session/load" => {
                    let Some(id) = id else { continue };
                    let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) else {
                        shared.error(id, -32602, "session/load requires sessionId");
                        continue;
                    };
                    let cwd = params
                        .get("cwd")
                        .and_then(|v| v.as_str())
                        .map(PathBuf::from);
                    if let Some(cwd) = cwd {
                        if let Err(e) = session.set_workspace_root(&cwd) {
                            shared.error(id, -32000, format!("set cwd: {e}"));
                            continue;
                        }
                    }
                    let run_id = session_id.to_string();
                    if let Ok(mut sessions) = shared.sessions.lock() {
                        sessions.insert(session_id.to_string(), run_id.clone());
                    }
                    replay_run(&shared, session_id, &run_id, &session);
                    shared.respond(id, json!({}));
                }
                "session/prompt" => {
                    let Some(id) = id else { continue };
                    let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) else {
                        shared.error(id, -32602, "session/prompt requires sessionId");
                        continue;
                    };
                    let run_id = shared
                        .sessions
                        .lock()
                        .ok()
                        .and_then(|s| s.get(session_id).cloned())
                        .unwrap_or_else(|| session_id.to_string());
                    if shared.active.lock().map(|a| a.is_some()).unwrap_or(false) {
                        shared.error(id, -32000, "another prompt is already running");
                        continue;
                    }
                    let text = prompt_text(&params);
                    if text.trim().is_empty() {
                        shared.error(id, -32602, "prompt contains no text");
                        continue;
                    }
                    if let Ok(mut active) = shared.active.lock() {
                        *active = Some(run_id.clone());
                    }
                    let shared = Arc::clone(&shared);
                    let session = Arc::clone(&session);
                    let session_id = session_id.to_string();
                    std::thread::spawn(move || {
                        let result = run_prompt(&shared, &session, &session_id, &run_id, &text);
                        // The worker owns clearing `active`; do it before the
                        // response so a client can immediately send another turn.
                        if let Ok(mut active) = shared.active.lock() {
                            *active = None;
                        }
                        match result {
                            Ok(stop) => shared.respond(id, json!({"stopReason": stop})),
                            Err(e) => shared.error(id, -32000, e),
                        }
                    });
                }
                "session/cancel" => {
                    if let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) {
                        let run_id = shared
                            .sessions
                            .lock()
                            .ok()
                            .and_then(|s| s.get(session_id).cloned())
                            .unwrap_or_else(|| session_id.to_string());
                        session.cancel_current_run(&run_id, "ACP client cancelled");
                    }
                }
                "session/set_mode" => {
                    if let Some(id) = id {
                        let mode = params.get("modeId").and_then(|v| v.as_str());
                        session.set_mode(match mode {
                            Some("plan") => AgentMode::Plan,
                            _ => AgentMode::Build,
                        });
                        shared.respond(id, json!({}));
                    }
                }
                "session/set_model" => {
                    if let Some(id) = id {
                        let raw = params.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                        let (provider, model) = raw.split_once('/').unwrap_or(("", raw));
                        if provider.is_empty() || model.is_empty() {
                            shared.error(id, -32602, "modelId must be provider/model");
                        } else {
                            match session.switch_model(provider, model) {
                                Ok(()) => shared.respond(id, json!({})),
                                Err(e) => shared.error(id, -32000, e.to_string()),
                            }
                        }
                    }
                }
                "session/close" => {
                    if let Some(id) = id {
                        shared.respond(id, json!({}));
                    }
                }
                _ => {
                    if let Some(id) = id {
                        shared.error(id, -32601, format!("method not found: {method}"));
                    }
                }
            }
        }
        session.cancel_current_run("", "ACP client disconnected");
    }

    fn open_session(data_dir: &Path) -> Result<Session, String> {
        let cfg = config::Config::load(data_dir).ok();
        let model_policy = config::build_model_policy(cfg.as_ref(), None, None);
        let policy = crate::config_schema::policy_for_config(&cfg);
        let secrets = config::chat_secrets(cfg.as_ref());
        let session = Session::new(data_dir.to_path_buf(), policy, model_policy, secrets)
            .map_err(|e| format!("open session: {e}"))?;
        config::apply_tool_enablement(&session, cfg.as_ref());
        config::apply_budget_tiers(&session, cfg.as_ref());
        config::apply_delegation_section(&session, cfg.as_ref());
        Ok(session)
    }

    /// One prompt, including approval/input resume loops. Returns an ACP
    /// `stopReason` on success.
    fn run_prompt(
        shared: &Arc<Shared>,
        session: &Session,
        session_id: &str,
        run_id: &str,
        text: &str,
    ) -> Result<&'static str, String> {
        session.reset_cancel();
        let mut next_text = text.to_string();
        // A parked ask_user is answered by the next prompt instead of starting a
        // new model turn. The runtime emits the answer as the tool result.
        let pending = shared
            .pending_input
            .lock()
            .ok()
            .and_then(|mut p| p.remove(session_id));
        if let Some((call_id, _options)) = pending {
            session
                .supervisor
                .answer_input(run_id, &call_id, text)
                .map_err(|e| format!("answer question: {e}"))?;
            shared.notify(
                "_pantheon/input_resolved",
                json!({"sessionId": session_id, "requestId": call_id}),
            );
            next_text.clear();
        }

        for _ in 0..12 {
            let saw_delta = Arc::new(AtomicBool::new(false));
            let approvals = Arc::new(Mutex::new(Vec::<String>::new()));
            let stream = {
                let shared = Arc::clone(shared);
                let saw_delta = Arc::clone(&saw_delta);
                let approvals = Arc::clone(&approvals);
                let session_id = session_id.to_string();
                let run_id = run_id.to_string();
                session
                    .supervisor
                    .register_observer(Arc::new(move |event: &Event| match event {
                        Event::ModelDelta { run_id: r, delta } if r == &run_id => {
                            saw_delta.store(true, Ordering::Relaxed);
                            shared.text_chunk(&session_id, delta);
                        }
                        Event::AssistantMessage { run_id: r, message } if r == &run_id => {
                            if !saw_delta.load(Ordering::Relaxed) && !message.content.is_empty() {
                                shared.text_chunk(&session_id, &message.content);
                            }
                        }
                        Event::ToolStarted {
                            run_id: r,
                            call_id,
                            tool,
                            args,
                            ..
                        } if r == &run_id => {
                            shared.tool_call(&session_id, call_id, tool, args, "in_progress")
                        }
                        Event::ToolMessage { run_id: r, message } if r == &run_id => {
                            if let Some(call_id) = &message.tool_call_id {
                                shared.tool_update(
                                    &session_id,
                                    call_id,
                                    "completed",
                                    Some(&message.content),
                                );
                            }
                        }
                        Event::ApprovalRequested { run_id: r, scope } if r == &run_id => {
                            if let Ok(mut a) = approvals.lock() {
                                a.push(scope.clone());
                            }
                        }
                        Event::UiContent {
                            run_id: r,
                            ui_id,
                            document,
                            ..
                        } if r == &run_id => {
                            shared.ui_document(&session_id, ui_id, 0, document);
                        }
                        Event::UiUpdated {
                            run_id: r,
                            ui_id,
                            revision,
                            document,
                            ..
                        } if r == &run_id => {
                            shared.ui_document(&session_id, ui_id, *revision, document);
                        }
                        _ => {}
                    }))
            };

            let outcome = session.chat_turn(run_id, "", &next_text);
            drop(stream);
            let outcome = match outcome {
                Ok(o) => o,
                Err(e) if e.code == "RUN_PARKED" => {
                    // The run already rejected the turn because it was parked.
                    // Fall through to the pending-approval loop below instead of
                    // surfacing a transport error.
                    let scopes = session
                        .supervisor
                        .pending_approvals(run_id)
                        .unwrap_or_default();
                    if scopes.is_empty() {
                        return Err(e.to_string());
                    }
                    for scope in scopes {
                        if !resolve_approval(shared, session, session_id, run_id, &scope)? {
                            return Ok("cancelled");
                        }
                    }
                    next_text.clear();
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            };

            match outcome {
                pantheon_agent::LoopOutcome::Answered { .. } => return Ok("end_turn"),
                pantheon_agent::LoopOutcome::Canceled { .. } => return Ok("cancelled"),
                pantheon_agent::LoopOutcome::Denied { .. } => return Ok("refusal"),
                pantheon_agent::LoopOutcome::BudgetExhausted { cap } => {
                    return Ok(if cap.contains("token") {
                        "max_tokens"
                    } else {
                        "max_turn_requests"
                    });
                }
                pantheon_agent::LoopOutcome::AwaitingInput {
                    call_id,
                    question,
                    options,
                } => {
                    shared.text_chunk(session_id, &question);
                    // First-class ask-user: the notification carries the question
                    // and its options so a client can render its question form;
                    // the answer arrives as the next prompt (see run_prompt).
                    shared.notify(
                        "_pantheon/input_request",
                        json!({
                            "sessionId": session_id,
                            "requestId": call_id,
                            "request": {
                                "kind": "questions",
                                "questions": [{
                                    "id": "answer",
                                    "question": question,
                                    "isOther": true,
                                    "options": options
                                        .iter()
                                        .map(|option| json!({"label": option}))
                                        .collect::<Vec<_>>(),
                                }],
                            },
                        }),
                    );
                    if let Ok(mut pending) = shared.pending_input.lock() {
                        pending.insert(session_id.to_string(), (call_id, options));
                    }
                    return Ok("end_turn");
                }
                pantheon_agent::LoopOutcome::AwaitingUiAction { .. } => {
                    // A UI approval card parked the turn. Surface it as the same
                    // client permission request a gated tool call gets; the
                    // decision resumes the run through the runtime's own
                    // revision-pinned submission path, and the loop drains the
                    // resumed turn below.
                    resolve_ui_action(shared, session, session_id, run_id)?;
                    next_text.clear();
                }
                pantheon_agent::LoopOutcome::AwaitingApproval { scope, .. } => {
                    let scopes = {
                        let mut all = session
                            .supervisor
                            .pending_approvals(run_id)
                            .unwrap_or_default();
                        if !all.iter().any(|s| s == &scope) {
                            all.push(scope);
                        }
                        all
                    };
                    for scope in scopes {
                        if !resolve_approval(shared, session, session_id, run_id, &scope)? {
                            return Ok("cancelled");
                        }
                    }
                    next_text.clear();
                }
                pantheon_agent::LoopOutcome::Delegated { .. } => return Ok("end_turn"),
            }
        }
        Err("turn parked too many times without resolving".into())
    }

    /// Collect every node of a serialized `UiDocument` as `(id, node)` pairs.
    /// Works on JSON so the walker survives component-enum additions without
    /// changes here; the runtime validates documents before they reach events.
    fn ui_nodes(value: &serde_json::Value, out: &mut Vec<(String, serde_json::Value)>) {
        match value {
            serde_json::Value::Array(items) => {
                for item in items {
                    ui_nodes(item, out);
                }
            }
            serde_json::Value::Object(map) => {
                if let (Some(id), Some(tag)) = (
                    map.get("id").and_then(|v| v.as_str()),
                    map.get("type").and_then(|v| v.as_str()),
                ) {
                    out.push((id.to_string(), value.clone()));
                    let _ = tag;
                }
                for nested in map.values() {
                    ui_nodes(nested, out);
                }
            }
            _ => {}
        }
    }

    /// The protocol's own degradation rule: unknown renderers must still leave
    /// the content readable. Markdown and tables render directly; every other
    /// component becomes a tagged placeholder; approval cards state their
    /// decision explicitly.
    fn ui_document_markdown(document: &pantheon_api::ui::UiDocument) -> String {
        let Ok(value) = serde_json::to_value(document) else {
            return String::new();
        };
        let mut nodes = Vec::new();
        ui_nodes(&value, &mut nodes);
        let mut parts: Vec<String> = Vec::new();
        for (_id, node) in nodes {
            match node["type"].as_str().unwrap_or_default() {
                "markdown" => {
                    if let Some(title) = node["title"].as_str() {
                        parts.push(format!("### {title}"));
                    }
                    if let Some(text) = node["text"].as_str() {
                        parts.push(text.to_string());
                    }
                }
                "table" => {
                    let headers: Vec<&str> = node["headers"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|h| h.as_str()).collect())
                        .unwrap_or_default();
                    if headers.is_empty() {
                        continue;
                    }
                    parts.push(format!("| {} |", headers.join(" | ")));
                    parts.push(format!("|{}|", "| --- ".repeat(headers.len())));
                    for row in node["rows"].as_array().into_iter().flatten() {
                        let cells: Vec<String> = row
                            .as_array()
                            .map(|c| {
                                c.iter()
                                    .map(|x| x.as_str().unwrap_or_default().to_string())
                                    .collect()
                            })
                            .unwrap_or_default();
                        parts.push(format!("| {} |", cells.join(" | ")));
                    }
                }
                "approval" => {
                    let title = node["title"].as_str().unwrap_or("Approval");
                    let detail = node["detail"].as_str().unwrap_or_default();
                    let approve = node["approve"]["label"].as_str().unwrap_or("approve");
                    let deny = node["deny"]["label"].as_str().unwrap_or("deny");
                    parts.push(format!(
                        "**{title}** - {detail}\n\nThe client decides: **{approve}** or {deny}."
                    ));
                }
                other if !other.is_empty() => parts.push(format!("\u{ab}{other}\u{bb}")),
                _ => {}
            }
        }
        parts.join("\n\n")
    }

    fn fresh_ui_nonce() -> String {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!(
            "acp-{}-{}",
            nanos,
            SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        )
    }

    /// Resolve a parked UI approval card through the client's standard
    /// permission request. The decision is submitted as a real
    /// `UiActionSubmission` against the displayed revision, so park resolution,
    /// audit, and replay follow the runtime's own rules instead of an ACP
    /// side channel.
    fn resolve_ui_action(
        shared: &Shared,
        session: &Session,
        session_id: &str,
        run_id: &str,
    ) -> Result<(), String> {
        let Some((ui_id, call_id)) = session
            .pending_ui_action(run_id)
            .map_err(|e| format!("pending ui action: {e}"))?
        else {
            return Ok(());
        };
        // Latest document for this ui_id: replays carry the full replacement.
        let entries = session
            .supervisor
            .replay(run_id)
            .map_err(|e| format!("replay: {e}"))?;
        let mut latest: Option<(u64, &pantheon_api::ui::UiDocument)> = None;
        for entry in &entries {
            match &entry.event {
                Event::UiContent {
                    ui_id: u, document, ..
                } if u == &ui_id => {
                    if latest.as_ref().is_none_or(|(rev, _)| *rev < 1) {
                        latest = Some((0, document));
                    }
                }
                Event::UiUpdated {
                    ui_id: u,
                    revision,
                    document,
                    ..
                } if u == &ui_id && latest.as_ref().is_none_or(|(rev, _)| *revision > *rev) => {
                    latest = Some((*revision, document));
                }
                _ => {}
            }
        }
        let Some((revision, document)) = latest else {
            return Ok(());
        };
        let Ok(doc_value) = serde_json::to_value(document) else {
            return Ok(());
        };
        let mut nodes = Vec::new();
        ui_nodes(&doc_value, &mut nodes);
        // The card's approve ref carries decision "grant", deny carries "deny"
        // (`ActionKind::Approval` is internally tagged, so `decision` is a
        // sibling of the `kind` string in JSON).
        let mut approve: Option<(&str, &str, &str)> = None; // (node_id, action_id, label)
        let mut deny: Option<(&str, &str, &str)> = None;
        let mut title = "Approve?";
        let mut detail = String::new();
        for (node_id, node) in &nodes {
            if node["type"].as_str() != Some("approval") {
                continue;
            }
            if let Some(t) = node["title"].as_str() {
                title = t;
            }
            if let Some(d) = node["detail"].as_str() {
                detail = d.to_string();
            }
            for (key, decision) in [("approve", "grant"), ("deny", "deny")] {
                if node[key]["decision"].as_str() != Some(decision) {
                    continue;
                }
                let found = (
                    node_id.as_str(),
                    node[key]["action_id"].as_str().unwrap_or_default(),
                    node[key]["label"].as_str().unwrap_or(decision),
                );
                if key == "approve" {
                    approve = Some(found);
                } else {
                    deny = Some(found);
                }
            }
        }
        let (
            Some((approve_node, approve_action, approve_label)),
            Some((deny_node, deny_action, deny_label)),
        ) = (approve, deny)
        else {
            // Card without a resolvable approve/deny pair: degrade to the text
            // path so the park stays operator-resolvable in a full client.
            shared.text_chunk(
                session_id,
                "This turn is waiting for a structured UI decision. Respond in the client UI, then continue the session.",
            );
            return Ok(());
        };
        let request = json!({
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": call_id,
                "title": "render_ui",
                "kind": "read",
                "status": "pending",
                "content": [{"type": "content", "content": {"type": "text", "text": format!("{title}\n{detail}")}}],
            },
            "options": [
                {"optionId": "allow_once", "name": approve_label, "kind": "allow_once"},
                {"optionId": "reject_once", "name": deny_label, "kind": "reject_once"}
            ]
        });
        let Some(response) = shared.request_client("session/request_permission", request) else {
            // No client answer: record the deny decision so the park resolves
            // instead of hanging the session.
            let chosen = (deny_node, deny_action);
            return submit_ui_decision(session, run_id, &ui_id, revision, chosen);
        };
        let approved = response
            .get("outcome")
            .and_then(|o| o.get("outcome"))
            .and_then(|o| o.as_str())
            == Some("selected")
            && response
                .get("outcome")
                .and_then(|o| o.get("optionId"))
                .and_then(|o| o.as_str())
                == Some("allow_once");
        let chosen = if approved {
            (approve_node, approve_action)
        } else {
            (deny_node, deny_action)
        };
        submit_ui_decision(session, run_id, &ui_id, revision, chosen)
    }

    fn submit_ui_decision(
        session: &Session,
        run_id: &str,
        ui_id: &str,
        revision: u64,
        (node_id, action_id): (&str, &str),
    ) -> Result<(), String> {
        let submission = pantheon_api::ui::UiActionSubmission {
            ui_id: ui_id.to_string(),
            revision,
            node_id: node_id.to_string(),
            action_id: action_id.to_string(),
            params: Default::default(),
            nonce: fresh_ui_nonce(),
        };
        session
            .submit_ui_action(run_id, &submission)
            .map(|_| ())
            .map_err(|e| format!("ui action submit: {e}"))
    }

    fn resolve_approval(
        shared: &Shared,
        session: &Session,
        session_id: &str,
        run_id: &str,
        scope: &str,
    ) -> Result<bool, String> {
        let (call_id, tool, args) = split_scope(scope);
        let request = json!({
            "sessionId": session_id,
            "toolCall": {
                "toolCallId": call_id,
                "title": tool,
                "kind": tool_kind(&tool),
                "status": "pending",
                "content": [{"type": "content", "content": {"type": "text", "text": args}}],
                "rawInput": parse_raw_input(&args),
            },
            "options": [
                {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}
            ]
        });
        let Some(response) = shared.request_client("session/request_permission", request) else {
            // No client answer: deny rather than silently running the call.
            let _ = session.supervisor.deny(run_id, scope);
            return Ok(false);
        };
        let outcome = response.get("outcome").and_then(|o| o.get("outcome"));
        let option = response
            .get("outcome")
            .and_then(|o| o.get("optionId"))
            .and_then(|o| o.as_str());
        match (outcome.and_then(|o| o.as_str()), option) {
            (Some("selected"), Some("allow_once")) => {
                session
                    .supervisor
                    .grant(run_id, scope)
                    .map_err(|e| format!("grant: {e}"))?;
                Ok(true)
            }
            (Some("selected"), Some("reject_once")) | (Some("cancelled"), _) => {
                session
                    .supervisor
                    .deny(run_id, scope)
                    .map_err(|e| format!("deny: {e}"))?;
                Ok(outcome.and_then(|o| o.as_str()) != Some("cancelled"))
            }
            _ => {
                let _ = session.supervisor.deny(run_id, scope);
                Ok(false)
            }
        }
    }

    fn replay_run(shared: &Shared, session_id: &str, run_id: &str, session: &Session) {
        let entries = match session.supervisor.replay(run_id) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries {
            match entry.event {
                Event::UserMessage { text, .. } => shared.update(
                    session_id,
                    json!({
                        "sessionUpdate": "user_message_chunk",
                        "content": {"type": "text", "text": text}
                    }),
                ),
                Event::AssistantMessage { message, .. } if !message.content.is_empty() => {
                    shared.text_chunk(session_id, &message.content)
                }
                Event::ToolStarted {
                    call_id,
                    tool,
                    args,
                    ..
                } => shared.tool_call(session_id, &call_id, &tool, &args, "completed"),
                Event::ToolMessage { message, .. } => {
                    if let Some(call_id) = message.tool_call_id {
                        shared.tool_update(
                            session_id,
                            &call_id,
                            "completed",
                            Some(&message.content),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    fn split_scope(scope: &str) -> (String, String, String) {
        let mut parts = scope.splitn(3, ':');
        let call = parts.next().unwrap_or(scope).to_string();
        let tool = parts.next().unwrap_or("tool").to_string();
        let args = parts.next().unwrap_or("").to_string();
        (call, tool, args)
    }

    fn tool_kind(tool: &str) -> &'static str {
        match tool {
            "read_file" | "list_dir" | "grep" | "glob" | "codegraph_search" | "session_search" => {
                "read"
            }
            "write_file" | "apply_files" | "apply_staged" | "edit" | "patch" => "edit",
            "delete_file" | "remove_file" => "delete",
            "move_file" | "rename_file" => "move",
            "shell" | "terminal_open" | "terminal_write" | "terminal_read" | "terminal_close" => {
                "execute"
            }
            "websearch" | "webfetch" | "browser" => "fetch",
            "todo" => "think",
            _ => "other",
        }
    }

    fn parse_raw_input(args: &str) -> Value {
        serde_json::from_str(args).unwrap_or_else(|_| json!(args))
    }

    fn prompt_text(params: &Value) -> String {
        let Some(prompt) = params.get("prompt").and_then(|p| p.as_array()) else {
            return String::new();
        };
        let mut out = String::new();
        for block in prompt {
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
        }
        out
    }

    fn agent_mode_id(mode: AgentMode) -> &'static str {
        match mode {
            AgentMode::Plan => "plan",
            AgentMode::Build => "build",
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn prompt_text_joins_text_blocks_and_ignores_other_kinds() {
            let params = json!({
                "prompt": [
                    {"type": "text", "text": "first"},
                    {"type": "resource_link", "uri": "file:///x"},
                    {"type": "text", "text": "second"}
                ]
            });
            assert_eq!(prompt_text(&params), "first\nsecond");
        }

        #[test]
        fn scope_splits_into_call_tool_and_verbatim_args() {
            let (call, tool, args) = split_scope("call_1_0:shell:{\"command\":\"ls\"}");
            assert_eq!(call, "call_1_0");
            assert_eq!(tool, "shell");
            assert_eq!(args, "{\"command\":\"ls\"}");
        }

        #[test]
        fn tool_kinds_cover_the_documented_editor_categories() {
            assert_eq!(tool_kind("read_file"), "read");
            assert_eq!(tool_kind("apply_files"), "edit");
            assert_eq!(tool_kind("shell"), "execute");
            assert_eq!(tool_kind("unknown"), "other");
        }

        #[test]
        fn agent_mode_ids_are_stable() {
            assert_eq!(agent_mode_id(AgentMode::Build), "build");
            assert_eq!(agent_mode_id(AgentMode::Plan), "plan");
        }
    }
}

pub mod agui {
    //! AG-UI CLI verb: serve, plus the post-grant resume. Thin surface over
    //! pantheon-api + pantheon-gateway; no business logic here.
    //!
    //! `pantheon serve` and `pantheon dashboard` are the same unified gateway
    //! listener: one token, one port, serving the dashboard control plane
    //! (`/api/*` + PWA assets) and the AG-UI surface (`/agui/*`, voice
    //! endpoints) together. The shared pieces below (session factory,
    //! voice edge, live-voice config, genui base, the [`AguiServeConfig`])
    //! live in one builder so the two commands cannot drift apart.
    use crate::terminal::data_dir;
    /// Reject a flag whose value cannot be used, naming the flag and what it
    /// expected. Silently falling back to a default meant the user talked to a
    /// different server than the one they asked for.
    fn fatal_bad_flag(flag: &str, value: &str, expected: &str) -> ! {
        eprintln!("serve: {flag} expects {expected}, got {value:?}");
        std::process::exit(2);
    }

    fn flag(args: &[String], name: &str) -> Option<String> {
        let mut i = 0;
        while i < args.len() {
            if args[i] == name && i + 1 < args.len() {
                return Some(args[i + 1].clone());
            }
            // `--port=8080` is the same flag, not an unknown one: the old
            // exact-match-only read silently served the default port.
            if let Some(v) = args[i].strip_prefix(&format!("{name}=")) {
                return Some(v.to_string());
            }
            i += 1;
        }
        None
    }

    /// The listener's single token: `PANTHEON_SERVE_TOKEN` when non-empty,
    /// else a fresh 256-bit token, printed once at startup. The dashboard
    /// control plane and the AG-UI routes share it - one front door.
    ///
    /// INVARIANT: the dashboard `App.token` and the gateway `AuthCtx.token`
    /// are the same value. Callers resolve the token once and pass it to
    /// both [`build_agui_serve_parts`] and the dashboard mount; the
    /// `AuthCtx` is built from the dashboard mount's own `auth_ctx()` so the
    /// two cannot drift.
    pub(crate) fn resolve_serve_token(command: &str) -> String {
        match std::env::var("PANTHEON_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty())
        {
            Some(t) => t,
            None => {
                let t = pantheon_gateway::http::generate_token();
                eprintln!("{command}: no PANTHEON_SERVE_TOKEN set; generated a one-time token:");
                eprintln!("  {t}");
                t
            }
        }
    }

    /// The CLI's approval callback: no live session to resume here, so the
    /// approval decision is surfaced on stderr for the operator.
    pub(crate) fn approval_callback() -> pantheon_dashboard::ApprovalCallback {
        std::sync::Arc::new(|run_id: &str, granted: bool| {
            eprintln!(
                "dashboard: approval {} for run {run_id}",
                if granted { "granted" } else { "denied" }
            );
        })
    }

    /// Build the AG-UI half of the unified serve surface: registers the
    /// session factory, builds the voice edge / live-voice config / genui
    /// base, and returns the [`AguiServeConfig`].
    ///
    /// `token` is the listener's single token ([`resolve_serve_token`]); the
    /// AG-UI web client needs it injected so its fetch calls carry auth.
    pub(crate) fn build_agui_serve_parts(
        data_dir: &std::path::Path,
        host: &str,
        port: u16,
        token: &str,
    ) -> pantheon_runtime::agui_serve::AguiServeConfig {
        use crate::config;
        use crate::config::build_model_policy;
        use std::sync::Arc;

        // Resolve the Session exactly as the terminal interface does: the
        // config document, its key names, and its auxiliary models. The AG-UI
        // surface used to translate a subset of config.toml into environment
        // variables and let the runtime read those, which silently dropped
        // every auxiliary section and any key name the runtime does not know.
        // A builder keeps one definition of "what a turn runs on".
        let file_cfg = config::Config::load_or_report(data_dir);
        let startup_cfg = file_cfg.clone();
        pantheon_runtime::agui::set_session_factory(Arc::new(move |dir: &std::path::PathBuf| {
            // Reload per turn so a config edited while the server runs is
            // picked up without a restart.
            let cfg = config::Config::load(dir).ok().or_else(|| file_cfg.clone());
            let model_policy = build_model_policy(cfg.as_ref(), None, None);
            let policy = crate::config_schema::policy_for_config(&cfg);
            let secrets = config::chat_secrets(cfg.as_ref());
            pantheon_runtime::session::Session::new(dir.clone(), policy, model_policy, secrets)
                .inspect(|s| {
                    config::apply_tool_enablement(s, cfg.as_ref());
                    config::apply_budget_tiers(s, cfg.as_ref());
                    config::apply_delegation_section(s, cfg.as_ref());
                })
        }));

        let base = std::env::var("PANTHEON_GENUI_BASE")
            .unwrap_or_else(|_| format!("http://{host}:{port}/agui/blob"));
        // Voice edge for the mobile app's speech endpoints: resolved from the
        // [stt]/[tts] sections the setup wizard writes. Absent sections mean
        // the routes 400 with `voice_not_configured`.
        let voice = pantheon_gateway::voice::VoiceEdge::from_config(
            startup_cfg.as_ref().and_then(|c| c.tools.as_ref()),
            startup_cfg.as_ref().and_then(|c| c.stt.as_ref()),
            startup_cfg.as_ref().and_then(|c| c.tts.as_ref()),
            &config::chat_secrets(startup_cfg.as_ref()),
        );
        // Live voice mode (`GET /agui/voice/live`): same double gate as the
        // edge, plus the `[voice]` limits. The per-session gate refuses when
        // `live_enabled` is false or a backend is missing.
        let live_voice = Arc::new(pantheon_gateway::live_voice::LiveVoiceConfig::from_config(
            startup_cfg.as_ref().and_then(|c| c.tools.as_ref()),
            startup_cfg.as_ref().and_then(|c| c.stt.as_ref()),
            startup_cfg.as_ref().and_then(|c| c.tts.as_ref()),
            startup_cfg
                .as_ref()
                .map(|c| c.live_voice())
                .unwrap_or_default(),
            &config::chat_secrets(startup_cfg.as_ref()),
        ));
        pantheon_runtime::agui_serve::AguiServeConfig {
            data_dir: data_dir.to_path_buf(),
            host: host.to_string(),
            port,
            genui_base: base,
            auth_token: Some(token.to_string()),
            voice,
            live_voice,
        }
    }

    pub fn cmd_serve(args: &[String]) {
        use crate::config;

        // `serve acp` is a stdio transport, not a listener. Dispatch it before
        // the HTTP help/config path: ACP clients may pass `--help` only when a
        // human asks, and no token or port is involved either way.
        if args.get(2).map(String::as_str) == Some("acp") {
            if args.iter().any(|a| a == "--help" || a == "-h") {
                println!(
                    "pantheon serve acp\n\
                     \n\
                     Speak the Agent Client Protocol (ACP) over stdin/stdout.\n\
                     Use this as the agent command in Zed or another ACP client.\n\
                     Protocol messages are the only stdout output; diagnostics\n\
                     go to stderr.\n\
                     \n\
                     The session uses the same config.toml, policy, tools, and\n\
                     ledger as the rest of Pantheon."
                );
                return;
            }
            crate::acp::run_stdio();
            return;
        }

        // Help is a question with no side effects: it must print and exit
        // BEFORE any config load, token generation, or bind. (A generated
        // token printed for a `--help` invocation is a live credential on a
        // terminal nobody asked to serve from.)
        if args.iter().any(|a| a == "--help" || a == "-h") {
            println!(
                "pantheon serve [--port 18789] [--host 127.0.0.1] [--bind 127.0.0.1]\n\
                 \n\
                 Start the unified serve surface: the dashboard control plane\n\
                 (/api/* + PWA assets) and the AG-UI surface (/agui/*, voice\n\
                 endpoints) on one listener with one token.\n\
                 \n\
                 --host is the canonical bind-address flag; --bind is accepted\n\
                 as an alias so `serve` and `dashboard` take the same flags.\n\
                 The [server] table in config.toml supplies defaults when the\n\
                 flags are absent.\n\
                 \n\
                 The per-instance token prints at startup (or set\n\
                 PANTHEON_SERVE_TOKEN to pin it). Keep it on 127.0.0.1;\n\
                 exposing it publicly requires a reverse proxy with real\n\
                 authentication."
            );
            return;
        }

        // Unknown flags are an error, not a default serve: `serve --bogus`
        // used to start the server anyway, ignoring whatever the operator
        // thought they had configured.
        // All three flags take a value (`--port 8080` or `--port=8080`,
        // as `flag()` reads them), so the value token is consumed here,
        // not mistaken for a stray positional. A value flag with no
        // value is not the default: `serve --port` at the end used to
        // bind the configured port while looking like an explicit choice.
        let rest: &[String] = args.get(2..).unwrap_or(&[]);
        let mut i = 0;
        while i < rest.len() {
            let a = &rest[i];
            if a.starts_with("--") {
                let name = a.split('=').next().unwrap_or(a.as_str());
                if !matches!(name, "--port" | "--host" | "--bind") {
                    eprintln!("serve: unknown flag '{a}' (want --port, --host, --bind)");
                    std::process::exit(2);
                }
                if !a.contains('=') {
                    match rest.get(i + 1) {
                        Some(v) if !v.starts_with("--") => i += 1,
                        _ => {
                            eprintln!("serve: {name} needs a value");
                            std::process::exit(2);
                        }
                    }
                }
            } else if a.starts_with('-') {
                eprintln!("serve: unknown flag '{a}' (want --port, --host, --bind)");
                std::process::exit(2);
            } else {
                eprintln!("serve: unexpected argument '{a}' (this command takes flags only; `serve acp` is the stdio transport)");
                std::process::exit(2);
            }
            i += 1;
        }
        // Flags win, then `[server]`, then the built-in default. A flag that is
        // present but unparseable is an error: silently binding 18789 after
        // `--port abc` leaves the user talking to the wrong server.
        let dir = data_dir();
        let file_cfg = config::Config::load_or_report(&dir);
        let server = file_cfg.as_ref().and_then(|c| c.server.clone());
        let port: u16 = match flag(args, "--port") {
            Some(v) => v
                .parse()
                .unwrap_or_else(|_| fatal_bad_flag("--port", &v, "a port number")),
            None => crate::config::server_port(server.as_ref()),
        };
        // An explicit `--port 0` is the same undiscoverable ephemeral bind
        // as a configured 0: resolve it to the default, loudly, rather
        // than serving on a port nothing can address.
        let port = if port == 0 {
            eprintln!("serve: port 0 (auto) resolved to 18789 so GenUI URLs and remotes can address the server");
            18789
        } else {
            port
        };
        // B-14: `--host` is canonical; `--bind` (the dashboard's historic
        // name) is accepted as an alias on both commands.
        let host = flag(args, "--host")
            .or_else(|| flag(args, "--bind"))
            .or_else(|| server.as_ref().map(|s| s.host.clone()))
            .unwrap_or_else(|| "127.0.0.1".into());
        let token = resolve_serve_token("pantheon serve");
        let agui_cfg = build_agui_serve_parts(&dir, &host, port, &token);
        // The dashboard control plane mounts on the same listener (one token,
        // one port). `open_browser` stays off for `serve`; the dashboard
        // command owns that flag.
        let bind_all = host == "0.0.0.0" || host == "::";
        let dash_mount = std::sync::Arc::new(pantheon_dashboard::DashboardMount::new(
            pantheon_dashboard::App {
                data_dir: dir.clone(),
                token,
                bind: host.clone(),
                bind_all,
                on_approval: Some(approval_callback()),
                send_locks: Default::default(),
                ui_action_locks: Default::default(),
                turn_children: Default::default(),
                swarm: pantheon_dashboard::swarm::orchestrator_for(&dir),
                desktop: Default::default(),
            },
        ));
        // Single-token invariant: the gateway's auth context is built from the
        // dashboard mount's own `auth_ctx()`, so the token the AG-UI routes
        // and the dashboard routes enforce is the same value.
        let auth = dash_mount.auth_ctx();
        let agui_mount =
            std::sync::Arc::new(pantheon_runtime::agui_serve::AguiMount { cfg: agui_cfg });
        let cfg = pantheon_gateway::http::ServerConfig {
            bind_addr: format!("{host}:{port}"),
            auth,
            mounts: vec![dash_mount, agui_mount],
            label: "pantheon serve".to_string(),
        };
        if let Err(e) = pantheon_gateway::http::serve(cfg) {
            eprintln!("serve: {e}");
            std::process::exit(1);
        }
    }
    /// Continue a parked run after a grant: an empty turn rebuilds the
    /// transcript from the ledger and settles the granted call. Never resends
    /// the original user message, which would append a duplicate turn.
    pub fn resume_after_grant(run_id: &str) {
        use crate::config;
        use crate::config::build_model_policy;
        let file_cfg = config::Config::load_or_report(&data_dir());
        let model_policy = build_model_policy(file_cfg.as_ref(), None, None);
        // Policy preset honored verbatim: a Reader config must stay
        // read-only on resume, not silently widen to coder.
        let policy = crate::config_schema::policy_for_config(&file_cfg);
        let secrets = config::chat_secrets(file_cfg.as_ref());
        let session = match pantheon_runtime::session::Session::new(
            data_dir(),
            policy,
            model_policy,
            secrets,
        ) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("open session: {e}");
                std::process::exit(1);
            }
        };
        config::apply_tool_enablement(&session, file_cfg.as_ref());
        config::apply_budget_tiers(&session, file_cfg.as_ref());
        config::apply_delegation_section(&session, file_cfg.as_ref());
        match session.chat_turn(run_id, "", "") {
            Ok(_) => {
                println!("run {run_id} continued");
                crate::terminal::drain_queued_turns(&session, run_id, "session");
            }
            Err(e) => {
                eprintln!("resume {run_id}: {e}");
                std::process::exit(1);
            }
        }
    }
}
