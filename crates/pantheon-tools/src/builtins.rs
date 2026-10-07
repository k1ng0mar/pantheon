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
use pantheon_exec::compact_output;
use pantheon_exec::confine::confine;
use pantheon_exec::safewrite::{atomic_write, FileEdit, SafeWriter};
use pantheon_exec::sandbox::{SandboxLevel, SandboxProfile};
use std::path::{Path, PathBuf};
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
    /// Workspace root for path confinement of the fs tools. The tool layer
    /// has no workspace concept of its own, so when this is `None` the
    /// process working directory is captured here, at registration time.
    /// Every `read_file`/`write_file`/`list_dir` path is confined with
    /// `pantheon_exec::confine` (deny globs, then containment) before any
    /// read or write - a granted `FilesystemRead`/`FilesystemWrite`
    /// capability never widens it.
    pub workspace_root: Option<std::path::PathBuf>,
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
}

impl std::fmt::Debug for BuiltinOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BuiltinOptions")
            .field("safewrite_state_dir", &self.safewrite_state_dir)
            .field("workspace_root", &self.workspace_root)
            .field("enable_terminal", &self.enable_terminal)
            .field("enable_files", &self.enable_files)
            .field("enable_ask_user", &self.enable_ask_user)
            .field("enable_plugins", &self.enable_plugins)
            .field("data_dir", &self.data_dir)
            .field("shell_child_hook", &self.shell_child_hook.is_some())
            .field("shell_env_hook", &self.shell_env_hook.is_some())
            .finish()
    }
}

impl Default for BuiltinOptions {
    /// Absent `[tools]` section = every group on, matching the
    /// pre-section behavior exactly.
    fn default() -> Self {
        Self {
            safewrite_state_dir: None,
            workspace_root: None,
            enable_terminal: true,
            enable_files: true,
            enable_ask_user: true,
            enable_plugins: true,
            data_dir: None,
            shell_child_hook: None,
            shell_env_hook: None,
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

fn arg_str(v: &serde_json::Value, key: &str) -> Result<String, PantheonError> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            crate::tools::tool_err(
                "TOOL_BAD_ARGS",
                Layer::Execution,
                false,
                format!("missing string arg '{key}'"),
                "check tool arguments",
            )
        })
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
    // Workspace root for confinement, captured once at registration: no
    // workspace concept exists in the tool layer, so this is the process
    // cwd unless the caller overrides it. Confine errors propagate with
    // their CONFINE_* codes (deny globs are evaluated before containment,
    // and independently of the capability gate).
    let root: std::sync::Arc<PathBuf> = std::sync::Arc::new(
        opts.workspace_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    );
    if opts.enable_terminal {
        // 'static closure: the child hook is an Arc, so the clone here
        // is cheap and the registry owns its copy.
        let shell_child_hook = opts.shell_child_hook.clone();
        let shell_env_hook = opts.shell_env_hook.clone();
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
            let command = arg_str(&v, "command")?;
            pantheon_exec::danger::gate(&command)?;
            run_shell_with_env(
                args,
                shell_child_hook.as_ref(),
                shell_env_hook.as_ref().map(|h| h(command.as_str())).unwrap_or_default().as_slice(),
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
    let r0 = root.clone();
    if opts.enable_files {
        reg.register(
            ToolSchema {
                name: "read_file".into(),
                description:
                    "Read a text file (compacted if very large). Confined to the workspace.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            },
            Capability::FilesystemRead,
            move |args| {
                let v = parse_args(args)?;
                let path = arg_str(&v, "path")?;
                let cpath = confine(Path::new(&path), &r0)?;
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
    let r1 = root.clone();
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
            let path = arg_str(&v, "path")?;
            // Confine before either write path (safe or unsafe fallback).
            let cpath = confine(Path::new(&path), &r1)?;
            let content = arg_str(&v, "content")?;
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
                    .with_workspace_root((*r1).clone());
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
                Ok(format!(
                    "wrote {} bytes to {}; checkpoint={}",
                    content.len(),
                    cpath.display(),
                    receipt.checkpoint_id
                ))
            } else {
                // Unsafe fallback: caller opted out of the safe path.
                atomic_write(&cpath, content.as_bytes())
                    .map_err(|e| {
                        crate::tools::tool_err("TOOL_FS", Layer::Execution, false, format!("write {}: {e}", cpath.display()), "check tool arguments")
                    })?;
                Ok(format!("wrote {} bytes to {}", content.len(), cpath.display()))
            }
        },
    );
    }
    let r2 = root.clone();
    if opts.enable_files {
        reg.register(
            ToolSchema {
                name: "list_dir".into(),
                description: "List a directory's entries, one per line. Confined to the workspace."
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
                let path = arg_str(&v, "path")?;
                let cpath = confine(Path::new(&path), &r2)?;
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
    // operator grants. Never silent. Only bundled-catalog names are
    // accepted - there is no agent path to install or enable arbitrary
    // plugins. The config file is the single enablement state, so the
    // toggle lands in `[plugins.<name>]` where the dashboard, the mobile
    // app, and the TUI all read it. Without a data dir the tool cannot
    // function, so it is not registered at all rather than advertised
    // broken.
    if opts.enable_plugins && opts.data_dir.is_some() {
        let dd = opts.data_dir.clone();
        reg.register(
        ToolSchema {
            name: "enable_plugin".into(),
            description: "Propose enabling one of Pantheon's bundled plugins (first-party code shipped with Pantheon, all disabled by default). `name` must be a bundled-catalog name - anything else is refused. Calling this parks the run for operator approval: the proposal is audit-logged and the plugin switches on only if the operator grants. Use when a bundled plugin would genuinely help the task; say why in your message first."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Bundled plugin name from the catalog (e.g. \"time-gap\")" }
                },
                "required": ["name"]
            }),
        },
        Capability::PluginEnable,
        move |args| {
            let v = parse_args(args)?;
            let name = arg_str(&v, "name")?;
            let data_dir = dd.clone().ok_or_else(|| {
                crate::tools::tool_err("TOOL_NO_DATA_DIR", Layer::Execution, false, "plugin enablement needs a data dir; registration did not provide one".to_string(), "check tool arguments")
            })?;
            let plugin = pantheon_extensions::bundled::find(&name).ok_or_else(|| {
                crate::tools::tool_err("TOOL_UNKNOWN_PLUGIN", Layer::Execution, false, format!("'{name}' is not a bundled plugin; only catalog plugins can be enabled"), "check tool arguments")
            })?;
            pantheon_extensions::bundled::set_enabled(&data_dir, &plugin.name, true).map_err(|e| {
                crate::tools::tool_err("TOOL_PLUGIN_ENABLE", Layer::Execution, false, e.to_string(), "check tool arguments")
            })?;
            Ok(format!(
                "bundled plugin '{}' ({}) enabled: [plugins.{}] enabled = true",
                plugin.name, plugin.kind, plugin.name
            ))
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
            let name = arg_str(&v, "name")?;
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
) -> Result<String, PantheonError> {
    let v = parse_args(args)?;
    let command = arg_str(&v, "command")?;
    // Shell runs at HIGH isolation: bwrap with dropped caps + no-new-privs
    // + rlimits. The capability gate already ran before we get here.
    let profile = SandboxProfile::from(SandboxLevel::High);
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "/tmp".to_string());

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

    fn call_enable_mcp(reg: &ToolRegistry) -> Result<String, pantheon_api::error::PantheonError> {
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
        let err = call_enable_mcp(&reg).expect_err("poisoned table must be refused, not enabled");
        let msg = err.to_string();
        assert!(
            msg.contains("TOOL_MCP_NAME_COLLISION"),
            "refusal must name the collision; got: {msg}"
        );

        // Never silently kept: the enable flag must NOT have flipped.
        let cfg_text = std::fs::read_to_string(data_dir.join("config.toml")).expect("read config");
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

        let cfg =
            pantheon_api::mcp_catalog::load_config(data_dir).expect("config loads after enable");
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

        let cfg =
            pantheon_api::mcp_catalog::load_config(data_dir).expect("config loads after enable");
        assert!(cfg.mcp_server_enabled("playwright"));
    }
}
