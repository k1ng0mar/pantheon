//! Swarm dashboard routes: fan-out multi-agent runs through real
//! subprocess turns, driven by
//! [`pantheon_runtime::swarm_exec::SwarmOrchestrator`].
//!
//! - `POST /api/swarm` - body `{"task": "...", "agents": 3, "judge": false}`
//!   (agents defaults to 3, must be 1..=8). Returns 201 with the status view.
//! - `GET /api/swarm/status?id=<swarm_id>` - poll until the run settles;
//!   the orchestrator refreshes agent states and runs the judge on each
//!   call, so polling is the only client loop needed.
//! - `GET /api/swarm/transcript?id=<swarm_id>` - combined transcript text.
//! - `POST /api/swarm/<id>/retry` - start a new round on an incomplete
//!   swarm (judge feedback is appended to each agent's task).
//!
//! Production agents run as real `pantheon run --taskID <run_id> --say
//! <task> [--agent <profile>] --deliver session` turn children (see
//! [`spawn_turn_child`]); handler tests drive the same orchestrator over
//! [`pantheon_runtime::swarm_exec::ScriptedWorker`], so no test ever
//! launches a subprocess.

use crate::{
    bad_json, body_json, created_json, err_json, json_ok, spawn_turn_child_with_stdin, App,
};
use pantheon_agent::TurnOutcome;
use pantheon_api::events::Event;
use pantheon_gateway::http::{Request, Response};
use pantheon_providers::http::{aux_complete, aux_request, aux_transport, resolve_aux_wire};
use pantheon_runtime::judge::JudgeTransport;
use pantheon_runtime::swarm_exec::{
    SwarmAgentStatus, SwarmError, SwarmOrchestrator, SwarmStatusView, SwarmWorker,
};
use pantheon_runtime::{new_run_id, Supervisor};
use pantheon_storage::Ledger;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Default agent count when `POST /api/swarm` omits `agents`.
/// Default agent count for `mode: "count"` when `subagent_count` is absent.
const DEFAULT_AGENTS: u64 = 4;
/// Longest `[judge]` model response we need: a verdict plus a few notes.
const JUDGE_MAX_TOKENS: u32 = 1024;
/// `[judge]` timeout fallback when the section sets none.
const JUDGE_TIMEOUT_FALLBACK_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Production worker: real subprocess turns
// ---------------------------------------------------------------------------

/// [`SwarmWorker`] that runs each swarm agent as a real Pantheon turn: a
/// fresh run admitted to the ledger, then a detached `pantheon run
/// --taskID <run_id> --say <task> [--agent <profile>] --deliver session`
/// child (the same path the dashboard's `POST /api/runs` create uses).
/// Profiles-mode agents run under their declared profile via `--agent`,
/// so the profile's SOUL.md / USER.md / AGENTS.md reach the session
/// prompt. The children are headless (stdio nulled, results land in the
/// ledger); `--deliver session` is the delivery target, not an
/// interactive session.
///
/// Status polling reads the ledger's run status and the child's liveness:
/// a terminal ledger status wins; a dead child with a non-terminal status
/// means the turn died without settling and maps to `Failed`.
pub struct SubprocessWorker {
    data_dir: PathBuf,
    /// Turn-child PIDs keyed by run id, for the dead-child check.
    pids: Mutex<HashMap<String, u32>>,
}

impl SubprocessWorker {
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            pids: Mutex::new(HashMap::new()),
        }
    }

    fn ledger(&self) -> Result<Ledger, SwarmError> {
        Ledger::open(&self.data_dir.join("ledger.db"))
            .map_err(|e| SwarmError::worker(format!("open ledger: {e}")))
    }
}

/// Whether a swarm worker child should carry `--agent <profile>`.
///
/// Profiles-mode agents always run as their declared profile. Count
/// mode's `"default"` sentinel only passes `--agent` when a profile is
/// actually declared under that name - otherwise the child would fail
/// closed and every count-mode swarm would break on installs that
/// predate profiles.
fn agent_flag_for(data_dir: &Path, profile: &str) -> Option<String> {
    if profile != "default" {
        return Some(profile.to_string());
    }
    let declared = crate::session_factory::load_config(data_dir)
        .ok()
        .flatten()
        .map(|c| c.agents.contains_key(profile))
        .unwrap_or(false);
    declared.then(|| profile.to_string())
}

/// argv for a swarm worker child: `pantheon run --taskID <run_id> --say
/// <task> [--agent <profile>] --deliver session`.
///
/// Pure constructor so the profile threading stays assertable without
/// spawning anything.
///
/// `--deliver session` drives a real model turn in the child (the same
/// path as `POST /api/runs`); without it `run` only writes synthetic
/// ledger events and the `--agent` profile would never reach a session
/// prompt.
fn swarm_child_argv<'a>(
    run_id: &'a str,
    agent: Option<&'a str>,
    verdict_tool: bool,
) -> Vec<&'a str> {
    // The task text travels on stdin (`--say -`), never on argv, so it
    // stays out of process listings. The caller pipes it via
    // `spawn_turn_child_with_stdin`.
    let mut args = vec!["run", "--taskID", run_id, "--say", "-"];
    if let Some(profile) = agent {
        args.push("--agent");
        args.push(profile);
    }
    if verdict_tool {
        args.push("--verdict-tool");
    }
    args.push("--deliver");
    args.push("session");
    args
}

impl SubprocessWorker {
    /// Spawn one child turn. `verdict_tool` registers the reviewer
    /// `verdict` tool on the turn - set only for staged review stages
    /// via [`SwarmWorker::spawn_reviewer`].
    fn spawn_agent_inner(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
        verdict_tool: bool,
    ) -> Result<String, SwarmError> {
        let run_id = new_run_id();
        // Admit the run before the turn starts (same ordering as
        // `runs::create`); the Supervisor is dropped before spawning so a
        // second live writer never blocks on the busy timeout.
        {
            let sup = Supervisor::open(self.data_dir.clone())
                .map_err(|e| SwarmError::worker(format!("open runtime: {e}")))?;
            sup.start_run(&run_id)
                .map_err(|e| SwarmError::worker(format!("start run: {e}")))?;
            // The profile rides in the title for visibility, and is also
            // threaded into the child via `--agent` so the subprocess
            // executes under the intended profile.
            let mut title = if profile == "default" {
                format!("swarm {swarm_id} · {agent_name}")
            } else {
                format!("swarm {swarm_id} · {agent_name} ({profile})")
            };
            if title.chars().count() > 120 {
                title = title.chars().take(120).collect();
            }
            let _ = sup.emit(Event::SessionTitled {
                run_id: run_id.clone(),
                title,
                model: String::new(),
                source: "swarm".into(),
            });
        }
        // Borrow dance: `spawn_turn_child` takes `&[&str]` with one
        // lifetime, so the slices must outlive the call.
        let task_id = run_id.clone();
        let agent = agent_flag_for(&self.data_dir, profile);
        let args = swarm_child_argv(task_id.as_str(), agent.as_deref(), verdict_tool);
        let pid = spawn_turn_child_with_stdin(&args, task.as_bytes())
            .map_err(|e| SwarmError::worker(format!("spawn turn child: {e}")))?;
        self.pids.lock().unwrap().insert(run_id.clone(), pid);
        Ok(run_id)
    }
}

impl SwarmWorker for SubprocessWorker {
    fn spawn_agent(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
    ) -> Result<String, SwarmError> {
        self.spawn_agent_inner(swarm_id, agent_name, profile, task, false)
    }

    /// Reviewer spawn for staged review stages: like `spawn_agent`, but
    /// the child turn registers the `verdict` tool so the reviewer emits
    /// its verdict as a structured tool call instead of prose.
    fn spawn_reviewer(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
    ) -> Result<String, SwarmError> {
        self.spawn_agent_inner(swarm_id, agent_name, profile, task, true)
    }

    fn agent_status(&self, run_id: &str) -> Result<SwarmAgentStatus, SwarmError> {
        let status = self
            .ledger()?
            .status(run_id)
            .map_err(|e| SwarmError::worker(format!("run status: {e}")))?;
        match status.as_deref() {
            Some("completed") => Ok(SwarmAgentStatus::Done),
            Some("failed") | Some("canceled") => Ok(SwarmAgentStatus::Failed),
            _ => {
                // Non-terminal (or unknown) ledger status: the child may
                // have died without settling. A dead recorded PID means
                // the turn is gone for good - report failure so the swarm
                // can judge on what's there instead of polling forever.
                let pid = self.pids.lock().unwrap().get(run_id).copied();
                match pid {
                    Some(pid) if pid != 0 && !pid_alive(pid) => Ok(SwarmAgentStatus::Failed),
                    _ => Ok(SwarmAgentStatus::Working),
                }
            }
        }
    }

    fn agent_transcript(&self, run_id: &str) -> Result<String, SwarmError> {
        let entries = self
            .ledger()?
            .replay(run_id)
            .map_err(|e| SwarmError::worker(format!("replay run: {e}")))?;
        Ok(transcript_text(&entries))
    }

    /// Every tool call the agent's run issued, replayed from the ledger's
    /// `ToolStarted` events in issue order. Unknown runs replay to an
    /// empty list (same as `agent_transcript`).
    fn agent_tool_calls(
        &self,
        run_id: &str,
    ) -> Result<Vec<pantheon_runtime::swarm_exec::ToolCall>, SwarmError> {
        let entries = self
            .ledger()?
            .replay(run_id)
            .map_err(|e| SwarmError::worker(format!("replay run: {e}")))?;
        Ok(entries
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::ToolStarted { tool, args, .. } => {
                    Some(pantheon_runtime::swarm_exec::ToolCall {
                        tool: tool.clone(),
                        args: args.clone(),
                    })
                }
                _ => None,
            })
            .collect())
    }
}

/// `true` when the process still exists. `kill(pid, 0)` fails with EPERM
/// for processes owned by another user, which still counts as alive.
#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 performs no action; only error-checking.
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            return true;
        }
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Windows: `OpenProcess` fails with access-denied for processes this
/// user cannot query, which still counts as alive, mirroring the unix
/// EPERM case.
#[cfg(target_os = "windows")]
fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_ACCESS_DENIED, STILL_ACTIVE,
    };
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: OpenProcess with query rights is a read-only handle;
    // GetExitCodeProcess only reads it; the handle is always closed.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle != 0 {
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            // Opened but the exit status is unreadable: treat as alive
            // rather than reporting a live child dead. STILL_ACTIVE is an
            // NTSTATUS (i32); the exit code out-param is u32.
            return ok == 0 || code == STILL_ACTIVE as u32;
        }
        GetLastError() == ERROR_ACCESS_DENIED
    }
}

/// Fold one run's ledger events into plain-text transcript lines.
fn transcript_text(entries: &[pantheon_storage::LedgerEntry]) -> String {
    let mut lines = Vec::new();
    for entry in entries {
        let line: Option<String> = match &entry.event {
            Event::UserMessage { text, .. } => Some(format!("user: {text}")),
            Event::AssistantMessage { message, .. } => {
                Some(format!("assistant: {}", message.text_with_image_notes()))
            }
            Event::ToolMessage { message, .. } => {
                Some(format!("tool: {}", message.text_with_image_notes()))
            }
            Event::RunFailed { code, .. } => Some(format!("run failed: {code}")),
            Event::RunCanceled { reason, .. } => Some(format!("run canceled: {reason}")),
            Event::RunCompleted { .. } => Some("[run completed]".to_string()),
            _ => None,
        };
        if let Some(line) = line {
            lines.push(line);
        }
    }
    lines.join("\n")
}

// ---------------------------------------------------------------------------
// Judge transport: one-shot call to the configured `[judge]` aux model
// ---------------------------------------------------------------------------

/// [`JudgeTransport`] resolving the `[judge]` aux section from the
/// dashboard's config.toml and making a single non-streaming aux call.
/// `provider: "default"` (or empty) inherits `[model]`'s provider, matching
/// the TUI's aux resolution.
pub struct ConfigJudgeTransport {
    provider: String,
    model: String,
    api_key_env: Option<String>,
    timeout_secs: u64,
}

impl JudgeTransport for ConfigJudgeTransport {
    fn judge(&self, prompt: &str) -> Result<String, String> {
        let configured = self
            .api_key_env
            .as_deref()
            .and_then(|env| std::env::var(env).ok())
            .unwrap_or_default();
        let wire = resolve_aux_wire(&self.provider, &configured, JUDGE_MAX_TOKENS)
            .map_err(|e| e.cause.clone())?;
        let request = aux_request(&wire, &self.model, prompt.to_string());
        let transport = aux_transport(self.timeout_secs);
        let turn = aux_complete(transport.as_ref(), &wire, request).map_err(|e| e.cause.clone())?;
        match turn.outcome {
            TurnOutcome::Text { text, .. } => Ok(text),
            _ => Err("judge model returned a non-text turn".to_string()),
        }
    }
}

/// Build the judge transport from `[judge]` in the dashboard's config.toml.
/// `None` when the section is absent or the provider can't be resolved
/// the caller then runs swarms judge-free.
pub fn judge_transport_for(data_dir: &Path) -> Option<ConfigJudgeTransport> {
    let cfg = crate::session_factory::load_config(data_dir).ok()??;
    let section = cfg.judge.as_ref()?;
    let model_cfg = cfg.model.as_ref();
    let mut provider = section.provider.clone();
    if provider.is_empty() || provider == "default" {
        provider = model_cfg.map(|m| m.provider.clone()).unwrap_or_default();
    }
    if provider.is_empty() {
        return None;
    }
    let mut model = section.model.clone();
    if model.is_empty() {
        model = model_cfg.map(|m| m.model.clone()).unwrap_or_default();
    }
    Some(ConfigJudgeTransport {
        provider,
        model,
        api_key_env: section
            .api_key_env
            .clone()
            .or_else(|| model_cfg.and_then(|m| m.api_key_env.clone())),
        timeout_secs: section
            .timeout
            .filter(|&t| t > 0)
            .unwrap_or(JUDGE_TIMEOUT_FALLBACK_SECS),
    })
}

/// Production orchestrator for an `App`: subprocess agents plus the
/// configured `[judge]` transport when one resolves.
pub fn orchestrator_for(data_dir: &Path) -> Arc<SwarmOrchestrator> {
    let worker = Arc::new(SubprocessWorker::new(data_dir.to_path_buf()));
    let judge: Option<Arc<dyn JudgeTransport>> =
        judge_transport_for(data_dir).map(|t| Arc::new(t) as Arc<dyn JudgeTransport>);
    Arc::new(SwarmOrchestrator::new(worker, judge))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn swarm_err(e: SwarmError) -> Response {
    err_json(e.http_status, &e.code, &e.message)
}

fn status_json(view: &SwarmStatusView) -> serde_json::Value {
    serde_json::to_value(view).unwrap_or(serde_json::Value::Null)
}

/// `POST /api/swarm`: create a swarm - body
/// `{"task": "...", "mode": "count"|"profiles", "subagent_count": 4,
///   "profiles": ["nyx"], "judge": true}`.
/// `mode` defaults to `"count"`; `subagent_count` 1..=8 defaults to 4
/// (legacy `"agents"` integer is accepted as an alias); `profiles` mode
/// needs 1..=8 names that exist in the config's `[agents]` table;
/// `judge` defaults to true.
pub fn create(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let task = body
        .get("task")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if task.is_empty() {
        return bad_json("field \"task\" is required");
    }
    let mode = body.get("mode").and_then(|v| v.as_str()).unwrap_or("count");
    if mode != "count" && mode != "profiles" {
        return bad_json("field \"mode\" must be \"count\" or \"profiles\"");
    }
    let judge = body.get("judge").and_then(|v| v.as_bool()).unwrap_or(true);
    let specs: Vec<pantheon_runtime::swarm_exec::AgentSpec> = if mode == "profiles" {
        let profiles: Vec<String> = match body.get("profiles").and_then(|v| v.as_array()) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect(),
            None => Vec::new(),
        };
        if profiles.is_empty() || profiles.len() > 8 {
            return bad_json("field \"profiles\" must be 1..=8 profile names");
        }
        // Every named profile must be declared - never silently run a
        // profile that does not exist.
        let cfg = crate::session_factory::load_config(&app.data_dir)
            .ok()
            .flatten();
        let mut specs = Vec::with_capacity(profiles.len());
        for name in profiles {
            let known = cfg
                .as_ref()
                .map(|c| c.agents.contains_key(&name))
                .unwrap_or(false);
            if !known {
                return bad_json(&format!(
                    "unknown profile \"{name}\": not declared in [agents]"
                ));
            }
            specs.push(pantheon_runtime::swarm_exec::AgentSpec {
                name: name.clone(),
                profile: name,
            });
        }
        specs
    } else {
        let count = match body.get("subagent_count").or_else(|| body.get("agents")) {
            None => DEFAULT_AGENTS,
            Some(v) => match v.as_u64() {
                Some(n) => n,
                None => return bad_json("field \"subagent_count\" must be an integer"),
            },
        };
        (1..=count)
            .map(|i| pantheon_runtime::swarm_exec::AgentSpec {
                name: format!("subagent-{i}"),
                profile: "default".to_string(),
            })
            .collect()
    };
    match app.swarm.create_with_specs(task, specs, judge) {
        Ok(created) => {
            let names: Vec<&str> = created
                .status
                .agents
                .iter()
                .map(|a| a.name.as_str())
                .collect();
            let mut body = serde_json::json!({
                "swarm_id": created.id,
                "id": created.id,
                "agents": names,
            });
            // The full first status view rides along for clients that want
            // it (run ids, per-agent profiles); the documented keys above
            // are all a launcher needs.
            body["status_view"] = status_json(&created.status);
            created_json(body)
        }
        Err(e) => swarm_err(e),
    }
}

fn swarm_id_param(req: &Request) -> Result<&str, Response> {
    req.query
        .get("swarm")
        .or_else(|| req.query.get("id"))
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| bad_json("query param \"swarm\" (or \"id\") is required"))
}

/// `GET /api/swarm/status?swarm=<swarm_id>`: refresh agent states (and run
/// the judge when everyone settles), then return the status view.
pub fn status(app: &App, req: &Request) -> Response {
    let id = match swarm_id_param(req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match app.swarm.status(id) {
        Ok(view) => {
            let mut v = status_json(&view);
            // `swarm_id` is the documented key; `id` stays as an alias.
            v["swarm_id"] = serde_json::Value::String(view.id.clone());
            json_ok(v)
        }
        Err(e) => swarm_err(e),
    }
}

/// `GET /api/swarm/transcript?swarm=<swarm_id>[&agent=<name>]`: one agent's
/// live transcript when `agent` is given, else the combined transcript.
pub fn transcript(app: &App, req: &Request) -> Response {
    let id = match swarm_id_param(req) {
        Ok(id) => id,
        Err(r) => return r,
    };
    match req.query.get("agent").map(String::as_str) {
        Some(agent) if !agent.trim().is_empty() => match app.swarm.agent_transcript(id, agent) {
            Ok(t) => json_ok(serde_json::json!({
                "swarm_id": id,
                "id": id,
                "agent": t.name,
                "profile": t.profile,
                "status": t.status.as_str(),
                "transcript": t.transcript,
            })),
            Err(e) => swarm_err(e),
        },
        _ => match app.swarm.transcript(id) {
            Ok(text) => json_ok(serde_json::json!({
                "swarm_id": id,
                "id": id,
                "transcript": text,
            })),
            Err(e) => swarm_err(e),
        },
    }
}

/// `POST /api/swarm/<id>/retry`: start a new round on an incomplete swarm.
pub fn retry(app: &App, id: &str, _req: &Request) -> Response {
    match app.swarm.retry(id) {
        Ok(view) => json_ok(serde_json::json!({
            "swarm_id": id,
            "id": id,
            "round": view.round,
        })),
        Err(e) => swarm_err(e),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
