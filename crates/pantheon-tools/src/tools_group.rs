//! Tool layer merged group. All *_tools files in one place.

pub mod builtins {
    //! Built-in tools: shell, read_file, write_file, list_dir.
    //! Each maps to its capability and compacts output before it hits context.
    //!
    //! `write_file` is NOT a plain atomic write - it routes through the
    //! safe-writer so every write is checkpointed, journaled, and recoverable.
    //! That makes the safe path the default, not an opt-in side door.

    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_api::workspace::WorkspaceRoot;
    use pantheon_exec::compact_output;
    use pantheon_exec::confine::confine;
    use pantheon_exec::safewrite::{atomic_write, FileEdit, SafeWriter};
    use pantheon_exec::sandbox::{SandboxLevel, SandboxProfile};
    use std::path::Path;
    use std::sync::Arc;

    /// Lifecycle event for a `shell` tool child process. Fired on the
    /// thread running the tool call: `Spawned` comes from the runner's
    /// spawn hook (synchronously, right after spawn); `Exited` is fired by
    /// `run_shell` after the wait loop ends (exit, timeout kill, or wait
    /// error). The pid is also the child's process-group id - the runner
    /// does `setsid()` in pre-exec - so a cancel path can register the
    /// group for `killpg` on `Spawned` and unregister it on `Exited`, when
    /// the pid is stale.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ShellChildEvent {
        Spawned,
        Exited,
    }

    /// `ShellChildEvent -> pid` observer for `shell` children, e.g. the
    /// runtime registering the pgid so cancel can kill an in-flight shell.
    /// Must be cheap and non-blocking: it runs on the tool-call thread.
    pub type ShellChildHook = Arc<dyn Fn(ShellChildEvent, u32) + Send + Sync>;

    /// Per-call extra env for the shell child: command text -> named
    /// variables the child may receive. The single sanctioned gate for
    /// secrets into a sandboxed shell child (the Cloudflare token path).
    pub type ShellEnvHook = Arc<dyn Fn(&str) -> Vec<(String, String)> + Send + Sync>;

    /// Options for `register_builtins`. Empty defaults keep the old call site
    /// working; supplying `safewrite_state_dir` routes `write_file` through the
    /// SafeWriter instead of leaving the safe path as an opt-in side door.
    #[derive(Clone)]
    pub struct BuiltinOptions {
        pub safewrite_state_dir: Option<std::path::PathBuf>,
        /// Session-scoped workspace root: the boundary every file path is
        /// confined to and the directory relative paths resolve against.
        ///
        /// The handle is read at *call* time, so a session can move to
        /// another directory (a git worktree, a sibling project) and every
        /// tool follows it on the next call - no re-registration, no new
        /// process. `None` captures the process working directory at
        /// registration time, which is the pre-session behavior.
        ///
        /// Every `read_file`/`write_file`/`list_dir` path is confined with
        /// `pantheon_exec::confine` (deny globs, then containment) before
        /// any read or write - a granted `FilesystemRead`/`FilesystemWrite`
        /// capability never widens it.
        pub workspace: Option<Arc<WorkspaceRoot>>,
        /// Tool-group toggles from `[tools]` in config.toml: a disabled group
        /// is never registered, so it cannot appear in the model's tool list.
        /// `shell` belongs to Terminal; `read_file`/`write_file`/`list_dir`
        /// to Files; `ask_user` to Ask User.
        pub enable_terminal: bool,
        pub enable_files: bool,
        pub enable_ask_user: bool,
        /// `enable_plugin` belongs to the Plugins group. Off = the agent
        /// cannot propose plugin enablement at all.
        pub enable_plugins: bool,
        /// Data dir holding `config.toml`. Required for `enable_plugin`:
        /// the config file is the single enablement state, so the tool must
        /// know where it lives. `None` = the tool reports a configuration
        /// error instead of guessing.
        pub data_dir: Option<std::path::PathBuf>,
        /// Observer for `shell` child-process lifecycle. `None` (default) =
        /// no observation. Set by hosts that need to kill an in-flight shell
        /// on cancel: the runtime registers the child's pgid on `Spawned`
        /// and unregisters it on `Exited`.
        pub shell_child_hook: Option<ShellChildHook>,
        /// Per-call extra env for the shell child, resolved by the host at
        /// call time. Called with the command text; returns the named
        /// variables the child may receive (empty = none). This is the
        /// single sanctioned gate for secrets into a sandboxed shell child:
        /// the host resolves through the secrets broker, the hook decides,
        /// the runner injects past the scrub. `None` (default) = no extras
        /// ever.
        pub shell_env_hook: Option<ShellEnvHook>,
        /// Opt-in post-write formatter. `None` = writes are byte-for-byte
        /// untouched. A formatter failure is reported but never rolls back a
        /// successful write.
        pub formatter: Option<Arc<crate::tools_group::format::Formatter>>,
        /// Session-scoped persistent terminals. Registered only when the
        /// Terminal group is enabled and the host supplied a manager.
        pub pty_manager: Option<Arc<crate::tools_group::pty_tools::PtyManager>>,
    }

    impl std::fmt::Debug for BuiltinOptions {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("BuiltinOptions")
                .field("safewrite_state_dir", &self.safewrite_state_dir)
                .field("workspace", &self.workspace.is_some())
                .field("enable_terminal", &self.enable_terminal)
                .field("enable_files", &self.enable_files)
                .field("enable_ask_user", &self.enable_ask_user)
                .field("enable_plugins", &self.enable_plugins)
                .field("data_dir", &self.data_dir)
                .field("shell_child_hook", &self.shell_child_hook.is_some())
                .field("shell_env_hook", &self.shell_env_hook.is_some())
                .field("formatter", &self.formatter.is_some())
                .field("pty_manager", &self.pty_manager.is_some())
                .finish()
        }
    }

    impl Default for BuiltinOptions {
        /// Absent `[tools]` section = every group on, matching the
        /// pre-section behavior exactly.
        fn default() -> Self {
            Self {
                safewrite_state_dir: None,
                workspace: None,
                enable_terminal: true,
                enable_files: true,
                enable_ask_user: true,
                enable_plugins: true,
                data_dir: None,
                shell_child_hook: None,
                shell_env_hook: None,
                formatter: None,
                pty_manager: None,
            }
        }
    }

    /// True when a config entry is exactly what the bundled catalog recipe
    /// materializes (transport, command, args, env, url). `enabled` and
    /// `timeout_secs` are operator-controlled and excluded; `pinned_version`
    /// is informational and not part of the parsed entry. Used by
    /// `enable_mcp` to re-validate a pre-existing table before flipping the
    /// flag, so a name-colliding table with an arbitrary command is flagged
    /// instead of silently kept.
    fn mcp_entry_matches_recipe(
        entry: &pantheon_api::config::McpServerEntry,
        recipe: &pantheon_api::mcp_catalog::BundledMcpServer,
    ) -> bool {
        let canonical = recipe.to_config_entry();
        entry.transport == canonical.transport
            && entry.command == canonical.command
            && entry.args == canonical.args
            && entry.env == canonical.env
            && entry.url == canonical.url
    }

    /// Register the default toolset on a registry.
    pub fn register_builtins(reg: &mut ToolRegistry) {
        register_builtins_with(reg, BuiltinOptions::default());
    }

    /// Register with options. When `safewrite_state_dir` is set, `write_file`
    /// routes through the SafeWriter (checkpoint + journal + atomic publish +
    /// stale-hash rejection). Without it, `write_file` falls back to a plain
    /// atomic write - kept for callers that explicitly want the unsafe path.
    pub fn register_builtins_with(reg: &mut ToolRegistry, opts: BuiltinOptions) {
        let sd = opts.safewrite_state_dir.clone();
        let pty_manager = opts.pty_manager.clone();
        // Session-scoped workspace root. The handle is read at *call* time, so
        // a live worktree move re-roots every tool on its next call without a
        // re-registration or a new process. `None` pins the process working
        // directory captured here, which is the pre-session behavior. Confine
        // errors propagate with their CONFINE_* codes (deny globs are
        // evaluated before containment, and independently of the capability
        // gate).
        let workspace: Arc<WorkspaceRoot> = opts
            .workspace
            .clone()
            .unwrap_or_else(|| Arc::new(WorkspaceRoot::from_cwd()));
        if opts.enable_terminal {
            // 'static closure: the child hook is an Arc, so the clone here
            // is cheap and the registry owns its copy.
            let shell_child_hook = opts.shell_child_hook.clone();
            let shell_env_hook = opts.shell_env_hook.clone();
            let shell_workspace = workspace.clone();
            reg.register_with(
        ToolSchema {
            name: "shell".into(),
            description: "Run a shell command with a timeout. Returns compacted stdout+stderr.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The command to run" } },
                "required": ["command"]
            }),
        },
        Capability::ShellExecute,
        move |args| {
            // Dangerous-pattern pre-gate: deterministic, in-process, runs
            // before any spawn. Not the security boundary (policy is), but
            // it fails fast on `rm -rf /` class commands and keeps them
            // out of the audit trail as executed calls. run_shell parses
            // the args itself; parse here only for the gate.
            let v = parse_args(args)?;
            let command = crate::tools::arg_str(&v, "command")?;
            pantheon_exec::danger::gate(&command)?;
            run_shell_with_env(
                args,
                shell_child_hook.as_ref(),
                shell_env_hook.as_ref().map(|h| h(command.as_str())).unwrap_or_default().as_slice(),
                &shell_workspace,
            )
        },
        Some(Box::new(|args: &str| {
            // `git push` is the one shell operation the policies single
            // out (coder marks git.push Approval). The static ShellExecute
            // capability alone let every push through unapproved, so the
            // command is inspected and the call picks up GitPush.
            // `cf ...` is the second: Cloudflare CLI calls classify into
            // read/write/destroy (pantheon-exec::cloudflare), and the
            // destroy/write classes carry their own tokens so the default
            // policies park on them. Unknown cf shapes fail closed to
            // Destroy, the strictest class.
            let Ok(v) = parse_args(args) else {
                return Vec::new();
            };
            let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
                return Vec::new();
            };
            if pantheon_exec::danger::is_git_push(cmd) {
                return vec![Capability::GitPush];
            }
            if pantheon_exec::cloudflare::is_cf_command(cmd) {
                match pantheon_exec::cloudflare::classify(cmd).capability_token() {
                    Some(token) => return vec![Capability::Other(token.to_string())],
                    None => return Vec::new(),
                }
            }
            Vec::new()
        })),
    );
        }
        if opts.enable_terminal {
            if let Some(manager) = pty_manager {
                crate::tools_group::pty_tools::register_pty_tools(reg, manager);
            }
        }
        let r0 = workspace.clone();
        if opts.enable_files {
            reg.register(
                ToolSchema {
                    name: "read_file".into(),
                    description:
                        "Read a text file (compacted if very large). Confined to the workspace."
                            .into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    }),
                },
                Capability::FilesystemRead,
                move |args| {
                    let v = parse_args(args)?;
                    let path = crate::tools::arg_str(&v, "path")?;
                    let cpath = confine(Path::new(&path), &r0.path())?;
                    let raw = std::fs::read_to_string(&cpath).map_err(|e| {
                        crate::tools::tool_err(
                            "TOOL_FS",
                            Layer::Execution,
                            false,
                            format!("read {}: {e}", cpath.display()),
                            "check tool arguments",
                        )
                    })?;
                    Ok(compact_output(&raw, &Default::default()).text)
                },
            );
        }
        let r1 = workspace.clone();
        let formatter = opts.formatter.clone();
        if opts.enable_files {
            reg.register(
        ToolSchema {
            name: "write_file".into(),
            description: "Write a file safely: preview, checkpoint, atomic publish. Fails on stale expected_hash. Confined to the workspace.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "expected_hash": { "type": "string", "description": "Optional stale-edit guard. When omitted, a fresh fingerprint is captured first." }
                },
                "required": ["path", "content"]
            }),
        },
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let path = crate::tools::arg_str(&v, "path")?;
            // Confine before either write path (safe or unsafe fallback).
            let cpath = confine(Path::new(&path), &r1.path())?;
            let content = crate::tools::arg_str(&v, "content")?;
            let expected = v
                .get("expected_hash")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if let Some(ref dir) = sd {
                // Safe path: checkpoint + atomic publish + journal.
                let w = SafeWriter::new(dir.clone())
                    .map_err(|e| {
                        crate::tools::tool_err("TOOL_FS", Layer::Execution, false, format!("open safewrite state {dir:?}: {e}"), "check tool arguments")
                    })?
                    .with_workspace_root(r1.path());
                // Capture the current fingerprint so stale edits get
                // rejected by apply_edits when no explicit hash is given.
                let expected = expected.or_else(|| {
                    pantheon_exec::safewrite::fingerprint_of(&cpath)
                        .ok()
                        .map(|fp| fp.hash)
                });
                let receipt = w
                    .apply_edits(
                        vec![FileEdit {
                            path: cpath.clone(),
                            new_content: content.as_bytes().to_vec(),
                            expected_hash: expected,
                        }],
                        -1,
                    )
                    .map_err(|e| {
                        crate::tools::tool_err("TOOL_FS", Layer::Execution, false, format!("safewrite apply {}: {e}", cpath.display()), "check tool arguments")
                    })?;
                let msg = format!(
                    "wrote {} bytes to {}; checkpoint={}",
                    content.len(),
                    cpath.display(),
                    receipt.checkpoint_id
                );
                Ok(append_format(msg, formatter.as_deref(), &cpath))
            } else {
                // Unsafe fallback: caller opted out of the safe path.
                atomic_write(&cpath, content.as_bytes())
                    .map_err(|e| {
                        crate::tools::tool_err("TOOL_FS", Layer::Execution, false, format!("write {}: {e}", cpath.display()), "check tool arguments")
                    })?;
                let msg = format!("wrote {} bytes to {}", content.len(), cpath.display());
                Ok(append_format(msg, formatter.as_deref(), &cpath))
            }
        },
    );
        }
        let r2 = workspace.clone();
        if opts.enable_files {
            reg.register(
                ToolSchema {
                    name: "list_dir".into(),
                    description:
                        "List a directory's entries, one per line. Confined to the workspace."
                            .into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    }),
                },
                Capability::FilesystemRead,
                move |args| {
                    let v = parse_args(args)?;
                    let path = crate::tools::arg_str(&v, "path")?;
                    let cpath = confine(Path::new(&path), &r2.path())?;
                    let mut names: Vec<String> = std::fs::read_dir(&cpath)
                        .map_err(|e| {
                            crate::tools::tool_err(
                                "TOOL_FS",
                                Layer::Execution,
                                false,
                                format!("list {}: {e}", cpath.display()),
                                "check tool arguments",
                            )
                        })?
                        .filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    names.sort();
                    Ok(names.join("\n"))
                },
            );
        }
        // `ask_user` never executes: the agent loop intercepts the call
        // before gating and parks the turn for operator input (the clarify
        // card) - production `Session::drive` pre-gates it, so it can never
        // require approval, consume budget, or execute. It is registered so the model sees it
        // in the tool list and so direct `execute("ask_user", ...)` callers get
        // a structured refusal instead of TOOL_UNKNOWN.
        if opts.enable_ask_user {
            reg.register(
        ToolSchema {
            name: "ask_user".into(),
            description: "Ask the operator a question and wait for their answer. Use when you genuinely cannot proceed without input - a genuine fork in the road, not a guess you could make. `question` is required; `options` (max 9) offers quick-pick choices but the operator can always type free text."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question to ask" },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "Optional quick-pick choices" }
                },
                "required": ["question"]
            }),
        },
        Capability::Other("ask_user".into()),
        |_args| {
            Err(crate::tools::tool_err("TOOL_HOST_MEDIATED", Layer::Execution, false, "ask_user is host-mediated: call it through the agent loop, which parks for operator input".to_string(), "check tool arguments"))
        },
    );
        }
        // `enable_plugin` lets the agent propose enabling a bundled plugin.
        // The capability gate runs before this closure: default policies mark
        // `plugin.enable` Approval, so the run parks, an `ApprovalRequested`
        // event is audit-logged, and the write below happens only after the
        // operator grants. Never silent. Only bundled names are accepted -
        // the embedded registry (tool/hook plugins compiled into the binary)
        // or the vendored extensions catalog - so there is no agent path to
        // install or enable arbitrary plugins. The config file is the single
        // enablement state, so the toggle lands in `[plugins.<name>]` where
        // the dashboard, the mobile app, and the TUI all read it. Without a
        // data dir the tool cannot function, so it is not registered at all
        // rather than advertised broken.
        if opts.enable_plugins && opts.data_dir.is_some() {
            let dd = opts.data_dir.clone();
            reg.register(
        ToolSchema {
            name: "enable_plugin".into(),
            description: "Propose enabling one of Pantheon's bundled plugins (first-party code shipped with Pantheon, disabled by default unless it ships on). `name` must be a bundled plugin name (the embedded registry - cf-ops, noisegate, hookify, ... - or the vendored catalog); anything else is refused. Calling this parks the run for operator approval: the proposal is audit-logged and the plugin switches on only if the operator grants. Use when a bundled plugin would genuinely help the task; say why in your message first."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Bundled plugin name (embedded registry or vendored catalog)" }
                },
                "required": ["name"]
            }),
        },
        Capability::PluginEnable,
        move |args| {
            let v = parse_args(args)?;
            let name = crate::tools::arg_str(&v, "name")?;
            let data_dir = dd.clone().ok_or_else(|| {
                crate::tools::tool_err("TOOL_NO_DATA_DIR", Layer::Execution, false, "plugin enablement needs a data dir; registration did not provide one".to_string(), "check tool arguments")
            })?;
            if pantheon_extensions::bundled::find(&name).is_some() {
                pantheon_extensions::bundled::set_enabled(&data_dir, &name, true).map_err(|e| {
                    crate::tools::tool_err("TOOL_PLUGIN_ENABLE", Layer::Execution, false, e.to_string(), "check tool arguments")
                })?;
                return Ok(format!(
                    "bundled plugin '{name}' (hook) enabled: [plugins.{name}] enabled = true"
                ));
            }
            if pantheon_exec::plugins::bundled_plugin_infos()
                .iter()
                .any(|i| i.name == name)
            {
                pantheon_exec::plugins::set_bundled_enabled(&data_dir, &name, true).map_err(|e| {
                    crate::tools::tool_err("TOOL_PLUGIN_ENABLE", Layer::Execution, false, e.to_string(), "check tool arguments")
                })?;
                return Ok(format!(
                    "bundled plugin '{name}' (registry) enabled: [plugins.{name}] enabled = true"
                ));
            }
            Err(crate::tools::tool_err("TOOL_UNKNOWN_PLUGIN", Layer::Execution, false, format!("'{name}' is not a bundled plugin; only embedded-registry or vendored-catalog names can be enabled"), "run `pantheon plugins` to list bundled plugins"))
        },
    );
        }
        // `enable_mcp` lets the agent propose enabling a bundled MCP server.
        // The capability gate runs before this closure: default policies mark
        // `mcp.enable` Approval, so the run parks, an `ApprovalRequested`
        // event is audit-logged, and the write below happens only after the
        // operator grants. Never silent. Only bundled-catalog names are
        // accepted - there is no agent path to put an arbitrary command on
        // the spawn line (the supply-chain boundary). The config file is the
        // single enablement state, so the toggle lands in
        // `[mcp.servers.<name>]` where the dashboard, the mobile app, and the
        // TUI all read it. The pinned catalog version is materialized, never
        // `latest`.
        if opts.enable_plugins {
            let dd = opts.data_dir.clone();
            reg.register(
        ToolSchema {
            name: "enable_mcp".into(),
            description: "Propose enabling one of Pantheon's bundled MCP servers (first-party catalog entries, all disabled by default). `name` must be a bundled-catalog name - anything else is refused, so this can never install an arbitrary server command. Calling this parks the run for operator approval: the proposal is audit-logged and the server switches on only if the operator grants. Use when a bundled server would genuinely help the task; say why in your message first."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Bundled MCP server name from the catalog (e.g. \"github\")" }
                },
                "required": ["name"]
            }),
        },
        Capability::McpEnable,
        move |args| {
            let v = parse_args(args)?;
            let name = crate::tools::arg_str(&v, "name")?;
            let data_dir = dd.clone().ok_or_else(|| {
                crate::tools::tool_err("TOOL_NO_DATA_DIR", Layer::Execution, false, "MCP enablement needs a data dir; registration did not provide one".to_string(), "check tool arguments")
            })?;
            let server = pantheon_api::mcp_catalog::find(&name).ok_or_else(|| {
                crate::tools::tool_err("TOOL_UNKNOWN_MCP_SERVER", Layer::Execution, false, format!("'{name}' is not a bundled MCP server; only catalog servers can be enabled"), "check tool arguments")
            })?;
            // Re-validate any pre-existing table under this bundled name
            // against the canonical recipe: `set_enabled` preserves
            // existing keys, so a name-colliding table carrying an
            // arbitrary command would otherwise survive a legitimate
            // enable click and run under the bundled name. Flag it
            // loudly and refuse - never silently keep the poison.
            if let Some(entry) = pantheon_api::mcp_catalog::load_config(&data_dir)
                .as_ref()
                .and_then(|cfg| cfg.mcp.as_ref())
                .and_then(|mcp| mcp.servers.get(&server.name))
            {
                if !mcp_entry_matches_recipe(entry, &server) {
                    return Err(crate::tools::tool_err(
                        "TOOL_MCP_NAME_COLLISION",
                        Layer::Execution,
                        false,
                        format!(
                            "[mcp.servers.{}] exists but does not match the canonical bundled recipe \
                             (different transport, command, args, env, or url): refusing to enable what \
                             looks like a name-collision table; remove or rename the table and call \
                             enable_mcp again to materialize the genuine recipe",
                            server.name
                        ),
                        "inspect [mcp.servers.<name>] in config.toml",
                    ));
                }
            }
            pantheon_api::mcp_catalog::set_enabled(&data_dir, &server.name, true).map_err(|e| {
                crate::tools::tool_err("TOOL_MCP_ENABLE", Layer::Execution, false, e.to_string(), "check tool arguments")
            })?;
            Ok(format!(
                "bundled MCP server '{}' (pinned {}) enabled: [mcp.servers.{}] enabled = true",
                server.name, server.version, server.name
            ))
        },
    );
        }
    }

    /// Run a shell command, with an optional caller-supplied extra-env list
    /// (see `run_sandboxed_with_env`). The Cloudflare integration uses the
    /// env list to hand the broker-resolved API token to `cf` children;
    /// the default path passes an empty list and children get nothing
    /// beyond the scrub allowlist.
    fn run_shell_with_env(
        args: &str,
        child_hook: Option<&ShellChildHook>,
        extra_env: &[(String, String)],
        workspace: &WorkspaceRoot,
    ) -> Result<String, PantheonError> {
        let v = parse_args(args)?;
        let command = crate::tools::arg_str(&v, "command")?;
        // Shell runs at HIGH isolation: bwrap with dropped caps + no-new-privs
        // + rlimits. The capability gate already ran before we get here.
        let profile = SandboxProfile::from(SandboxLevel::High);
        // Resolve relative to the session root, not the process cwd: the root
        // is read here so a worktree move takes effect on the next call.
        let cwd = workspace.path().to_string_lossy().to_string();

        // The spawn hook fires synchronously on this thread right after the
        // child is spawned; the pid doubles as the process-group id (the
        // runner does setsid() in pre-exec). Capture it so the Exited event
        // below names the same child.
        let spawned: std::cell::Cell<Option<u32>> = std::cell::Cell::new(None);
        let spawn_hook = child_hook.map(|hook| {
            let hook = Arc::clone(hook);
            let spawned = &spawned;
            move |pid: u32| {
                spawned.set(Some(pid));
                hook(ShellChildEvent::Spawned, pid);
            }
        });
        let result = pantheon_exec::sandbox::runner::run_sandboxed_with_spawn_hook_and_env(
            &profile,
            "sh",
            &["-c", &command],
            &cwd,
            spawn_hook.as_ref().map(|f| f as &dyn Fn(u32)),
            extra_env,
        );
        if let (Some(hook), Some(pid)) = (child_hook, spawned.get()) {
            // Fires on every terminal path (exit, timeout kill, wait error):
            // the pid is stale now - unregister it.
            hook(ShellChildEvent::Exited, pid);
        }
        let result = result?;

        let code = result.exit_code;
        let raw = if result.output.is_empty() {
            format!("(exit {code})")
        } else {
            format!("{}\n(exit {code})", result.output)
        };
        // The runner reports whether isolation actually happened. By default
        // it fails closed: on a host without a working wrapper (a common
        // case - most EC2/container instances block `bwrap`'s uid map) the
        // call above already returned a SANDBOX_UNAVAILABLE error. A
        // `sandboxed == false` result is only possible with the direct
        // fallback explicitly opted in (PANTHEON_SANDBOX_FALLBACK=allow or
        // allow_direct_fallback); say so in the result rather than silently
        // presenting degraded isolation as HIGH.
        let notice = if result.sandboxed {
            String::new()
        } else {
            "[sandbox unavailable on this host: ran WITHOUT namespace isolation]\n".to_string()
        };
        let compacted = compact_output(&format!("{notice}{raw}"), &Default::default());
        Ok(compacted.text)
    }

    /// Run the configured formatter after a successful write. Formatting is
    /// deliberately best-effort: a missing binary, timeout, or syntax error is
    /// surfaced in the tool result, but the write itself is never rolled back.
    fn append_format(
        mut msg: String,
        formatter: Option<&crate::tools_group::format::Formatter>,
        path: &Path,
    ) -> String {
        let Some(formatter) = formatter else {
            return msg;
        };
        if let Some(outcome) = formatter.format(path) {
            if outcome.success {
                msg.push_str(&format!("\nformatted ({})", outcome.command));
            } else if outcome.output.is_empty() {
                msg.push_str(&format!("\nformatter failed ({})", outcome.command));
            } else {
                msg.push_str(&format!(
                    "\nformatter failed ({}): {}",
                    outcome.command, outcome.output
                ));
            }
        }
        msg
    }

    #[cfg(test)]
    mod tests {
        use super::{register_builtins_with, BuiltinOptions};
        use crate::tools::ToolRegistry;

        fn enable_mcp_registry(data_dir: &std::path::Path) -> ToolRegistry {
            let mut reg = ToolRegistry::new();
            register_builtins_with(
                &mut reg,
                BuiltinOptions {
                    data_dir: Some(data_dir.to_path_buf()),
                    ..Default::default()
                },
            );
            reg
        }

        fn call_enable_mcp(
            reg: &ToolRegistry,
        ) -> Result<String, pantheon_api::error::PantheonError> {
            let tool = reg.get("enable_mcp").expect("enable_mcp is registered");
            (tool.run)(r#"{"name": "playwright"}"#)
        }

        /// A poisoned table under a bundled name must be flagged, never
        /// silently kept: `set_enabled` preserves pre-existing keys, so
        /// without the re-validation the attacker's command would survive a
        /// legitimate enable click and run under the bundled exemption.
        #[test]
        fn enable_mcp_flags_poisoned_table() {
            let dir = tempfile::tempdir().expect("tempdir");
            let data_dir = dir.path();
            std::fs::write(
                data_dir.join("config.toml"),
                "[mcp.servers.playwright]\n\
             transport = \"stdio\"\n\
             command = \"python3\"\n\
             args = [\"/tmp/evil_mcp_server.py\"]\n\
             enabled = false\n",
            )
            .expect("write poisoned config");

            let reg = enable_mcp_registry(data_dir);
            let err =
                call_enable_mcp(&reg).expect_err("poisoned table must be refused, not enabled");
            let msg = err.to_string();
            assert!(
                msg.contains("TOOL_MCP_NAME_COLLISION"),
                "refusal must name the collision; got: {msg}"
            );

            // Never silently kept: the enable flag must NOT have flipped.
            let cfg_text =
                std::fs::read_to_string(data_dir.join("config.toml")).expect("read config");
            assert!(
                !cfg_text.contains("enabled = true"),
                "poisoned table must not be enabled; config now:\n{cfg_text}"
            );
            assert!(
                cfg_text.contains("command = \"python3\""),
                "refusal must not rewrite the table either; config now:\n{cfg_text}"
            );
        }

        /// The normal path is untouched: no pre-existing table materializes
        /// the canonical recipe and enables it.
        #[test]
        fn enable_mcp_still_enables_clean_state() {
            let dir = tempfile::tempdir().expect("tempdir");
            let data_dir = dir.path();

            let reg = enable_mcp_registry(data_dir);
            let out = call_enable_mcp(&reg).expect("clean enable_mcp works");
            assert!(out.contains("enabled"), "unexpected output: {out}");

            let cfg = pantheon_api::mcp_catalog::load_config(data_dir)
                .expect("config loads after enable");
            assert!(cfg.mcp_server_enabled("playwright"));
            let entry = cfg
                .mcp
                .as_ref()
                .and_then(|m| m.servers.get("playwright"))
                .expect("table materialized");
            assert_eq!(entry.command.as_deref(), Some("npx"));
        }

        /// Idempotent: enabling an already-canonical table again is fine.
        #[test]
        fn enable_mcp_accepts_canonical_table() {
            let dir = tempfile::tempdir().expect("tempdir");
            let data_dir = dir.path();

            let reg = enable_mcp_registry(data_dir);
            call_enable_mcp(&reg).expect("first enable works");
            call_enable_mcp(&reg).expect("second enable over the canonical table works");

            let cfg = pantheon_api::mcp_catalog::load_config(data_dir)
                .expect("config loads after enable");
            assert!(cfg.mcp_server_enabled("playwright"));
        }
    }
}
pub mod code_mode {
    //! Code Mode: a sandboxed JavaScript scratchpad with a narrow host bridge.
    //!
    //! The model writes JavaScript once and calls Pantheon tools from it:
    //!
    //! ```js
    //! const hits = tools.call("grep", { pattern: "TODO", path: "src" });
    //! const files = tools.call("list_dir", { path: "src" });
    //! console.log(hits);
    //! files;
    //! ```
    //!
    //! The JS runtime has no filesystem, network, process, or module APIs of
    //! its own. Its only escape hatch is `tools.call`, which goes through
    //! [`ToolRegistry::execute_gated`] with the session's policy. A nested call
    //! that needs operator approval is denied rather than silently granted:
    //! Code Mode cannot park the turn for a human, so the safe default is to
    //! tell the model to use the direct tool or ask first.
    //!
    //! The outer `execute` tool is registered as a host-mediated stub; the
    //! runtime intercepts it and calls [`execute`] with the real registry and
    //! policy. This keeps the registry immutable and avoids a self-referential
    //! tool closure.

    use crate::tools::{tool_err, ToolCallContext, ToolRegistry};
    use pantheon_api::capability::Policy;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use rquickjs::{Context, Function, Runtime};
    use std::cell::RefCell;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// The bridge callbacks are installed as `'static` QuickJS functions, but
    /// they only run synchronously inside [`run_js`], where these borrows are
    /// valid. The thread-local keeps that lifetime contract explicit instead of
    /// making the registry cloneable (its tool closures are not `Clone`).
    #[derive(Clone)]
    struct CodeCtx {
        registry: *const ToolRegistry,
        policy: *const Policy,
        call: Option<ToolCallContext>,
    }

    thread_local! {
        static CODE_CTX: RefCell<Option<CodeCtx>> = const { RefCell::new(None) };
    }

    struct CodeCtxGuard;

    impl Drop for CodeCtxGuard {
        fn drop(&mut self) {
            CODE_CTX.with(|c| *c.borrow_mut() = None);
        }
    }

    fn install_ctx(
        registry: &ToolRegistry,
        policy: &Policy,
        call: Option<ToolCallContext>,
    ) -> CodeCtxGuard {
        CODE_CTX.with(|c| {
            *c.borrow_mut() = Some(CodeCtx {
                registry,
                policy,
                call,
            })
        });
        CodeCtxGuard
    }

    fn with_ctx<T>(
        f: impl FnOnce(&ToolRegistry, &Policy, &Option<ToolCallContext>) -> T,
    ) -> Option<T> {
        CODE_CTX.with(|c| {
            let guard = c.borrow();
            let ctx = guard.clone()?;
            // SAFETY: the guard installed in `run_js` keeps both referents
            // alive for the whole synchronous eval, and this thread is the only
            // reader of the thread-local.
            unsafe { Some(f(&*ctx.registry, &*ctx.policy, &ctx.call)) }
        })
    }

    pub const CODE_MODE_TOOL: &str = "execute";
    const MAX_CODE_BYTES: usize = 128 * 1024;
    const MAX_RESULT_BYTES: usize = 256 * 1024;
    const DEFAULT_TIMEOUT_MS: u64 = 5_000;
    const MAX_TIMEOUT_MS: u64 = 30_000;

    /// Register the model-visible `execute` stub. Execution happens in the
    /// runtime's tool runner, which owns the registry and policy.
    pub fn register_code_mode_stub(reg: &mut ToolRegistry) {
        reg.register(
        ToolSchema {
            name: CODE_MODE_TOOL.into(),
            description: "Run JavaScript in a sandbox and compose Pantheon tool calls. Use `tools.call(name, argsObject)` to invoke a tool; it returns that tool's text result (JSON where the tool returns JSON). `tools.names()` lists callable tools. `console.log(...)` output is returned with the result. No filesystem, network, process, or import APIs are available. A nested tool call is denied if it would need operator approval, so call approval-gated tools directly. The final expression is returned (top-level `return` is not supported; leave the value as the last expression)."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "JavaScript source. The final expression is returned."},
                    "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 30000}
                },
                "required": ["code"]
            }),
        },
        pantheon_api::capability::Capability::ShellExecute,
        |_args| {
            Err(tool_err(
                "TOOL_HOST_MEDIATED",
                Layer::Execution,
                false,
                "execute is host-mediated: call it through the agent loop, which supplies the tool registry and policy"
                    .into(),
                "check tool name and arguments",
            ))
        },
    );
    }

    /// Execute one Code Mode request. `args_json` is the tool-call arguments:
    /// `{"code": "...", "timeout_ms": 5000}`.
    pub fn execute(
        registry: &ToolRegistry,
        policy: &Policy,
        args_json: &str,
    ) -> Result<String, PantheonError> {
        execute_with_context(registry, policy, args_json, None)
    }

    /// Like [`execute`], but installs the outer call's durable context for
    /// nested tool calls. The production adapter supplies this so a tool called
    /// from Code Mode can link its side effects to the same step.
    pub fn execute_with_context(
        registry: &ToolRegistry,
        policy: &Policy,
        args_json: &str,
        call: Option<ToolCallContext>,
    ) -> Result<String, PantheonError> {
        let v: serde_json::Value = serde_json::from_str(args_json).map_err(|e| {
            tool_err(
                "CODE_BAD_ARGS",
                Layer::Execution,
                false,
                format!("invalid execute args: {e}"),
                "pass {\"code\": \"...\"}",
            )
        })?;
        let code = v.get("code").and_then(|x| x.as_str()).ok_or_else(|| {
            tool_err(
                "CODE_BAD_ARGS",
                Layer::Execution,
                false,
                "missing 'code'".into(),
                "pass {\"code\": \"...\"}",
            )
        })?;
        if code.trim().is_empty() {
            return Err(tool_err(
                "CODE_EMPTY",
                Layer::Execution,
                false,
                "empty code".into(),
                "pass JavaScript to execute",
            ));
        }
        if code.len() > MAX_CODE_BYTES {
            return Err(tool_err(
                "CODE_TOO_LARGE",
                Layer::Execution,
                false,
                format!("code is {} bytes; limit is {MAX_CODE_BYTES}", code.len()),
                "split the work into smaller execute calls",
            ));
        }
        let timeout = Duration::from_millis(
            v.get("timeout_ms")
                .and_then(|x| x.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(100, MAX_TIMEOUT_MS),
        );
        run_js(registry, policy, code, timeout, call)
    }

    fn run_js(
        registry: &ToolRegistry,
        policy: &Policy,
        code: &str,
        timeout: Duration,
        call: Option<ToolCallContext>,
    ) -> Result<String, PantheonError> {
        let rt = Runtime::new().map_err(|e| code_err("CODE_RUNTIME", e.to_string()))?;
        rt.set_memory_limit(64 * 1024 * 1024);
        rt.set_max_stack_size(512 * 1024);
        let deadline = Instant::now() + timeout;
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() >= deadline)));
        let ctx = Context::full(&rt).map_err(|e| code_err("CODE_CONTEXT", e.to_string()))?;
        let _ctx_guard = install_ctx(registry, policy, call);

        ctx.with(|ctx| {
        let host_call = Function::new(
            ctx.clone(),
            |name: String, args: String| -> String {
                if name == CODE_MODE_TOOL {
                    return serde_json::json!({
                        "ok": false,
                        "error": "recursive execute is not allowed"
                    })
                    .to_string();
                }
                let Some(result) = with_ctx(|registry, policy, call| match call {
                    Some(call) => registry.execute_gated_with_context(
                        policy,
                        &name,
                        &args,
                        call.clone(),
                    ),
                    None => registry.execute_gated(policy, &name, &args),
                }) else {
                    return serde_json::json!({
                        "ok": false,
                        "error": "Code Mode bridge is not installed"
                    })
                    .to_string();
                };
                match result {
                    Ok(out) => {
                        let out = truncate(out, MAX_RESULT_BYTES);
                        serde_json::json!({"ok": true, "result": out}).to_string()
                    }
                    Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}).to_string(),
                }
            },
        )
        .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;
        ctx.globals()
            .set("__pantheon_call", host_call)
            .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;

        let host_names = Function::new(ctx.clone(), || -> String {
            let Some(names) = with_ctx(|registry, _, _| {
                let mut names: Vec<String> = registry
                    .names()
                    .into_iter()
                    .filter(|n| n != CODE_MODE_TOOL)
                    .collect();
                names.sort();
                names
            }) else {
                return "[]".to_string();
            };
            serde_json::to_string(&names).unwrap_or_else(|_| "[]".to_string())
        })
        .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;
        ctx.globals()
            .set("__pantheon_names", host_names)
            .map_err(|e| code_err("CODE_BRIDGE", e.to_string()))?;

        let code_literal = serde_json::to_string(code)
            .map_err(|e| code_err("CODE_ARGS", e.to_string()))?;
        let source = format!(
            r#"
const __pantheon_logs = [];
const __pantheon_fmt = (x) => {{
  if (typeof x === "string") return x;
  try {{ return JSON.stringify(x); }} catch (_e) {{ return String(x); }}
}};
globalThis.console = {{
  log: (...args) => __pantheon_logs.push(args.map(__pantheon_fmt).join(" ")),
  error: (...args) => __pantheon_logs.push(args.map(__pantheon_fmt).join(" ")),
}};
globalThis.tools = {{
  call: (name, args) => {{
    const raw = __pantheon_call(String(name), typeof args === "string" ? args : JSON.stringify(args ?? {{}}));
    const env = JSON.parse(raw);
    if (!env.ok) throw new Error(env.error);
    return env.result;
  }},
  names: () => JSON.parse(__pantheon_names()),
}};
let __pantheon_out;
try {{
  const __pantheon_value = eval({code_literal});
  __pantheon_out = {{
    ok: true,
    result: __pantheon_value === undefined ? null : __pantheon_value,
  }};
}} catch (e) {{
  __pantheon_out = {{ ok: false, error: String(e) }};
}}
__pantheon_out.logs = __pantheon_logs;
globalThis.__pantheon_raw = JSON.stringify(__pantheon_out);
"#
        );
        ctx.eval::<(), _>(source)
            .map_err(|e| code_err("CODE_EVAL", format!("{e}")))?;
        let raw: String = ctx
            .eval("globalThis.__pantheon_raw")
            .map_err(|e| code_err("CODE_EVAL", format!("{e}")))?;
        let envelope: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            code_err(
                "CODE_RESULT",
                format!("Code Mode returned invalid JSON: {e}"),
            )
        })?;
        if !envelope.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            let error = envelope
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("JavaScript execution failed");
            let logs = render_logs(&envelope);
            return Err(tool_err(
                "CODE_EVAL",
                Layer::Execution,
                false,
                if logs.is_empty() {
                    error.to_string()
                } else {
                    format!("{error}\n{logs}")
                },
                "fix the JavaScript and retry",
            ));
        }
        let mut out = match envelope.get("result") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Null) | None => String::new(),
            Some(other) => serde_json::to_string_pretty(other).unwrap_or_default(),
        };
        let logs = render_logs(&envelope);
        if !logs.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&logs);
        }
        if out.is_empty() {
            out.push_str("(no result)");
        }
        Ok(truncate(out, MAX_RESULT_BYTES))
    })
    }

    fn render_logs(envelope: &serde_json::Value) -> String {
        let Some(logs) = envelope.get("logs").and_then(|v| v.as_array()) else {
            return String::new();
        };
        if logs.is_empty() {
            return String::new();
        }
        let mut out = String::from("logs:");
        for line in logs {
            out.push('\n');
            out.push_str(&truncate(
                line.as_str().unwrap_or_default().to_string(),
                4_000,
            ));
        }
        out
    }

    fn truncate(mut s: String, max: usize) -> String {
        if s.len() <= max {
            return s;
        }
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n…[truncated]");
        s
    }

    fn code_err(code: &str, cause: String) -> PantheonError {
        tool_err(
            code,
            Layer::Execution,
            false,
            cause,
            "fix the JavaScript and retry",
        )
    }

    /// Help the model discover the bridge without exposing implementation
    /// internals in the tool schema.
    pub fn bridge_summary() -> &'static str {
        "tools.call(name, args), tools.names(), console.log(...)"
    }

    // The bridge summary is referenced by tests and can be used by a future
    // system-prompt hint; keep it out of the hot path.
    #[allow(dead_code)]
    fn _assert_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Arc<()>>();
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::tools::ToolRegistry;
        use pantheon_api::capability::Capability;
        use pantheon_api::message::ToolSchema;

        fn registry() -> ToolRegistry {
            let mut reg = ToolRegistry::new();
            reg.register(
                ToolSchema {
                    name: "probe".into(),
                    description: "echo args".into(),
                    parameters: serde_json::json!({"type":"object"}),
                },
                Capability::FilesystemRead,
                |args| Ok(format!("probe:{args}")),
            );
            reg
        }

        #[test]
        fn evaluates_and_returns_final_expression() {
            let out = execute(&registry(), &Policy::coder(), r#"{"code":"1 + 2"}"#).unwrap();
            assert_eq!(out, "3");
        }

        #[test]
        fn calls_a_registered_tool_with_json_args() {
            let out = execute(
                &registry(),
                &Policy::coder(),
                r#"{"code":"tools.call('probe', {x: 7})"}"#,
            )
            .unwrap();
            assert!(out.contains("probe:{\"x\":7}"), "{out}");
        }

        #[test]
        fn console_logs_are_returned() {
            let out = execute(
                &registry(),
                &Policy::coder(),
                r#"{"code":"console.log('hello', {a: 1}); 5"}"#,
            )
            .unwrap();
            assert!(out.contains("hello"), "{out}");
            assert!(out.contains("logs:"), "{out}");
        }

        #[test]
        fn nested_denial_is_a_js_exception() {
            let mut reg = registry();
            reg.register(
                ToolSchema {
                    name: "needs_approval".into(),
                    description: "test".into(),
                    parameters: serde_json::json!({"type":"object"}),
                },
                Capability::GitPush,
                |_| Ok("should not run".into()),
            );
            let out = execute(
                &reg,
                &Policy::coder(),
                r#"{"code":"tools.call('needs_approval', {})"}"#,
            );
            assert!(out.is_err(), "{out:?}");
            assert!(out.unwrap_err().to_string().contains("TOOL_DENIED"));
        }

        #[test]
        fn timeout_stops_an_infinite_loop() {
            let out = execute(
                &registry(),
                &Policy::coder(),
                r#"{"code":"while (true) {}","timeout_ms":100}"#,
            );
            assert!(out.is_err(), "{out:?}");
        }
    }
}
pub mod codegraph_tools {
    //! Codebase graph tools: a native, cacheable symbol/call index.
    //!
    //! The parser and graph live in `pantheon-exec::codegraph`; this module
    //! exposes orientation queries to the agent:
    //!
    //!   - codebase.index     build/refresh the index
    //!   - codebase.search    find symbols by name, qualified name, or file
    //!   - codebase.callers   lexical call sites for a symbol
    //!   - codebase.callees   lexical calls made by a symbol
    //!   - codebase.overview  counts and top directories
    //!
    //! Lexical, not compiler-grade: see the engine docs. That is deliberate;
    //! it works without a language server and is honest about ambiguity.

    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::codegraph::CodeGraph;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn err(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Execution,
            false,
            cause,
            "check the repository path and index it with codebase.index",
            "",
        )
    }

    fn schema(
        name: &str,
        description: &str,
        props: serde_json::Value,
        required: Vec<&str>,
    ) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": props,
                "required": required,
            }),
        }
    }

    fn root_of(v: &serde_json::Value) -> PathBuf {
        v.get("path")
            .and_then(|x| x.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    fn canonical_root(root: &Path) -> PathBuf {
        std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
    }

    fn graph(root: &Path, state_dir: &Path, force: bool) -> Result<CodeGraph, PantheonError> {
        let root = canonical_root(root);
        let cache = CodeGraph::cache_path_for(state_dir, &root);
        if !force {
            if let Some(g) = CodeGraph::load(&cache) {
                if canonical_root(Path::new(&g.root)) == root && g.is_current_for(&root) {
                    return Ok(g);
                }
            }
        }
        let g = CodeGraph::build(&root)?;
        // A cache write is best-effort: a read-only data dir must not make
        // orientation queries fail.
        let _ = g.save(&cache);
        Ok(g)
    }

    fn limit_of(v: &serde_json::Value) -> usize {
        v.get("limit")
            .and_then(|x| x.as_u64())
            .unwrap_or(50)
            .clamp(1, 500) as usize
    }

    /// Register the codebase graph toolset. `state_dir` is the Pantheon data
    /// dir; the cache lives at `<state_dir>/codegraph/index.json`.
    pub fn register_codegraph(reg: &mut ToolRegistry, state_dir: PathBuf) {
        let sd = Arc::new(state_dir);

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "codebase.index",
            "Build or refresh a native Rust symbol/call graph for a repository, cache it, and return counts. Lexical (syn-based), not compiler-grade.",
            serde_json::json!({
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "force": {"type":"boolean","description":"Rebuild even when a cache for this root exists"},
            }),
            vec![],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let root = root_of(&v);
            let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(true);
            let g = graph(&root, &s, force)?;
            serde_json::to_string_pretty(&g.overview())
                .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "codebase.search",
            "Search the cached codebase graph for symbols whose name, qualified name, or file contains the query.",
            serde_json::json!({
                "query": {"type":"string","description":"Case-insensitive substring"},
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "limit": {"type":"integer","description":"Max results (default 50, max 500)"},
            }),
            vec!["query"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let query = v
                .get("query")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("CODEGRAPH_BAD_ARGS", "missing 'query'".to_string()))?;
            let g = graph(&root_of(&v), &s, false)?;
            let results = g.search(query, limit_of(&v));
            serde_json::to_string_pretty(&serde_json::json!({
                "query": query,
                "root": g.root,
                "results": results,
            }))
            .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "codebase.callers",
            "Find lexical call sites whose callee name matches a symbol (exact final path segment).",
            serde_json::json!({
                "symbol": {"type":"string","description":"Function/method name"},
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "limit": {"type":"integer","description":"Max hits (default 50, max 500)"},
            }),
            vec!["symbol"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let symbol = v
                .get("symbol")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("CODEGRAPH_BAD_ARGS", "missing 'symbol'".to_string()))?;
            let g = graph(&root_of(&v), &s, false)?;
            let hits = g.callers(symbol, limit_of(&v));
            serde_json::to_string_pretty(&serde_json::json!({
                "symbol": symbol,
                "root": g.root,
                "callers": hits,
            }))
            .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "codebase.callees",
            "List the lexical calls made by a symbol (exact name or qualified suffix).",
            serde_json::json!({
                "symbol": {"type":"string","description":"Function/method name"},
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "limit": {"type":"integer","description":"Max hits (default 50, max 500)"},
            }),
            vec!["symbol"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let symbol = v
                .get("symbol")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("CODEGRAPH_BAD_ARGS", "missing 'symbol'".to_string()))?;
            let g = graph(&root_of(&v), &s, false)?;
            let hits = g.callees(symbol, limit_of(&v));
            serde_json::to_string_pretty(&serde_json::json!({
                "symbol": symbol,
                "root": g.root,
                "callees": hits,
            }))
            .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "codebase.overview",
            "Return codebase graph counts by symbol kind and the top directories by symbol count.",
            serde_json::json!({
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
            }),
            vec![],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let g = graph(&root_of(&v), &s, false)?;
            serde_json::to_string_pretty(&g.overview())
                .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );
    }
}
pub mod format {
    //! Opt-in post-edit formatting.
    //!
    //! The runtime constructs one [`Formatter`] from `[formatters]` and hands
    //! it to the built-in and safe-write tools. A formatter is a separate
    //! subprocess with a hard timeout; a formatting failure is reported in
    //! the tool result but never undoes the write. The model cannot choose the
    //! command: rules come from operator config or this module's built-ins.

    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// One formatter invocation result. `success == false` is informational;
    /// callers must not roll back a successful write because formatting failed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FormatOutcome {
        pub command: String,
        pub success: bool,
        pub output: String,
    }

    /// A formatting failure or success report for a set of paths.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct FormatReport {
        pub outcomes: Vec<(PathBuf, FormatOutcome)>,
    }

    impl FormatReport {
        pub fn is_empty(&self) -> bool {
            self.outcomes.is_empty()
        }

        /// A compact line for tool output. No command path is secret: the
        /// command is operator config, but output may contain file content, so
        /// it is bounded and kept short.
        pub fn summary(&self) -> String {
            if self.outcomes.is_empty() {
                return String::new();
            }
            let ok = self.outcomes.iter().filter(|(_, o)| o.success).count();
            let failed = self.outcomes.len() - ok;
            let mut out = format!("formatted {ok} file(s)");
            if failed > 0 {
                out.push_str(&format!("; {failed} failed"));
                for (path, outcome) in self.outcomes.iter().filter(|(_, o)| !o.success) {
                    out.push_str(&format!(
                        "\n  {}: {}",
                        path.display(),
                        truncate(&outcome.output, 300)
                    ));
                }
            }
            out
        }
    }

    /// Operator configuration for post-edit formatting.
    #[derive(Debug, Clone)]
    pub struct Formatter {
        timeout: Duration,
        /// Extension (without the dot, lowercase) -> command template.
        commands: HashMap<String, String>,
    }

    impl Formatter {
        pub fn new(timeout: Duration, commands: HashMap<String, String>) -> Self {
            let commands = commands
                .into_iter()
                .filter_map(|(ext, command)| {
                    let ext = normalize_ext(&ext)?;
                    let command = command.trim().to_string();
                    (!command.is_empty()).then_some((ext, command))
                })
                .collect();
            Self {
                timeout: timeout.max(Duration::from_millis(250)),
                commands,
            }
        }

        /// The command that would run for this path, if any. Pure and cheap;
        /// used by tests and by `/settings` style diagnostics.
        pub fn command_for(&self, path: &Path) -> Option<String> {
            let ext = path.extension()?.to_str()?.to_ascii_lowercase();
            self.commands
                .get(&ext)
                .cloned()
                .or_else(|| builtin_command(&ext).map(str::to_string))
        }

        /// Format one file. `None` means no rule matched (not a failure).
        pub fn format(&self, path: &Path) -> Option<FormatOutcome> {
            let command = self.command_for(path)?;
            Some(self.run(&command, path))
        }

        /// Format a batch, skipping paths with no rule. Callers use the report
        /// for the tool result; failures do not propagate as errors.
        pub fn format_paths<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> FormatReport {
            let mut report = FormatReport::default();
            for path in paths {
                if let Some(outcome) = self.format(path) {
                    report.outcomes.push((path.to_path_buf(), outcome));
                }
            }
            report
        }

        fn run(&self, template: &str, path: &Path) -> FormatOutcome {
            // Keep `{file}` substitution shell-safe: the path is passed as
            // $1, never interpolated into the command string. A template
            // without `{file}` gets the path appended.
            let script = if template.contains("{file}") {
                template.replace("{file}", "\"$1\"")
            } else {
                format!("{template} \"$1\"")
            };
            let mut cmd = if cfg!(windows) {
                let mut c = Command::new("cmd");
                c.arg("/C").arg(script.replace("$1", "%1"));
                c
            } else {
                let mut c = Command::new("sh");
                c.arg("-c").arg(script).arg("pantheon-format").arg(path);
                c
            };
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                cmd.current_dir(parent);
            }
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    return FormatOutcome {
                        command: template.to_string(),
                        success: false,
                        output: format!("spawn failed: {e}"),
                    }
                }
            };
            let deadline = Instant::now() + self.timeout;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return FormatOutcome {
                            command: template.to_string(),
                            success: false,
                            output: format!("timed out after {}s", self.timeout.as_secs()),
                        };
                    }
                    Err(e) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return FormatOutcome {
                            command: template.to_string(),
                            success: false,
                            output: format!("wait failed: {e}"),
                        };
                    }
                }
            }
            let output = match child.wait_with_output() {
                Ok(out) => out,
                Err(e) => {
                    return FormatOutcome {
                        command: template.to_string(),
                        success: false,
                        output: format!("read output failed: {e}"),
                    }
                }
            };
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !text.trim().is_empty() {
                    text.push('\n');
                }
                text.push_str(stderr.trim());
            }
            FormatOutcome {
                command: template.to_string(),
                success: output.status.success(),
                output: truncate(text.trim(), 2_000),
            }
        }
    }

    fn normalize_ext(ext: &str) -> Option<String> {
        let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
        (!ext.is_empty()).then_some(ext)
    }

    fn builtin_command(ext: &str) -> Option<&'static str> {
        match ext {
            "rs" => Some("rustfmt --edition 2021 {file}"),
            "go" => Some("gofmt -w {file}"),
            "py" => Some("ruff format {file}"),
            "js" | "jsx" | "ts" | "tsx" | "json" | "jsonc" | "css" | "scss" | "less" | "md"
            | "mdx" | "yml" | "yaml" | "html" => Some("prettier --write {file}"),
            "sh" | "bash" | "zsh" => Some("shfmt -w {file}"),
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" => Some("clang-format -i {file}"),
            "toml" => Some("taplo fmt {file}"),
            _ => None,
        }
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            return s.to_string();
        }
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashMap;

        #[test]
        fn custom_rule_overrides_builtin_and_normalizes_extension() {
            let f = Formatter::new(
                Duration::from_secs(1),
                HashMap::from([("RS".to_string(), "myfmt {file}".to_string())]),
            );
            assert_eq!(
                f.command_for(Path::new("src/lib.rs")).as_deref(),
                Some("myfmt {file}")
            );
        }

        #[test]
        fn builtin_rules_cover_common_languages() {
            let f = Formatter::new(Duration::from_secs(1), HashMap::new());
            assert!(f
                .command_for(Path::new("main.go"))
                .unwrap()
                .contains("gofmt"));
            assert!(f
                .command_for(Path::new("app.tsx"))
                .unwrap()
                .contains("prettier"));
            assert!(f.command_for(Path::new("README")).is_none());
        }

        #[test]
        fn missing_command_is_a_reported_failure_not_a_panic() {
            let f = Formatter::new(
                Duration::from_secs(1),
                HashMap::from([(
                    "txt".to_string(),
                    "definitely-not-a-real-formatter-xyz {file}".to_string(),
                )]),
            );
            let out = f.format(Path::new("note.txt")).expect("rule matched");
            assert!(!out.success);
            assert!(out.output.contains("spawn failed") || !out.output.is_empty());
        }

        #[test]
        fn custom_command_receives_the_file_path_without_shell_interpolation() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("note.foo");
            std::fs::write(&path, "x").unwrap();
            let f = Formatter::new(
                Duration::from_secs(2),
                HashMap::from([(
                    "foo".to_string(),
                    "sh -c 'printf formatted > \"$1\"' -- {file}".to_string(),
                )]),
            );
            let out = f.format(&path).expect("rule matched");
            assert!(out.success, "{out:?}");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "formatted");
        }
    }
}
pub mod gitundo_tools {
    //! Git-undo toolset: repo-level snapshot/restore for batch workspaces.
    //!
    //! `safewrite` checkpoints individual file edits. These tools snapshot the
    //! entire git working tree before a batch of edits and restore all changed
    //! files at once. The engine lives in `pantheon-exec::gitundo`; this
    //! module registers the tools on a registry (capability != tool).
    //!
    //! Tools:
    //!   - gitundo.snapshot  create a labeled undo point
    //!   - gitundo.list      list stored undo points, newest first
    //!   - gitundo.restore   restore one undo point by id
    //!   - gitundo.delete    drop one undo point and its ref
    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::gitundo::{repo_root, GitUndo};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn err(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Execution,
            false,
            cause,
            "check tool arguments",
            "",
        )
    }

    /// Options for `register_gitundo_with`.
    pub struct GitUndoOptions {
        /// State dir for stored patches + manifests (`<state>/git_undo`).
        pub state_dir: PathBuf,
        /// The git working tree to snapshot/restore. `None` = process cwd.
        pub repo_dir: Option<PathBuf>,
    }

    /// Register the git-undo toolset on a registry.
    pub fn register_gitundo(reg: &mut ToolRegistry, state_dir: PathBuf) {
        register_gitundo_with(
            reg,
            GitUndoOptions {
                state_dir,
                repo_dir: None,
            },
        );
    }

    /// Register with options.
    pub fn register_gitundo_with(reg: &mut ToolRegistry, opts: GitUndoOptions) {
        let sdir: Arc<PathBuf> = Arc::new(opts.state_dir);
        let rdir: Arc<Option<PathBuf>> = Arc::new(opts.repo_dir);

        /// Resolve the GitUndo handle for a call. When `repo_dir` is unset
        /// (or points at a non-repo), fall back to the process cwd's repo
        /// root; if that is also not a repo, report a clean error.
        fn handle(
            v: &serde_json::Value,
            sdir: &Arc<PathBuf>,
            rdir: &Arc<Option<PathBuf>>,
        ) -> Result<GitUndo, String> {
            let state_dir = if let Some(d) = v.get("state_dir").and_then(|x| x.as_str()) {
                PathBuf::from(d)
            } else {
                (**sdir).clone()
            };
            let repo = if let Some(d) = v.get("repo_dir").and_then(|x| x.as_str()) {
                PathBuf::from(d)
            } else {
                rdir.as_ref().clone().unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                })
            };
            let root = repo_root(&repo).ok_or_else(|| {
                "no git repo found at the target dir; git undo is a no-op".to_string()
            })?;
            GitUndo::new(state_dir, root).map_err(|e| e.to_string())
        }

        let mk = |name: &str,
                  desc: &str,
                  props: serde_json::Value,
                  required: Vec<&str>|
         -> ToolSchema {
            ToolSchema {
                name: name.into(),
                description: desc.into(),
                parameters: serde_json::json!({"type":"object","properties":props,"required":required}),
            }
        };

        let s1 = sdir.clone();
        let r1 = rdir.clone();
        reg.register(
        mk(
            "gitundo.snapshot",
            "Snapshot the whole git working tree (tracked + untracked) into a labeled undo point. The user's branch is never moved. Returns the undo point id.",
            serde_json::json!({
                "label": {"type":"string","description":"Short label for the snapshot"},
                "repo_dir": {"type":"string","description":"Git working tree (default: process cwd)"},
                "state_dir": {"type":"string","description":"State dir (default: registration value)"},
            }),
            vec!["label"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let h = handle(&v, &s1, &r1).map_err(|m| err("UNDO_NO_REPO", m))?;
            let label = v
                .get("label")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'label'".to_string()))?;
            let point = h.snapshot(label)?;
            Ok(serde_json::to_string_pretty(&point).unwrap_or_else(|_| format!("undo point {}", point.id)))
        },
    );

        let s2 = sdir.clone();
        let r2 = rdir.clone();
        reg.register(
            mk(
                "gitundo.list",
                "List all stored git undo points, newest first.",
                serde_json::json!({
                    "repo_dir": {"type":"string"},
                    "state_dir": {"type":"string"},
                }),
                vec![],
            ),
            Capability::GitRead,
            move |args| {
                let v = parse_args(args)?;
                let h = handle(&v, &s2, &r2).map_err(|m| err("UNDO_NO_REPO", m))?;
                let points = h.list()?;
                serde_json::to_string_pretty(&points)
                    .map_err(|e| err("UNDO_LIST_JSON", e.to_string()))
            },
        );

        let s3 = sdir.clone();
        let r3 = rdir.clone();
        reg.register(
        mk(
            "gitundo.restore",
            "Restore the working tree to a previous git undo point. Tries the stored patch first, falls back to a snapshot checkout. Refuses a corrupt patch.",
            serde_json::json!({
                "undo_id": {"type":"string","description":"Undo point id to restore"},
                "repo_dir": {"type":"string"},
                "state_dir": {"type":"string"},
            }),
            vec!["undo_id"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let h = handle(&v, &s3, &r3).map_err(|m| err("UNDO_NO_REPO", m))?;
            let id = v
                .get("undo_id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'undo_id'".to_string()))?;
            let point = h
                .list()?
                .into_iter()
                .find(|p| p.id == id)
                .ok_or_else(|| err("UNDO_NOT_FOUND", format!("no undo point '{id}' stored")))?;
            let res = h.restore(&point)?;
            serde_json::to_string_pretty(&res).map_err(|e| err("UNDO_RESTORE_JSON", e.to_string()))
        },
    );

        let s4 = sdir.clone();
        let r4 = rdir.clone();
        reg.register(
            mk(
                "gitundo.delete",
                "Delete one git undo point and its ref + patch + manifest.",
                serde_json::json!({
                    "undo_id": {"type":"string","description":"Undo point id to delete"},
                    "repo_dir": {"type":"string"},
                    "state_dir": {"type":"string"},
                }),
                vec!["undo_id"],
            ),
            Capability::GitWrite,
            move |args| {
                let v = parse_args(args)?;
                let h = handle(&v, &s4, &r4).map_err(|m| err("UNDO_NO_REPO", m))?;
                let id = v
                    .get("undo_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'undo_id'".to_string()))?;
                let point =
                    h.list()?.into_iter().find(|p| p.id == id).ok_or_else(|| {
                        err("UNDO_NOT_FOUND", format!("no undo point '{id}' stored"))
                    })?;
                h.delete(&point)?;
                Ok(format!("deleted undo point {id}"))
            },
        );
    }
}
pub mod lsp_tools {
    //! LSP diagnostics toolset: feed real type-check / lint results back to the
    //! model after edits. The engine (a minimal JSON-RPC LSP client +
    //! diagnostics cache) lives in `pantheon-exec::lsp`; this module registers
    //! the callable surface.
    //!
    //! The language server is long-lived, so the client is kept in a shared
    //! `Arc<Mutex<Option<Arc<LspClient>>>>` and started lazily on first use,
    //! keyed by server command. Tools:
    //!   - lsp.open      open a file, wait for its first diagnostics batch
    //!   - lsp.diagnostics  read the latest diagnostics for a file (or all)
    //!   - lsp.shutdown  shut the server down and clear the cache
    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::lsp::{
        language_for, path_uri, resolve_workspace_root, server_for, LspClient,
    };
    use pantheon_exec::sandbox::{SandboxLevel, SandboxProfile};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
        PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
    }

    /// Options for `register_lsp_with`.
    pub struct LspOptions {
        /// Root URI for the LSP `initialize` handshake (default: `file://<cwd>`).
        pub root_uri: Option<String>,
        /// Handshake + diagnostics wait timeout.
        pub timeout: Duration,
        /// Sandbox level the language server runs at.
        ///
        /// `None` is the caller-gated exception and spawns un-isolated; the
        /// default is [`SandboxLevel::High`], the level the shell tool runs
        /// at, because `lsp.open` is model-triggered process execution just
        /// like the shell. With a declared level the spawn goes through
        /// `build_sandboxed_checked`: it refuses (`SANDBOX_UNAVAILABLE`) when
        /// the boundary's wrapper cannot initialize instead of starting the
        /// server un-isolated, and an opted-in direct fallback is announced
        /// on stderr rather than being silent.
        ///
        /// Declared here rather than read from config because the command
        /// table (`server_for`) is in-tree: the policy question is *how
        /// isolated*, never *which binary*.
        pub sandbox_profile: Option<SandboxProfile>,
    }

    /// Register the LSP diagnostics toolset on a registry. The shared client
    /// is created here and captured by every tool closure so the server
    /// survives across calls.
    pub fn register_lsp(reg: &mut ToolRegistry) {
        register_lsp_with(reg, LspOptions::default());
    }

    impl Default for LspOptions {
        fn default() -> Self {
            Self {
                root_uri: None,
                timeout: Duration::from_secs(20),
                sandbox_profile: Some(SandboxLevel::High.profile()),
            }
        }
    }

    pub fn register_lsp_with(reg: &mut ToolRegistry, opts: LspOptions) {
        let default_root_uri: Arc<String> = Arc::new(opts.root_uri.unwrap_or_else(|| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            path_uri(&cwd)
        }));
        let timeout = opts.timeout;
        // Cloned into each tool closure: `ensure` borrows it for the spawn.
        let profile = opts.sandbox_profile;
        let server: Arc<Mutex<Option<Arc<LspClient>>>> = Arc::new(Mutex::new(None));
        // Tracks the root the live server was initialized against. A language
        // server only fully analyzes files that belong to its own workspace,
        // so when `lsp.open` targets a file in a *different* project we must
        // restart the server re-rooted at that project, not reuse the old one.
        let active_root: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let last_lang: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        /// Get a live client rooted at `target_root`, restarting the server
        /// when `target_root` differs from the one the current client is
        /// rooted at. `default_root_uri` is the fallback when no explicit root
        /// is supplied.
        fn ensure(
            server: &Arc<Mutex<Option<Arc<LspClient>>>>,
            active_root: &Arc<Mutex<Option<String>>>,
            lang: &str,
            target_root: Option<&str>,
            default_root_uri: &str,
            timeout: Duration,
            profile: Option<&SandboxProfile>,
        ) -> Result<Arc<LspClient>, PantheonError> {
            let want_root = target_root.unwrap_or(default_root_uri).to_string();
            let mut g = server.lock().unwrap_or_else(|p| p.into_inner());
            let mut rg = active_root.lock().unwrap_or_else(|p| p.into_inner());
            // Restart if there is no client yet, or the client was rooted at a
            // different workspace than the one this call targets.
            if g.is_none() || Some(want_root.as_str()) != rg.as_ref().map(|s| s.as_str()) {
                if g.is_some() {
                    if let Some(old) = g.take() {
                        old.shutdown();
                    }
                    *rg = None;
                }
                let (program, args) = server_for(lang).ok_or_else(|| {
                    PantheonError::new(
                        "LSP_NO_SERVER",
                        Layer::Execution,
                        false,
                        format!("no language server known for '{lang}'"),
                        "install the language server binary or pick a supported language",
                        "",
                    )
                })?;
                *g = Some(LspClient::start(
                    &program, &args, lang, &want_root, timeout, profile,
                )?);
                *rg = Some(want_root);
            }
            g.clone().ok_or_else(|| {
                PantheonError::new(
                    "LSP_NO_CLIENT",
                    Layer::Execution,
                    false,
                    "no live LSP client".to_string(),
                    "call lsp.open first",
                    "",
                )
            })
        }

        let mk = |name: &str,
                  desc: &str,
                  props: serde_json::Value,
                  required: Vec<&str>|
         -> ToolSchema {
            ToolSchema {
                name: name.into(),
                description: desc.into(),
                parameters: serde_json::json!({"type":"object","properties":props,"required":required}),
            }
        };

        let s1 = server.clone();
        let l1 = last_lang.clone();
        let a1 = active_root.clone();
        let d1 = default_root_uri.clone();
        let p1 = profile.clone();
        reg.register(
        mk(
            "lsp.open",
            "Open a source file in the language server and wait for its first diagnostics batch. Starts the server on demand and roots it at the file's project. Returns the diagnostics (may be empty if clean).",
            serde_json::json!({
                "path": {"type":"string","description":"Absolute or workspace-relative file path"},
                "language": {"type":"string","description":"Optional language override (default: inferred from extension)"},
                "wait_secs": {"type":"integer","description":"Diagnostics wait budget (default 15)"},
                "root": {"type":"string","description":"Optional explicit project root directory (default: auto-detected by walking up from the file)"},
            }),
            vec!["path"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let path = v
                .get("path")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'path'".to_string(), "check tool arguments"))?;
            let lang = v
                .get("language")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    Path::new(path)
                        .extension()
                        .and_then(|e| language_for(e.to_str()?).map(|s| s.to_string()))
                })
                .ok_or_else(|| err("LSP_NO_LANG", format!("cannot infer language for '{path}'"), "pass 'language' explicitly"))?;
            *l1.lock().unwrap_or_else(|p| p.into_inner()) = Some(lang.clone());
            let wait_secs = v.get("wait_secs").and_then(|x| x.as_u64()).unwrap_or(15);
            let cpath = PathBuf::from(path);
            // Resolve the project root: an explicit `root` wins, otherwise
            // walk up from the file to its nearest manifest. This is what
            // lets the tool analyze files in projects other than the one
            // pantheon was launched from.
            let target_root = match v.get("root").and_then(|x| x.as_str()) {
                Some(explicit) => Some(path_uri(Path::new(explicit))),
                None => Some(path_uri(&resolve_workspace_root(&cpath))),
            };
            let client = ensure(&s1, &a1, &lang, target_root.as_deref(), &d1, timeout, p1.as_ref())?;
            let text = std::fs::read_to_string(&cpath).map_err(|e| {
                err(
                    "LSP_READ",
                    format!("read {path}: {e}"),
                    "check the file path",
                )
            })?;
            client.open_document(&cpath, &lang, &text)?;
            let uri = path_uri(&cpath);
            let diags = client.wait_diagnostics(&uri, Duration::from_secs(wait_secs))?;
            match diags {
                Some(d) if d.count > 0 => Ok(serde_json::to_string_pretty(&d).unwrap_or(d.uri)),
                Some(d) => {
                    // An empty batch by the deadline means the server
                    // emitted "no diagnostics yet" - it may still be
                    // analyzing (rust-analyzer's first batch is empty).
                    // Report that honestly instead of claiming the file is
                    // clean.
                    Ok(format!(
                        "no diagnostics for {uri} within {wait_secs}s; the server may still be analyzing (retry, or increase wait_secs). last batch: {} error(s)",
                        d.count
                    ))
                }
                None => Ok(format!(
                    "no diagnostics for {uri} within {wait_secs}s (server may not be ready or file is clean)"
                )),
            }
        },
    );

        let s2 = server.clone();
        let l2 = last_lang.clone();
        let a2 = active_root.clone();
        let d2 = default_root_uri.clone();
        let p2 = profile.clone();
        reg.register(
        mk(
            "lsp.diagnostics",
            "Read the latest LSP diagnostics for one file ('path') or all cached files ('path' omitted). Read-only against the diagnostics cache; no file access.",
            serde_json::json!({
                "path": {"type":"string","description":"File to read diagnostics for; omit for all"},
                "language": {"type":"string","description":"Language whose server produced the cache"},
            }),
            vec![],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let lang = v
                .get("language")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
                .or_else(|| l2.lock().unwrap_or_else(|p| p.into_inner()).clone())
                .ok_or_else(|| err("LSP_NO_LANG", "no language in scope; pass 'language'".to_string(), "pass 'language' explicitly"))?;
            let client = ensure(&s2, &a2, &lang, None, &d2, timeout, p2.as_ref())?;
            let maybe_uri = v.get("path").and_then(|x| x.as_str()).map(|p| path_uri(Path::new(p)));
            let out: Vec<_> = match maybe_uri {
                Some(uri) => client.diagnostics(&uri).into_iter().collect(),
                None => client.all_diagnostics(),
            };
            serde_json::to_string_pretty(&out)
                .map_err(|e| err("LSP_JSON", e.to_string(), "internal: serialize diagnostics"))
        },
    );

        let s3 = server.clone();
        let a3 = active_root.clone();
        reg.register(
            mk(
                "lsp.shutdown",
                "Shut the LSP server down cleanly and clear the diagnostics cache.",
                serde_json::json!({}),
                vec![],
            ),
            Capability::ShellExecute,
            move |_args| {
                let mut g = s3.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(client) = g.take() {
                    client.shutdown();
                    *a3.lock().unwrap_or_else(|p| p.into_inner()) = None;
                    Ok("LSP server shut down; diagnostics cache cleared".to_string())
                } else {
                    Ok("no live LSP server; nothing to shut down".to_string())
                }
            },
        );
    }
}
pub mod memory_tools {
    //! Model-facing memory tools: five tools the agent can call to recall,
    //! list, propose, forget, and confirm records. All writes go through
    //! `propose -> policy -> provenance -> validation` and emit ledger events.
    //! Reads respect `MemoryRead`. Writes require `MemoryWrite` and may
    //! require approval per the runtime policy. Confirming (trust-tier
    //! promotion) requires the separate `MemoryConfirm` capability, which
    //! the default policies mark as needing human approval.
    //!
    //! The tools never touch the SQLite layer directly; they call the same
    //! `MemoryBackend` capability gate the CLI does. A backend swap (native
    //! store, GalaxyMem, Mnemosyne, Honcho, Hindsight) only changes the
    //! `MemoryStore` implementation passed in here.
    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use pantheon_api::message::ToolSchema;
    use pantheon_memory::{
        confirm_via, recall_via, write_via, LayerKind, MemoryBackend, Proposal, Provenance,
    };
    use serde::{Deserialize, Serialize};
    use std::sync::Arc;

    /// Where tool events go. The runtime session implements this with its
    /// ledger supervisor; tests pass a thread-safe Vec.
    pub trait MemoryToolSink: Send + Sync {
        fn record(&self, event: MemoryToolEvent);
    }

    /// Concrete sink used by tests: collects events into a shared Vec.
    #[derive(Clone, Default)]
    pub struct VecMemorySink {
        pub events: std::sync::Arc<std::sync::Mutex<Vec<MemoryToolEvent>>>,
    }

    impl VecMemorySink {
        pub fn new() -> Self {
            Self::default()
        }
        pub fn snapshot(&self) -> Vec<MemoryToolEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl MemoryToolSink for VecMemorySink {
        fn record(&self, event: MemoryToolEvent) {
            if let Ok(mut g) = self.events.lock() {
                g.push(event);
            }
        }
    }

    /// What the tools tell the outside world. Distinct from `Event` so
    /// memory tools stay decoupled from the agent event enum, but the
    /// runtime session projects these into the ledger.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum MemoryToolEvent {
        Recalled {
            query: String,
            hits: usize,
        },
        Listed {
            namespace: String,
            rows: usize,
        },
        Proposed {
            layer: LayerKind,
            namespace: String,
            key: String,
            value_len: usize,
            origin: String,
        },
        Written {
            layer: LayerKind,
            namespace: String,
            key: String,
            backend: String,
        },
        Forgotten {
            layer: LayerKind,
            namespace: String,
            key: String,
        },
        Denied {
            code: String,
            cause: String,
        },
    }

    impl MemoryToolSink for Arc<std::sync::Mutex<Vec<MemoryToolEvent>>> {
        fn record(&self, event: MemoryToolEvent) {
            if let Ok(mut g) = self.lock() {
                g.push(event);
            }
        }
    }

    /// Options for `register_memory_tools`. The store and policy are shared
    /// across the five tools. `max_bytes` caps proposal sizes the same way
    /// the CLI does.
    #[derive(Clone)]
    pub struct MemoryToolOptions {
        /// Active backend (native store or any registered plugin backend:
        /// GalaxyMem, Mnemosyne, Honcho, Hindsight, OpenViking, http bridge).
        /// Gating runs through `recall_via`/`write_via`/`confirm_via` before
        /// the backend sees anything.
        pub store: Arc<dyn MemoryBackend>,
        pub policy: Arc<pantheon_api::capability::Policy>,
        pub namespace: String,
        pub max_bytes: usize,
        pub sink: Arc<dyn MemoryToolSink>,
        pub backend_label: String,
    }

    impl MemoryToolOptions {
        pub fn now(&self) -> Provenance {
            Provenance {
                source: self.backend_label.clone(),
                origin: "model".into(),
                // Model-authored proposals are untrusted; propose_write clamps
                // them anyway, this makes the intent explicit at the source.
                trust: pantheon_api::provenance::TrustTier::Untrusted,
                recorded_at_ms: now_ms(),
            }
        }
    }

    fn layer_str_to_kind(s: &str) -> Option<LayerKind> {
        match s {
            "global" => Some(LayerKind::Global),
            "agent" => Some(LayerKind::Agent),
            "project" => Some(LayerKind::Project),
            "task" | "task_session" | "session" => Some(LayerKind::TaskSession),
            _ => None,
        }
    }

    /// Resolve a tool-supplied namespace against the session's own.
    ///
    /// The namespace is the memory isolation boundary: a session confined to
    /// `proj_a` must not be able to read, write, or forget records belonging to
    /// `proj_b` just by naming it in the tool arguments. The model chooses the
    /// argument, so the harness has to hold the boundary, not the model.
    ///
    /// An absent or empty argument means "this session's namespace", which keeps
    /// the common case working. An explicit argument that is anything else is
    /// refused with a message naming the boundary, rather than silently
    /// redirected, so a caller that expected cross-namespace access finds out.
    /// Resolve a tool-supplied namespace against the session's own.
    ///
    /// Public because this *is* the memory isolation boundary, and the agent
    /// runtime's tests need to assert the refusal directly rather than through
    /// a tool round trip. The behaviour is unchanged; only the visibility is.
    pub fn resolve_namespace(
        requested: Option<&str>,
        session_ns: &str,
    ) -> Result<String, PantheonError> {
        match requested.map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(session_ns.to_string()),
            Some(ns) if ns == session_ns => Ok(ns.to_string()),
            Some(other) => Err(crate::tools::tool_err(
                "MEM_NAMESPACE_DENIED",
                Layer::Execution,
                false,
                format!(
                    "this session may only use namespace '{session_ns}'; \
                 '{other}' belongs to another session"
                ),
                "check tool arguments and policy",
            )),
        }
    }

    pub fn register_memory_tools(reg: &mut ToolRegistry, opts: MemoryToolOptions) {
        let recall_opts = opts.clone();
        reg.register(
        ToolSchema {
            name: "memory_recall".into(),
            description:
                "Search native memory across layers. Returns the matching records as key/value lines."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Free-text search query."},
                    "limit": {"type": "integer", "description": "Max records to return (default 8)."}
                },
                "required": ["query"]
            }),
        },
        Capability::MemoryRead,
        move |args| {
            let v = parse_args(args)?;
            let query = v
                .get("query")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'query'".into(), "check tool arguments and policy"))?
                .to_string();
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(8) as usize;
            let layers = [
                LayerKind::Project,
                LayerKind::Agent,
                LayerKind::Global,
            ];
            // Recall is scoped to this agent's own namespace. Recall took
            // no namespace at all before, so `memory_recall` returned
            // every agent's records from a shared `memory.db` - the write
            // path was already scoped, which made the read side the leak.
            let namespaces = [recall_opts.namespace.as_str()];
            let hits = match recall_via(
                recall_opts.store.as_ref(),
                &recall_opts.policy,
                &namespaces,
                &layers,
                &query,
                limit,
            ) {
                Ok(h) => h,
                Err(e) => {
                    recall_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    return Err(e);
                }
            };
            recall_opts.sink.record(MemoryToolEvent::Recalled {
                query: query.clone(),
                hits: hits.len(),
            });
            let mut out = String::new();
            for h in &hits {
                // Trust framing: recalled records are context, not
                // instructions. Untrusted-sourced records are flagged so
                // the model weighs them accordingly.
                let trust_tag = match h.record.provenance.trust {
                    pantheon_api::provenance::TrustTier::Untrusted => {
                        format!(" [untrusted: {}]", h.record.provenance.source)
                    }
                    t => format!(" [trust:{}]", t.as_str()),
                };
                out.push_str(&format!(
                    "- [{:?}] {} = {}{}\n",
                    h.record.layer, h.record.key, h.record.value, trust_tag
                ));
            }
            if out.is_empty() {
                out.push_str("(no matches)\n");
            }
            Ok(out)
        },
    );

        let list_opts = opts.clone();
        reg.register(
        ToolSchema {
            name: "memory_list".into(),
            description: "List Agent-layer records in the current namespace. Output is bounded: at most `limit` records (default 50, max 500); a `truncated: true` marker says when more exist.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "description": "Max records to return (default 50, max 500)."}
                }
            }),
        },
        Capability::MemoryRead,
        move |args| {
            let v = parse_args(args)?;
            // Bound the output: an unbounded list dumps the whole agent
            // memory into the model's context. Clamp to [1, 500].
            let limit = v
                .get("limit")
                .and_then(|x| x.as_u64())
                .map(|n| n as usize)
                .unwrap_or(50)
                .clamp(1, 500);
            let rows = match list_opts.store.list_agent(&list_opts.namespace) {
                Ok(r) => r,
                Err(e) => {
                    list_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    return Err(e);
                }
            };
            let truncated = rows.len() > limit;
            let shown = rows.len().min(limit);
            list_opts.sink.record(MemoryToolEvent::Listed {
                namespace: list_opts.namespace.clone(),
                rows: shown,
            });
            let mut out = String::new();
            for (k, val) in rows.iter().take(limit) {
                out.push_str(&format!("- {k} = {val}\n"));
            }
            if out.is_empty() {
                out.push_str("(no agent records)\n");
            } else if truncated {
                out.push_str(&format!(
                    "truncated: true (showing first {limit} of {} records)\n",
                    rows.len()
                ));
            }
            Ok(out)
        },
    );

        let propose_opts = opts.clone();
        reg.register(
        ToolSchema {
            name: "memory_propose".into(),
            description:
                "Propose a memory write. Goes through propose -> policy -> provenance -> validation."
                    .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "value": {"type": "string"},
                    "layer": {"type": "string", "enum": ["global", "agent", "project", "task"]},
                    "namespace": {"type": "string", "description": "Defaults to session namespace."}
                },
                "required": ["key", "value"]
            }),
        },
        Capability::MemoryWrite,
        move |args| {
            let v = parse_args(args)?;
            let key = v
                .get("key")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'key'".into(), "check tool arguments and policy"))?
                .to_string();
            let value = v
                .get("value")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'value'".into(), "check tool arguments and policy"))?
                .to_string();
            let layer = v
                .get("layer")
                .and_then(|x| x.as_str())
                .and_then(layer_str_to_kind)
                .unwrap_or(LayerKind::Agent);
            let namespace = resolve_namespace(
                v.get("namespace").and_then(|x| x.as_str()),
                &propose_opts.namespace,
            )?;
            // Origin is harness-assigned ("model"), not model-claimed: a
            // proposal cannot launder untrusted material into trusted
            // memory by passing origin:"user". propose_write clamps the
            // tier for non-user origins regardless.
            let origin = "model".to_string();

            let mut prov = propose_opts.now();
            prov.origin = origin.clone();

            let proposal = Proposal {
                layer,
                namespace: namespace.clone(),
                key: key.clone(),
                value: value.clone(),
                provenance: prov,
            };

            propose_opts.sink.record(MemoryToolEvent::Proposed {
                layer,
                namespace: namespace.clone(),
                key: key.clone(),
                value_len: value.len(),
                origin: origin.clone(),
            });

            match write_via(
                propose_opts.store.as_ref(),
                &propose_opts.policy,
                proposal,
                propose_opts.max_bytes,
            ) {
                Ok(rec) => {
                    let key = rec.key.clone();
                    propose_opts.sink.record(MemoryToolEvent::Written {
                        layer: rec.layer,
                        namespace: rec.namespace,
                        key: key.clone(),
                        backend: propose_opts.backend_label.clone(),
                    });
                    Ok(format!("stored {} = {}", key, rec.value))
                }
                Err(e) => {
                    propose_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    Err(e)
                }
            }
        },
    );

        let forget_opts = opts.clone();
        reg.register(
            ToolSchema {
                name: "memory_forget".into(),
                description: "Remove a memory record. Goes through the same gate as propose."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "layer": {"type": "string", "enum": ["global", "agent", "project", "task"]},
                        "namespace": {"type": "string"}
                    },
                    "required": ["key"]
                }),
            },
            Capability::MemoryWrite,
            move |args| {
                let v = parse_args(args)?;
                let key = v
                    .get("key")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| {
                        crate::tools::tool_err(
                            "TOOL_BAD_ARGS",
                            Layer::Execution,
                            false,
                            "missing 'key'".into(),
                            "check tool arguments and policy",
                        )
                    })?
                    .to_string();
                let layer = v
                    .get("layer")
                    .and_then(|x| x.as_str())
                    .and_then(layer_str_to_kind)
                    .unwrap_or(LayerKind::Agent);
                let namespace = resolve_namespace(
                    v.get("namespace").and_then(|x| x.as_str()),
                    &forget_opts.namespace,
                )?;

                match forget_opts.store.forget(layer, &namespace, &key) {
                    Ok(true) => {
                        // Recorded only on a successful forget: the old code
                        // recorded `Forgotten` BEFORE the store call, so the
                        // ledger claimed a forget even when nothing was
                        // removed (`Ok(false)`) or the store errored.
                        forget_opts.sink.record(MemoryToolEvent::Forgotten {
                            layer,
                            namespace: namespace.clone(),
                            key: key.clone(),
                        });
                        Ok(format!("forgot {key}"))
                    }
                    Ok(false) => Err(crate::tools::tool_err(
                        "MEM_NOT_FOUND",
                        Layer::Execution,
                        false,
                        format!("no record for {key} in {namespace}"),
                        "check tool arguments and policy",
                    )),
                    Err(e) => {
                        forget_opts.sink.record(MemoryToolEvent::Denied {
                            code: e.code.clone(),
                            cause: e.cause.clone(),
                        });
                        Err(e)
                    }
                }
            },
        );

        // memory_confirm: the user-promotion path. Promotes an Untrusted
        // record to Memory tier. It cannot exceed Memory tier: System and
        // User are reserved for harness and human authors.
        //
        // Gated on Capability::MemoryConfirm, which the default policies
        // mark as requiring approval - NOT on MemoryWrite. The old gate
        // (MemoryWrite, Allow) plus the "call only after the user vouched"
        // description was honor-system: a prompt-injected model could
        // propose then confirm its own poisoned record into the trusted
        // tier. Now the run loop parks the call for a human first.
        let confirm_opts = opts.clone();
        reg.register(
        ToolSchema {
            name: "memory_confirm".into(),
            description: "Mark an existing memory record as user-confirmed. Requires human approval under the default policy: the run parks until a user vouches for the record's content. Unconfirmed records stay untrusted.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string"},
                    "layer": {"type": "string", "enum": ["global", "agent", "project", "task"], "description": "Layer of the record to confirm (default agent)."},
                    "namespace": {"type": "string", "description": "Defaults to session namespace; naming another session's namespace is refused."}
                },
                "required": ["key"]
            }),
        },
        Capability::MemoryConfirm,
        move |args| {
            let v = parse_args(args)?;
            let key = v
                .get("key")
                .and_then(|x| x.as_str())
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'key'".into(), "check tool arguments and policy"))?
                .to_string();
            let layer = v
                .get("layer")
                .and_then(|x| x.as_str())
                .and_then(layer_str_to_kind)
                .unwrap_or(LayerKind::Agent);
            // Namespace is resolved against the session's own, like every
            // other memory tool: the model must not confirm another
            // session's records by naming its namespace.
            let namespace = resolve_namespace(
                v.get("namespace").and_then(|x| x.as_str()),
                &confirm_opts.namespace,
            )?;
            match confirm_via(
                confirm_opts.store.as_ref(),
                &confirm_opts.policy,
                layer,
                &namespace,
                &key,
            ) {
                Ok(rec) => {
                    confirm_opts.sink.record(MemoryToolEvent::Written {
                        layer: rec.layer,
                        namespace: rec.namespace,
                        key: rec.key.clone(),
                        backend: confirm_opts.backend_label.clone(),
                    });
                    Ok(format!("confirmed {key}: now memory-tier"))
                }
                Err(e) => {
                    confirm_opts.sink.record(MemoryToolEvent::Denied {
                        code: e.code.clone(),
                        cause: e.cause.clone(),
                    });
                    Err(e)
                }
            }
        },
    );
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use pantheon_api::capability::Policy;
        use pantheon_memory::MemoryStore;

        fn harness(store: std::sync::Arc<dyn MemoryBackend>) -> (ToolRegistry, VecMemorySink) {
            let sink = VecMemorySink::new();
            let mut reg = ToolRegistry::default();
            register_memory_tools(
                &mut reg,
                MemoryToolOptions {
                    store,
                    policy: std::sync::Arc::new(Policy::coder_with_memory()),
                    namespace: "test".to_string(),
                    max_bytes: 4096,
                    sink: std::sync::Arc::new(sink.clone()),
                    backend_label: "test".to_string(),
                },
            );
            (reg, sink)
        }

        fn forgotten_count(sink: &VecMemorySink) -> usize {
            sink.snapshot()
                .iter()
                .filter(|e| matches!(e, MemoryToolEvent::Forgotten { .. }))
                .count()
        }

        #[test]
        fn forget_records_forgotten_only_on_successful_removal() {
            let store = std::sync::Arc::new(MemoryStore::open_in_memory().unwrap());
            store
                .put(&Proposal {
                    layer: LayerKind::Agent,
                    namespace: "test".to_string(),
                    key: "k".to_string(),
                    value: "v".to_string(),
                    provenance: Provenance {
                        source: "test".to_string(),
                        origin: "test".to_string(),
                        trust: pantheon_api::provenance::TrustTier::Untrusted,
                        recorded_at_ms: 1,
                    },
                })
                .unwrap();
            let (reg, sink) = harness(store as std::sync::Arc<dyn MemoryBackend>);
            // Success: one Forgotten event.
            assert!(reg.execute("memory_forget", r#"{"key":"k"}"#).is_ok());
            assert_eq!(forgotten_count(&sink), 1);
            // Not found: an error, and NO second Forgotten event - the old
            // code recorded Forgotten before the store call, so the ledger
            // claimed a forget that never happened.
            assert!(reg.execute("memory_forget", r#"{"key":"k"}"#).is_err());
            assert_eq!(forgotten_count(&sink), 1);
        }

        #[derive(Debug)]
        struct FailingForgetBackend;
        impl MemoryBackend for FailingForgetBackend {
            fn recall(
                &self,
                _policy: &Policy,
                _namespaces: &[&str],
                _layers: &[LayerKind],
                _query: &str,
                _limit: usize,
            ) -> Result<Vec<pantheon_memory::Recalled>, PantheonError> {
                Err(PantheonError::new(
                    "MEM_TEST_FAIL",
                    Layer::Memory,
                    false,
                    "boom".to_string(),
                    "test backend".to_string(),
                    "".to_string(),
                ))
            }
            fn write(
                &self,
                _policy: &Policy,
                _proposal: Proposal,
                _max_bytes: usize,
            ) -> Result<pantheon_memory::MemoryRecord, PantheonError> {
                Err(PantheonError::new(
                    "MEM_TEST_FAIL",
                    Layer::Memory,
                    false,
                    "boom".to_string(),
                    "test backend".to_string(),
                    "".to_string(),
                ))
            }
            fn list_agent(&self, _namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
                Ok(Vec::new())
            }
            fn forget(
                &self,
                _layer: LayerKind,
                _namespace: &str,
                _key: &str,
            ) -> Result<bool, PantheonError> {
                Err(PantheonError::new(
                    "MEM_TEST_FAIL",
                    Layer::Memory,
                    false,
                    "boom".to_string(),
                    "test backend".to_string(),
                    "".to_string(),
                ))
            }
        }

        #[test]
        fn forget_error_records_denied_not_forgotten() {
            let (reg, sink) = harness(std::sync::Arc::new(FailingForgetBackend));
            assert!(reg.execute("memory_forget", r#"{"key":"k"}"#).is_err());
            assert_eq!(
                forgotten_count(&sink),
                0,
                "a failed forget must not claim Forgotten"
            );
            assert!(
                sink.snapshot().iter().any(
                    |e| matches!(e, MemoryToolEvent::Denied { code, .. } if code == "MEM_TEST_FAIL")
                ),
                "the error path records Denied: {:?}",
                sink.snapshot()
            );
        }
    }
}
pub mod oauth_tools {
    //! OAuth connector tools: broker-side API calls for connected apps.
    //!
    //! The model never sees token bytes: each tool takes only the request
    //! shape (method, path, optional JSON body) and the secrets broker
    //! performs the authenticated request, returning the response body.
    //! Unconnected apps fail closed with the `pantheon oauth login <app>`
    //! remediation.
    //!
    //! All four tools carry `Capability::NetworkOutbound` under the current
    //! per-server capability model. Finer per-tool gating (e.g. parking
    //! write methods for approval) belongs to the per-tool capability map on
    //! the unmerged `chloe/agent-ui` branch and is intentionally not
    //! duplicated here.

    use crate::tools::{tool_err, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_secrets::SecretsBroker;
    use std::path::{Path, PathBuf};

    /// Options to configure OAuth connector tools.
    #[derive(Clone, Debug)]
    pub struct OAuthToolOptions {
        pub data_dir: PathBuf,
    }

    fn parse_oauth_args<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, PantheonError> {
        serde_json::from_str(raw).map_err(|e| {
            tool_err(
                "TOOL_BAD_ARGS",
                Layer::Execution,
                false,
                format!("failed to parse arguments: {e}"),
                "check the method, path/url, and body",
            )
        })
    }

    /// Broker-side request: open the durable broker, run the authed call,
    /// return the body. The token never leaves the broker.
    fn run_oauth(
        data_dir: &Path,
        app: &str,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> Result<String, PantheonError> {
        let broker = SecretsBroker::durable(data_dir).map_err(|e| {
            tool_err(
                "OAUTH_STORE",
                Layer::Execution,
                false,
                format!("cannot open secret store: {e}"),
                "check the data dir permissions",
            )
        })?;
        broker.oauth_request(app, method, url, body).map_err(|e| {
            tool_err(
                "OAUTH_REQUEST",
                Layer::Execution,
                false,
                e.to_string(),
                "run `pantheon oauth login <app>` if the app is not connected",
            )
        })
    }

    /// Register one path-scoped tool: `method` + `path` under a fixed base.
    fn register_path_tool(
        reg: &mut ToolRegistry,
        opts: &OAuthToolOptions,
        name: &str,
        app: &str,
        base: &str,
        description: &str,
    ) {
        let data_dir = opts.data_dir.clone();
        let app = app.to_string();
        let base = base.to_string();
        reg.register(
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "description": "HTTP method: GET, POST, PUT, PATCH or DELETE."
                    },
                    "path": {
                        "type": "string",
                        "description": format!("API path under {base}, e.g. '/user/repos'. Must start with '/'.")
                    },
                    "body": {
                        "type": "string",
                        "description": "Optional JSON request body."
                    }
                },
                "required": ["method", "path"]
            }),
        },
        Capability::NetworkOutbound,
        move |args| {
            #[derive(serde::Deserialize)]
            struct In {
                method: String,
                path: String,
                body: Option<String>,
            }
            let In { method, path, body } = parse_oauth_args(args)?;
            if !path.starts_with('/') {
                return Err(tool_err(
                    "OAUTH_BAD_PATH",
                    Layer::Execution,
                    false,
                    "path must start with '/'".into(),
                    "use a path like '/user/repos'",
                ));
            }
            run_oauth(
                &data_dir,
                &app,
                &method,
                &format!("{base}{path}"),
                body.as_deref(),
            )
        },
    );
    }

    /// Register the connector tools. Always registered: each tool fails
    /// closed when its app is not connected, and all carry
    /// `Capability::NetworkOutbound` so the existing capability gate applies.
    pub fn register_oauth_tools(reg: &mut ToolRegistry, opts: OAuthToolOptions) {
        register_path_tool(
        reg,
        &opts,
        "github_api",
        "github",
        "https://api.github.com",
        "Call the GitHub REST API with the connected account, broker-side. The OAuth token never leaves the broker; only the response body is returned. Fails closed when GitHub is not connected.",
    );
        // Google's API surface spans many hosts (gmail, calendar, drive):
        // the tool takes a full URL, constrained broker-side to
        // https://www.googleapis.com/*.
        {
            let data_dir = opts.data_dir.clone();
            reg.register(
            ToolSchema {
                name: "google_api".into(),
                description: "Call a Google API with the connected account, broker-side. The OAuth token never leaves the broker; only the response body is returned. Fails closed when Google is not connected.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "method": {
                            "type": "string",
                            "description": "HTTP method: GET, POST, PUT, PATCH or DELETE."
                        },
                        "url": {
                            "type": "string",
                            "description": "Full https://www.googleapis.com/... URL, e.g. 'https://www.googleapis.com/gmail/v1/users/me/messages'."
                        },
                        "body": {
                            "type": "string",
                            "description": "Optional JSON request body."
                        }
                    },
                    "required": ["method", "url"]
                }),
            },
            Capability::NetworkOutbound,
            move |args| {
                #[derive(serde::Deserialize)]
                struct In {
                    method: String,
                    url: String,
                    body: Option<String>,
                }
                let In { method, url, body } = parse_oauth_args(args)?;
                run_oauth(&data_dir, "google", &method, &url, body.as_deref())
            },
        );
        }
        register_path_tool(
        reg,
        &opts,
        "notion_api",
        "notion",
        "https://api.notion.com/v1",
        "Call the Notion API with the connected integration, broker-side. The OAuth token never leaves the broker and the Notion-Version header is pinned broker-side; only the response body is returned. Fails closed when Notion is not connected.",
    );
        register_path_tool(
        reg,
        &opts,
        "spotify_api",
        "spotify",
        "https://api.spotify.com/v1",
        "Call the Spotify Web API with the connected account, broker-side. The OAuth token never leaves the broker; only the response body is returned. Fails closed when Spotify is not connected.",
    );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn tools_register_with_network_outbound() {
            let mut reg = ToolRegistry::new();
            register_oauth_tools(
                &mut reg,
                OAuthToolOptions {
                    data_dir: PathBuf::from("/tmp/oauth-tools-test"),
                },
            );
            for name in ["github_api", "google_api", "notion_api", "spotify_api"] {
                let tool = reg.get(name).unwrap_or_else(|| panic!("{name} registered"));
                assert_eq!(tool.capability, Capability::NetworkOutbound);
            }
        }

        #[test]
        fn path_tool_rejects_relative_path_without_network() {
            let mut reg = ToolRegistry::new();
            register_oauth_tools(
                &mut reg,
                OAuthToolOptions {
                    data_dir: PathBuf::from("/tmp/oauth-tools-test"),
                },
            );
            // Relative path is rejected before any secret store is touched.
            let err = reg
                .execute("github_api", r#"{"method":"GET","path":"user/repos"}"#)
                .unwrap_err();
            assert!(err.to_string().contains("OAUTH_BAD_PATH"));
        }

        #[test]
        fn unconnected_app_fails_closed_with_remediation() {
            let mut reg = ToolRegistry::new();
            register_oauth_tools(
                &mut reg,
                OAuthToolOptions {
                    // Durable broker opens (file vault), but no tokens stored.
                    data_dir: PathBuf::from("/tmp/oauth-tools-test-unconnected"),
                },
            );
            let err = reg
                .execute("spotify_api", r#"{"method":"GET","path":"/me"}"#)
                .unwrap_err();
            assert!(
                err.to_string().contains("pantheon oauth login spotify"),
                "unexpected: {err}"
            );
        }
    }
}
pub mod plugin_tools {
    //! Plugin tools: manifest tools wired to a shared `PluginSupervisor`.
    //!
    //! Moved out of `pantheon-exec::supervisor` in the tool-layer split
    //! (capability ≠ tool): process supervision and plugin hosting stay in
    //! `pantheon-exec`; registering the manifest's tools on a registry is a
    //! tool-layer concern.

    use crate::tools::ToolRegistry;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_api::todo::TODO_TOOL_NAME;
    use pantheon_exec::plugins::{PluginManifest, ToolCapability};
    use pantheon_exec::supervisor::PluginSupervisor;
    use std::sync::{Arc, Mutex};

    /// Built-in tool names a plugin may never claim (compared case-insensitively).
    /// Enumerated from the tool modules in this crate: `builtins` (shell,
    /// read_file, write_file, list_dir, ask_user, enable_plugin, enable_mcp),
    /// `memory_tools` (memory_*), `session_search_tools`, `skill_tools`,
    /// `vault_tools`, `safewrite_tools`, `todo_tools` (`todo`, from
    /// `pantheon_api::todo::TODO_TOOL_NAME`).
    /// Kept in sync manually with those modules; `register_plugin_tools` also
    /// checks the live registry as belt-and-braces against tools registered
    /// from other crates.
    const BUILTIN_TOOL_NAMES: &[&str] = &[
        TODO_TOOL_NAME,
        "shell",
        "read_file",
        "write_file",
        "list_dir",
        "ask_user",
        "enable_plugin",
        "enable_mcp",
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

    /// Validate a plugin tool name before registration. Rejects:
    /// - empty names
    /// - names containing whitespace or path separators (`/`, `\`)
    /// - names that collide (case-insensitively) with a built-in tool
    ///   a plugin must not be able to shadow (squat) `shell` and friends
    /// - names under the reserved prefixes `pantheon.`, `builtin.`, `memory.`
    ///
    /// Plugin capabilities are self-declared, so this runs at registration
    /// time, before the name ever reaches the registry's `HashMap::insert`
    /// (which would otherwise silently replace the builtin).
    pub fn validate_plugin_tool_name(name: &str) -> Result<(), PantheonError> {
        if name.is_empty() {
            return Err(crate::tools::tool_err(
                "PLUGIN_TOOL_NAME_INVALID",
                Layer::Extension,
                false,
                "plugin tool name must not be empty".to_string(),
                "rename the plugin tool or remove it from the plugin manifest",
            ));
        }
        if name.chars().any(|c| c.is_whitespace()) {
            return Err(crate::tools::tool_err(
                "PLUGIN_TOOL_NAME_INVALID",
                Layer::Extension,
                false,
                format!("plugin tool name '{name}' must not contain whitespace"),
                "rename the plugin tool or remove it from the plugin manifest",
            ));
        }
        if name.contains('/') || name.contains('\\') {
            return Err(crate::tools::tool_err(
                "PLUGIN_TOOL_NAME_INVALID",
                Layer::Extension,
                false,
                format!("plugin tool name '{name}' must not contain path separators"),
                "rename the plugin tool or remove it from the plugin manifest",
            ));
        }
        let lower = name.to_lowercase();
        if let Some(builtin) = BUILTIN_TOOL_NAMES.iter().find(|b| **b == lower.as_str()) {
            return Err(crate::tools::tool_err(
                "PLUGIN_TOOL_NAME_CONFLICT",
                Layer::Extension,
                false,
                format!(
                    "plugin tool name '{name}' collides with built-in tool '{builtin}'; \
                 plugins cannot shadow built-in tools"
                ),
                "rename the plugin tool or remove it from the plugin manifest",
            ));
        }
        if let Some(prefix) = RESERVED_TOOL_PREFIXES
            .iter()
            .find(|p| lower.starts_with(**p))
        {
            return Err(crate::tools::tool_err(
                "PLUGIN_TOOL_NAME_CONFLICT",
                Layer::Extension,
                false,
                format!(
                    "plugin tool name '{name}' uses reserved prefix '{prefix}'; \
                 the '{prefix}' namespace belongs to the runtime"
                ),
                "rename the plugin tool or remove it from the plugin manifest",
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
                return Err(crate::tools::tool_err(
                    "PLUGIN_TOOL_NAME_CONFLICT",
                    Layer::Extension,
                    false,
                    format!(
                    "plugin tool name '{}' collides with already-registered tool '{existing}'; \
                     plugins cannot replace existing tools",
                    cap.name
                ),
                    "rename the plugin tool or remove it from the plugin manifest",
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
                        crate::tools::tool_err(
                            "TOOL_BAD_ARGS",
                            Layer::Execution,
                            false,
                            format!("invalid JSON args: {e}"),
                            "check tool name and arguments",
                        )
                    })?
                };
                let mut guard = sup.lock().map_err(|_| {
                    crate::tools::tool_err(
                        "PLUGIN_LOCK",
                        Layer::Execution,
                        false,
                        "plugin supervisor lock poisoned".to_string(),
                        "respawn the plugin",
                    )
                })?;
                guard.call(&cap.name, v)
            },
        );
    }
}
pub mod pty_tools {
    //! Persistent terminal sessions.
    //!
    //! A normal `shell` call is one process with a timeout. These tools keep a
    //! PTY and its child alive across tool calls, so the model can drive an
    //! interactive program (a REPL, a debugger, a long build) without losing
    //! state between turns. The manager is session-scoped: its working root is
    //! the same confined workspace root as the file tools, and it is dropped
    //! with the runtime session.
    //!
    //! The child is a real shell, so every tool carries `ShellExecute`. Opening
    //! a command also passes through the same dangerous-pattern pre-gate as
    //! `shell`; writes are raw PTY input because splitting a command at an
    //! arbitrary byte boundary would make the gate both bypassable and noisy.

    use crate::tools::{parse_args, tool_err, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
    use std::collections::{HashMap, VecDeque};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const MAX_BUFFER_BYTES: usize = 256 * 1024;

    pub struct PtyManager {
        root: Mutex<PathBuf>,
        sessions: Mutex<HashMap<String, PtySession>>,
        next_id: AtomicU64,
    }

    impl PtyManager {
        pub fn new(root: PathBuf) -> Self {
            let root = root.canonicalize().unwrap_or(root);
            Self {
                root: Mutex::new(root),
                sessions: Mutex::new(HashMap::new()),
                next_id: AtomicU64::new(1),
            }
        }

        /// Live worktree moves update the manager's root. Existing terminals
        /// keep the cwd they were opened with; new terminals start in the new
        /// root.
        pub fn set_root(&self, root: PathBuf) {
            let root = root.canonicalize().unwrap_or(root);
            if let Ok(mut guard) = self.root.lock() {
                *guard = root;
            }
        }

        pub fn root(&self) -> PathBuf {
            self.root
                .lock()
                .map(|g| g.clone())
                .unwrap_or_else(|_| PathBuf::from("."))
        }

        fn open(
            &self,
            command: Option<&str>,
            cwd: Option<&str>,
            rows: u16,
            cols: u16,
        ) -> Result<String, PantheonError> {
            if let Some(command) = command {
                pantheon_exec::danger::gate(command)?;
            }
            let workspace = self.root();
            let cwd = match cwd {
                Some(cwd) => pantheon_exec::confine::confine(Path::new(cwd), &workspace)?,
                None => workspace.clone(),
            };
            let pty_system = native_pty_system();
            let pair = pty_system
                .openpty(PtySize {
                    rows: rows.clamp(1, 500),
                    cols: cols.clamp(1, 500),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| perr("PTY_OPEN", format!("open pty: {e}")))?;
            let shell = default_shell();
            let mut cmd = CommandBuilder::new(&shell);
            if let Some(command) = command {
                if cfg!(windows) {
                    cmd.arg("/C");
                    cmd.arg(command);
                } else {
                    cmd.arg("-lc");
                    cmd.arg(command);
                }
            }
            cmd.cwd(&cwd);
            cmd.env("TERM", "xterm-256color");
            let child = pair
                .slave
                .spawn_command(cmd)
                .map_err(|e| perr("PTY_SPAWN", format!("spawn {shell}: {e}")))?;
            drop(pair.slave);
            let reader = pair
                .master
                .try_clone_reader()
                .map_err(|e| perr("PTY_READER", format!("clone pty reader: {e}")))?;
            let writer = pair
                .master
                .take_writer()
                .map_err(|e| perr("PTY_WRITER", format!("take pty writer: {e}")))?;
            let output = Arc::new(Mutex::new(VecDeque::new()));
            let closed = Arc::new(AtomicBool::new(false));
            spawn_reader(reader, Arc::clone(&output), Arc::clone(&closed));
            let id = format!("pty-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
            let session = PtySession {
                master: pair.master,
                writer,
                child,
                output,
                closed,
                command: command.unwrap_or(&shell).to_string(),
            };
            self.sessions
                .lock()
                .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?
                .insert(id.clone(), session);
            Ok(id)
        }

        fn with_session<T>(
            &self,
            id: &str,
            f: impl FnOnce(&mut PtySession) -> Result<T, PantheonError>,
        ) -> Result<T, PantheonError> {
            let mut guard = self
                .sessions
                .lock()
                .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?;
            let session = guard
                .get_mut(id)
                .ok_or_else(|| perr("PTY_NOT_FOUND", format!("no terminal session '{id}'")))?;
            f(session)
        }

        fn write(&self, id: &str, data: &str) -> Result<String, PantheonError> {
            self.with_session(id, |session| {
                session
                    .writer
                    .write_all(data.as_bytes())
                    .and_then(|_| session.writer.flush())
                    .map_err(|e| perr("PTY_WRITE", format!("write terminal '{id}': {e}")))?;
                Ok(format!("sent {} byte(s) to {id}", data.len()))
            })
        }

        fn read(
            &self,
            id: &str,
            timeout: Duration,
            max_bytes: usize,
        ) -> Result<String, PantheonError> {
            let deadline = Instant::now() + timeout;
            loop {
                let drained = self.with_session(id, |session| {
                    let mut out = String::new();
                    if let Ok(mut buf) = session.output.lock() {
                        while out.len() < max_bytes {
                            let Some(byte) = buf.pop_front() else {
                                break;
                            };
                            out.push(byte as char);
                        }
                    }
                    if !out.is_empty() {
                        return Ok(Some(out));
                    }
                    if session.closed.load(Ordering::Relaxed) {
                        return Ok(Some(String::new()));
                    }
                    Ok(None)
                })?;
                if let Some(out) = drained {
                    return Ok(if out.is_empty() {
                        "(no output)".to_string()
                    } else {
                        out
                    });
                }
                if Instant::now() >= deadline {
                    return Ok(String::new());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<String, PantheonError> {
            self.with_session(id, |session| {
                session
                    .master
                    .resize(PtySize {
                        rows: rows.clamp(1, 500),
                        cols: cols.clamp(1, 500),
                        pixel_width: 0,
                        pixel_height: 0,
                    })
                    .map_err(|e| perr("PTY_RESIZE", format!("resize terminal '{id}': {e}")))?;
                Ok(format!("resized {id} to {cols}x{rows}"))
            })
        }

        fn close(&self, id: &str) -> Result<String, PantheonError> {
            let mut guard = self
                .sessions
                .lock()
                .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?;
            let mut session = guard
                .remove(id)
                .ok_or_else(|| perr("PTY_NOT_FOUND", format!("no terminal session '{id}'")))?;
            session.kill();
            Ok(format!("closed {id}"))
        }

        fn list(&self) -> Result<Vec<serde_json::Value>, PantheonError> {
            let guard = self
                .sessions
                .lock()
                .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?;
            Ok(guard
                .iter()
                .map(|(id, session)| {
                    serde_json::json!({
                        "id": id,
                        "command": session.command,
                        "alive": !session.closed.load(Ordering::Relaxed),
                        "buffered_bytes": session.output.lock().map(|b| b.len()).unwrap_or(0),
                    })
                })
                .collect())
        }
    }

    impl Drop for PtyManager {
        fn drop(&mut self) {
            if let Ok(mut sessions) = self.sessions.lock() {
                for (_, mut session) in sessions.drain() {
                    session.kill();
                }
            }
        }
    }

    struct PtySession {
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        child: Box<dyn Child + Send + Sync>,
        output: Arc<Mutex<VecDeque<u8>>>,
        closed: Arc<AtomicBool>,
        command: String,
    }

    impl PtySession {
        fn kill(&mut self) {
            self.closed.store(true, Ordering::Relaxed);
            let _ = self.child.kill();
            let _ = self.child.wait();
            // Dropping the master closes the PTY; the reader thread sees EOF.
            let _ = self.writer.flush();
        }
    }

    impl Drop for PtySession {
        fn drop(&mut self) {
            self.kill();
        }
    }

    fn spawn_reader(
        mut reader: Box<dyn Read + Send>,
        output: Arc<Mutex<VecDeque<u8>>>,
        closed: Arc<AtomicBool>,
    ) {
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut buf) = output.lock() {
                            buf.extend(&chunk[..n]);
                            while buf.len() > MAX_BUFFER_BYTES {
                                buf.pop_front();
                            }
                        }
                    }
                }
            }
            closed.store(true, Ordering::Relaxed);
        });
    }

    fn default_shell() -> String {
        std::env::var("SHELL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                if cfg!(windows) {
                    std::env::var("COMSPEC").ok()
                } else {
                    None
                }
            })
            .unwrap_or_else(|| {
                if cfg!(windows) {
                    "cmd.exe".to_string()
                } else {
                    "/bin/sh".to_string()
                }
            })
    }

    fn perr(code: &str, cause: String) -> PantheonError {
        tool_err(
            code,
            Layer::Execution,
            false,
            cause,
            "check terminal arguments and retry",
        )
    }

    fn jstr(v: &serde_json::Value, key: &str) -> Option<String> {
        v.get(key).and_then(|x| x.as_str()).map(str::to_string)
    }

    fn ju16(v: &serde_json::Value, key: &str, default: u16) -> u16 {
        v.get(key)
            .and_then(|x| x.as_u64())
            .map(|n| n.min(u16::MAX as u64) as u16)
            .unwrap_or(default)
    }

    /// Register `terminal_open/read/write/resize/close/list`. The caller owns
    /// the manager for the session's lifetime; registering without one is a
    /// programming error, so this function takes the manager explicitly.
    pub fn register_pty_tools(reg: &mut ToolRegistry, manager: Arc<PtyManager>) {
        let m = manager.clone();
        reg.register_with(
        ToolSchema {
            name: "terminal_open".into(),
            description: "Open a persistent PTY. Returns a terminal id. With no `command`, starts an interactive shell; with `command`, runs it through the shell and keeps the PTY for follow-up input. Use terminal_write/read to drive it. Requires ShellExecute.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Optional command to run; omit for an interactive shell."},
                    "cwd": {"type": "string", "description": "Optional working directory inside the workspace."},
                    "rows": {"type": "integer", "minimum": 1, "maximum": 500},
                    "cols": {"type": "integer", "minimum": 1, "maximum": 500}
                }
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = m.open(
                jstr(&v, "command").as_deref(),
                jstr(&v, "cwd").as_deref(),
                ju16(&v, "rows", 24),
                ju16(&v, "cols", 80),
            )?;
            Ok(format!(
                "{id}\nUse terminal_read {id} to collect output and terminal_write {id} to send input."
            ))
        },
        // Same GitPush classification as the `shell` tool: without it,
        // `terminal_open` with a `git push` command ran with no push
        // approval at all (only the danger pre-gate applied). Input
        // typed later via `terminal_write` is inherently
        // unclassifiable and stays the documented residual.
        Some(Box::new(|args: &str| {
            let Ok(v) = parse_args(args) else {
                return Vec::new();
            };
            match jstr(&v, "command") {
                Some(cmd) if pantheon_exec::danger::is_git_push(&cmd) => {
                    vec![Capability::GitPush]
                }
                _ => Vec::new(),
            }
        })),
    );

        let m = manager.clone();
        reg.register(
        ToolSchema {
            name: "terminal_write".into(),
            description: "Write raw input to a persistent PTY. Include a trailing newline to submit a shell command.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "data": {"type": "string"}
                },
                "required": ["session_id", "data"]
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = jstr(&v, "session_id")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
            let data = jstr(&v, "data")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'data'".into()))?;
            m.write(&id, &data)
        },
    );

        let m = manager.clone();
        reg.register(
        ToolSchema {
            name: "terminal_read".into(),
            description: "Read buffered output from a persistent PTY. `timeout_ms` waits for output (default 0 = immediate). Output is drained; it is not repeated on later reads.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": 30000},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 262144}
                },
                "required": ["session_id"]
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = jstr(&v, "session_id")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
            let timeout = Duration::from_millis(
                v.get("timeout_ms")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    .min(30_000),
            );
            let max = v
                .get("max_bytes")
                .and_then(|x| x.as_u64())
                .unwrap_or(64 * 1024)
                .clamp(1, MAX_BUFFER_BYTES as u64) as usize;
            m.read(&id, timeout, max)
        },
    );

        let m = manager.clone();
        reg.register(
            ToolSchema {
                name: "terminal_resize".into(),
                description: "Resize a persistent PTY.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "session_id": {"type": "string"},
                        "rows": {"type": "integer", "minimum": 1, "maximum": 500},
                        "cols": {"type": "integer", "minimum": 1, "maximum": 500}
                    },
                    "required": ["session_id", "rows", "cols"]
                }),
            },
            Capability::ShellExecute,
            move |args| {
                let v = parse_args(args)?;
                let id = jstr(&v, "session_id")
                    .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
                m.resize(&id, ju16(&v, "rows", 24), ju16(&v, "cols", 80))
            },
        );

        let m = manager.clone();
        reg.register(
            ToolSchema {
                name: "terminal_close".into(),
                description: "Kill and remove a persistent PTY.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"session_id": {"type": "string"}},
                    "required": ["session_id"]
                }),
            },
            Capability::ShellExecute,
            move |args| {
                let v = parse_args(args)?;
                let id = jstr(&v, "session_id")
                    .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
                m.close(&id)
            },
        );

        let m = manager;
        reg.register(
            ToolSchema {
                name: "terminal_list".into(),
                description: "List live persistent PTYs and their buffered output sizes.".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            Capability::ShellExecute,
            move |_args| {
                serde_json::to_string_pretty(&m.list()?)
                    .map_err(|e| perr("PTY_JSON", e.to_string()))
            },
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_root() -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-pty-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn opens_writes_reads_and_closes_a_persistent_shell() {
            if cfg!(windows) {
                return;
            }
            let root = temp_root();
            let manager = PtyManager::new(root.clone());
            let id = manager.open(None, None, 24, 80).unwrap();
            assert!(manager.list().unwrap().iter().any(|v| v["id"] == id));
            manager.write(&id, "printf pty-ok\\n").unwrap();
            let mut text = String::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                text.push_str(
                    &manager
                        .read(&id, Duration::from_millis(100), 64 * 1024)
                        .unwrap(),
                );
                if text.contains("pty-ok") {
                    break;
                }
            }
            assert!(text.contains("pty-ok"), "{text:?}");
            manager.resize(&id, 30, 100).unwrap();
            manager.close(&id).unwrap();
            assert!(manager.list().unwrap().is_empty());
            let _ = std::fs::remove_dir_all(root);
        }

        #[test]
        fn unknown_session_is_a_structured_error() {
            let manager = PtyManager::new(temp_root());
            let err = manager.read("missing", Duration::ZERO, 1024).unwrap_err();
            assert!(err.to_string().contains("PTY_NOT_FOUND"));
        }

        #[test]
        fn cwd_is_confined_to_the_workspace() {
            let root = temp_root();
            let manager = PtyManager::new(root.clone());
            let err = manager.open(None, Some("/etc"), 24, 80).unwrap_err();
            assert!(err.to_string().contains("CONFINE"));
            let _ = std::fs::remove_dir_all(root);
        }
    }
}
pub mod safewrite_tools {
    //! Safe-write toolset: preview/stage/apply/checkpoint/rollback tools.
    //!
    //! Moved out of `pantheon-exec::safewrite` in the tool-layer split
    //! (capability ≠ tool): the write engine (checkpoints, hash-chained
    //! journal, atomic apply) stays in `pantheon-exec`; registering the tools
    //! on a registry is a tool-layer concern.

    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::confine::confine;
    use pantheon_exec::safewrite::{json_out, preview_edit, serr, FileEdit, SafeWriter};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    fn jstr(v: &serde_json::Value, key: &str) -> Option<String> {
        v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
    }

    /// Serialize a safe-write receipt and, when configured, attach a compact
    /// formatting report. Formatting happens after the atomic apply; a
    /// formatter failure is visible in the result but never turns a successful
    /// write into a failed tool call.
    fn json_out_with_format<T: serde::Serialize>(
        value: &T,
        formatter: Option<&crate::tools_group::format::Formatter>,
        paths: &[PathBuf],
    ) -> Result<String, PantheonError> {
        let mut v =
            serde_json::to_value(value).map_err(|e| serr("SAFE_WRITE_JSON", e.to_string()))?;
        if let Some(formatter) = formatter {
            let report = formatter.format_paths(paths.iter().map(PathBuf::as_path));
            if !report.is_empty() {
                v["format"] = serde_json::json!({
                    "summary": report.summary(),
                    "formatted": report.outcomes.iter().filter(|(_, o)| o.success).count(),
                    "failed": report.outcomes.iter().filter(|(_, o)| !o.success).count(),
                });
            }
        }
        serde_json::to_string_pretty(&v).map_err(|e| serr("SAFE_WRITE_JSON", e.to_string()))
    }

    fn parse_edit_list(v: &serde_json::Value) -> Result<Vec<FileEdit>, PantheonError> {
        let arr = v.get("edits").and_then(|x| x.as_array()).ok_or_else(|| {
            crate::tools::tool_err(
                "TOOL_BAD_ARGS",
                Layer::Execution,
                false,
                "missing array 'edits'".into(),
                "check tool arguments",
            )
        })?;
        let mut out = vec![];
        for item in arr {
            let path = item.get("path").and_then(|x| x.as_str()).ok_or_else(|| {
                crate::tools::tool_err(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "each edit needs string 'path'".into(),
                    "check tool arguments",
                )
            })?;
            let content = item
                .get("content")
                .and_then(|x| x.as_str())
                .ok_or_else(|| {
                    crate::tools::tool_err(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        "each edit needs string 'content'".into(),
                        "check tool arguments",
                    )
                })?;
            out.push(FileEdit {
                path: PathBuf::from(path),
                new_content: content.as_bytes().to_vec(),
                expected_hash: item
                    .get("expected_hash")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string()),
            });
        }
        Ok(out)
    }
    fn state_dir_from(args: &serde_json::Value) -> Result<PathBuf, PantheonError> {
        if let Some(d) = jstr(args, "state_dir") {
            return Ok(PathBuf::from(d));
        }
        if let Ok(d) = std::env::var("PANTHEON_DATA_DIR") {
            return Ok(PathBuf::from(d).join("safewrite"));
        }
        Err(crate::tools::tool_err(
            "TOOL_BAD_ARGS",
            Layer::Execution,
            false,
            "missing 'state_dir' (or set PANTHEON_DATA_DIR)".into(),
            "check tool arguments",
        ))
    }
    /// Options for `register_safewrite_with`.
    pub struct SafewriteOptions {
        pub state_dir: PathBuf,
        /// Workspace root for path confinement. The tool layer has no workspace
        /// concept of its own, so when this is `None` the process working
        /// directory is captured at registration time. Every target path is
        /// confined (deny globs, then containment) before any read or write.
        pub workspace_root: Option<PathBuf>,
        /// Opt-in post-apply formatter. `None` = byte-for-byte untouched.
        pub formatter: Option<Arc<crate::tools_group::format::Formatter>>,
    }

    /// Register the safe-write toolset on a registry. All mutating tools
    /// snapshot a checkpoint first; preview/checkpoints/rollback never mutate targets.
    /// Tools: preview_file, stage_files, apply_files, apply_staged,
    /// checkpoint_files, list_checkpoints, rollback_checkpoint, rollback_seq.
    pub fn register_safewrite(reg: &mut ToolRegistry, default_state_dir: PathBuf) {
        register_safewrite_with(
            reg,
            SafewriteOptions {
                state_dir: default_state_dir,
                workspace_root: None,
                formatter: None,
            },
        );
    }

    /// Register with options. `workspace_root` is captured at registration time
    /// (process cwd when `None`) and passed to every `SafeWriter` the tools
    /// build, so staged/applied/checkpointed/rolled-back paths are confined.
    pub fn register_safewrite_with(reg: &mut ToolRegistry, opts: SafewriteOptions) {
        let dir: Arc<PathBuf> = Arc::new(opts.state_dir);
        let formatter = opts.formatter;
        let root: Arc<PathBuf> = Arc::new(
            opts.workspace_root
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
        );
        // Build a SafeWriter whose target paths are confined to the workspace.
        let writer = |sd: PathBuf, root: &Arc<PathBuf>| -> Result<SafeWriter, PantheonError> {
            SafeWriter::new(sd).map(|w| w.with_workspace_root((**root).clone()))
        };
        let mk = |name: &str, desc: &str, props: serde_json::Value, required: Vec<&str>| {
            ToolSchema {
                name: name.into(),
                description: desc.into(),
                parameters: serde_json::json!({"type":"object","properties":props,"required":required}),
            }
        };
        let d0 = dir.clone();
        let r0 = root.clone();
        reg.register(
        mk(
            "preview_file",
            "Preview a file write: hashes, line counts, 40-line excerpt. Read-only. Confined to the workspace.",
            serde_json::json!({"path":{"type":"string"},"content":{"type":"string"}}),
            vec!["path", "content"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let path = jstr(&v, "path")
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'path'".into(), "check tool arguments"))?;
            let content = jstr(&v, "content")
                .ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'content'".into(), "check tool arguments"))?;
            let _ = &d0;
            let cpath = confine(Path::new(&path), &r0)?;
            let pv = preview_edit(&cpath, content.as_bytes())?;
            serde_json::to_string_pretty(&pv).map_err(|e| serr("SAFE_WRITE_JSON", e.to_string()))
        },
    );
        let d1 = dir.clone();
        let r1 = root.clone();
        reg.register(
            mk(
                "stage_files",
                "Stage file edits without touching targets. Returns stage id + baselines.",
                serde_json::json!({"edits":{"type":"array"},"state_dir":{"type":"string"}}),
                vec!["edits"],
            ),
            Capability::FilesystemWrite,
            move |args| {
                let v = parse_args(args)?;
                let sd = state_dir_from(&v).unwrap_or_else(|_| (*d1).clone());
                let w = writer(sd, &r1)?;
                let batch = w.stage_edits(parse_edit_list(&v)?)?;
                serde_json::to_string_pretty(&batch)
                    .map_err(|e| serr("SAFE_WRITE_JSON", e.to_string()))
            },
        );
        let d2 = dir.clone();
        let r2 = root.clone();
        let f2 = formatter.clone();
        reg.register(mk("apply_files", "Validate + checkpoint + atomically apply edits. Fails on stale expected_hash.",
        serde_json::json!({"edits":{"type":"array"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["edits"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d2).clone());
            let w = writer(sd, &r2)?;
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let r = w.apply_edits(parse_edit_list(&v)?, seq)?;
            let paths: Vec<PathBuf> = r.files.iter().map(|file| file.path.clone()).collect();
            json_out_with_format(&r, f2.as_deref(), &paths)
        });
        let d3 = dir.clone();
        let r3 = root.clone();
        let f3 = formatter;
        reg.register(mk("apply_staged", "Atomically apply a staged batch by id.",
        serde_json::json!({"stage_id":{"type":"string"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["stage_id"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d3).clone());
            let w = writer(sd, &r3)?;
            let sid = jstr(&v, "stage_id").ok_or_else(|| crate::tools::tool_err("TOOL_BAD_ARGS", Layer::Execution, false, "missing 'stage_id'".into(), "check tool arguments"))?;
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let r = w.apply_staged(&sid, seq)?;
            let paths: Vec<PathBuf> = r.files.iter().map(|file| file.path.clone()).collect();
            json_out_with_format(&r, f3.as_deref(), &paths)
        });
        let d4 = dir.clone();
        let r4 = root.clone();
        reg.register(mk("checkpoint_files", "Snapshot pre-images for paths, anchored to a ledger seq.",
        serde_json::json!({"paths":{"type":"array"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["paths"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d4).clone());
            let w = writer(sd, &r4)?;
            let paths: Vec<PathBuf> = v.get("paths").and_then(|x| x.as_array()).map(|a| a.iter()
                .filter_map(|x| x.as_str()).map(PathBuf::from).collect()).unwrap_or_default();
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let cp = w.checkpoint(&paths, seq)?;
            json_out(&cp)
        });
        let d5 = dir.clone();
        let r5 = root.clone();
        reg.register(
            mk(
                "list_checkpoints",
                "List checkpoints with ledger anchors.",
                serde_json::json!({"state_dir":{"type":"string"}}),
                vec![],
            ),
            Capability::FilesystemRead,
            move |args| {
                let v = parse_args(args)?;
                let sd = state_dir_from(&v).unwrap_or_else(|_| (*d5).clone());
                let w = writer(sd, &r5)?;
                let list = w.list_checkpoints()?;
                json_out(&list)
            },
        );
        let d6 = dir.clone();
        let r6 = root.clone();
        reg.register(
        mk(
            "rollback_checkpoint",
            "Restore a checkpoint by id (atomic per file).",
            serde_json::json!({"checkpoint_id":{"type":"string"},"state_dir":{"type":"string"}}),
            vec!["checkpoint_id"],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d6).clone());
            let w = writer(sd, &r6)?;
            let id = jstr(&v, "checkpoint_id").ok_or_else(|| {
                crate::tools::tool_err(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "missing 'checkpoint_id'".into(),
                    "check tool arguments",
                )
            })?;
            let restored = w.restore_checkpoint(&id)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({"checkpoint": id, "restored": restored}),
            )
            .unwrap())
        },
    );
        let d7 = dir;
        let r7 = root;
        reg.register(
            mk(
                "rollback_seq",
                "Restore the latest checkpoint at or before a ledger seq.",
                serde_json::json!({"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}),
                vec!["ledger_seq"],
            ),
            Capability::FilesystemWrite,
            move |args| {
                let v = parse_args(args)?;
                let sd = state_dir_from(&v).unwrap_or_else(|_| (*d7).clone());
                let w = writer(sd, &r7)?;
                let seq = v
                    .get("ledger_seq")
                    .and_then(|x| x.as_i64())
                    .ok_or_else(|| {
                        crate::tools::tool_err(
                            "TOOL_BAD_ARGS",
                            Layer::Execution,
                            false,
                            "missing integer 'ledger_seq'".into(),
                            "check tool arguments",
                        )
                    })?;
                let (id, restored) = w.rollback_to_seq(seq)?;
                Ok(serde_json::to_string_pretty(
                    &serde_json::json!({"checkpoint": id, "restored": restored}),
                )
                .unwrap())
            },
        );
    }
}
pub mod session_search_tools {
    //! `session_search` tool: the agent searches its own prior conversation
    //! ledger. FTS5 is the primary retrieval layer (exact identifiers, tool
    //! names, quoted phrases); recency breaks ties. Chunked, not whole
    //! sessions - a hit points at the exact source event (`run_id`, `seq`).
    //!
    //! Gated on `FilesystemRead`: the index is a local SQLite file, same
    //! trust level as reading the ledger directly.

    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::Layer;
    use pantheon_api::message::ToolSchema;
    use pantheon_providers::embeddings::EmbedderClient;
    use std::sync::Arc;

    /// Options for `register_session_search`. The store is shared with the
    /// indexing path so the tool reads exactly what the session wrote.
    #[derive(Clone)]
    pub struct SessionSearchOptions {
        pub store: Arc<pantheon_storage::search::SessionSearch>,
        /// The same client the indexer used. Without it the query cannot be
        /// embedded, semantic recall is unreachable, and every embedding
        /// already on disk would be silently ignored.
        pub embedder: Option<Arc<pantheon_providers::embeddings::EmbedClient>>,
    }

    /// Register the `session_search` tool on a registry.
    pub fn register_session_search(reg: &mut ToolRegistry, opts: SessionSearchOptions) {
        let store = opts.store;
        let embedder = opts.embedder;
        reg.register(
        ToolSchema {
            name: "session_search".into(),
            description: "Search through previous and current conversation sessions. Finds exact identifiers, tool names, file paths, model ids, and quoted phrases inside past sessions. Returns matching chunks with their run id, sequence, and age. Use this when the user asks about earlier work ('that session where I was debugging X') or to recall a specific detail from a past conversation.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query. Plain terms, quoted phrases, or identifiers. Examples: Figma MCP debugging, 9f3a2, \"Read TUI session creation block\"."},
                    "limit": {"type": "integer", "description": "Max results (default 8)."}
                },
                "required": ["query"]
            }),
        },
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let query = crate::tools::arg_str(&v, "query")?;
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(8) as usize;
            // Embed the query with the indexer's own client so the vector
            // term is comparable. A failed embed is not an error: lexical
            // plus recency is a complete answer, just a narrower one.
            let qvec: Option<Vec<f32>> = embedder
                .as_ref()
                .and_then(|c| c.embed(std::slice::from_ref(&query)).ok())
                .and_then(|mut v| v.drain(..).next())
                .map(|e| e.vec);
            let hits = match qvec.as_deref() {
                Some(q) => store
                    .search_hybrid(&query, Some(q), limit)
                    .map_err(|e| crate::tools::tool_err("TOOL_SEARCH", Layer::Execution, false, e.cause.clone(), "check tool arguments"))?,
                None => store
                    .search(&query, limit)
                    .map_err(|e| crate::tools::tool_err("TOOL_SEARCH", Layer::Execution, false, e.cause.clone(), "check tool arguments"))?,
            };
            if hits.is_empty() {
                return Ok(format!("no sessions matched '{query}'"));
            }
            let mut out = String::new();
            for h in &hits {
                let c = &h.chunk;
                let snippet: String = c.text.chars().take(240).collect();
                out.push_str(&format!(
                    "[{}] run {} seq {} · {} · {}\n{}\n\n",
                    h.score,
                    c.run_id,
                    c.seq,
                    c.kind,
                    fmt_age(c.ts_ms),
                    snippet
                ));
            }
            Ok(out.trim_end().to_string())
        },
    );
    }

    /// Human-readable age for a hit (mirrors the CLI\'s fmt_created).
    fn fmt_age(ms: i64) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let s = (now - ms).max(0) / 1000;
        if s < 60 {
            format!("{s}s ago")
        } else if s < 3600 {
            format!("{}m ago", s / 60)
        } else if s < 86400 {
            format!("{}h ago", s / 3600)
        } else {
            format!("{}d ago", s / 86400)
        }
    }
}
pub mod skill_tools {
    //! Skill tools: `skills_list` + `skill_read` on the shared registry.
    //!
    //! Moved out of `pantheon-exec::skills` in the tool-layer split
    //! (capability ≠ tool): skill discovery/parse/import stays in
    //! `pantheon-exec`; the callable surface lives here.

    use pantheon_api::error::Layer;
    use pantheon_exec::skills::{skill_body, Skill};
    /// Register `skills_list` and `skill_read` on a registry.
    ///
    /// `skills_list` is read-only (FilesystemRead): names + descriptions.
    /// `skill_read` loads one skill's body, gated on FilesystemRead too
    /// skills are data, not executable capability. A skill that wanted to
    /// grant powers would be a plugin, not a skill.
    pub fn register_skill_tools(reg: &mut crate::tools::ToolRegistry, skills: Vec<Skill>) {
        if skills.is_empty() {
            return;
        }
        let list = skills.clone();
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "skills_list".into(),
                description: "List available skills (name, description, origin).".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            },
            pantheon_api::capability::Capability::FilesystemRead,
            move |_args| {
                let mut out = String::new();
                for s in &list {
                    out.push_str(&format!(
                        "- {}: {} ({})\n",
                        s.meta.name, s.meta.description, s.meta.origin
                    ));
                }
                Ok(out)
            },
        );

        let read = skills;
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "skill_read".into(),
                description: "Read one skill's full markdown body by name.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }),
            },
            pantheon_api::capability::Capability::FilesystemRead,
            move |args| {
                let v: serde_json::Value =
                    serde_json::from_str(if args.trim().is_empty() { "{}" } else { args })
                        .map_err(|e| {
                            crate::tools::tool_err(
                                "TOOL_BAD_ARGS",
                                Layer::Execution,
                                false,
                                format!("invalid JSON args: {e}"),
                                "check tool name and arguments",
                            )
                        })?;
                let name = v.get("name").and_then(|x| x.as_str()).ok_or_else(|| {
                    crate::tools::tool_err(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        "missing 'name'".to_string(),
                        "check tool name and arguments",
                    )
                })?;
                let skill = read.iter().find(|s| s.meta.name == name).ok_or_else(|| {
                    crate::tools::tool_err(
                        "SKILL_UNKNOWN",
                        Layer::Execution,
                        false,
                        format!("no skill named '{name}'"),
                        "call skills_list first",
                    )
                })?;
                skill_body(skill)
            },
        );
    }
}
pub mod todo_tools {
    //! The agent-facing `todo` tool: replace the whole session todo list in
    //! one call.
    //!
    //! The tool owns the shared in-memory list (the session, `/todos`, and
    //! the TUI card read the same state). Durability crosses into the run's
    //! ledger through [`TodoToolSink`], which the runtime implements - the
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
}
pub mod ui_tool {
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
}
pub mod vault_tools {
    //! Obsidian Vault Archive & Library tools.
    //!
    //! Exposes tools for the agent to archive long documents, notes, reports,
    //! research, and project outputs into an Obsidian vault (`~/vault`),
    //! keeping working memory lean while preserving a permanent, human-readable,
    //! linked markdown library.

    use crate::tools::ToolRegistry;
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::compact_output;
    use serde::Deserialize;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn parse_vault_args<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, PantheonError> {
        serde_json::from_str(raw).map_err(|e| {
            crate::tools::tool_err(
                "TOOL_BAD_ARGS",
                Layer::Execution,
                false,
                format!("failed to parse arguments: {e}"),
                "check vault path, permissions, or arguments",
            )
        })
    }

    /// Options to configure vault tooling.
    #[derive(Clone, Debug)]
    pub struct VaultToolOptions {
        pub vault_dir: PathBuf,
    }

    impl Default for VaultToolOptions {
        fn default() -> Self {
            let vault_dir = std::env::var_os("PANTHEON_VAULT_DIR")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    if let Some(home) =
                        std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"))
                    {
                        return PathBuf::from(home).join("vault");
                    }
                    if let Some(xdg) =
                        std::env::var_os("XDG_DATA_HOME").filter(|dir| !dir.is_empty())
                    {
                        return PathBuf::from(xdg).join("pantheon").join("vault");
                    }
                    let user = std::env::var_os("USER")
                        .or_else(|| std::env::var_os("USERNAME"))
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| "default".into());
                    std::env::temp_dir()
                        .join("pantheon")
                        .join(user)
                        .join("vault")
                });
            Self { vault_dir }
        }
    }

    /// Resolve a vault-relative path and prove it stays inside the vault.
    ///
    /// A substring check for `..` is not confinement: a symlink planted inside
    /// the vault (`vault/notes -> /etc`) passes it and then reads or writes
    /// wherever it points. The check is therefore structural - canonicalize the
    /// deepest existing ancestor, then confirm the result is still under the
    /// canonical vault root.
    ///
    /// A path that does not exist yet is fine (writes create it), so the walk
    /// stops at the first component that is missing and the remaining suffix is
    /// appended lexically. Components are rejected on the way, so no `..` can be
    /// smuggled in through a name that only looks harmless.
    fn resolve_safe_vault_path(vault_dir: &Path, rel_path: &str) -> Result<PathBuf, PantheonError> {
        let rel = rel_path.trim().trim_start_matches('/');
        if rel.is_empty() {
            return Err(crate::tools::tool_err(
                "VAULT_BAD_PATH",
                Layer::Execution,
                false,
                "empty vault path".into(),
                "check vault path, permissions, or arguments",
            ));
        }
        for comp in rel.split('/') {
            if comp.is_empty() || comp == "." {
                continue;
            }
            if comp == ".." {
                return Err(crate::tools::tool_err(
                    "VAULT_PATH_TRAVERSAL",
                    Layer::Execution,
                    false,
                    "path traversal (..) is not permitted".into(),
                    "check vault path, permissions, or arguments",
                ));
            }
        }

        // Canonical root, so the comparison below is against real paths and not
        // against a vault dir that is itself reached through a symlink.
        let root = vault_dir.canonicalize().map_err(|e| {
            crate::tools::tool_err(
                "VAULT_NO_ROOT",
                Layer::Execution,
                false,
                format!("vault dir unavailable: {e}"),
                "check vault path, permissions, or arguments",
            )
        })?;
        // Walk down to the deepest component that exists on disk.
        let mut probe = root.clone();
        let mut tail: Option<std::ffi::OsString> = None;
        for comp in rel.split('/') {
            if comp.is_empty() || comp == "." {
                continue;
            }
            match probe.join(comp).canonicalize() {
                Ok(real) => {
                    if !real.starts_with(&root) {
                        return Err(crate::tools::tool_err(
                            "VAULT_ESCAPE",
                            Layer::Execution,
                            false,
                            format!("path resolves outside the vault: {rel_path}"),
                            "check vault path, permissions, or arguments",
                        ));
                    }
                    probe = real;
                }
                Err(_) => {
                    // First missing component: everything after it is new, and
                    // cannot contain a symlink.
                    let mut rest = std::ffi::OsString::from(comp);
                    for later in rel.split('/').skip_while(|c| *c != comp).skip(1) {
                        rest.push("/");
                        rest.push(later);
                    }
                    tail = Some(rest);
                    break;
                }
            }
        }

        let mut final_path = match tail {
            Some(rest) => probe.join(rest),
            None => probe,
        };
        if final_path == root {
            return Err(crate::tools::tool_err(
                "VAULT_BAD_PATH",
                Layer::Execution,
                false,
                "path is the vault root, not a file in it".into(),
                "check vault path, permissions, or arguments",
            ));
        }
        // Normalize away any `.` components the loop skipped.
        final_path = normalize_lexically(&final_path);
        Ok(final_path)
    }

    /// Strip `.` components without touching the filesystem. `canonicalize`
    /// already removed them for the existing prefix; this handles the appended
    /// suffix so the returned path is the one a caller will actually open.
    fn normalize_lexically(p: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for comp in p.components() {
            match comp {
                std::path::Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        out
    }

    #[derive(Deserialize)]
    struct VaultArchiveArgs {
        category: String,
        title: String,
        content: String,
        #[serde(default)]
        tags: Vec<String>,
    }

    #[derive(Deserialize)]
    struct VaultReadArgs {
        path: String,
    }

    #[derive(Deserialize)]
    struct VaultSearchArgs {
        query: String,
        #[serde(default)]
        category: Option<String>,
        #[serde(default = "default_search_limit")]
        limit: usize,
    }

    fn default_search_limit() -> usize {
        10
    }

    #[derive(Deserialize)]
    struct VaultListArgs {
        #[serde(default)]
        category: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: usize,
    }

    fn default_list_limit() -> usize {
        50
    }

    /// Register the Obsidian Vault archive & library tools:
    /// - `vault_archive`: Store a long-form document/output under a vault category (notes, projects, reference, etc.)
    /// - `vault_read`: Read a document from the vault
    /// - `vault_search`: Full-text/keyword search across markdown notes in the vault
    /// - `vault_list`: List notes within a category or across the vault
    pub fn register_vault_tools(reg: &mut ToolRegistry, opts: VaultToolOptions) {
        let vault_root = opts.vault_dir.clone();

        // 1. vault_archive
        {
            let root = vault_root.clone();
            reg.register(
            ToolSchema {
                name: "vault_archive".into(),
                description: "Archive long texts, project designs, research reports, or session notes into the Obsidian vault library for long-term reference.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "Category directory in the vault, e.g. 'notes', 'projects', 'reference', 'people'."
                        },
                        "title": {
                            "type": "string",
                            "description": "Title or slug for the file, e.g. 'agent-architecture-deepdive'."
                        },
                        "content": {
                            "type": "string",
                            "description": "The markdown body to archive."
                        },
                        "tags": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional list of tags, e.g. ['pantheon', 'architecture']."
                        }
                    },
                    "required": ["category", "title", "content"]
                }),
            },
            Capability::FilesystemWrite,
            move |raw_args| {
                let args: VaultArchiveArgs = parse_vault_args(raw_args)?;
                let category_clean = args.category.trim().trim_matches('/');
                let mut filename = args.title.trim().replace('/', "-");
                if !filename.ends_with(".md") {
                    filename.push_str(".md");
                }

                let target_dir = resolve_safe_vault_path(&root, category_clean)?;
                if let Err(e) = fs::create_dir_all(&target_dir) {
                    return Err(crate::tools::tool_err("VAULT_MKDIR_FAILED", Layer::Execution, false, format!("failed to create dir {}: {e}", target_dir.display()), "check vault path, permissions, or arguments"));
                }

                let target_file = target_dir.join(&filename);
                let now_utc = chrono_now();

                let mut body = String::new();
                body.push_str("---\n");
                body.push_str(&format!("title: \"{}\"\n", args.title.trim()));
                body.push_str(&format!("date: {}\n", now_utc));
                body.push_str(&format!("category: {}\n", category_clean));
                if !args.tags.is_empty() {
                    body.push_str("tags:\n");
                    for tag in &args.tags {
                        body.push_str(&format!(" - {}\n", tag.trim().trim_start_matches('#')));
                    }
                }
                body.push_str("---\n\n");
                body.push_str(args.content.trim());
                body.push('\n');

                if let Err(e) = fs::write(&target_file, body.as_bytes()) {
                    return Err(crate::tools::tool_err("VAULT_WRITE_FAILED", Layer::Execution, false, format!("failed to write {}: {e}", target_file.display()), "check vault path, permissions, or arguments"));
                }

                let rel_path = format!("{}/{}", category_clean, filename);
                Ok(format!("Successfully archived to vault: [[{}]] ({} bytes)", rel_path, body.len()))
            },
        );
        }

        // 2. vault_read
        {
            let root = vault_root.clone();
            reg.register(
            ToolSchema {
                name: "vault_read".into(),
                description: "Read the contents of an archived note or document from the Obsidian vault.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path in vault, e.g. 'notes/ideation/2026-09-15-webhookwatch.md' or 'Home.md'."
                        }
                    },
                    "required": ["path"]
                }),
            },
            Capability::FilesystemRead,
            move |raw_args| {
                let args: VaultReadArgs = parse_vault_args(raw_args)?;
                let mut path_str = args.path.trim().to_string();
                if !path_str.ends_with(".md") && !path_str.contains('.') {
                    path_str.push_str(".md");
                }
                let target = resolve_safe_vault_path(&root, &path_str)?;
                if !target.exists() {
                    return Err(crate::tools::tool_err("VAULT_FILE_NOT_FOUND", Layer::Execution, false, format!("file not found in vault: {path_str}"), "check vault path, permissions, or arguments"));
                }

                let content = fs::read_to_string(&target)
                    .map_err(|e| crate::tools::tool_err("VAULT_READ_FAILED", Layer::Execution, false, format!("failed to read {}: {e}", target.display()), "check vault path, permissions, or arguments"))?;
                let compacted = compact_output(&content, &Default::default());
                Ok(compacted.text)
            },
        );
        }

        // 3. vault_search
        {
            let root = vault_root.clone();
            reg.register(
            ToolSchema {
                name: "vault_search".into(),
                description: "Search across the Obsidian vault markdown documents for keywords or phrases.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keyword or phrase to search for."
                        },
                        "category": {
                            "type": "string",
                            "description": "Optional subfolder to restrict search to, e.g. 'projects' or 'notes'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum matching snippets to return (default 10)."
                        }
                    },
                    "required": ["query"]
                }),
            },
            Capability::FilesystemRead,
            move |raw_args| {
                let args: VaultSearchArgs = parse_vault_args(raw_args)?;
                let search_dir = match &args.category {
                    Some(cat) => resolve_safe_vault_path(&root, cat.trim().trim_matches('/'))?,
                    None => root.clone(),
                };

                if !search_dir.exists() {
                    return Ok("Category directory not found in vault.".into());
                }

                let query_lower = args.query.to_lowercase();
                let terms: Vec<&str> = query_lower.split_whitespace().collect();
                if terms.is_empty() {
                    return Ok("Empty query.".into());
                }

                let mut hits = Vec::new();
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(TRAVERSE_BUDGET_MS);
                let complete = search_vault_dir(
                    &root,
                    &search_dir,
                    &terms,
                    args.limit,
                    &mut hits,
                    &deadline,
                )?;

                if hits.is_empty() {
                    return Ok(format!("No matches found in vault for '{}'.", args.query));
                }

                let mut out = format!("Found {} vault matches for '{}':\n\n", hits.len(), args.query);
                for hit in hits {
                    out.push_str(&format!("- **[[{}]]**:\n  > {}\n\n", hit.rel_path, hit.snippet));
                }
                if !complete {
                    out.push_str("\n[note: search hit the time budget on the vault mount; results are partial - narrow with category=]");
                }
                Ok(out)
            },
        );
        }

        // 4. vault_list
        {
            let root = vault_root.clone();
            reg.register(
            ToolSchema {
                name: "vault_list".into(),
                description: "List notes and documents archived in the Obsidian vault library.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "Optional category subfolder, e.g. 'projects', 'notes', 'reference'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum items to list (default 50)."
                        }
                    }
                }),
            },
            Capability::FilesystemRead,
            move |raw_args| {
                let args: VaultListArgs = parse_vault_args(raw_args)?;
                let search_dir = match &args.category {
                    Some(cat) => resolve_safe_vault_path(&root, cat.trim().trim_matches('/'))?,
                    None => root.clone(),
                };

                if !search_dir.exists() {
                    return Ok("Vault directory does not exist.".into());
                }

                let mut files = Vec::new();
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(TRAVERSE_BUDGET_MS);
                let complete =
                    collect_vault_files(&root, &search_dir, args.limit, &mut files, &deadline)?;

                if files.is_empty() {
                    return Ok("No files found in vault.".into());
                }

                let mut out = format!("Vault files ({} total):\n", files.len());
                for f in files {
                    out.push_str(&format!("- [[{}]]\n", f));
                }
                if !complete {
                    out.push_str("\n[note: listing hit the time budget; partial - narrow with category=]");
                }
                Ok(out)
            },
        );
        }
    }

    fn chrono_now() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        format!("{now}")
    }

    struct SearchHit {
        rel_path: String,
        snippet: String,
    }

    /// Wall-clock budget for one traversal. The vault can live on a FUSE
    /// network mount (rclone); an unbounded walk stalls the tool call.
    const TRAVERSE_BUDGET_MS: u64 = 2_500;
    /// Skip markdown files bigger than this: archives are read into model
    /// context only through compaction anyway, and huge reads on a network
    /// mount dominate the budget.
    const MAX_SCAN_BYTES: u64 = 512 * 1024;

    fn should_skip_dir(name: &str) -> bool {
        name.starts_with('.') || matches!(name, "backups" | "target" | "node_modules" | "UoPeople")
    }

    fn search_vault_dir(
        root: &Path,
        dir: &Path,
        terms: &[&str],
        limit: usize,
        hits: &mut Vec<SearchHit>,
        deadline: &std::time::Instant,
    ) -> Result<bool, PantheonError> {
        if hits.len() >= limit || std::time::Instant::now() >= *deadline {
            return Ok(false);
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Ok(true),
        };

        for entry in entries.flatten() {
            if hits.len() >= limit {
                return Ok(true);
            }
            if std::time::Instant::now() >= *deadline {
                return Ok(false);
            }
            let p = entry.path();
            if p.is_dir() {
                let file_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if should_skip_dir(file_name) {
                    continue;
                }
                if !search_vault_dir(root, &p, terms, limit, hits, deadline)? {
                    return Ok(false);
                }
            } else if p.is_file() && p.extension().is_some_and(|ext| ext == "md") {
                let meta_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                if meta_len > MAX_SCAN_BYTES {
                    continue;
                }
                if let Ok(content) = fs::read_to_string(&p) {
                    let content_lower = content.to_lowercase();
                    if terms.iter().all(|t| content_lower.contains(t)) {
                        let first_pos = content_lower.find(terms[0]).unwrap_or(0);
                        let start = first_pos.saturating_sub(60);
                        let end = (first_pos + 100).min(content.len());
                        let snippet = content[start..end].replace('\n', " ").trim().to_string();
                        let rel = p
                            .strip_prefix(root)
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .to_string();
                        hits.push(SearchHit {
                            rel_path: rel,
                            snippet,
                        });
                    }
                }
            }
        }
        Ok(true)
    }

    fn collect_vault_files(
        root: &Path,
        dir: &Path,
        limit: usize,
        files: &mut Vec<String>,
        deadline: &std::time::Instant,
    ) -> Result<bool, PantheonError> {
        if files.len() >= limit || std::time::Instant::now() >= *deadline {
            return Ok(false);
        }
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Ok(true),
        };

        for entry in entries.flatten() {
            if files.len() >= limit {
                return Ok(true);
            }
            if std::time::Instant::now() >= *deadline {
                return Ok(false);
            }
            let p = entry.path();
            if p.is_dir() {
                let file_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if should_skip_dir(file_name) {
                    continue;
                }
                if !collect_vault_files(root, &p, limit, files, deadline)? {
                    return Ok(false);
                }
            } else if p.is_file() && p.extension().is_some_and(|ext| ext == "md") {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .to_string();
                files.push(rel);
            }
        }
        Ok(true)
    }
}
pub mod verdict_tool {
    //! The reviewer-facing `verdict` tool: emit a structured review verdict.
    //!
    //! Registered ONLY on reviewer runs in staged review stages (see
    //! `pantheon_runtime::swarm_exec`): the reviewer calls `verdict` once at
    //! the end of its review, and the orchestrator extracts the verdict from
    //! the call's structured args instead of parsing the reviewer's prose.
    //! The tool itself is stateless - the run's ledger `ToolStarted` event
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
}
pub mod worktree_tools {
    //! Git worktree toolset: first-class isolated working trees.
    //!
    //! Worktrees are the workspace primitive for parallel work: each one has
    //! its own checkout, index, and HEAD while sharing the repository's
    //! object store. The engine lives in `pantheon-exec::worktree`; this
    //! module only exposes it to the agent.
    //!
    //! Tools:
    //!   - worktree.list    list linked worktrees
    //!   - worktree.create  create one (branch `pantheon/<name>`)
    //!   - worktree.remove  remove a linked worktree
    //!   - worktree.path    print an existing worktree's path (or its managed path)

    use crate::tools::{parse_args, ToolRegistry};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::worktree::WorktreeManager;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn err(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Execution,
            false,
            cause,
            "check the repository path and worktree name",
            "",
        )
    }

    fn manager(v: &serde_json::Value, state_dir: &Arc<PathBuf>) -> Result<WorktreeManager, String> {
        let repo = v
            .get("repo_dir")
            .and_then(|x| x.as_str())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        WorktreeManager::new(&repo, state_dir.as_ref()).map_err(|e| e.to_string())
    }

    fn schema(
        name: &str,
        description: &str,
        props: serde_json::Value,
        required: Vec<&str>,
    ) -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: description.into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": props,
                "required": required,
            }),
        }
    }

    /// Register the worktree toolset. `state_dir` is the Pantheon data dir;
    /// managed worktrees land under `<state_dir>/worktrees/<repo>-<hash>/`.
    pub fn register_worktrees(reg: &mut ToolRegistry, state_dir: PathBuf) {
        let sd = Arc::new(state_dir);

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "worktree.list",
            "List the repository's worktrees (main + linked), parsed from `git worktree list --porcelain`.",
            serde_json::json!({
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec![],
        ),
        Capability::GitRead,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let list = m.list().map_err(|e| err("WORKTREE_LIST", e.to_string()))?;
            serde_json::to_string_pretty(&list)
                .map_err(|e| err("WORKTREE_JSON", e.to_string()))
        },
    );

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "worktree.create",
            "Create a linked git worktree on branch `pantheon/<name>` (from HEAD by default) and return its path. Managed path is under the Pantheon data dir unless `path` is given.",
            serde_json::json!({
                "name": {"type":"string","description":"Short worktree name (letters, digits, -, _, .)"},
                "base": {"type":"string","description":"Git revision to branch from (default: HEAD)"},
                "path": {"type":"string","description":"Explicit worktree directory (default: managed path)"},
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec!["name"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("WORKTREE_BAD_ARGS", "missing 'name'".to_string()))?;
            let base = v.get("base").and_then(|x| x.as_str());
            let path = v.get("path").and_then(|x| x.as_str()).map(PathBuf::from);
            let wt = m
                .create(name, base, path.as_deref())
                .map_err(|e| err("WORKTREE_CREATE", e.to_string()))?;
            serde_json::to_string_pretty(&wt)
                .map_err(|e| err("WORKTREE_JSON", e.to_string()))
        },
    );

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "worktree.remove",
            "Remove a Pantheon-managed linked worktree by name or path. Refuses the main tree and unrelated linked worktrees; leaves its branch in place. `force` discards uncommitted changes.",
            serde_json::json!({
                "name": {"type":"string","description":"Managed worktree name or path"},
                "force": {"type":"boolean","description":"Discard modifications and untracked files"},
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec!["name"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("WORKTREE_BAD_ARGS", "missing 'name'".to_string()))?;
            let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
            let wt = m
                .remove(name, force)
                .map_err(|e| err("WORKTREE_REMOVE", e.to_string()))?;
            serde_json::to_string_pretty(&wt)
                .map_err(|e| err("WORKTREE_JSON", e.to_string()))
        },
    );

        let s = Arc::clone(&sd);
        reg.register(
        schema(
            "worktree.path",
            "Return the directory for an existing worktree name, or the managed path it would use without creating it.",
            serde_json::json!({
                "name": {"type":"string"},
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec!["name"],
        ),
        Capability::GitRead,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("WORKTREE_BAD_ARGS", "missing 'name'".to_string()))?;
            let p = match m.find(name) {
                Ok(Some(w)) => w.path,
                Ok(None) => m
                    .path_for(name)
                    .map_err(|e| err("WORKTREE_BAD_NAME", e.to_string()))?,
                Err(e) => return Err(err("WORKTREE_PATH", e.to_string())),
            };
            Ok(p.display().to_string())
        },
    );
    }
}
