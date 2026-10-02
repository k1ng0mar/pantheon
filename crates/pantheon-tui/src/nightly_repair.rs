//! Production repair targets for the nightly repair phase.
//!
//! [`pantheon_nightly::run_repair_phase`] is snapshot-driven and
//! host-agnostic: it sees broken MCP servers, scheduled jobs, and tools
//! through the [`McpRepairTarget`], [`ScheduleRepairTarget`], and
//! [`ToolRepairTarget`] traits. These adapters wire those traits to the
//! real hosts:
//!
//! - [`McpRepairAdapter`] drives the live [`McpManager`]: health snapshots
//!   from `health()`, real reconnects through a backoff-bypassing
//!   [`McpManager::retry_now`], config re-resolution through
//!   [`McpManager::configure`], and disable persisted to config.toml (or
//!   the declaring migration file) plus the live manager.
//! - [`ScheduleRepairAdapter`] reads/writes `schedule.json` through the
//!   scheduler core's [`load_jobs`]/[`save_jobs`] and merges per-job
//!   failure counters from [`RunHistory`]. It deliberately does not touch
//!   the tick loop's locking, pause propagation, or fire recording.
//! - [`ToolRepairAdapter`] smoke-probes tools with empty args behind the
//!   capability gate (allowlist-only), re-resolves MCP-backed tools
//!   through their owning server's config, and disables through
//!   [`ToolRegistry::remove`] plus the durable disable list in
//!   [`pantheon_runtime::nightly_tools`] so the next registry build skips
//!   the name.
//!
//! Behavioral coverage of the phase itself lives in
//! `eval/tests/nightly_repair.rs` (fakes); the tests here cover the
//! adapter mapping and persistence that need no live infrastructure.

use pantheon_api::capability::Policy;
use pantheon_mcp::manager::{sanitize_segment, McpManager, McpServerSpec, ServerHealth};
use pantheon_migration::read_mcp_declarations;
use pantheon_nightly::{
    McpRepairTarget, McpServerSnapshot, ScheduleRepairTarget, ScheduledJobSnapshot,
    ToolRepairTarget,
};
use pantheon_scheduler::{load_jobs, save_jobs, RunHistory, ScheduleKind};
use pantheon_tools::builtins::{register_builtins_with, BuiltinOptions};
use pantheon_tools::tools::ToolRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use pantheon_runtime::nightly_tools::{load_disabled_tools, record_disabled_tool};

// ---------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------

/// [`McpRepairTarget`] over the live [`McpManager`].
pub struct McpRepairAdapter {
    manager: Arc<McpManager>,
    data_dir: PathBuf,
}

impl McpRepairAdapter {
    pub fn new(manager: Arc<McpManager>, data_dir: PathBuf) -> Self {
        Self { manager, data_dir }
    }
}

/// One server's health as the repair phase sees it.
fn snapshot_of(h: &ServerHealth) -> McpServerSnapshot {
    McpServerSnapshot {
        name: h.name.clone(),
        status: h.status.as_str().to_string(),
        failures: h.failures,
        last_error: h.last_error.clone(),
    }
}

/// Re-read one server's spec from the on-disk config + declarations and
/// push it through [`McpManager::configure`].
///
/// A changed spec drops the live connection (the manager reconnects
/// lazily on next use); an unchanged spec is a no-op by `configure`'s
/// idempotence contract - callers that need a real attempt follow up
/// with [`McpManager::retry_now`]. Approval records are untouched: this
/// never approves, un-approves, or otherwise touches the launcher's
/// approval logic.
fn reresolve_mcp_server(manager: &McpManager, data_dir: &Path, name: &str) -> Result<(), String> {
    let cfg = crate::config::Config::load_or_report(data_dir);
    let declarations = read_mcp_declarations(data_dir);
    let mcfg = crate::config::resolve_mcp_section(
        cfg.as_ref().and_then(|c| c.mcp.as_ref()),
        &declarations,
    );
    let spec = mcfg
        .servers
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("mcp server '{name}': not declared (or disabled) in config"))?;
    let mut specs: Vec<McpServerSpec> = manager
        .spec_names()
        .into_iter()
        .filter_map(|n| manager.spec(&n))
        .collect();
    match specs.iter_mut().find(|s| s.name == name) {
        Some(slot) => *slot = spec,
        None => specs.push(spec),
    }
    manager.configure(specs);
    Ok(())
}

/// Persist `enabled = false` for one MCP server: `[mcp.servers.<name>]`
/// in config.toml when the server is config-defined, else the
/// declaration file that declares it (mirroring `pantheon mcp disable`,
/// whose verb refuses to rewrite config-defined servers and vice
/// versa).
fn persist_mcp_disabled(data_dir: &Path, name: &str) -> Result<(), String> {
    let mut cfg = crate::config::Config::load_or_report(data_dir).unwrap_or_default();
    if let Some(entry) = cfg.mcp.as_mut().and_then(|m| m.servers.get_mut(name)) {
        entry.enabled = false;
        return cfg
            .save(data_dir)
            .map_err(|e| format!("save config: {}: {}", e.code, e.cause));
    }
    // Declaration-sourced server: rewrite its declaration file.
    let declarations = read_mcp_declarations(data_dir);
    let source = declarations
        .iter()
        .find(|d| d.servers.iter().any(|s| s.name == name))
        .map(|d| d.source.clone())
        .ok_or_else(|| format!("mcp server '{name}': not declared in config or declarations"))?;
    let path = data_dir.join("mcp").join(format!("{source}.json"));
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?;
    let mut touched = false;
    if let Some(servers) = v.get_mut("servers").and_then(|s| s.as_array_mut()) {
        for s in servers {
            if s.get("name").and_then(|n| n.as_str()) == Some(name) {
                s["enabled"] = serde_json::Value::Bool(false);
                touched = true;
            }
        }
    }
    if !touched {
        return Err(format!(
            "mcp server '{name}': not found in {}",
            path.display()
        ));
    }
    // Atomic rewrite: temp file + rename.
    let tmp = path.with_extension("json.tmp");
    let text =
        serde_json::to_string_pretty(&v).map_err(|e| format!("encode {}: {e}", path.display()))?;
    std::fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("publish {}: {e}", path.display()))?;
    Ok(())
}

impl McpRepairTarget for McpRepairAdapter {
    fn servers(&mut self) -> Vec<McpServerSnapshot> {
        // Cheap liveness only, no probing: the repair ladder owns every
        // mutating attempt (retry_connect / re_resolve), so snapshotting
        // must not spend them as a side effect.
        self.manager.health().iter().map(snapshot_of).collect()
    }

    fn retry_connect(&mut self, name: &str) -> Result<(), String> {
        // One real connect attempt: retry_now bypasses the reconnect
        // backoff cooldown, which the lazy paths would otherwise honor
        // (a server inside its cooldown would never actually be
        // retried). Re-resolving + reconfiguring cannot do this:
        // configure is idempotent on an unchanged spec and only
        // reconnects lazily on a changed one.
        self.manager.retry_now(name).map_err(|e| e.to_string())
    }

    fn re_resolve(&mut self, name: &str) -> Result<(), String> {
        // Re-resolve the server's config - env vars and paths may have
        // changed since it was configured - then make the retry real
        // instead of lazy: the ladder counts this step as
        // re-resolve + retry.
        reresolve_mcp_server(&self.manager, &self.data_dir, name)?;
        self.manager.retry_now(name).map_err(|e| e.to_string())
    }

    fn disable(&mut self, name: &str, _reason: &str) -> Result<(), String> {
        // The reason rides the phase's escalation; the adapter's job is
        // containment: persist the disable and stop the live manager
        // reconnecting.
        persist_mcp_disabled(&self.data_dir, name)?;
        let mut specs: Vec<McpServerSpec> = self
            .manager
            .spec_names()
            .into_iter()
            .filter_map(|n| self.manager.spec(&n))
            .collect();
        let spec = specs
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("mcp server '{name}': not managed"))?;
        spec.enabled = false;
        // The changed-spec branch drops the live connection (Drop shuts
        // the child down) and marks the server Disabled, so nothing
        // reconnects it. The approval record is kept: re-enabling later
        // does not need re-approval.
        self.manager.configure(specs);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

/// [`ScheduleRepairTarget`] over `schedule.json` + [`RunHistory`].
pub struct ScheduleRepairAdapter {
    data_dir: PathBuf,
}

impl ScheduleRepairAdapter {
    pub fn new(data_dir: PathBuf) -> Self {
        Self { data_dir }
    }
}

impl ScheduleRepairTarget for ScheduleRepairAdapter {
    fn jobs(&mut self) -> Vec<ScheduledJobSnapshot> {
        let jobs = load_jobs(&self.data_dir).unwrap_or_default();
        let history = RunHistory::open(&self.data_dir).ok();
        jobs.into_iter()
            .map(|s| {
                let stats = history.as_ref().and_then(|h| h.stats(&s.job.id));
                let (kind, cron_expr) = match &s.job.kind {
                    ScheduleKind::Cron { expr } => ("cron", Some(expr.clone())),
                    ScheduleKind::Interval { .. } => ("interval", None),
                    ScheduleKind::OneShot { .. } => ("one-shot", None),
                    ScheduleKind::Webhook { path } => ("webhook", Some(path.clone())),
                };
                ScheduledJobSnapshot {
                    id: s.job.id.clone(),
                    kind: kind.to_string(),
                    cron_expr,
                    paused: s.job.paused,
                    consecutive_failures: stats.map(|st| st.consecutive_failures).unwrap_or(0),
                    last_error: stats.and_then(|st| st.last_error.clone()),
                }
            })
            .collect()
    }

    fn repair_cron(&mut self, id: &str, new_expr: &str) -> Result<(), String> {
        // The expression was already validated by the repair phase; the
        // host persists it and resets the job's run history, per the
        // trait contract.
        let mut jobs = load_jobs(&self.data_dir).map_err(|e| format!("load schedule: {e}"))?;
        let job = jobs
            .iter_mut()
            .find(|j| j.job.id == id)
            .ok_or_else(|| format!("scheduled job '{id}': not found"))?;
        job.job.kind = ScheduleKind::Cron {
            expr: new_expr.to_string(),
        };
        save_jobs(&self.data_dir, &jobs).map_err(|e| format!("save schedule: {e}"))?;
        let mut history =
            RunHistory::open(&self.data_dir).map_err(|e| format!("open run history: {e}"))?;
        history.reset(id)
    }

    fn pause(&mut self, id: &str, _reason: &str) -> Result<(), String> {
        // The reason rides the phase's escalation and audit events; the
        // store carries only the paused flag (same shape as
        // `pantheon schedule pause`).
        let mut jobs = load_jobs(&self.data_dir).map_err(|e| format!("load schedule: {e}"))?;
        let job = jobs
            .iter_mut()
            .find(|j| j.job.id == id)
            .ok_or_else(|| format!("scheduled job '{id}': not found"))?;
        job.job.paused = true;
        save_jobs(&self.data_dir, &jobs).map_err(|e| format!("save schedule: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Owning server for an `mcp_<server>_<tool>` projected tool name: the
/// longest sanitized-server prefix wins. `None` when the name is not
/// MCP-backed.
fn mcp_server_for_tool(servers: &[String], tool: &str) -> Option<String> {
    let rest = tool.strip_prefix("mcp_")?;
    servers
        .iter()
        .filter_map(|s| {
            let san = sanitize_segment(s);
            rest.strip_prefix(san.as_str())
                .filter(|r| r.starts_with('_'))
                .map(|_| (san.len(), s.clone()))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, s)| s)
}

/// [`ToolRepairTarget`] over a [`ToolRegistry`] plus the durable disable
/// list.
///
/// The registry is the probe surface (builtins per `[tools]` enablement
/// plus the MCP-projected tools); it is built lazily on the first
/// allowlisted probe so a pass that never probes never connects to
/// anything. `disable` removes the name from the built registry when
/// there is one, but the durable disable list is the real containment:
/// every future [`Session::build_tool_registry`] skips the name.
pub struct ToolRepairAdapter {
    data_dir: PathBuf,
    allowlist: Vec<String>,
    mcp: Option<Arc<McpManager>>,
    registry: Option<ToolRegistry>,
}

impl ToolRepairAdapter {
    pub fn new(data_dir: PathBuf, allowlist: Vec<String>, mcp: Option<Arc<McpManager>>) -> Self {
        Self {
            data_dir,
            allowlist,
            mcp,
            registry: None,
        }
    }

    /// The probe registry: builtins per `[tools]` enablement plus the
    /// MCP-projected tools, mirroring the session's registry closely
    /// enough for smoke probes. Built lazily: probing is allowlist-gated,
    /// so a pass that never probes never connects to anything.
    fn probe_registry(&mut self) -> &ToolRegistry {
        if self.registry.is_none() {
            let mut reg = ToolRegistry::new();
            let file_cfg = crate::config::Config::load_or_report(&self.data_dir);
            let tools = crate::config::tool_enablement(file_cfg.as_ref());
            register_builtins_with(
                &mut reg,
                BuiltinOptions {
                    safewrite_state_dir: Some(self.data_dir.join("safewrite")),
                    workspace_root: None,
                    enable_terminal: tools.terminal,
                    enable_files: tools.files,
                    enable_ask_user: tools.ask_user,
                    enable_plugins: tools.plugins,
                    data_dir: Some(self.data_dir.clone()),
                    // Drive-by compile fix (another leaf added this field to
                    // BuiltinOptions): None matches the struct's Default.
                    shell_child_hook: None,
                },
            );
            if let Some(mcp) = &self.mcp {
                // Project the servers' tools so allowlisted `mcp_*` names
                // probe the real thing. This connects to approved servers
                // - the same thing session startup does - which is why it
                // stays behind the lazy build.
                let _ = mcp.register_tools(&mut reg);
            }
            self.registry = Some(reg);
        }
        self.registry.as_ref().expect("built above")
    }
}

impl ToolRepairTarget for ToolRepairAdapter {
    fn probe(&mut self, name: &str) -> Result<(), String> {
        // Probing executes the tool: the allowlist is the safety gate,
        // and the adapter refuses anything outside it even though the
        // phase already enforces the same list (defense in depth).
        if !self.allowlist.iter().any(|a| a == name) {
            return Err(format!(
                "refusing to probe '{name}': not in the nightly tool_probe_allowlist"
            ));
        }
        // Empty-args smoke probe behind the capability gate. The policy is
        // deliberately read-only: a probe must never mutate, and a tool
        // that cannot run read-only fails closed - the ladder then
        // contains it instead of trusting it.
        let policy = Policy::researcher_readonly();
        let reg = self.probe_registry();
        reg.execute_gated(&policy, name, "{}")
            .map(|_| ())
            .map_err(|e| format!("probe '{name}' failed: {}: {}", e.code, e.cause))
    }

    fn re_resolve(&mut self, name: &str) -> Result<(), String> {
        // The only re-resolution path in this host: MCP-projected tools
        // (`mcp_<server>_<tool>`) re-resolve through their owning
        // server's config (env vars and paths may have changed). Anything
        // else has no backing config to re-resolve - and returning Ok here
        // would be a lie: the ladder treats Ok as "recovered" and would
        // skip containment for a still-broken tool.
        let mcp = self
            .mcp
            .clone()
            .ok_or_else(|| format!("tool '{name}': no MCP manager; nothing to re-resolve"))?;
        let server = mcp_server_for_tool(&mcp.spec_names(), name)
            .ok_or_else(|| format!("tool '{name}': not MCP-backed; no re-resolution path"))?;
        reresolve_mcp_server(&mcp, &self.data_dir, &server)?;
        mcp.retry_now(&server).map_err(|e| e.to_string())?;
        // Confirmation probe, still allowlist-gated: Ok only when the
        // tool demonstrably works again.
        self.probe(name)
    }

    fn disable(&mut self, name: &str, reason: &str) -> Result<(), String> {
        if let Some(reg) = self.registry.as_mut() {
            reg.remove(name);
        }
        record_disabled_tool(&self.data_dir, name, reason)
    }
}

// ---------------------------------------------------------------------------
// Tests (no live infrastructure: mapping, gating, persistence)
// ---------------------------------------------------------------------------
