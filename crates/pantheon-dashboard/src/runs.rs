//! Runs, overview, transcript export, and prune.
//!
//! All reads come from the real event ledger (`pantheon_storage::Ledger`);
//! the transcript is rebuilt with `pantheon_runtime::session`'s own
//! functions, so the dashboard shows exactly what the TUI would.

use crate::util::{now_ms, round_cost_usd};
use crate::{
    bad_json, body_json, conflict, created_json, err_json, json_ok, query_usize, spawn_turn_child,
    spawn_turn_child_with_stdin, uploads, App,
};
use pantheon_api::events::Event;
use pantheon_api::logging::redact;
use pantheon_api::message::Role;
use pantheon_api::model::{bound_title, TITLE_MAX_CHARS};
use pantheon_api::todo::{TodoItem, TodoList};
use pantheon_gateway::http::{Request, Response};
use pantheon_runtime::session::{rebuild_transcript, TranscriptItem};
use pantheon_runtime::{new_run_id, Supervisor};
use pantheon_storage::{Ledger, HOME_SESSION_ID};
use std::collections::HashMap;
use std::path::PathBuf;

/// Fallback run mode when the ledger stores none for a run. Mirrors the
/// stable string form of `pantheon_api::AgentMode::Build`.
pub(crate) const DEFAULT_RUN_MODE: &str = "build";
/// The other accepted run-mode string (checked in `set_mode`).
const PLAN_RUN_MODE: &str = "plan";
/// Upper bound for full-ledger scans. `list_runs` here feeds rollups and
/// KPI aggregation, never pagination, so the cap just needs to exceed any
/// realistic run count while keeping the scan bounded.
const MAX_RUNS_SCAN: usize = 10_000;
/// Max characters for one queued message (queue append and PATCH edit).
/// Queued text rides the next turn's prompt; unbounded text is a prompt-
/// injection amplifier and a ledger-bloat vector.
const MAX_QUEUED_CHARS: usize = 8_000;

/// 413 when `text` exceeds [`MAX_QUEUED_CHARS`].
fn reject_overlong_queue(text: &str) -> Option<Response> {
    if text.chars().count() > MAX_QUEUED_CHARS {
        Some(err_json(
            413,
            "QUEUE_TOO_LARGE",
            &format!("queued message exceeds {MAX_QUEUED_CHARS} characters"),
        ))
    } else {
        None
    }
}

fn ledger(app: &App) -> Result<Ledger, Response> {
    Ledger::open(&app.data_dir.join("ledger.db"))
        .map_err(|e| err_json(500, "LEDGER", &format!("open ledger: {e}")))
}

/// Per-run rollup folded from its events: model, tokens, cost, end time.
#[derive(Default)]
struct Rollup {
    model: Option<String>,
    provider: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    cost_usd: f64,
    ended_ms: Option<i64>,
    turns: u64,
    tool_calls: u64,
    approvals_pending: u64,
    /// ts_ms of the newest event seen (recency for the sessions list).
    updated_ms: i64,
    /// Human-readable last activity, e.g. "Mapping registrations".
    last_activity: Option<String>,
}

fn rollup(entries: &[pantheon_storage::LedgerEntry]) -> Rollup {
    let mut r = Rollup::default();
    for e in entries {
        match &e.event {
            Event::ModelRequested { model, .. } => {
                if r.model.is_none() {
                    let (provider, name) = match model.split_once(':') {
                        Some((p, n)) => (p.to_string(), n.to_string()),
                        None => (String::new(), model.clone()),
                    };
                    r.provider = Some(provider);
                    r.model = Some(name);
                }
            }
            Event::UsageRecorded {
                model,
                provider,
                input_tokens,
                output_tokens,
                cost_usd,
                ..
            } => {
                if r.model.is_none() && model != "unknown" {
                    r.model = Some(model.clone());
                }
                if !provider.is_empty() {
                    r.provider = Some(provider.clone());
                }
                r.input_tokens += input_tokens;
                r.output_tokens += output_tokens;
                r.cost_usd += cost_usd.unwrap_or(0.0);
            }
            Event::RunCompleted { .. } | Event::RunFailed { .. } | Event::RunCanceled { .. } => {
                r.ended_ms = Some(e.ts_ms);
            }
            Event::TurnStarted { .. } => r.turns += 1,
            Event::AssistantMessage { message, .. } => {
                r.tool_calls += message.tool_calls.len() as u64;
            }
            Event::ApprovalRequested { .. } => r.approvals_pending += 1,
            Event::ApprovalGranted { .. } | Event::ApprovalDenied { .. } => {
                r.approvals_pending = r.approvals_pending.saturating_sub(1);
            }
            _ => {}
        }
        // Session-list activity: the newest meaningful event wins.
        r.updated_ms = r.updated_ms.max(e.ts_ms);
        let activity: Option<String> = match &e.event {
            Event::RunProgress { detail, .. } => Some(redact(detail)),
            Event::RunStarted { .. } => Some("Session started".to_string()),
            Event::RunCompleted { .. } => Some("Run completed".to_string()),
            Event::RunFailed { code, .. } => Some(format!("Failed: {code}")),
            Event::RunCanceled { .. } => Some("Run canceled".to_string()),
            Event::RunRecovered { .. } => Some("Run recovered".to_string()),
            Event::TurnStarted { .. } => Some("Working…".to_string()),
            Event::ApprovalRequested { .. } => Some("Waiting on approval".to_string()),
            Event::ApprovalGranted { .. } => Some("Approval granted".to_string()),
            Event::ApprovalDenied { .. } => Some("Approval denied".to_string()),
            Event::AgentSpawned { agent, .. } => Some(format!("Subagent started: {agent}")),
            Event::AgentCompleted { agent, .. } => Some(format!("Subagent finished: {agent}")),
            Event::ToolStarted { tool, .. } => Some(format!("Running {tool}")),
            _ => None,
        };
        if activity.is_some() {
            r.last_activity = activity;
        }
    }
    r
}

fn run_json(
    run_id: &str,
    status: &str,
    created_ms: i64,
    title: Option<&str>,
    r: &Rollup,
    project: Option<&str>,
    pinned: bool,
    archived: bool,
) -> serde_json::Value {
    serde_json::json!({
        "id": run_id,
        "status": status,
        "created_ms": created_ms,
        "title": title.unwrap_or(""),
        "is_home": run_id == HOME_SESSION_ID,
        "model": r.model,
        "provider": r.provider,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "cost_usd": round_cost_usd(r.cost_usd),
        "ended_ms": r.ended_ms,
        "turns": r.turns,
        "tool_calls": r.tool_calls,
        "approvals_pending": r.approvals_pending,
        "updated_ms": r.updated_ms,
        "last_activity": r.last_activity,
        "project": project,
        "pinned": pinned,
        "archived": archived,
    })
}

/// `GET /api/overview`: KPI cards — totals, costs, pending approvals,
/// scheduler counts, gateway state. Empty ledger = zeros, not an error.
pub fn overview(app: &App) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let runs = ledger.list_runs(MAX_RUNS_SCAN).unwrap_or_default();
    let now_ms = now_ms();
    let day_ms: i64 = 86_400_000;
    let mut total = 0u64;
    let mut by_status: HashMap<String, u64> = HashMap::new();
    let mut cost_24h = 0.0f64;
    let mut tokens_24h = 0u64;
    let mut pending_approvals = 0u64;
    for (id, status, created_ms, ..) in &runs {
        total += 1;
        *by_status.entry(status.clone()).or_insert(0) += 1;
        let entries = ledger.replay(id).unwrap_or_default();
        let r = rollup(&entries);
        pending_approvals += r.approvals_pending;
        if *created_ms >= now_ms - day_ms {
            cost_24h += r.cost_usd;
            tokens_24h += r.input_tokens + r.output_tokens;
        }
    }
    // Scheduler: count stored jobs directly (same file the CLI reads).
    let jobs = pantheon_scheduler::load_jobs(&app.data_dir).unwrap_or_default();
    let active_jobs = jobs.iter().filter(|j| !j.job.paused).count();
    json_ok(serde_json::json!({
        "runs": {"total": total, "by_status": by_status},
        "last_24h": {
            "cost_usd": round_cost_usd(cost_24h),
            "tokens": tokens_24h,
        },
        "approvals_pending": pending_approvals,
        "schedule": {"total": jobs.len(), "active": active_jobs},
    }))
}

/// `GET /api/runs?q=&status=&limit=`: run list with rollups. This is also
/// the gateway mobile-API session list: the mobile app fetches its sessions
/// here. The permanent home session is auto-created on first access and
/// pinned first, ahead of every ordinary session.
pub fn list(app: &App, req: &Request) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    if let Err(e) = ledger.ensure_home_session() {
        return err_json(500, "LEDGER", &format!("home session: {e}"));
    }
    let limit = query_usize(&req.query, "limit", 100).min(1000);
    let q = req.query.get("q").map(|s| s.to_lowercase());
    let status_filter = req.query.get("status").cloned();
    // Archived runs are hidden from the list by default; `?include_archived=1`
    // brings them back (the dashboard restores or prunes them from there).
    let include_archived = req.query.get("include_archived").map(String::as_str) == Some("1");
    let mut out = Vec::new();
    let rows = Ledger::pin_home_first(ledger.list_runs(limit * 4).unwrap_or_default());
    for (id, status, created_ms, title, project, pinned, archived) in rows {
        if archived && !include_archived {
            continue;
        }
        if let Some(sf) = &status_filter {
            if &status != sf {
                continue;
            }
        }
        if let Some(qq) = &q {
            let hay = format!("{id} {}", title.as_deref().unwrap_or("")).to_lowercase();
            if !hay.contains(qq) {
                continue;
            }
        }
        let entries = ledger.replay(&id).unwrap_or_default();
        let r = rollup(&entries);
        out.push(run_json(
            &id,
            &status,
            created_ms,
            title.as_deref(),
            &r,
            project.as_deref(),
            pinned,
            archived,
        ));
        if out.len() >= limit {
            break;
        }
    }
    json_ok(serde_json::json!({"runs": out}))
}

/// A redacted timeline entry: tool arguments go through the same
/// [`redact`] pass the TUI's log views use, so a key pasted into a tool
/// arg never reaches the browser. Tool durations are derived from the
/// ledger's own timestamps (ToolStarted → ToolCompleted by call_id), so
/// no new event fields were needed.
fn timeline_item(
    e: &pantheon_storage::LedgerEntry,
    tool_starts: &std::collections::HashMap<String, i64>,
) -> serde_json::Value {
    let kind: &str = match &e.event {
        Event::RunStarted { .. } => "run_started",
        Event::RunCompleted { .. } => "run_completed",
        Event::RunFailed { .. } => "run_failed",
        Event::RunCanceled { .. } => "run_canceled",
        Event::TurnStarted { .. } => "turn_started",
        Event::UserMessage { .. } => "user_message",
        Event::TurnCompleted { .. } => "turn_completed",
        Event::TurnParked { .. } => "turn_parked",
        Event::ModelRequested { .. } => "model_requested",
        Event::ModelCompleted { .. } => "model_completed",
        Event::RunProgress { detail, .. } if detail.starts_with("fallback ") => "model_fallback",
        Event::RunProgress { detail, .. } if detail.starts_with("model attempt failed: ") => {
            "model_attempt_failed"
        }
        Event::RunProgress { detail, .. } if detail.starts_with("provider chain exhausted: ") => {
            "model_exhausted"
        }
        Event::UsageRecorded { .. } => "usage",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::ApprovalGranted { .. } => "approval_granted",
        Event::ApprovalDenied { .. } => "approval_denied",
        Event::AgentSpawned { .. } => "agent_spawned",
        Event::AgentMessage { .. } => "agent_message",
        Event::AgentCompleted { .. } => "agent_completed",
        Event::SessionTitled { .. } => "titled",
        Event::ToolRequested { .. } => "tool_requested",
        Event::ToolStarted { .. } => "tool_started",
        Event::ToolOutput { .. } => "tool_output",
        Event::ToolCompleted { .. } => "tool_completed",
        Event::ScheduledTaskFailed { .. } => "scheduled_task_failed",
        Event::ScheduledTaskRecovered { .. } => "scheduled_task_recovered",
        _ => "other",
    };
    let detail = match &e.event {
        Event::RunFailed { code, .. } => Some(code.clone()),
        Event::RunCanceled { reason, .. } => Some(reason.clone()),
        Event::TurnParked { reason, .. } => Some(reason.clone()),
        Event::TurnStarted { turn_id, .. } => Some(format!("turn {turn_id}")),
        Event::UserMessage { text, .. } => Some(redact(text)),
        Event::ModelRequested { model, .. } => Some(model.clone()),
        Event::RunProgress { detail, .. } if detail.starts_with("fallback ") => {
            Some(detail.clone())
        }
        Event::RunProgress { detail, .. } if detail.starts_with("model attempt failed: ") => {
            Some(detail.clone())
        }
        Event::RunProgress { detail, .. } if detail.starts_with("provider chain exhausted: ") => {
            Some(detail.clone())
        }
        Event::UsageRecorded {
            model,
            input_tokens,
            output_tokens,
            cost_usd,
            ..
        } => Some(format!(
            "{model}: {input_tokens} in / {output_tokens} out{}",
            cost_usd.map(|c| format!(" (${c:.4})")).unwrap_or_default()
        )),
        Event::ApprovalRequested { scope, .. } => Some(redact(scope)),
        Event::ApprovalGranted { scope, .. } => Some(redact(scope)),
        Event::ApprovalDenied { scope, .. } => Some(redact(scope)),
        Event::AgentSpawned { agent, .. } => Some(agent.clone()),
        Event::AgentMessage { agent, .. } => Some(agent.clone()),
        Event::AgentCompleted { agent, .. } => Some(agent.clone()),
        Event::SessionTitled { title, .. } => Some(title.clone()),
        Event::ToolRequested { tool, .. } => Some(tool.clone()),
        Event::ToolStarted { tool, args, .. } => Some(format!("{} {}", tool, redact(args))),
        Event::ToolOutput {
            tool, truncated, ..
        } => Some(if *truncated {
            format!("{tool} (truncated)")
        } else {
            tool.clone()
        }),
        Event::ToolCompleted { tool, call_id, .. } => {
            let ms = tool_starts
                .get(call_id)
                .map(|s| e.ts_ms.saturating_sub(*s))
                .unwrap_or(0);
            Some(format!("{tool} · {}", fmt_tool_ms(ms)))
        }
        Event::ScheduledTaskFailed { job_id, error, .. } => {
            Some(format!("{job_id}: {}", redact(&cap(error, 300))))
        }
        Event::ScheduledTaskRecovered { job_id, detail, .. } => {
            Some(format!("{job_id} recovered: {}", redact(&cap(detail, 300))))
        }
        _ => None,
    };
    serde_json::json!({"seq": e.seq, "ts_ms": e.ts_ms, "kind": kind, "detail": detail})
}

/// Duration of a tool call from the ledger's own timestamps:
/// ToolCompleted minus ToolStarted by call_id. `None` when either row is
/// missing — a call that never started (plan-mode refusal) or never
/// completed (killed mid-call, crash).
fn tool_duration_ms(
    call_id: &str,
    tool_starts: &HashMap<String, i64>,
    tool_ends: &HashMap<String, i64>,
) -> Option<i64> {
    match (tool_starts.get(call_id), tool_ends.get(call_id)) {
        (Some(s), Some(e)) => Some(e.saturating_sub(*s)),
        _ => None,
    }
}

/// One transcript message item: the wire fields (`type`, `role`,
/// `content`) plus the metadata the ledger already persists but the
/// old serialization dropped — `ts_ms`, the model's tool requests
/// (`tool_calls`), and the matching tool-result id (`tool_call_id`).
/// Tool arguments go through the same [`redact`] pass as message
/// content, so a secret pasted into a tool arg never reaches the
/// browser.
fn transcript_message(
    m: &pantheon_api::message::Message,
    tool_starts: &HashMap<String, i64>,
    tool_ends: &HashMap<String, i64>,
) -> serde_json::Value {
    let tool_calls: Vec<serde_json::Value> = m
        .tool_calls
        .iter()
        .map(|tc| {
            serde_json::json!({
                "id": tc.id,
                "name": tc.name,
                "arguments": redact(&tc.arguments),
                "started_ms": tool_starts.get(&tc.id).copied(),
                "duration_ms": tool_duration_ms(&tc.id, tool_starts, tool_ends),
            })
        })
        .collect();
    let mut item = serde_json::json!({
        "type": "message",
        "role": format!("{:?}", m.role).to_lowercase(),
        "content": redact(&m.content),
        "ts_ms": m.ts_ms,
        "tool_calls": tool_calls,
        "tool_call_id": m.tool_call_id,
    });
    // Tool-result rows carry the call duration at the top level too: the
    // assistant row above holds the request, the timing belongs to the
    // pair and shouldn't need a second lookup to find.
    if let Some(id) = &m.tool_call_id {
        item["duration_ms"] = serde_json::json!(tool_duration_ms(id, tool_starts, tool_ends));
    }
    item
}

/// Compact duration for tool timeline rows: `42ms`, `3.2s`.
fn fmt_tool_ms(ms: i64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Cap a timeline detail at `max_chars` characters (timeline rows must
/// stay glanceable; the full text lives in the ledger).
fn cap(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// `GET /api/runs/:id`: detail with transcript + redacted timeline.
pub fn detail(app: &App, run_id: &str) -> Response {
    match detail_value(app, run_id) {
        Ok(v) => json_ok(v),
        Err(r) => r,
    }
}

fn detail_value(app: &App, run_id: &str) -> Result<serde_json::Value, Response> {
    let ledger = ledger(app)?;
    let entries = match ledger.replay(run_id) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => return Err(err_json(404, "NOT_FOUND", "unknown run")),
        Err(e) => return Err(err_json(500, "LEDGER", &format!("replay: {e}"))),
    };
    let r = rollup(&entries);
    // Tool call timing comes from the ledger's own timestamps: match
    // each ToolCompleted to its ToolStarted by call_id. The transcript
    // carries the timing per tool call so clients don't have to walk the
    // timeline to find it; the timeline below reuses the same maps.
    let tool_starts: HashMap<String, i64> = entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::ToolStarted { call_id, .. } => Some((call_id.clone(), e.ts_ms)),
            _ => None,
        })
        .collect();
    let tool_ends: HashMap<String, i64> = entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::ToolCompleted { call_id, .. } => Some((call_id.clone(), e.ts_ms)),
            _ => None,
        })
        .collect();
    let transcript: Vec<serde_json::Value> = rebuild_transcript(entries.clone())
        .into_iter()
        .map(|item| match item {
            TranscriptItem::Message(m) => transcript_message(&m, &tool_starts, &tool_ends),
            TranscriptItem::Reasoning(t) => serde_json::json!({
                "type": "reasoning",
                "content": redact(&t),
            }),
        })
        .collect();
    let timeline: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| timeline_item(e, &tool_starts))
        .collect();
    let header = ledger
        .list_runs(MAX_RUNS_SCAN)
        .unwrap_or_default()
        .into_iter()
        .find(|(id, _, _, _, _, _, _)| id == run_id);
    let (status, created_ms, title, project, pinned, archived) = match header {
        Some((_, s, c, t, p, pin, arch)) => (s, c, t.unwrap_or_default(), p, pin, arch),
        None => ("unknown".to_string(), 0, String::new(), None, false, false),
    };
    let mut v = run_json(
        run_id,
        &status,
        created_ms,
        Some(&title),
        &r,
        project.as_deref(),
        pinned,
        archived,
    );
    v["transcript"] = serde_json::Value::Array(transcript);
    v["timeline"] = serde_json::Value::Array(timeline);
    // Richer run state the mobile app and dashboard need for a fully
    // interactive run: the unanswered ask_user question (if any), the
    // FIFO list of queued follow-ups (oldest first), the run's agent
    // mode, and the UsageRecorded token totals. NOTE: `queued_message`
    // used to be a single string (or null); it is now always an array
    // (possibly empty). App clients must parse it as a list.
    let pending_input = Supervisor::open(app.data_dir.clone())
        .ok()
        .and_then(|sup| sup.pending_input(run_id).ok())
        .and_then(|v| v.into_iter().next())
        .map(|(call_id, question, options)| {
            serde_json::json!({"call_id": call_id, "question": question, "options": options})
        })
        .unwrap_or(serde_json::Value::Null);
    let queued_message = serde_json::Value::Array(
        ledger
            .queued_messages(run_id)
            .unwrap_or_default()
            .into_iter()
            .map(serde_json::Value::String)
            .collect(),
    );
    let mode = ledger
        .run_mode(run_id)
        .unwrap_or_else(|_| DEFAULT_RUN_MODE.to_string());
    v["pending_input"] = pending_input;
    v["queued_message"] = queued_message;
    v["mode"] = serde_json::Value::String(mode);
    v["context_tokens"] = serde_json::json!({
        "input": r.input_tokens,
        "output": r.output_tokens,
        "total": r.input_tokens + r.output_tokens,
    });
    Ok(v)
}

/// `POST /api/runs`: start a new run (new chat) with the first user message.
///
/// Body: `{"message": "...", "title": "..."}` (`title` optional). The run
/// is admitted durably (RunStarted, plus SessionTitled when a title is
/// given) before the turn is handed to the real CLI path —
/// `pantheon run --taskID <id> --say <message> --deliver session` —
/// spawned as a subprocess, the same pattern the dashboard already uses
/// for schedule/gateway/reflect actions. The turn runs in the background;
/// the 201 carries the run in detail shape.
pub fn create(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if message.is_empty() {
        return bad_json("field \"message\" is required");
    }
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty());
    let run_id = new_run_id();
    // Admit the run durably before the turn starts, so the 201 below and
    // any immediate GET see it. The Supervisor is dropped before spawning:
    // two live writers on one ledger file block on the busy timeout.
    // Emitting the title here (rather than after the turn starts) also
    // suppresses the model's auto-titler, which skips titled runs.
    {
        let sup = match Supervisor::open(app.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
        };
        if let Err(e) = sup.start_run(&run_id) {
            return err_json(500, "RUNTIME", &format!("start run: {e}"));
        }
        if let Some(t) = title {
            let titled = Event::SessionTitled {
                run_id: run_id.clone(),
                title: bound_title(t, TITLE_MAX_CHARS),
                model: String::new(),
                source: "api".into(),
            };
            if let Err(e) = sup.emit(titled) {
                return err_json(500, "RUNTIME", &format!("title run: {e}"));
            }
        }
    }
    // The real turn path, not a reimplementation: the subprocess resolves
    // config, model policy, and secrets exactly as the terminal does. A
    // fresh run has no stored mode yet, so the ledger default ("build")
    // applies.
    let mode = match ledger(app) {
        Ok(l) => l
            .run_mode(&run_id)
            .unwrap_or_else(|_| DEFAULT_RUN_MODE.to_string()),
        Err(r) => return r,
    };
    // The turn child runs in its own process group so `POST
    // /api/runs/:id/kill` can hard-stop it; its PID is recorded for the
    // kill endpoint.
    if let Ok(pid) = spawn_turn_child_with_stdin(
        &[
            "run",
            "--taskID",
            run_id.as_str(),
            "--say",
            "-",
            "--deliver",
            "session",
            "--mode",
            mode.as_str(),
        ],
        message.as_bytes(),
    ) {
        app.register_turn_child(&run_id, pid);
    }
    match detail_value(app, &run_id) {
        Ok(v) => created_json(v),
        Err(r) => r,
    }
}

/// `POST /api/runs/:id/message`: continue a chat by injecting a user
/// message into an existing run.
///
/// Body: `{"message": "..."}`. Terminal runs (completed/failed/canceled)
/// are reopened — those statuses end the latest turn, not the session.
/// 409 only for runs parked on approval (grant/deny first) and runs with
/// a turn already in flight. Otherwise the message goes to the same CLI
/// turn path as `create`, and the endpoint returns once the turn is
/// admitted.
pub fn send_message(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if message.is_empty() {
        return bad_json("field \"message\" is required");
    }
    // Optional file attachments (mobile app): resolve the upload ids to
    // absolute paths and append them to the message text so the agent can
    // read them with its file tools. Image uploads are validated here and
    // converted to picture parts by the runtime at turn start
    // (`attachment_images` in pantheon-api). Built before the
    // busy/queue/steer branching so attachments ride the queue and steer
    // paths too (both store message text via `set_queued_message`).
    let attachments: Vec<uploads::UploadInfo> = match body.get("attachments") {
        None => Vec::new(),
        Some(v) => {
            let ids = match v.as_array() {
                Some(a) => a,
                None => return bad_json("field \"attachments\" must be an array of upload ids"),
            };
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                let id = match id.as_str() {
                    Some(s) => s,
                    None => {
                        return bad_json("field \"attachments\" must be an array of upload ids")
                    }
                };
                match uploads::resolve(app, id) {
                    Some(info) => out.push(info),
                    None => {
                        return err_json(
                            400,
                            "UPLOAD_NOT_FOUND",
                            &format!("unknown upload id \"{id}\""),
                        )
                    }
                }
            }
            out
        }
    };
    // Vision attach-site validation: an upload claiming an image mime must
    // actually decode as a sendable image within the wire limits. Loud 400
    // here — at the attach site — not a silent drop and not a provider 400
    // mid-turn. The mime sniff re-derives from magic bytes; the stored mime
    // is client-supplied and never trusted.
    for info in &attachments {
        if info.mime.starts_with("image/") {
            if let Err(e) = uploads::image_part(info) {
                return err_json(400, "IMAGE_ATTACHMENT_INVALID", &e);
            }
        }
    }
    // Zip attachments are inflated server-side so the agent can read the
    // contents with its file tools; the archive itself stays attached too.
    // An extraction failure is reported inline in the message (not a 400):
    // the raw archive remains readable at its path either way.
    struct ZipListing {
        id: String,
        dest: PathBuf,
        result: Result<Vec<uploads::ExtractedEntry>, String>,
    }
    let mut zip_listings: Vec<ZipListing> = Vec::new();
    for info in &attachments {
        if uploads::is_zip_upload(info) {
            let dest = uploads::zip_extract_dir(app, &info.id);
            let result = uploads::extract_zip_upload(app, info);
            zip_listings.push(ZipListing {
                id: info.id.clone(),
                dest,
                result,
            });
        }
    }
    let message = if attachments.is_empty() {
        message.to_string()
    } else {
        let mut block = String::from("\n\n[attachments]\n");
        for info in &attachments {
            block.push_str(&format!(
                "- {} ({}, {}, id: {}): {}\n",
                info.name,
                info.mime,
                uploads::human_size(info.size_bytes),
                info.id,
                info.path.display()
            ));
            if let Some(z) = zip_listings.iter().find(|z| z.id == info.id) {
                match &z.result {
                    Ok(entries) => {
                        block.push_str(&format!(
                            "  unzipped {} file{} to {}:\n",
                            entries.len(),
                            if entries.len() == 1 { "" } else { "s" },
                            z.dest.display()
                        ));
                        for e in entries.iter().take(uploads::MAX_ZIP_LISTED) {
                            block.push_str(&format!(
                                "  - {} ({}): {}\n",
                                e.rel_path.display(),
                                uploads::human_size(e.size_bytes),
                                z.dest.join(&e.rel_path).display()
                            ));
                        }
                        if entries.len() > uploads::MAX_ZIP_LISTED {
                            block.push_str(&format!(
                                "  - … and {} more\n",
                                entries.len() - uploads::MAX_ZIP_LISTED
                            ));
                        }
                    }
                    Err(e) => {
                        block.push_str(&format!(
                            "  zip extraction failed ({e}); the raw archive is attached above.\n"
                        ));
                    }
                }
            }
        }
        block.push_str("The agent can read these files with its file tools. Image attachments are also sent to the model as pictures. Video attachments are analyzed by the video model (native input when supported, otherwise keyframes) and the description is injected before the turn runs.");
        format!("{message}{block}")
    };
    if let Some(r) = reject_overlong_queue(&message) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let known = match ledger.replay(run_id) {
        Ok(e) => !e.is_empty(),
        Err(e) => return err_json(500, "LEDGER", &format!("replay: {e}")),
    };
    if !known {
        return err_json(404, "NOT_FOUND", "unknown run");
    }
    // Terminal statuses (completed/failed/canceled) only mark the end of
    // the latest turn: `chat_turn` reopens the run and rebuilds the
    // transcript from the ledger, so a settled session keeps talking.
    // Only a parked run (awaiting approval) refuses a new turn until it
    // is granted or denied.
    match ledger.status(run_id).unwrap_or(None).as_deref() {
        Some("awaiting_approval") => {
            return conflict(
                "RUN_PARKED",
                &format!("run {run_id} is parked on approval; grant or deny it first"),
            )
        }
        _ => {}
    }
    // Serialize turn-starting requests per run: the busy-check → drain →
    // spawn sequence below must be atomic, or two concurrent POSTs both
    // see idle, both spawn, and the loser's turn dies with RT_LEASE_BUSY
    // after we already returned 200 — silently dropping its message.
    let send_lock = app.send_guard(run_id);
    let _send_guard = send_lock.lock().unwrap_or_else(|e| e.into_inner());
    // One turn at a time per run: without this the spawned turn would
    // fail fast with RT_LEASE_BUSY after we already returned 200.
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    let busy = sup.has_active_lease(run_id).unwrap_or(false);
    // One-slot queue/steer, matching the TUI's Ctrl+Enter interrupt and
    // follow-up queue: a busy run can take a `queue: true` follow-up
    // (newer replaces the older slot) or a `steer: true` interrupt that
    // cancels the in-flight turn and promotes the message to the front.
    // A busy run with neither still 409s as before.
    let mode = ledger
        .run_mode(run_id)
        .unwrap_or_else(|_| DEFAULT_RUN_MODE.to_string());
    if busy {
        let queue = body.get("queue").and_then(|v| v.as_bool()).unwrap_or(false);
        let steer = body.get("steer").and_then(|v| v.as_bool()).unwrap_or(false);
        if steer {
            match sup.cancel_run_intent(run_id, "steered from dashboard") {
                Ok(()) => {}
                Err(e) if e.code == "RT_TERMINAL" => {
                    return conflict("RT_TERMINAL", &format!("{e}"))
                }
                Err(e) => return err_json(500, "RUNTIME", &format!("cancel: {e}")),
            }
            // The in-flight turn winds down cooperatively and releases its
            // lease; the steered message waits in the queue for the next
            // turn instead of racing the dying one for the lease.
            // Steer is a redirection, not an append: the queue is cleared
            // first so anything queued before the steer doesn't outlive
            // the redirection and fire afterwards.
            if let Err(e) = ledger.set_queued_message(run_id, None) {
                return err_json(500, "LEDGER", &format!("clear queue: {e}"));
            }
            if let Err(e) = ledger.set_queued_message(run_id, Some(message.as_str())) {
                return err_json(500, "LEDGER", &format!("queue message: {e}"));
            }
            return Response::accepted_json(
                serde_json::json!({"ok": true, "steered": true, "queued": true, "run_id": run_id})
                    .to_string(),
            );
        }
        if queue {
            if let Err(e) = ledger.set_queued_message(run_id, Some(message.as_str())) {
                return err_json(500, "LEDGER", &format!("queue message: {e}"));
            }
            return Response::accepted_json(
                serde_json::json!({"ok": true, "queued": true, "run_id": run_id}).to_string(),
            );
        }
        return conflict(
            "TURN_IN_FLIGHT",
            &format!("run {run_id} already has a turn running; wait for it to finish"),
        );
    }
    // Idle: drain the queue head into this turn. A message queued (or
    // steered) while the run was busy was already accepted with 202 — it
    // must ride the next turn as the earliest input, not vanish.
    //
    // Peek, don't pop: the head is only removed after the child is
    // confirmed alive (lease observed). If the spawn fails, the head
    // stays queued for the next attempt instead of vanishing with the
    // dead `--say`.
    let drained = ledger
        .queued_messages(run_id)
        .unwrap_or_default()
        .into_iter()
        .next();
    let say = compose_idle_say(drained.as_deref(), &message);
    match spawn_turn_child_with_stdin(
        &[
            "run",
            "--taskID",
            run_id,
            "--say",
            "-",
            "--deliver",
            "session",
            "--mode",
            mode.as_str(),
        ],
        say.as_bytes(),
    ) {
        Ok(pid) => app.register_turn_child(run_id, pid),
        Err(e) => return err_json(500, "SPAWN", &format!("failed to start turn: {e}")),
    }
    // The child takes the lease asynchronously after spawn. Wait for it
    // (bounded) while still holding the send guard, so a racing second
    // POST observes `busy` instead of spawning a doomed twin. Skipped in
    // unit tests, where the spawn is a no-op and no lease ever appears.
    #[cfg(not(test))]
    if !wait_for_turn_lease(&sup, run_id) {
        return err_json(500, "SPAWN", "turn process failed to start");
    }
    if drained.is_some() {
        let _ = ledger.take_queued_message(run_id);
    }
    // Idle: the turn is accepted for processing, not complete — 202 like
    // every other accepted turn (queued/steered paths above).
    Response::accepted_json(serde_json::json!({"ok": true, "run_id": run_id}).to_string())
}

/// Wait (bounded) for a freshly spawned turn to take the run's lease.
/// Closes the spawn→lease race window: while the caller holds the run's
/// send guard, a second request must observe `busy` once the child is up
/// instead of spawning a twin that dies with RT_LEASE_BUSY.
#[cfg(not(test))]
fn wait_for_turn_lease(sup: &Supervisor, run_id: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if sup.has_active_lease(run_id).unwrap_or(false) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return sup.has_active_lease(run_id).unwrap_or(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Combine a drained queue head with an idle-path message into the
/// `--say` text for the next turn. The drained message rides first;
/// when the client already sent the same text as the message itself
/// (the app echoes the oldest queued text when the run looks idle),
/// it is not duplicated.
fn compose_idle_say(drained: Option<&str>, message: &str) -> String {
    match drained {
        Some(queued) if queued != message => format!("{queued}\n\n{message}"),
        _ => message.to_string(),
    }
}

/// `POST /api/runs/:id/retry`: re-run the last settled turn from its
/// recorded input. Fail-closed: 409 when a turn is in flight or the run
/// is parked on approval; 404 on an unknown run; 400 when the ledger
/// holds no user prompt to retry (turns that predate prompt recording,
/// or runs that never took user input).
pub fn retry_turn(app: &App, run_id: &str) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let entries = match ledger.replay(run_id) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => return err_json(404, "NOT_FOUND", "unknown run"),
        Err(e) => return err_json(500, "LEDGER", &format!("replay: {e}")),
    };
    match ledger.status(run_id).unwrap_or(None).as_deref() {
        Some("awaiting_approval") => {
            return conflict(
                "RUN_PARKED",
                &format!("run {run_id} is parked on approval; grant or deny it first"),
            )
        }
        _ => {}
    }
    let send_lock = app.send_guard(run_id);
    let _send_guard = send_lock.lock().unwrap_or_else(|e| e.into_inner());
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    if sup.has_active_lease(run_id).unwrap_or(false) {
        return conflict(
            "TURN_IN_FLIGHT",
            &format!("run {run_id} already has a turn running; wait for it to finish"),
        );
    }
    // The last turn's input: the `UserMessage` row new turns record, or —
    // for imported sessions — the last user-role message in the
    // transcript. Either way the client never resends the text.
    let from_event = entries
        .iter()
        .rev()
        .filter_map(|e| match &e.event {
            Event::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .find(|t| !t.trim().is_empty());
    let say = match from_event {
        Some(s) => Some(s),
        None => rebuild_transcript(entries)
            .into_iter()
            .rev()
            .filter_map(|item| match item {
                TranscriptItem::Message(m) if m.role == Role::User => Some(m.content),
                _ => None,
            })
            .find(|t| !t.trim().is_empty()),
    };
    let say = match say {
        Some(s) => s,
        None => return bad_json("nothing to retry: the run has no recorded user prompt"),
    };
    let mode = ledger
        .run_mode(run_id)
        .unwrap_or_else(|_| DEFAULT_RUN_MODE.to_string());
    // The retried input is idempotent text: a failed spawn just 500s and
    // nothing is drained or lost. Still hold the guard across spawn so a
    // racing `send_message` sees the new lease, not a phantom idle.
    match spawn_turn_child_with_stdin(
        &[
            "run",
            "--taskID",
            run_id,
            "--say",
            "-",
            "--deliver",
            "session",
            "--mode",
            mode.as_str(),
        ],
        say.as_bytes(),
    ) {
        Ok(pid) => app.register_turn_child(run_id, pid),
        Err(e) => return err_json(500, "SPAWN", &format!("failed to start turn: {e}")),
    }
    #[cfg(not(test))]
    if !wait_for_turn_lease(&sup, run_id) {
        return err_json(500, "SPAWN", "turn process failed to start");
    }
    Response::accepted_json(
        serde_json::json!({"ok": true, "retried": true, "run_id": run_id}).to_string(),
    )
}

/// `GET /api/runs/:id/export?format=json|markdown`: download the
/// transcript. Redacted the same way as the detail view.
pub fn export(app: &App, req: &Request, run_id: &str) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let entries = match ledger.replay(run_id) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => return err_json(404, "NOT_FOUND", "unknown run"),
        Err(e) => return err_json(500, "LEDGER", &format!("replay: {e}")),
    };
    let items = rebuild_transcript(entries);
    let format = req
        .query
        .get("format")
        .map(String::as_str)
        .unwrap_or("json");
    match format {
        "markdown" | "md" => {
            let mut md = format!("# Run {run_id}\n\n");
            for item in items {
                match item {
                    TranscriptItem::Message(m) => {
                        // Image parts render as `[image: ...]` notes: the
                        // export is text, so pictures are named, never
                        // silently dropped.
                        let body = m.text_with_image_notes();
                        md.push_str(&format!("## {:?}\n\n{}\n\n", m.role, redact(&body)));
                    }
                    TranscriptItem::Reasoning(t) => {
                        md.push_str(&format!("## Reasoning\n\n{}\n\n", redact(&t)));
                    }
                }
            }
            Response::download(
                &format!("run-{run_id}.md"),
                "text/markdown; charset=utf-8",
                md.into_bytes(),
            )
        }
        _ => {
            let arr: Vec<serde_json::Value> = items
                .into_iter()
                .map(|item| match item {
                    TranscriptItem::Message(m) => serde_json::json!({
                        "type": "message",
                        "role": format!("{:?}", m.role).to_lowercase(),
                        // Same `[image: ...]` notes as the markdown export:
                        // JSON is text too, so pictures are named, not dropped.
                        "content": redact(&m.text_with_image_notes()),
                    }),
                    TranscriptItem::Reasoning(t) => serde_json::json!({
                        "type": "reasoning",
                        "content": redact(&t),
                    }),
                })
                .collect();
            let body = serde_json::to_string_pretty(&serde_json::json!({
                "run_id": run_id,
                "transcript": arr,
            }))
            .unwrap_or_default();
            Response::download(
                &format!("run-{run_id}.json"),
                "application/json",
                body.into_bytes(),
            )
        }
    }
}

/// `DELETE /api/runs/:id?confirm=true`: honest delete from the ledger.
/// The UI confirms first; the endpoint requires the flag too, so a
/// stray link or prefetch can never prune a run.
pub fn prune(app: &App, req: &Request, run_id: &str) -> Response {
    if req.query.get("confirm").map(String::as_str) != Some("true") {
        return bad_json("prune requires ?confirm=true");
    }
    // The home session can never be deleted.
    if run_id == HOME_SESSION_ID {
        return err_json(
            403,
            "HOME_PROTECTED",
            "the home session can never be deleted",
        );
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match ledger.delete_run(run_id) {
        Ok((events, rows)) if rows > 0 => {
            json_ok(serde_json::json!({"ok": true, "events_deleted": events}))
        }
        Ok(_) => err_json(404, "NOT_FOUND", "unknown run"),
        Err(e) => err_json(500, "LEDGER", &format!("delete: {e}")),
    }
}

/// Non-empty replay or a ledger error, mapped to the shared responses.
fn known_run(app: &App, run_id: &str) -> Result<(), Response> {
    let ledger = ledger(app)?;
    match ledger.replay(run_id) {
        Ok(e) if !e.is_empty() => Ok(()),
        Ok(_) => Err(err_json(404, "NOT_FOUND", "unknown run")),
        Err(e) => Err(err_json(500, "LEDGER", &format!("replay: {e}"))),
    }
}

/// `POST /api/runs/:id/cancel`: interrupt the run's in-flight turn.
/// Cooperative: the turn stops at its next checkpoint. 404 on unknown
/// runs; a terminal run (completed/failed) 409s with RT_TERMINAL — there
/// is no turn left to cancel.
pub fn cancel_run(app: &App, run_id: &str) -> Response {
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    match sup.cancel_run_intent(run_id, "interrupted from dashboard") {
        Ok(()) => json_ok(serde_json::json!({"ok": true})),
        Err(e) if e.code == "RT_TERMINAL" => conflict("RT_TERMINAL", &format!("{e}")),
        Err(e) if e.code == "RT_NO_RUN" => err_json(404, "NOT_FOUND", "unknown run"),
        Err(e) => err_json(500, "RUNTIME", &format!("cancel: {e}")),
    }
}

/// `POST /api/runs/:id/kill`: hard-stop the run's in-flight turn.
///
/// Unlike `/cancel` (cooperative — the turn winds down at its next
/// checkpoint), this force-terminates the turn child process: the PID
/// recorded at spawn is identity-checked against `/proc` (Linux) and
/// then TERM → grace → KILLed as a process group. The cancel intent is
/// recorded first so the ledger stays honest, and the dead child's
/// lease expires on its own.
///
/// Only kills turns spawned by this dashboard process: 409 when no
/// turn PID was recorded for the run, or the PID no longer identifies
/// this run's turn child. 404 on unknown runs.
pub fn kill_run(app: &App, run_id: &str) -> Response {
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let pid = match app.turn_child_pid(run_id) {
        Some(pid) => pid,
        None => {
            return conflict(
                "NO_TURN_IN_FLIGHT",
                &format!("run {run_id} has no recorded turn process"),
            )
        }
    };
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    // Synchronous: TERM → grace → KILL can block up to the grace
    // period, but the response then reports the recorded outcome —
    // the client knows the turn is actually dead, not just dying.
    match sup.kill_run_turn(run_id, pid) {
        Ok(()) => {
            app.forget_turn_child(run_id);
            json_ok(serde_json::json!({"ok": true, "killed": true}))
        }
        Err(e) => {
            // Stale or foreign PIDs are forgotten so a later kill
            // doesn't retry a PID that was never ours.
            if matches!(e.code.as_str(), "RT_NO_TURN" | "RT_TERMINAL") {
                app.forget_turn_child(run_id);
            }
            match e.code.as_str() {
                "RT_NO_TURN" => conflict("NO_TURN_IN_FLIGHT", &format!("{e}")),
                "RT_TERMINAL" => conflict("RT_TERMINAL", &format!("{e}")),
                "RT_NO_RUN" => err_json(404, "NOT_FOUND", "unknown run"),
                _ => err_json(500, "RUNTIME", &format!("kill: {e}")),
            }
        }
    }
}

/// `DELETE /api/runs/:id/queue`: drop the run's queued follow-up
/// messages (the whole FIFO list) without touching the in-flight turn.
pub fn clear_queue(app: &App, run_id: &str) -> Response {
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match ledger.set_queued_message(run_id, None) {
        Ok(()) => json_ok(serde_json::json!({"ok": true, "queue": []})),
        Err(e) => err_json(500, "LEDGER", &format!("clear queue: {e}")),
    }
}

fn parse_queue_index(idx: &str) -> Option<usize> {
    idx.parse::<usize>().ok()
}

fn queue_json(ledger: &Ledger, run_id: &str) -> serde_json::Value {
    let q: Vec<serde_json::Value> = ledger
        .queued_messages(run_id)
        .unwrap_or_default()
        .into_iter()
        .map(serde_json::Value::String)
        .collect();
    serde_json::Value::Array(q)
}

/// `DELETE /api/runs/:id/queue/:index`: delete one queued message by
/// index (0 = oldest), e.g. when the user removes a queued item in the
/// app before it is sent. 404 when the run is unknown or the index is
/// out of range.
pub fn delete_queue_item(app: &App, run_id: &str, idx: &str) -> Response {
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let index = match parse_queue_index(idx) {
        Some(i) => i,
        None => return err_json(404, "QUEUE_INDEX", "queue index out of range"),
    };
    match ledger.remove_queued_at(run_id, index) {
        Ok(true) => json_ok(serde_json::json!({"ok": true, "queue": queue_json(&ledger, run_id)})),
        Ok(false) => err_json(404, "QUEUE_INDEX", "queue index out of range"),
        Err(e) => err_json(500, "LEDGER", &format!("delete queued: {e}")),
    }
}

/// `PATCH /api/runs/:id/queue/:index` with body `{"text": "..."}`:
/// edit a queued message's text in place (position and FIFO order are
/// preserved). 404 when the index is out of range, 422 when the new
/// text is empty.
pub fn update_queue_item(app: &App, run_id: &str, idx: &str, req: &Request) -> Response {
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let text = body.get("text").and_then(|t| t.as_str()).unwrap_or("");
    if text.trim().is_empty() {
        return err_json(422, "QUEUE_EMPTY", "queued message text must not be empty");
    }
    if let Some(r) = reject_overlong_queue(text) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let index = match parse_queue_index(idx) {
        Some(i) => i,
        None => return err_json(404, "QUEUE_INDEX", "queue index out of range"),
    };
    match ledger.update_queued_at(run_id, index, text) {
        Ok(true) => json_ok(serde_json::json!({"ok": true, "queue": queue_json(&ledger, run_id)})),
        Ok(false) => err_json(404, "QUEUE_INDEX", "queue index out of range"),
        Err(e) if e.code == "QUEUE_EMPTY" => err_json(422, "QUEUE_EMPTY", &format!("{e}")),
        Err(e) => err_json(500, "LEDGER", &format!("update queued: {e}")),
    }
}

/// `GET /api/runs/:id/todos`: the run's todo snapshot, serde as-is.
pub fn get_todos(app: &App, run_id: &str) -> Response {
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    match sup.load_todos(run_id) {
        Ok(items) => json_ok(serde_json::json!({"run_id": run_id, "todos": items})),
        Err(e) => err_json(500, "LEDGER", &format!("load todos: {e}")),
    }
}

/// `PUT /api/runs/:id/todos`: replace the run's todo snapshot. Body:
/// `{"todos": [...]}`. Enforces the same invariants the todo tool does —
/// non-empty content, at most one in progress — 400 otherwise.
pub fn put_todos(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let items: Vec<TodoItem> = match body.get("todos") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(items) => items,
            Err(e) => return bad_json(&format!("field \"todos\" is not a todo list: {e}")),
        },
        None => return bad_json("field \"todos\" is required"),
    };
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let mut list = TodoList::default();
    if let Err(e) = list.replace(items.clone()) {
        return bad_json(&format!("invalid todos: {e}"));
    }
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    match sup.save_todos(run_id, &items) {
        Ok(()) => json_ok(serde_json::json!({"ok": true, "todos": items})),
        Err(e) => err_json(500, "LEDGER", &format!("save todos: {e}")),
    }
}

/// `POST /api/runs/:id/compress`: run the transcript compression fit
/// now and report what changed. Built on the same Session the TUI
/// resumes (coder policy, resolved model policy, secrets), so the fit
/// behaves exactly like the terminal's `/compress`.
pub fn compress(app: &App, run_id: &str) -> Response {
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let session = match crate::session_factory::open_maintenance_session(&app.data_dir) {
        Ok(s) => s,
        Err(e) => return err_json(500, "CONFIG", &e),
    };
    match session.compress_now(run_id) {
        Ok(report) => json_ok(serde_json::json!({
            "ok": true,
            "run_id": run_id,
            "before": report.before,
            "after": report.after,
            "changed": report.changed,
            "unknown_window": report.unknown_window,
        })),
        Err(e) => err_json(500, "COMPRESSION", &format!("{e}")),
    }
}

/// `POST /api/runs/:id/fork`: duplicate the run into a new run ID.
/// Body: `{"turn": n}` — fork from the start of turn `n`; absent forks
/// the whole run. 201 with the new ID and the copied turn count.
pub fn fork(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let turn = body
        .get("turn")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize);
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    match sup.fork_run(run_id, turn) {
        Ok((new_id, turns_copied)) => {
            // Carry the FIFO queue into the fork: queued follow-ups belong
            // to the conversation, and a fork that drops them silently
            // loses messages the client already accepted (202). Order is
            // preserved oldest-first.
            let ledger = match ledger(app) {
                Ok(l) => l,
                Err(r) => return r,
            };
            for queued in ledger.queued_messages(run_id).unwrap_or_default() {
                if let Err(e) = ledger.set_queued_message(&new_id, Some(&queued)) {
                    return err_json(500, "LEDGER", &format!("carry queue into fork: {e}"));
                }
            }
            created_json(serde_json::json!({
                "ok": true,
                "run_id": new_id,
                "turns_copied": turns_copied,
            }))
        }
        // No turns to fork, or the turn number is out of range: the
        // request is wrong, not the server.
        Err(e) if e.code == "RT_FORK_EMPTY" || e.code == "RT_FORK_RANGE" => {
            bad_json(&format!("{e}"))
        }
        Err(e) => err_json(500, "RUNTIME", &format!("fork: {e}")),
    }
}

/// `PUT /api/runs/:id/title`: rename the run. Empty titles 400; the
/// event follows the same shape the create path emits.
pub fn set_title(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if title.is_empty() {
        return bad_json("field \"title\" must be non-empty");
    }
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    let titled = Event::SessionTitled {
        run_id: run_id.to_string(),
        title: bound_title(title, TITLE_MAX_CHARS),
        model: String::new(),
        source: "api".into(),
    };
    match sup.emit(titled) {
        Ok(()) => json_ok(serde_json::json!({"ok": true, "title": title})),
        Err(e) => err_json(500, "RUNTIME", &format!("title run: {e}")),
    }
}

/// `POST /api/runs/:id/mode`: persist the run's agent mode (`plan` or
/// `build`). New turns spawned from this run inherit it.
pub fn set_mode(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let mode = body
        .get("mode")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if mode != PLAN_RUN_MODE && mode != DEFAULT_RUN_MODE {
        return bad_json("field \"mode\" must be \"plan\" or \"build\"");
    }
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match ledger.set_run_mode(run_id, mode) {
        Ok(()) => json_ok(serde_json::json!({"ok": true, "mode": mode})),
        Err(e) => err_json(500, "LEDGER", &format!("set mode: {e}")),
    }
}

/// `POST /api/runs/:id/pin`: set the operator-pinned flag.
/// Body: `{"pinned": true|false}`. Pinning is a visual convenience for
/// keeping an important run at hand; unlike archiving it never hides
/// the run from the list.
pub fn set_pin(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let pinned = match body.get("pinned").and_then(|v| v.as_bool()) {
        Some(p) => p,
        None => return bad_json("field \"pinned\" must be a boolean"),
    };
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match ledger.set_run_pinned(run_id, pinned) {
        Ok(()) => json_ok(serde_json::json!({"ok": true, "pinned": pinned})),
        Err(e) => err_json(500, "LEDGER", &format!("pin run: {e}")),
    }
}

/// `POST /api/runs/:id/archive`: archive (`{"archived": true}`) or
/// restore (`{"archived": false}`) a run. Archived runs stay in the
/// ledger and the detail view; they are just excluded from the run
/// list unless `?include_archived=1` is passed. Pruning still deletes
/// them permanently.
pub fn set_archive(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let archived = match body.get("archived").and_then(|v| v.as_bool()) {
        Some(a) => a,
        None => return bad_json("field \"archived\" must be a boolean"),
    };
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match ledger.set_run_archived(run_id, archived) {
        Ok(()) => json_ok(serde_json::json!({"ok": true, "archived": archived})),
        Err(e) => err_json(500, "LEDGER", &format!("archive run: {e}")),
    }
}

/// `POST /api/runs/:id/project`: assign the run to a named project
/// (`{"project": "name"}`), or unassign it (`{"project": null}` or
/// `{"project": ""}`). Projects are operator-created buckets — a run
/// belongs to at most one.
pub fn set_project(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let project = match body.get("project") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => match v.as_str().map(str::trim) {
            Some(s) if !s.is_empty() => Some(s),
            _ => return bad_json("field \"project\" must be a string or null"),
        },
    };
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    match ledger.set_run_project(run_id, project) {
        Ok(()) => json_ok(serde_json::json!({"ok": true, "project": project})),
        Err(e) => err_json(500, "LEDGER", &format!("set project: {e}")),
    }
}

/// `GET /api/projects`: every named project in use, most-recently-active
/// first, each with the run ids currently assigned to it. Derived from
/// the runs table, like everything else.
pub fn projects(app: &App) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let names = match ledger.list_projects() {
        Ok(n) => n,
        Err(e) => return err_json(500, "LEDGER", &format!("list projects: {e}")),
    };
    let mut members: HashMap<String, Vec<String>> = HashMap::new();
    for (id, _, _, _, project, _, _) in ledger.list_runs(MAX_RUNS_SCAN).unwrap_or_default() {
        if let Some(p) = project {
            members.entry(p).or_default().push(id);
        }
    }
    let out: Vec<serde_json::Value> = names
        .into_iter()
        .map(|name| {
            let runs = members.remove(&name).unwrap_or_default();
            serde_json::json!({"name": name, "runs": runs})
        })
        .collect();
    json_ok(serde_json::json!({"projects": out}))
}

/// `POST /api/runs/:id/input`: answer an `ask_user` question. Body:
/// `{"call_id": "...", "answer": "..."}` (both required). Records the
/// answer and resumes the turn with the run's stored mode. 409 for an
/// unknown or already-answered question, or a turn already in flight.
pub fn answer_input(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let call_id = body
        .get("call_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let answer = body
        .get("answer")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if call_id.is_empty() || answer.is_empty() {
        return bad_json("fields \"call_id\" and \"answer\" are required and must be non-empty");
    }
    if let Err(r) = known_run(app, run_id) {
        return r;
    }
    let sup = match Supervisor::open(app.data_dir.clone()) {
        Ok(s) => s,
        Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
    };
    // One turn at a time per run: without this the spawned `--resume`
    // would fail fast with RT_LEASE_BUSY after we already returned 200.
    // (A run parked awaiting input holds no lease — the turn ended when
    // it parked — so answering a real question is unaffected.)
    if sup.has_active_lease(run_id).unwrap_or(false) {
        return conflict(
            "TURN_IN_FLIGHT",
            &format!("run {run_id} already has a turn running; wait for it to finish"),
        );
    }
    if let Err(e) = sup.answer_input(run_id, call_id, answer) {
        return match e.code.as_str() {
            "RT_INPUT_UNKNOWN" => conflict("INPUT_UNKNOWN", &format!("{e}")),
            "RT_INPUT_RESOLVED" => conflict("INPUT_RESOLVED", &format!("{e}")),
            _ => err_json(500, "RUNTIME", &format!("answer input: {e}")),
        };
    }
    let mode = sup
        .ledger_run_mode(run_id)
        .unwrap_or_else(|_| DEFAULT_RUN_MODE.to_string());
    drop(sup);
    // Own process group so the resumed turn is hard-stoppable too.
    if let Ok(pid) = spawn_turn_child(&[
        "run",
        "--taskID",
        run_id,
        "--resume",
        "--deliver",
        "session",
        "--mode",
        mode.as_str(),
    ]) {
        app.register_turn_child(run_id, pid);
    }
    json_ok(serde_json::json!({"ok": true}))
}
