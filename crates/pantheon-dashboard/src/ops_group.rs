//! ops_group group: approvals, runs, schedule, stats, logs in one file.

pub mod approvals {
    //! Approval queue: the real [`pantheon_runtime::Supervisor`] decision path.
    //!
    //! Scopes are `call_id:tool:args` (args may contain colons); the approval
    //! id in the URL is the exact scope string, percent-decoded per path
    //! segment by the router. Tool arguments are redacted before display.

    use crate::{conflict, err_json, json_ok, spawn_pantheon, App};
    use pantheon_api::logging::redact;
    use pantheon_gateway::http::Response;
    use pantheon_runtime::Supervisor;
    use pantheon_storage::Ledger;

    /// Split `call_id:tool:args`. Documented scope shape; args keep their
    /// colons via `splitn(3, ...)`.
    fn split_scope(scope: &str) -> (&str, &str, &str) {
        let mut parts = scope.splitn(3, ':');
        (
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
            parts.next().unwrap_or(""),
        )
    }

    /// `GET /api/approvals`: every pending approval across runs.
    pub fn list(app: &App) -> Response {
        let ledger = match Ledger::open(&app.data_dir.join("ledger.db")) {
            Ok(l) => l,
            Err(e) => return err_json(500, "LEDGER", &format!("open ledger: {e}")),
        };
        let supervisor = match Supervisor::open(app.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => return err_json(500, "SUPERVISOR", &format!("open supervisor: {e}")),
        };
        // Fail closed, never fail empty: a list_runs or replay failure used to
        // degrade to an empty approval queue, which reads to the operator as
        // "nothing needs approval" while a parked run is actually waiting.
        // A 500 says the queue itself is unreadable, which is the truth.
        let runs = match ledger.list_runs(10_000) {
            Ok(r) => r,
            Err(e) => return err_json(500, "LEDGER", &format!("list runs: {e}")),
        };
        let mut out = Vec::new();
        for (run_id, status, created_ms, title, ..) in runs {
            if status != "awaiting_approval" {
                continue;
            }
            let pending = match supervisor.pending_approvals(&run_id) {
                Ok(p) => p,
                Err(e) => return err_json(500, "LEDGER", &format!("pending approvals: {e}")),
            };
            for scope in pending {
                let (call_id, tool, args) = split_scope(&scope);
                out.push(serde_json::json!({
                    "id": scope,
                    "run_id": run_id,
                    "run_title": title,
                    "run_created_ms": created_ms,
                    "call_id": call_id,
                    "tool": tool,
                    "args": redact(args),
                }));
            }
        }
        json_ok(serde_json::json!({"approvals": out}))
    }

    /// `POST /api/approvals/:id/grant|deny`: the durable decision, then the
    /// optional TUI resume callback (same path as the CLI's out-of-band
    /// grant). The ledger flips the run back to `running` on the last
    /// resolved approval by itself.
    pub fn decide(app: &App, scope: &str, granted: bool) -> Response {
        let ledger = match Ledger::open(&app.data_dir.join("ledger.db")) {
            Ok(l) => l,
            Err(e) => return err_json(500, "LEDGER", &format!("open ledger: {e}")),
        };
        let supervisor = match Supervisor::open(app.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => return err_json(500, "SUPERVISOR", &format!("open supervisor: {e}")),
        };
        // Find the run holding this scope. Scopes are unique per pending set;
        // the first match wins.
        let mut run_id: Option<String> = None;
        for (id, status, ..) in ledger.list_runs(10_000).unwrap_or_default() {
            if status != "awaiting_approval" {
                continue;
            }
            if supervisor
                .pending_approvals(&id)
                .unwrap_or_default()
                .iter()
                .any(|s| s == scope)
            {
                run_id = Some(id);
                break;
            }
        }
        let run_id = match run_id {
            Some(id) => id,
            None => return err_json(404, "NOT_FOUND", "no pending approval with that id"),
        };
        let result = if granted {
            supervisor.grant(&run_id, scope)
        } else {
            supervisor.deny(&run_id, scope)
        };
        if let Err(e) = result {
            // An expired park is a client-timing problem, not a server
            // failure: the request went stale, so the operator sends a new
            // message and the agent re-requests.
            if e.code == "RT_APPROVAL_EXPIRED" {
                return conflict(
                    "APPROVAL_EXPIRED",
                    "approval expired; send a new message to have the agent request approval again",
                );
            }
            return err_json(500, "DECISION", &format!("record decision: {e}"));
        }
        if let Some(cb) = &app.on_approval {
            cb(&run_id, granted);
        }
        // The ledger flips the run back to `running` when the last pending
        // approval resolves, but no turn is alive to continue it - without a
        // resume the run sits in "running" forever. Spawn one the way
        // `answer_input` does (a denial also resumes: the denied tool call
        // becomes a tool result the agent must react to). Guarded and
        // lease re-checked so a turn started in the gap is never doubled.
        if supervisor
            .pending_approvals(&run_id)
            .unwrap_or_default()
            .is_empty()
        {
            let send_lock = app.send_guard(&run_id);
            let _send_guard = send_lock.lock().unwrap_or_else(|e| e.into_inner());
            if !supervisor.has_active_lease(&run_id).unwrap_or(false) {
                let mode = supervisor
                    .ledger_run_mode(&run_id)
                    .unwrap_or_else(|_| crate::runs::DEFAULT_RUN_MODE.to_string());
                if let Err(e) = spawn_pantheon(&[
                    "run",
                    "--taskID",
                    run_id.as_str(),
                    "--resume",
                    "--deliver",
                    "session",
                    "--mode",
                    mode.as_str(),
                ]) {
                    // The decision above is already recorded, but without
                    // a resume turn the run sits in "running" forever - so
                    // a spawn failure is a 500, not an ok:true that hides
                    // it. The message says the decision landed, so the
                    // operator knows a retry must resume, not re-decide.
                    return err_json(
                        500,
                        "SPAWN",
                        &format!("approval recorded but resume turn failed to start: {e}"),
                    );
                }
            }
        }
        json_ok(serde_json::json!({
            "ok": true,
            "run_id": run_id,
            "granted": granted,
        }))
    }
}

pub mod runs {
    //! Runs, overview, transcript export, and prune.
    //!
    //! All reads come from the real event ledger (`pantheon_storage::Ledger`);
    //! the transcript is rebuilt with `pantheon_runtime::session`'s own
    //! functions, so the dashboard shows exactly what the TUI would.

    use crate::util::{now_ms, round_cost_usd};
    use crate::{
        bad_json, body_json, conflict, created_json, err_json, json_ok, query_usize,
        spawn_turn_child, spawn_turn_child_with_stdin, uploads, App,
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
    use std::collections::{HashMap, HashSet};
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
                Event::RunCompleted { .. }
                | Event::RunFailed { .. }
                | Event::RunCanceled { .. } => {
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
                Event::TurnStarted { .. } => Some("Working...".to_string()),
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

    /// The two pin/archive list flags rendered into run JSON. Bundled so
    /// `run_json` stays under the arg-count lint as the shape grows.
    #[derive(Clone, Copy, Default)]
    struct RunFlags {
        pinned: bool,
        archived: bool,
    }

    fn run_json(
        run_id: &str,
        status: &str,
        created_ms: i64,
        title: Option<&str>,
        r: &Rollup,
        project: Option<&str>,
        flags: RunFlags,
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
            "pinned": flags.pinned,
            "archived": flags.archived,
        })
    }

    /// `GET /api/overview`: KPI cards - totals, costs, pending approvals,
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
        // Archived exclusion happens in the SQL (`list_runs_visible`), so the
        // LIMIT window only ever holds listable rows - no post-limit filtering
        // that starved the page when the newest runs were mostly archived.
        // Status/query filter in SQL before LIMIT (see
        // `list_runs_visible_filtered`): the old Rust-side filter ran after
        // the LIMIT window and starved filtered pages.
        let rows = Ledger::pin_home_first(
            ledger
                .list_runs_visible_filtered(
                    limit,
                    include_archived,
                    status_filter.as_deref(),
                    q.as_deref(),
                )
                .unwrap_or_default(),
        );
        for (id, status, created_ms, title, project, pinned, archived) in rows {
            let entries = ledger.replay(&id).unwrap_or_default();
            let r = rollup(&entries);
            out.push(run_json(
                &id,
                &status,
                created_ms,
                title.as_deref(),
                &r,
                project.as_deref(),
                RunFlags { pinned, archived },
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
            Event::RunProgress { detail, .. } if detail.starts_with("fallback ") => {
                "model_fallback"
            }
            Event::RunProgress { detail, .. } if detail.starts_with("model attempt failed: ") => {
                "model_attempt_failed"
            }
            Event::RunProgress { detail, .. }
                if detail.starts_with("provider chain exhausted: ") =>
            {
                "model_exhausted"
            }
            Event::UsageRecorded { .. } => "usage",
            Event::ApprovalRequested { .. } => "approval_requested",
            Event::ApprovalGranted { .. } => "approval_granted",
            Event::ApprovalDenied { .. } => "approval_denied",
            Event::PreStateRecorded { .. } => "pre_state_recorded",
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
            Event::RunProgress { detail, .. }
                if detail.starts_with("provider chain exhausted: ") =>
            {
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
            Event::PreStateRecorded { path, .. } => Some(redact(path)),
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
                // No start row -> no duration at all (matches the
                // transcript's null), never a fabricated "0ms".
                match tool_starts
                    .get(call_id)
                    .map(|s| e.ts_ms.saturating_sub(*s).max(0))
                {
                    Some(ms) => Some(format!("{tool} · {}", fmt_tool_ms(ms))),
                    None => Some(tool.clone()),
                }
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
    /// missing - a call settled without execution (an ask_user sibling) or
    /// never completed (killed mid-call, crash).
    fn tool_duration_ms(
        call_id: &str,
        tool_starts: &HashMap<String, i64>,
        tool_ends: &HashMap<String, i64>,
    ) -> Option<i64> {
        match (tool_starts.get(call_id), tool_ends.get(call_id)) {
            // Clamp at zero: skewed ledger stamps (imports, manual writes)
            // must not surface as negative durations to clients.
            (Some(s), Some(e)) => Some(e.saturating_sub(*s).max(0)),
            _ => None,
        }
    }

    /// One transcript message item: the wire fields (`type`, `role`,
    /// `content`) plus the metadata the ledger already persists but the
    /// old serialization dropped - `ts_ms`, the model's tool requests
    /// (`tool_calls`), and the matching tool-result id (`tool_call_id`).
    /// Tool arguments go through the same [`redact`] pass as message
    /// content, so a secret pasted into a tool arg never reaches the
    /// browser.
    fn transcript_message(
        m: &pantheon_api::message::Message,
        tool_starts: &HashMap<String, i64>,
        tool_ends: &HashMap<String, i64>,
        child_runs: &HashMap<String, String>,
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
                    // Delegate calls link to the child run they spawned
                    // (null for every other call, and for delegations
                    // recorded before the link existed).
                    "child_run_id": child_runs.get(&tc.id),
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

    /// Build the dashboard's chat-shaped transcript directly from the ledger.
    /// Model tool requests stay nested in their assistant message, as they were
    /// before the display transcript gained standalone tool cards. Tool result
    /// messages remain separate rows so existing dashboard clients can pair them
    /// through `tool_call_id`.
    fn detail_transcript(
        entries: &[pantheon_storage::LedgerEntry],
        tool_starts: &HashMap<String, i64>,
        tool_ends: &HashMap<String, i64>,
        child_runs: &HashMap<String, String>,
    ) -> Vec<serde_json::Value> {
        let model_call_ids: HashSet<String> = entries
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::AssistantMessage { message, .. } => Some(
                    message
                        .tool_calls
                        .iter()
                        .map(|call| call.id.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();
        let mut transcript = Vec::new();
        let mut ui_indices = HashMap::<String, usize>::new();

        for entry in entries {
            match &entry.event {
                Event::AssistantMessage { message, .. } if message.role != Role::Tool => {
                    transcript.push(transcript_message(
                        message,
                        tool_starts,
                        tool_ends,
                        child_runs,
                    ));
                }
                Event::ToolMessage { message, .. } => {
                    transcript.push(transcript_message(
                        message,
                        tool_starts,
                        tool_ends,
                        child_runs,
                    ));
                }
                Event::ToolStarted {
                    call_id,
                    tool,
                    args,
                    ..
                } if !model_call_ids.contains(call_id) => {
                    transcript.push(serde_json::json!({
                        "type": "tool_call",
                        "id": call_id,
                        "name": tool,
                        "arguments": redact(args),
                        "output": "",
                        "ok": tool_ends.contains_key(call_id).then_some(true),
                        "started_ms": tool_starts.get(call_id).copied(),
                        "duration_ms": tool_duration_ms(call_id, tool_starts, tool_ends),
                        "child_run_id": child_runs.get(call_id),
                    }));
                }
                Event::ImportedReasoning { text, .. } => {
                    transcript.push(serde_json::json!({
                        "type": "reasoning",
                        "content": redact(text),
                    }));
                }
                Event::UiContent {
                    ui_id, document, ..
                } => {
                    let idx = transcript.len();
                    ui_indices.insert(ui_id.clone(), idx);
                    transcript.push(serde_json::json!({
                        "type": "ui",
                        "ui_id": ui_id,
                        "revision": 0,
                        "document": document,
                    }));
                }
                Event::UiUpdated {
                    ui_id,
                    revision,
                    document,
                    ..
                } => {
                    if let Some(&idx) = ui_indices.get(ui_id) {
                        let current = transcript[idx]["revision"].as_u64().unwrap_or(0);
                        if *revision > current {
                            transcript[idx]["revision"] = serde_json::json!(revision);
                            transcript[idx]["document"] = serde_json::json!(document);
                        }
                    } else {
                        let idx = transcript.len();
                        ui_indices.insert(ui_id.clone(), idx);
                        transcript.push(serde_json::json!({
                            "type": "ui",
                            "ui_id": ui_id,
                            "revision": revision,
                            "document": document,
                        }));
                    }
                }
                _ => {}
            }
        }

        transcript
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
        let mut out: String = s.chars().take(max_chars.saturating_sub(3)).collect();
        if s.chars().count() > max_chars {
            out.push_str("...");
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
        // Delegate linkage: AgentCompleted rows record which child run each
        // delegating call spawned, keyed by the parent call id. Last wins in
        // ledger order, matching the child whose result actually landed.
        let child_runs: HashMap<String, String> = entries
            .iter()
            .filter_map(|e| match &e.event {
                Event::AgentCompleted {
                    call_id: Some(call),
                    child_run_id: Some(child),
                    ..
                } => Some((call.clone(), child.clone())),
                _ => None,
            })
            .collect();
        let transcript = detail_transcript(&entries, &tool_starts, &tool_ends, &child_runs);
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
            RunFlags { pinned, archived },
        );
        v["transcript"] = serde_json::Value::Array(transcript);
        v["timeline"] = serde_json::Value::Array(timeline);
        // Richer run state the mobile app and dashboard need for a fully
        // interactive run: the unanswered ask_user question (if any), the
        // FIFO list of queued follow-ups (oldest first), the run's agent
        // mode, and the UsageRecorded token totals. NOTE: `queued_message`
        // used to be a single string (or null); it is now always an array
        // (possibly empty). App clients must parse it as a list.
        // All pending inputs, not just the first: a run can park on
        // several ask_user questions, and the old code surfaced only the
        // first while swallowing `Supervisor::open` and `pending_input`
        // errors entirely (`.ok()` twice) - a broken supervisor looked
        // identical to "nothing pending". `pending_input` keeps its old
        // shape (first item or null) for existing clients; `pending_inputs`
        // carries the full list and `pending_input_error` names the failure
        // when the list could not be read.
        let (pending_inputs, pending_input_error): (Vec<serde_json::Value>, Option<String>) =
            match Supervisor::open(app.data_dir.clone()) {
                Ok(sup) => match sup.pending_input(run_id) {
                    Ok(v) => (
                        v.into_iter()
                            .map(|(call_id, question, options)| {
                                serde_json::json!({"call_id": call_id, "question": question, "options": options})
                            })
                            .collect(),
                        None,
                    ),
                    Err(e) => (Vec::new(), Some(format!("pending input: {e}"))),
                },
                Err(e) => (Vec::new(), Some(format!("open runtime: {e}"))),
            };
        let pending_input = pending_inputs
            .first()
            .cloned()
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
        v["pending_inputs"] = serde_json::Value::Array(pending_inputs);
        v["pending_input_error"] = pending_input_error
            .map(serde_json::Value::String)
            .unwrap_or(serde_json::Value::Null);
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
    /// given) before the turn is handed to the real CLI path
    /// `pantheon run --taskID <id> --say <message> --deliver session`
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
        // A spawn failure used to be swallowed and the 201 below still
        // returned, claiming a turn was running that never started. Fail
        // loudly: the run stays admitted in the ledger (visible, retryable
        // via message), but the caller learns the turn never started.
        match spawn_turn_child_with_stdin(
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
            Ok(pid) => app.register_turn_child(&run_id, pid),
            Err(e) => {
                return err_json(500, "SPAWN", &format!("failed to start turn: {e}"));
            }
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
    /// are reopened - those statuses end the latest turn, not the session.
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
                    None => {
                        return bad_json("field \"attachments\" must be an array of upload ids")
                    }
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
        // here - at the attach site - not a silent drop and not a provider 400
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
                                    " - {} ({}): {}\n",
                                    e.rel_path.display(),
                                    uploads::human_size(e.size_bytes),
                                    z.dest.join(&e.rel_path).display()
                                ));
                            }
                            if entries.len() > uploads::MAX_ZIP_LISTED {
                                block.push_str(&format!(
                                    " - ... and {} more\n",
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
        if ledger.status(run_id).unwrap_or(None).as_deref() == Some("awaiting_approval") {
            return conflict(
                "RUN_PARKED",
                &format!("run {run_id} is parked on approval; grant or deny it first"),
            );
        }
        // Serialize turn-starting requests per run: the busy-check → drain →
        // spawn sequence below must be atomic, or two concurrent POSTs both
        // see idle, both spawn, and the loser's turn dies with RT_LEASE_BUSY
        // after we already returned 200 - silently dropping its message.
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
        // steered) while the run was busy was already accepted with 202 - it
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
        // Idle: the turn is accepted for processing, not complete - 202 like
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
        if ledger.status(run_id).unwrap_or(None).as_deref() == Some("awaiting_approval") {
            return conflict(
                "RUN_PARKED",
                &format!("run {run_id} is parked on approval; grant or deny it first"),
            );
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
        // The last turn's input: the `UserMessage` row new turns record, or
        // for imported sessions - the last user-role message in the
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

    /// `POST /api/runs/:id/pin-message`: pin or unpin one message.
    pub fn pin_message(app: &App, req: &Request, run_id: &str) -> Response {
        let ledger = match ledger(app) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let body: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return bad_json("body must be JSON"),
        };
        let message_id = body
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if message_id.is_empty() {
            return bad_json("field \"message_id\" is required");
        }
        let pinned = body.get("pinned").and_then(|v| v.as_bool()).unwrap_or(true);
        match ledger.pin_message(run_id, message_id, pinned) {
            Ok(()) => Response::accepted_json(
                serde_json::json!({"ok": true, "pinned": pinned}).to_string(),
            ),
            Err(e) => err_json(500, "LEDGER", &format!("pin: {e}")),
        }
    }

    /// `POST /api/runs/:id/edit-resubmit`: rewind to a turn, then run a
    /// new turn with edited text. Body: `{turn_id, text}`.
    pub fn edit_resubmit(app: &App, req: &Request, run_id: &str) -> Response {
        let ledger = match ledger(app) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let body: serde_json::Value = match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(_) => return bad_json("body must be JSON"),
        };
        let turn_id = body.get("turn_id").and_then(|v| v.as_str()).unwrap_or("");
        let text = body.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if turn_id.is_empty() || text.trim().is_empty() {
            return bad_json("fields \"turn_id\" and \"text\" are required");
        }
        if ledger.status(run_id).unwrap_or(None).as_deref() == Some("awaiting_approval") {
            return conflict(
                "RUN_PARKED",
                "run is parked on approval; grant or deny it first",
            );
        }
        let sup = match Supervisor::open(app.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
        };
        if sup.has_active_lease(run_id).unwrap_or(false) {
            return conflict("TURN_IN_FLIGHT", "run already has a turn running");
        }
        if let Err(e) = ledger.append(&Event::TurnRewound {
            run_id: run_id.to_string(),
            turn_id: turn_id.to_string(),
        }) {
            return err_json(500, "LEDGER", &format!("rewind: {e}"));
        }
        let mode = ledger
            .run_mode(run_id)
            .unwrap_or_else(|_| DEFAULT_RUN_MODE.to_string());
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
            text.as_bytes(),
        ) {
            Ok(pid) => app.register_turn_child(run_id, pid),
            Err(e) => return err_json(500, "SPAWN", &format!("failed to start turn: {e}")),
        }
        #[cfg(not(test))]
        if !wait_for_turn_lease(&sup, run_id) {
            return err_json(500, "SPAWN", "turn process failed to start");
        }
        Response::accepted_json(serde_json::json!({"ok": true, "edited": true}).to_string())
    }

    /// `POST /api/runs/:id/continue`: continue a failed turn from its
    /// partial response. Body: `{prompt}` optional override.
    pub fn continue_turn(app: &App, req: &Request, run_id: &str) -> Response {
        let ledger = match ledger(app) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let entries = match ledger.replay(run_id) {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => return err_json(404, "NOT_FOUND", "unknown run"),
            Err(e) => return err_json(500, "LEDGER", &format!("replay: {e}")),
        };
        let mut partial = String::new();
        let mut last_user = String::new();
        for e in &entries {
            match &e.event {
                Event::UserMessage { text, .. } => last_user = text.clone(),
                Event::ModelDelta { delta, .. } => partial.push_str(delta),
                _ => {}
            }
        }
        let body: serde_json::Value =
            serde_json::from_slice(&req.body).unwrap_or(serde_json::json!({}));
        let prompt = body
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("Continue from where you left off.");
        let say = if partial.is_empty() {
            prompt.to_string()
        } else {
            format!("{prompt}\n\n[Partial response so far, continue it]\n{partial}")
        };
        if last_user.is_empty() && partial.is_empty() {
            return bad_json(
                "nothing to continue: the run has no recorded prompt or partial response",
            );
        }
        let sup = match Supervisor::open(app.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => return err_json(500, "RUNTIME", &format!("open runtime: {e}")),
        };
        if sup.has_active_lease(run_id).unwrap_or(false) {
            return conflict("TURN_IN_FLIGHT", "run already has a turn running");
        }
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
                "ask",
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
        Response::accepted_json(serde_json::json!({"ok": true, "continued": true}).to_string())
    }

    /// `POST /api/runs/:id/clear-context`: drop transcript context
    /// without deleting the run. Emits a rewind at the first turn so
    /// replay sees a fresh conversation, history stays in the ledger.
    pub fn clear_context(app: &App, run_id: &str) -> Response {
        let ledger = match ledger(app) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let entries = match ledger.replay(run_id) {
            Ok(e) if !e.is_empty() => e,
            Ok(_) => return err_json(404, "NOT_FOUND", "unknown run"),
            Err(e) => return err_json(500, "LEDGER", &format!("replay: {e}")),
        };
        let first_turn = entries.iter().find_map(|e| match &e.event {
            Event::TurnStarted { turn_id, .. } => Some(turn_id.clone()),
            _ => None,
        });
        let Some(turn_id) = first_turn else {
            return bad_json("nothing to clear: the run has no turns");
        };
        if let Err(e) = ledger.append(&Event::TurnRewound {
            run_id: run_id.to_string(),
            turn_id,
        }) {
            return err_json(500, "LEDGER", &format!("clear: {e}"));
        }
        Response::accepted_json(serde_json::json!({"ok": true, "cleared": true}).to_string())
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
                        TranscriptItem::ToolCall {
                            call_id,
                            name,
                            args,
                            output,
                            ok,
                        } => {
                            let status = match ok {
                                Some(true) => "completed",
                                Some(false) => "failed",
                                None => "pending",
                            };
                            md.push_str(&format!(
                                "## Tool: {name} ({status})\n\nCall: `{call_id}`\n\nArguments:\n\n```json\n{}\n```\n\n",
                                redact(&args)
                            ));
                            if !output.is_empty() {
                                md.push_str(&format!(
                                    "Output:\n\n```text\n{}\n```\n\n",
                                    redact(&output.join("\n"))
                                ));
                            }
                        }
                        TranscriptItem::Reasoning(t) => {
                            md.push_str(&format!("## Reasoning\n\n{}\n\n", redact(&t)));
                        }
                        TranscriptItem::Ui {
                            ui_id, document, ..
                        } => {
                            md.push_str(&format!(
                                "## UI [{ui_id}] ({})\n\n{}\n\n",
                                document.root.component.tag(),
                                redact(&pantheon_api::ui::text_summary(&document.root))
                            ));
                        }
                        TranscriptItem::Status(t) => {
                            md.push_str(&format!("## Status\n\n{}\n\n", redact(&t)));
                        }
                    }
                }
                Response::download(
                    &format!("run-{run_id}.md"),
                    "text/markdown; charset=utf-8",
                    md.into_bytes(),
                )
            }
            "json" => {
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
                        TranscriptItem::ToolCall {
                            call_id,
                            name,
                            args,
                            output,
                            ok,
                        } => serde_json::json!({
                            "type": "tool_call",
                            "id": call_id,
                            "name": name,
                            "arguments": redact(&args),
                            "output": redact(&output.join("\n")),
                            "ok": ok,
                        }),
                        TranscriptItem::Reasoning(t) => serde_json::json!({
                            "type": "reasoning",
                            "content": redact(&t),
                        }),
                        TranscriptItem::Ui {
                            ui_id,
                            revision,
                            document,
                        } => serde_json::json!({
                            "type": "ui",
                            "ui_id": ui_id,
                            "revision": revision,
                            "document": document,
                        }),
                        TranscriptItem::Status(t) => serde_json::json!({
                            "type": "status",
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
            // An unknown format used to fall through to JSON silently; a
            // client asking for e.g. "yaml" got JSON labelled as a download
            // and never knew. 400 instead.
            _ => bad_json(&format!(
                "unknown export format '{format}' (expected 'json' or 'markdown')"
            )),
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
    pub(crate) fn known_run(app: &App, run_id: &str) -> Result<(), Response> {
        let ledger = ledger(app)?;
        match ledger.replay(run_id) {
            Ok(e) if !e.is_empty() => Ok(()),
            Ok(_) => Err(err_json(404, "NOT_FOUND", "unknown run")),
            Err(e) => Err(err_json(500, "LEDGER", &format!("replay: {e}"))),
        }
    }

    /// `POST /api/runs/:id/cancel`: interrupt the run's in-flight turn.
    /// Cooperative: the turn stops at its next checkpoint. 404 on unknown
    /// runs; a terminal run (completed/failed) 409s with RT_TERMINAL - there
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
    /// Unlike `/cancel` (cooperative - the turn winds down at its next
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
        // period, but the response then reports the recorded outcome
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
            Ok(true) => {
                json_ok(serde_json::json!({"ok": true, "queue": queue_json(&ledger, run_id)}))
            }
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
            Ok(true) => {
                json_ok(serde_json::json!({"ok": true, "queue": queue_json(&ledger, run_id)}))
            }
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
    /// `{"todos": [...]}`. Enforces the same invariants the todo tool does
    /// non-empty content, at most one in progress - 400 otherwise.
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
    /// Body: `{"turn": n}` - fork from the start of turn `n`; absent forks
    /// the whole run. 201 with the new ID and the copied turn count.
    pub fn fork(app: &App, run_id: &str, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        // A present-but-non-integer `turn` (string, float, negative) used
        // to fall through to `None` and silently fork the whole run. That
        // is a wrong request, not a whole-run fork: 400 it.
        let turn = match body.get("turn") {
            None => None,
            Some(v) => match v.as_u64() {
                Some(n) => Some(n as usize),
                None => return bad_json("field \"turn\" must be a non-negative integer"),
            },
        };
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
                        // Roll back the fork itself: a half-carried queue
                        // (some follow-ups on the new run, the rest lost
                        // from the client's view) is worse than no fork.
                        // The source run is untouched - only the brand-new
                        // run is deleted. Best-effort: if the delete itself
                        // fails, say so in the error instead of pretending
                        // the rollback happened.
                        let rollback = ledger
                            .delete_run(&new_id)
                            .err()
                            .map(|de| format!("; rollback failed: {de}"));
                        return err_json(
                            500,
                            "LEDGER",
                            &format!("carry queue into fork: {e}{}", rollback.unwrap_or_default()),
                        );
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
        // Return the stored (bound) title, not the raw request title: the
        // old response echoed the untruncated title while the ledger stored
        // the bound one, so clients displayed a title that was never saved.
        let stored_title = bound_title(title, TITLE_MAX_CHARS);
        let titled = Event::SessionTitled {
            run_id: run_id.to_string(),
            title: stored_title.clone(),
            model: String::new(),
            source: "api".into(),
        };
        match sup.emit(titled) {
            Ok(()) => json_ok(serde_json::json!({"ok": true, "title": stored_title})),
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
        // The home session can never be hidden either (mirrors prune's
        // HOME_PROTECTED guard): archiving it would vanish the permanent
        // session from every default list with no in-UI way back.
        if archived && run_id == HOME_SESSION_ID {
            return err_json(
                403,
                "HOME_PROTECTED",
                "the home session can never be archived",
            );
        }
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

    /// Max project-name length; operator metadata is echoed in every run
    /// list and projects response, so it gets a bound like titles do.
    const PROJECT_MAX_CHARS: usize = 128;

    /// `POST /api/runs/:id/project`: assign the run to a named project
    /// (`{"project": "name"}`), or unassign it (`{"project": null}` or
    /// `{"project": ""}`). Projects are operator-created buckets - a run
    /// belongs to at most one.
    pub fn set_project(app: &App, run_id: &str, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let project = match body.get("project") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => match v.as_str().map(str::trim) {
                // Documented contract: an empty name unassigns, like null.
                Some("") => None,
                Some(s) if s.chars().count() > PROJECT_MAX_CHARS => {
                    return bad_json("field \"project\" exceeds 128 characters")
                }
                Some(s) => Some(s),
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
    /// the runs table, like everything else. Archived runs don't count (see
    /// the membership loop below).
    pub fn projects(app: &App) -> Response {
        let ledger = match ledger(app) {
            Ok(l) => l,
            Err(r) => return r,
        };
        let names = match ledger.list_projects() {
            Ok(n) => n,
            Err(e) => return err_json(500, "LEDGER", &format!("list projects: {e}")),
        };
        let rows = match ledger.list_runs(MAX_RUNS_SCAN) {
            Ok(r) => r,
            Err(e) => return err_json(500, "LEDGER", &format!("list runs: {e}")),
        };
        let mut members: HashMap<String, Vec<String>> = HashMap::new();
        for (id, _, _, _, project, _, archived) in rows {
            // Archived runs are hidden work: they neither keep a project
            // listed nor count toward its membership. Archiving a project's
            // last visible run drops the project until a run is restored.
            if archived {
                continue;
            }
            if let Some(p) = project {
                members.entry(p).or_default().push(id);
            }
        }
        let out: Vec<serde_json::Value> = names
            .into_iter()
            .filter_map(|name| {
                members
                    .remove(&name)
                    .map(|runs| serde_json::json!({"name": name, "runs": runs}))
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
            return bad_json(
                "fields \"call_id\" and \"answer\" are required and must be non-empty",
            );
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
        // (A run parked awaiting input holds no lease - the turn ended when
        // it parked - so answering a real question is unaffected.)
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
        match spawn_turn_child(&[
            "run",
            "--taskID",
            run_id,
            "--resume",
            "--deliver",
            "session",
            "--mode",
            mode.as_str(),
        ]) {
            Ok(pid) => app.register_turn_child(run_id, pid),
            Err(e) => {
                return err_json(500, "SPAWN", &format!("failed to resume turn: {e}"));
            }
        }
        json_ok(serde_json::json!({"ok": true}))
    }
}

pub mod schedule {
    //! Schedule CRUD over the reworked scheduler core.
    //!
    //! Jobs live in `<data_dir>/schedule.json` - the same file the
    //! `pantheon schedule` CLI reads and writes. The dashboard does not keep
    //! its own store mirror: the shared [`ScheduledJob`] shape is read and
    //! written with the core's [`load_jobs`]/[`save_jobs`], so legacy rows
    //! migrate on load and the file stays the single source of truth.
    //! Templates go through [`TemplateStore::open`] (write-through to
    //! `templates.json`).
    //!
    //! Triggering a run does not reimplement the runner: it spawns the real
    //! `pantheon schedule run <id>` CLI path in the background.

    use crate::util::{now_ms, DEFAULT_AGENT_NAME};
    use crate::{bad_json, body_json, err_json, json_ok, spawn_pantheon, App};
    use pantheon_gateway::http::{Request, Response};
    use pantheon_scheduler::{
        apply_defaults, cron, expand_template, is_reserved_var, load_jobs, load_jobs_with_warnings,
        parse_duration, render_prompt, update_jobs, Job, OverlapPolicy, ScheduleKind,
        ScheduleTemplate, ScheduledJob, TemplateSchedule, TemplateStore, TemplateVar,
    };
    use std::collections::HashMap;

    /// Only the three live kinds can be created. Rows in the old file format
    fn kind_json(kind: &ScheduleKind) -> serde_json::Value {
        match kind {
            ScheduleKind::Cron { expr } => serde_json::json!({"type": "cron", "expr": expr}),
            ScheduleKind::Interval { every_ms } => {
                serde_json::json!({"type": "every", "every_ms": every_ms})
            }
            ScheduleKind::OneShot { at_ms } => {
                serde_json::json!({"type": "oneshot", "at_ms": at_ms})
            }
        }
    }

    /// The shared [`ScheduledJob`] shape: `{"job": {...}, "last_run": ...}`,
    /// with the kind expanded for the UI. The `job` object carries `task`,
    /// `template`, `template_vars`, `agent`, and `catch_up` straight from the
    /// core's serde.
    fn job_json(j: &ScheduledJob) -> serde_json::Value {
        let mut job = serde_json::to_value(&j.job).unwrap_or(serde_json::Value::Null);
        if let Some(obj) = job.as_object_mut() {
            obj.insert("kind".to_string(), kind_json(&j.job.kind));
        }
        serde_json::json!({
            "job": job,
            "last_run": j.last_run,
            "next_fire_ms": j.job.next_fire_ms(now_ms(), j.last_run),
        })
    }

    /// Success envelope for single-job responses: the shared
    /// `{"job": {...}, "last_run": ..., "next_fire_ms": ...}` shape plus `ok`.
    fn ok_job(j: &ScheduledJob) -> Response {
        let mut v = job_json(j);
        if let Some(obj) = v.as_object_mut() {
            obj.insert("ok".to_string(), serde_json::Value::Bool(true));
        }
        json_ok(v)
    }

    /// `GET /api/schedule/jobs`
    pub fn list_jobs(app: &App) -> Response {
        // Warnings name legacy rows skipped because their trigger kind was
        // removed (webhook/conditional/manual), so the dashboard can show
        // them instead of letting jobs vanish silently.
        let (result, warnings) = load_jobs_with_warnings(&app.data_dir);
        match result {
            Ok(jobs) => json_ok(serde_json::json!({
                "jobs": jobs.iter().map(job_json).collect::<Vec<_>>(),
                "warnings": warnings,
            })),
            Err(e) => err_json(500, "SCHEDULE", &e),
        }
    }

    /// `GET /api/schedule/templates`: built-ins + user templates with their
    /// variables, so the create form can render without hand-written fields.
    pub fn list_templates(app: &App) -> Response {
        let store = TemplateStore::open(&app.data_dir);
        let templates: Vec<serde_json::Value> = store
            .list()
            .iter()
            .map(|t| {
                let schedule = match &t.schedule {
                    TemplateSchedule::Every(d) => {
                        serde_json::json!({"type": "every", "duration": d})
                    }
                    TemplateSchedule::Cron(e) => serde_json::json!({"type": "cron", "expr": e}),
                };
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "schedule": schedule,
                    "vars": t.vars.iter().map(|v| serde_json::json!({
                        "name": v.name,
                        "question": v.question,
                        "default": v.default,
                        "reserved": is_reserved_var(&v.name),
                    })).collect::<Vec<_>>(),
                    "builtin": store.is_builtin(&t.name),
                })
            })
            .collect();
        json_ok(serde_json::json!({"templates": templates}))
    }

    fn template_json(t: &ScheduleTemplate, builtin: bool) -> serde_json::Value {
        let schedule = match &t.schedule {
            TemplateSchedule::Every(d) => serde_json::json!({"type": "every", "duration": d}),
            TemplateSchedule::Cron(e) => serde_json::json!({"type": "cron", "expr": e}),
        };
        serde_json::json!({
            "name": t.name,
            "description": t.description,
            "schedule": schedule,
            "prompt": t.prompt,
            "vars": t.vars,
            "builtin": builtin,
        })
    }

    /// `POST /api/schedule/templates`: body has `name`, `description`,
    /// `schedule` (`{"cron": "..."}` or `{"every": "30m"}`), `prompt`, and
    /// `vars` (`[{name, question, default?}]`). The core validates loudly
    /// (bad cron, empty prompt, ...) and writes through to `templates.json`.
    pub fn create_template(app: &App, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let get = |k: &str| body.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let schedule = match body.get("schedule") {
            Some(s) if s.get("cron").and_then(|x| x.as_str()).is_some() => {
                TemplateSchedule::Cron(s["cron"].as_str().unwrap_or_default().to_string())
            }
            Some(s) if s.get("every").and_then(|x| x.as_str()).is_some() => {
                TemplateSchedule::Every(s["every"].as_str().unwrap_or_default().to_string())
            }
            _ => return bad_json("schedule must be {\"cron\": \"...\"} or {\"every\": \"30m\"}"),
        };
        let mut vars = Vec::new();
        if let Some(list) = body.get("vars").and_then(|x| x.as_array()) {
            for v in list {
                let Some(name) = v.get("name").and_then(|x| x.as_str()) else {
                    return bad_json("each var needs a name");
                };
                vars.push(TemplateVar {
                    name: name.to_string(),
                    question: v
                        .get("question")
                        .and_then(|x| x.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    default: v
                        .get("default")
                        .and_then(|x| x.as_str())
                        .map(str::to_string),
                });
            }
        }
        let template = ScheduleTemplate {
            name: get("name").unwrap_or_default(),
            description: get("description").unwrap_or_default(),
            schedule,
            prompt: get("prompt").unwrap_or_default(),
            vars,
        };
        let mut store = TemplateStore::open(&app.data_dir);
        if let Err(e) = store.save(template.clone()) {
            return bad_json(&e);
        }
        json_ok(serde_json::json!({"ok": true, "template": template_json(&template, false)}))
    }

    /// `DELETE /api/schedule/templates/:name`. Removes a user template; when
    /// the name overrides a built-in, the override is removed and the built-in
    /// is revealed again. Built-in names with no override, and unknown names,
    /// are an error.
    pub fn delete_template(app: &App, name: &str) -> Response {
        let mut store = TemplateStore::open(&app.data_dir);
        if let Err(e) = store.delete(name) {
            return bad_json(&e);
        }
        json_ok(serde_json::json!({"ok": true}))
    }

    struct CreateInput {
        task: String,
        every: Option<String>,
        cron: Option<String>,
        at_ms: Option<i64>,
        template: Option<String>,
        vars: HashMap<String, String>,
        catch_up: bool,
        agent: Option<String>,
        deliver: Option<String>,
        model: Option<String>,
        provider: Option<String>,
        timeout: Option<String>,
        overlap: Option<String>,
    }

    fn parse_vars(
        obj: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<HashMap<String, String>, String> {
        let mut vars = HashMap::new();
        if let Some(map) = obj {
            for (k, val) in map {
                vars.insert(
                    k.clone(),
                    val.as_str()
                        .ok_or_else(|| format!("var '{k}' must be a string"))?
                        .to_string(),
                );
            }
        }
        Ok(vars)
    }

    fn parse_create(v: &serde_json::Value) -> Result<CreateInput, String> {
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        // `template_vars` is the documented key; `vars` is accepted as an alias.
        let mut vars = parse_vars(v.get("template_vars").and_then(|x| x.as_object()))?;
        if vars.is_empty() {
            vars = parse_vars(v.get("vars").and_then(|x| x.as_object()))?;
        }
        let at_ms = match v.get("at_ms") {
            None => None,
            Some(x) => Some(
                x.as_i64()
                    .ok_or_else(|| "at_ms must be an integer epoch-millis timestamp".to_string())?,
            ),
        };
        Ok(CreateInput {
            task: get("task").unwrap_or_default(),
            every: get("every"),
            cron: get("cron"),
            at_ms,
            template: get("template"),
            vars,
            catch_up: v.get("catch_up").and_then(|x| x.as_bool()).unwrap_or(true),
            agent: get("agent"),
            deliver: get("deliver"),
            model: get("model"),
            provider: get("provider"),
            timeout: get("timeout"),
            overlap: get("overlap"),
        })
    }

    /// Kind precedence: cron > every > at_ms, like the CLI.
    fn build_kind(input: &CreateInput) -> Result<ScheduleKind, String> {
        if let Some(expr) = &input.cron {
            if let Err(e) = cron::CronSchedule::validate(expr) {
                return Err(format!("invalid cron expression: {e}"));
            }
            Ok(ScheduleKind::Cron { expr: expr.clone() })
        } else if let Some(dur) = &input.every {
            match parse_duration(dur) {
                Ok(ms) => Ok(ScheduleKind::Interval { every_ms: ms }),
                Err(e) => Err(e),
            }
        } else if let Some(at_ms) = input.at_ms {
            Ok(ScheduleKind::OneShot { at_ms })
        } else {
            Err("need every (duration), cron (expression), or at_ms (epoch millis)".to_string())
        }
    }

    /// `POST /api/schedule/jobs`: create, with the same validation the CLI's
    /// `schedule create` applies (template rendering, cron check, model pin,
    /// delivery target). No TTY prompting: a template var without a default
    /// must be supplied or creation fails loudly. The rendered prompt becomes
    /// the `task` snapshot; the template name and vars are stored on the job
    /// for re-render at fire time.
    pub fn create_job(app: &App, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        let mut input = match parse_create(&body) {
            Ok(i) => i,
            Err(e) => return bad_json(&e),
        };
        // Template expansion: the shared `expand_template` machinery. The
        // rendered prompt is the task snapshot; the name + vars stay on the
        // job so the fire path can re-render with fresh defaults. No TTY
        // prompting: a template var without a default must be supplied, or
        // creation fails loudly naming every missing var at once.
        if let Some(tname) = input.template.clone() {
            let store = TemplateStore::open(&app.data_dir);
            let t = match store.get(&tname) {
                Some(t) => t,
                None => {
                    return bad_json(&format!(
                        "unknown template '{tname}'; see /api/schedule/templates"
                    ))
                }
            };
            let has_schedule =
                input.every.is_some() || input.cron.is_some() || input.at_ms.is_some();
            let mut missing = Vec::new();
            let mut collect = |name: &str, _question: &str| -> Result<String, String> {
                missing.push(name.to_string());
                // The error value is discarded: the caller reports `missing`.
                Err(format!("template '{tname}' needs --var {name}=<value>"))
            };
            if let Err(e) = expand_template(
                t,
                pantheon_scheduler::templates::TemplateParams {
                    vars: &mut input.vars,
                    model: &mut input.model,
                    provider: &mut input.provider,
                    task: &mut input.task,
                    every: &mut input.every,
                    cron: &mut input.cron,
                },
                has_schedule,
                &mut collect,
            ) {
                if !missing.is_empty() {
                    return bad_json(&format!(
                        "template '{tname}' needs vars: {}",
                        missing.join(", ")
                    ));
                }
                return bad_json(&format!("template render: {e}"));
            }
        }
        if input.task.trim().is_empty() {
            return bad_json("need a task (or --template with all vars)");
        }
        if let Some(d) = &input.deliver {
            if let Err(e) = pantheon_gateway::schedule_delivery::Deliver::parse(d) {
                return bad_json(&e);
            }
        }
        let kind = match build_kind(&input) {
            Ok(k) => k,
            Err(e) => return bad_json(&e),
        };
        let agent = input
            .agent
            .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string());
        let job_id = format!("job_{}", pantheon_runtime::new_run_id());
        let mut job = Job::new(&job_id, kind, &agent);
        if let Err(e) = job.validate() {
            return bad_json(&format!("invalid schedule: {e}"));
        }
        if let Some(m) = input.model.as_deref() {
            if let Err(e) = job.pin_model(m, input.provider.as_deref()) {
                return bad_json(&format!("bad model pin: {e}"));
            }
        } else if input.provider.is_some() {
            return bad_json("provider without model pins nothing; add a model");
        }
        let timeout_secs = match input.timeout.as_deref() {
            None => None,
            Some(d) => match parse_duration(d) {
                Ok(ms) => {
                    let secs = ms / 1000;
                    if secs == 0 {
                        return bad_json("timeout must be at least 1s");
                    }
                    Some(secs)
                }
                Err(e) => return bad_json(&format!("bad timeout: {e}")),
            },
        };
        let overlap = match input.overlap.as_deref() {
            None => OverlapPolicy::default(),
            Some(o) => match o.parse::<OverlapPolicy>() {
                Ok(p) => p,
                Err(e) => return bad_json(&format!("bad overlap: {e}")),
            },
        };
        job.task = input.task;
        job.template = input.template;
        job.template_vars = input.vars;
        job.catch_up = input.catch_up;
        job.timeout_secs = timeout_secs;
        job.overlap = overlap;
        job.deliver = input.deliver;
        let scheduled = ScheduledJob {
            job,
            last_run: None,
        };
        // Single locked read-modify-write: two dashboard threads creating jobs
        // at once can't clobber each other's rows.
        match update_jobs(&app.data_dir, |jobs| {
            jobs.push(scheduled.clone());
            Ok::<_, String>(scheduled.clone())
        }) {
            Ok(created) => ok_job(&created),
            Err(e) => err_json(500, "SCHEDULE", &e),
        }
    }

    /// `PUT /api/schedule/jobs/:id`: edit fields / pause / resume. `template`
    /// reassigns the blueprint (null clears it); `template_vars`/`vars`
    /// merge into the job's vars, validated against the template like creation.
    pub fn update_job(app: &App, req: &Request, id: &str) -> Response {
        let body = match body_json(req) {
            Ok(v) => v,
            Err(r) => return r,
        };
        // The edit, computed under the store's exclusive lock so a
        // concurrent dashboard request cannot slip an edit between our
        // load and save. Validation failures are data, not store errors:
        // the lock is released and the file rewrite is a harmless no-op.
        enum Edit {
            Done(Box<ScheduledJob>),
            Bad(String),
            Missing,
        }
        let outcome = match update_jobs(&app.data_dir, |jobs| {
            let pos = match jobs.iter().position(|j| j.job.id == id) {
                Some(p) => p,
                None => return Ok(Edit::Missing),
            };
            let get = |k: &str| body.get(k).and_then(|x| x.as_str()).map(str::to_string);
            if let Some(paused) = body.get("paused").and_then(|x| x.as_bool()) {
                jobs[pos].job.paused = paused;
            }
            if let Some(catch_up) = body.get("catch_up").and_then(|x| x.as_bool()) {
                jobs[pos].job.catch_up = catch_up;
            }
            // Template reassignment: `template` swaps the blueprint (explicit
            // null clears it); `template_vars` (alias `vars`) merges into the
            // job's vars. Validation mirrors creation: the template must exist,
            // reserved vars (`model`, `provider`) become the model pin when the
            // body sets none explicitly, defaults fill gaps, and the prompt must
            // render. A reassigned template refreshes the task snapshot; an
            // explicit `task` in the same body still wins (handled below).
            if body.get("template").is_some()
                || body.get("template_vars").is_some()
                || body.get("vars").is_some()
            {
                let store = TemplateStore::open(&app.data_dir);
                // Present-but-not-an-object is a 400, not a silent ignore.
                let strict_vars = |key: &str| -> Result<HashMap<String, String>, String> {
                    match body.get(key) {
                        None | Some(serde_json::Value::Null) => Ok(HashMap::new()),
                        Some(serde_json::Value::Object(map)) => parse_vars(Some(map)),
                        Some(_) => Err(format!("`{key}` must be an object of string values")),
                    }
                };
                let mut new_vars = match strict_vars("vars") {
                    Ok(v) => v,
                    Err(e) => return Ok(Edit::Bad(e)),
                };
                let template_vars = match strict_vars("template_vars") {
                    Ok(v) => v,
                    Err(e) => return Ok(Edit::Bad(e)),
                };
                for (k, v) in template_vars {
                    new_vars.insert(k, v);
                }
                // Reserved vars become the model pin, not prompt text - like
                // creation. An explicit model/provider in the body wins. Applied
                // once, before the template/vars branches below.
                let mut var_model: Option<String> = None;
                let mut var_provider: Option<String> = None;
                for reserved in ["model", "provider"] {
                    if let Some(v) = new_vars.remove(reserved) {
                        if v.trim().is_empty() {
                            continue;
                        }
                        if reserved == "model" {
                            var_model = Some(v);
                        } else {
                            var_provider = Some(v);
                        }
                    }
                }
                if let Some(m) = var_model {
                    if body.get("model").is_none() {
                        let mut probe = jobs[pos].job.clone();
                        let pin_provider = var_provider.clone().or_else(|| probe.provider.clone());
                        if let Err(e) = probe.pin_model(&m, pin_provider.as_deref()) {
                            return Ok(Edit::Bad(format!("bad model pin: {e}")));
                        }
                        jobs[pos].job.model = probe.model;
                        jobs[pos].job.provider = probe.provider;
                    }
                } else if var_provider.is_some() {
                    return Ok(Edit::Bad(
                        "provider without model pins nothing; add a model".to_string(),
                    ));
                }
                if body.get("template").is_some() {
                    match body.get("template") {
                        Some(serde_json::Value::String(tname)) => {
                            let t = match store.get(tname) {
                                Some(t) => t,
                                None => {
                                    return Ok(Edit::Bad(format!(
                                        "unknown template '{tname}'; see /api/schedule/templates"
                                    )))
                                }
                            };
                            // Keep old vars the new template still declares,
                            // overlaid with the newly provided ones.
                            let declared: std::collections::HashSet<&str> =
                                t.vars.iter().map(|v| v.name.as_str()).collect();
                            let mut merged: HashMap<String, String> = jobs[pos]
                                .job
                                .template_vars
                                .iter()
                                .filter(|(k, _)| declared.contains(k.as_str()))
                                .map(|(k, v)| (k.clone(), v.clone()))
                                .collect();
                            for (k, v) in new_vars {
                                merged.insert(k, v);
                            }
                            apply_defaults(t, &mut merged);
                            let missing: Vec<String> = t
                                .vars
                                .iter()
                                .filter(|tv| {
                                    merged
                                        .get(&tv.name)
                                        .map(String::as_str)
                                        .unwrap_or("")
                                        .is_empty()
                                        && tv.default.is_none()
                                })
                                .map(|tv| tv.name.clone())
                                .collect();
                            if !missing.is_empty() {
                                return Ok(Edit::Bad(format!(
                                    "template '{tname}' needs vars: {}",
                                    missing.join(", ")
                                )));
                            }
                            let rendered = match render_prompt(t, &merged) {
                                Ok(r) => r,
                                Err(e) => return Ok(Edit::Bad(format!("template render: {e}"))),
                            };
                            jobs[pos].job.template = Some(tname.to_string());
                            jobs[pos].job.template_vars = merged;
                            jobs[pos].job.task = rendered;
                        }
                        Some(serde_json::Value::Null) => {
                            // Explicit null: drop the blueprint. Provided vars are
                            // kept (inert without a template, like creation).
                            jobs[pos].job.template = None;
                            jobs[pos].job.template_vars = new_vars;
                        }
                        _ => {
                            return Ok(Edit::Bad(
                                "`template` must be a template name or null".to_string(),
                            ))
                        }
                    }
                } else {
                    // Vars only: merge into the existing vars. Reserved vars were
                    // already intercepted and pinned above.
                    for (k, v) in new_vars {
                        jobs[pos].job.template_vars.insert(k, v);
                    }
                    if let Some(tname) = jobs[pos].job.template.clone() {
                        let t = match store.get(&tname) {
                            Some(t) => t,
                            None => {
                                return Ok(Edit::Bad(format!(
                                "job's template '{tname}' no longer exists; reassign `template` first"
                            )))
                            }
                        };
                        if let Err(e) = render_prompt(t, &jobs[pos].job.template_vars) {
                            return Ok(Edit::Bad(format!("template render: {e}")));
                        }
                    }
                }
            }
            if let Some(task) = get("task") {
                if task.trim().is_empty() {
                    return Ok(Edit::Bad("task cannot be empty".to_string()));
                }
                jobs[pos].job.task = task;
            }
            if let Some(d) = get("deliver") {
                if let Err(e) = pantheon_gateway::schedule_delivery::Deliver::parse(&d) {
                    return Ok(Edit::Bad(e));
                }
                jobs[pos].job.deliver = Some(d);
            }
            if body.get("deliver").is_some() && get("deliver").is_none() {
                jobs[pos].job.deliver = None; // explicit null clears
            }
            if let Some(m) = get("model") {
                let mut probe = jobs[pos].job.clone();
                if let Err(e) = probe.pin_model(
                    &m,
                    get("provider")
                        .as_deref()
                        .or(jobs[pos].job.provider.as_deref()),
                ) {
                    return Ok(Edit::Bad(format!("bad model pin: {e}")));
                }
                jobs[pos].job.model = probe.model;
                jobs[pos].job.provider = probe.provider;
            }
            if let Some(agent) = get("agent") {
                if agent.trim().is_empty() {
                    return Ok(Edit::Bad("agent cannot be empty".to_string()));
                }
                jobs[pos].job.agent = agent;
            }
            // Rescheduling: rebuild the kind and re-validate like creation does
            // (cron > every > at_ms).
            if get("cron").is_some() || get("every").is_some() || body.get("at_ms").is_some() {
                let expr = get("cron");
                let dur = get("every");
                let at_ms = body.get("at_ms").and_then(|x| x.as_i64());
                let kind = if let Some(e) = expr {
                    if let Err(err) = cron::CronSchedule::validate(&e) {
                        return Ok(Edit::Bad(format!("invalid cron expression: {err}")));
                    }
                    ScheduleKind::Cron { expr: e }
                } else if let Some(d) = dur {
                    match parse_duration(&d) {
                        Ok(ms) => ScheduleKind::Interval { every_ms: ms },
                        Err(e) => return Ok(Edit::Bad(e)),
                    }
                } else if let Some(ms) = at_ms {
                    ScheduleKind::OneShot { at_ms: ms }
                } else {
                    return Ok(Edit::Bad(
                        "at_ms must be an integer epoch-millis timestamp".to_string(),
                    ));
                };
                let mut probe = jobs[pos].job.clone();
                probe.kind = kind.clone();
                if let Err(e) = probe.validate() {
                    return Ok(Edit::Bad(format!("invalid schedule: {e}")));
                }
                jobs[pos].job.kind = kind;
            }
            if let Some(o) = get("overlap") {
                match o.parse::<OverlapPolicy>() {
                    Ok(p) => jobs[pos].job.overlap = p,
                    Err(e) => return Ok(Edit::Bad(format!("bad overlap: {e}"))),
                }
            }
            Ok(Edit::Done(Box::new(jobs[pos].clone())))
        }) {
            Ok(o) => o,
            Err(e) => return err_json(500, "SCHEDULE", &e),
        };
        match outcome {
            Edit::Done(job) => ok_job(&job),
            Edit::Bad(msg) => bad_json(&msg),
            Edit::Missing => err_json(404, "NOT_FOUND", "unknown job"),
        }
    }

    /// `DELETE /api/schedule/jobs/:id`
    pub fn delete_job(app: &App, id: &str) -> Response {
        // Locked read-modify-write: the existence check and the removal are
        // one atomic step, so a concurrent edit can't resurrect or duplicate.
        match update_jobs(&app.data_dir, |jobs| {
            let before = jobs.len();
            jobs.retain(|j| j.job.id != id);
            Ok::<_, String>(jobs.len() != before)
        }) {
            Ok(true) => json_ok(serde_json::json!({"ok": true})),
            Ok(false) => err_json(404, "NOT_FOUND", "unknown job"),
            Err(e) => err_json(500, "SCHEDULE", &e),
        }
    }

    /// `POST /api/schedule/jobs/:id/trigger`: run now via the real CLI path,
    /// in the background. 200 immediately; the run appears in the ledger.
    pub fn trigger_job(app: &App, _req: &Request, id: &str) -> Response {
        let jobs = match load_jobs(&app.data_dir) {
            Ok(j) => j,
            Err(e) => return err_json(500, "SCHEDULE", &e),
        };
        if !jobs.iter().any(|j| j.job.id == id) {
            return err_json(404, "NOT_FOUND", "unknown job");
        }
        // `spawn_pantheon` only spawns the child (it does not wait for the
        // job), so doing it synchronously costs nothing - but doing it in a
        // detached thread used to swallow spawn failures and still answer
        // `triggered`. A spawn failure now 500s with SPAWN.
        if let Err(e) = spawn_pantheon(&["schedule", "run", id]) {
            return err_json(500, "SPAWN", &format!("failed to trigger job: {e}"));
        }
        json_ok(serde_json::json!({"ok": true, "triggered": id}))
    }
}

pub mod stats {
    //! Usage stats: the same fold the TUI's `pantheon stats` performs.
    //!
    //! The aggregation is a direct fold over
    //! [`Ledger::usage_between`](pantheon_storage::Ledger::usage_between)
    //! (`UsageRecorded` rows): totals, by model, by run, by UTC day. The
    //! dashboard cannot depend on the TUI crate, so the fold is mirrored here
    //! against the stable ledger API - the same rows, the same sums.

    use crate::util::{now_ms, round_cost_usd};
    use crate::{err_json, json_ok, query_usize, App};
    use pantheon_api::events::Event;
    use pantheon_gateway::http::{Request, Response};
    use pantheon_storage::Ledger;
    use std::collections::BTreeMap;

    const DAY_MS: i64 = 86_400_000;

    #[derive(Default)]
    struct Totals {
        calls: u64,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
        cost_usd: f64,
        priced_calls: u64,
    }

    impl Totals {
        fn add(&mut self, input: u64, output: u64, total: u64, cost: Option<f64>) {
            self.calls += 1;
            self.input_tokens += input;
            self.output_tokens += output;
            self.total_tokens += total;
            if let Some(c) = cost {
                self.cost_usd += c;
                self.priced_calls += 1;
            }
        }
        fn json(&self) -> serde_json::Value {
            serde_json::json!({
                "calls": self.calls,
                "input_tokens": self.input_tokens,
                "output_tokens": self.output_tokens,
                "total_tokens": self.total_tokens,
                "cost_usd": round_cost_usd(self.cost_usd),
                "priced_calls": self.priced_calls,
            })
        }
    }

    /// `YYYY-MM-DD` (UTC). Howard Hinnant's civil-from-days, same as the TUI.
    fn day_label(ts_ms: i64) -> String {
        let z = ts_ms.div_euclid(DAY_MS) + 719468;
        let era = z.div_euclid(146097);
        let doe = z - era * 146097;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// `GET /api/stats?days=30`: usage report for the trailing window.
    pub fn stats(app: &App, req: &Request) -> Response {
        let days = query_usize(&req.query, "days", 30).clamp(1, 365) as i64;
        let to_ms = now_ms();
        let from_ms = to_ms - days * DAY_MS;
        let ledger = match Ledger::open(&app.data_dir.join("ledger.db")) {
            Ok(l) => l,
            Err(e) => return err_json(500, "LEDGER", &format!("open ledger: {e}")),
        };
        let rows = match ledger.usage_between(from_ms, to_ms) {
            Ok(r) => r,
            Err(e) => return err_json(500, "LEDGER", &format!("usage_between: {e}")),
        };
        let mut titles: BTreeMap<String, String> = BTreeMap::new();
        let mut titles_loaded = false;
        let mut totals = Totals::default();
        let mut by_model: BTreeMap<String, Totals> = BTreeMap::new();
        let mut by_run: BTreeMap<String, Totals> = BTreeMap::new();
        let mut by_day: BTreeMap<String, Totals> = BTreeMap::new();
        for entry in &rows {
            let Event::UsageRecorded {
                run_id,
                model,
                input_tokens,
                output_tokens,
                total_tokens,
                cost_usd,
                ..
            } = &entry.event
            else {
                continue;
            };
            if !titles_loaded {
                for r in ledger.list_runs(usize::MAX).unwrap_or_default() {
                    if let Some(t) = r.3 {
                        titles.insert(r.0, t);
                    }
                }
                titles_loaded = true;
            }
            totals.add(*input_tokens, *output_tokens, *total_tokens, *cost_usd);
            by_model.entry(model.clone()).or_default().add(
                *input_tokens,
                *output_tokens,
                *total_tokens,
                *cost_usd,
            );
            by_run.entry(run_id.clone()).or_default().add(
                *input_tokens,
                *output_tokens,
                *total_tokens,
                *cost_usd,
            );
            by_day.entry(day_label(entry.ts_ms)).or_default().add(
                *input_tokens,
                *output_tokens,
                *total_tokens,
                *cost_usd,
            );
        }
        // Highest cost first (then tokens), like the TUI's sort.
        let mut by_model: Vec<(String, Totals)> = by_model.into_iter().collect();
        by_model.sort_by(|a, b| {
            b.1.cost_usd
                .partial_cmp(&a.1.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.total_tokens.cmp(&a.1.total_tokens))
        });
        let mut by_run: Vec<(String, Totals)> = by_run.into_iter().collect();
        by_run.sort_by(|a, b| {
            b.1.cost_usd
                .partial_cmp(&a.1.cost_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.1.total_tokens.cmp(&a.1.total_tokens))
        });
        json_ok(serde_json::json!({
            "from_ms": from_ms,
            "to_ms": to_ms,
            "days": days,
            "totals": totals.json(),
            "by_model": by_model.iter().map(|(m, t)| serde_json::json!({"model": m, "totals": t.json()})).collect::<Vec<_>>(),
            "by_run": by_run.iter().map(|(r, t)| serde_json::json!({"run_id": r, "title": titles.get(r), "totals": t.json()})).collect::<Vec<_>>(),
            "by_day": by_day.iter().map(|(d, t)| serde_json::json!({"day": d, "totals": t.json()})).collect::<Vec<_>>(),
        }))
    }
}

pub mod logs {
    //! Log tailing over `<data_dir>/logs/*.log`.
    //!
    //! The files are the closed set from `pantheon_api::logging`
    //! (`KNOWN_LOGS`), and the line format is its `"TIMESTAMP LEVEL
    //! [component] message"` contract - parsed the same way the `pantheon
    //! logs` reader parses it. `GET /api/logs` tails; `GET /api/logs/stream`
    //! is Server-Sent Events over chunked encoding for live follow.

    use crate::{bad_json, err_json, json_ok, query_usize, App};
    use pantheon_api::logging::{self, Level};
    use pantheon_gateway::http::{write_chunk, Request, Response};
    use std::io::Read;
    use std::net::TcpStream;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Width of `YYYY-MM-DD HH:MM:SS.mmm`. Same fixed-offset parse as the
    /// CLI reader: splitting on whitespace looks equivalent and is not.
    const TS_WIDTH: usize = 23;
    /// How far back the SSE stream starts: the last 64 KiB of the log, like
    /// `tail` - enough context without dumping a huge file to the client.
    const STREAM_TAIL_BYTES: u64 = 64 * 1024;
    /// Poll interval for new log lines on the SSE stream.
    const STREAM_POLL: Duration = Duration::from_millis(500);
    /// Hard cap on one SSE stream's lifetime; clients reconnect for longer
    /// follows so a forgotten tab cannot hold a connection forever.
    const STREAM_MAX_AGE: Duration = Duration::from_secs(600);

    struct Filter {
        level: Option<Level>,
        grep: Option<String>,
        since_ms: Option<i64>,
    }

    fn parse_level(line: &str) -> Option<Level> {
        let after_ts = line.get(TS_WIDTH..)?;
        let rest = after_ts.trim_start();
        [Level::Debug, Level::Info, Level::Warning, Level::Error]
            .into_iter()
            .find(|l| rest.starts_with(l.as_str()))
    }

    /// Millis since epoch from the fixed-width timestamp prefix.
    fn parse_ts_ms(line: &str) -> Option<i64> {
        const D: &[std::ops::Range<usize>] = &[0..4, 5..7, 8..10];
        const T: &[std::ops::Range<usize>] = &[11..13, 14..16, 17..19];
        const MS: std::ops::Range<usize> = 20..23;
        let ts = line.get(..TS_WIDTH)?;
        let num = |r: std::ops::Range<usize>| -> Option<i64> { ts.get(r)?.parse().ok() };
        let (y, mo, d) = (num(D[0].clone())?, num(D[1].clone())?, num(D[2].clone())?);
        let (h, mi, sec) = (num(T[0].clone())?, num(T[1].clone())?, num(T[2].clone())?);
        let millis = num(MS)?;
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
            return None;
        }
        Some(((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + millis)
    }

    fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn keep(line: &str, f: &Filter) -> bool {
        if let Some(min) = f.level {
            // Lines without a parseable level are kept: dropping them would
            // hide multi-line continuations and foreign writes.
            if let Some(l) = parse_level(line) {
                if (l as u8) < (min as u8) {
                    return false;
                }
            }
        }
        if let Some(since) = f.since_ms {
            if let Some(ts) = parse_ts_ms(line) {
                if ts < since {
                    return false;
                }
            }
        }
        if let Some(g) = &f.grep {
            if !line.contains(g.as_str()) {
                return false;
            }
        }
        true
    }

    fn resolve_source(source: &str) -> Result<&'static str, String> {
        match source {
            "agent" => Ok(logging::AGENT_LOG),
            "errors" | "error" => Ok(logging::ERRORS_LOG),
            "gateway" => Ok(logging::GATEWAY_LOG),
            other => Err(format!(
                "unknown log '{other}'; known: agent, errors, gateway"
            )),
        }
    }

    fn log_path(app: &App, source: &str) -> Result<Option<PathBuf>, String> {
        let file = resolve_source(source)?;
        let Some(dir) = logging::log_dir() else {
            return Ok(None);
        };
        // Belt and suspenders: the file name comes from the closed KNOWN_LOGS
        // set, but never let a path escape the log dir.
        let p = dir.join(file);
        if !p.starts_with(&dir) {
            return Err("invalid log path".to_string());
        }
        let _ = app;
        Ok(Some(p))
    }

    fn parse_filter(req: &Request) -> Result<Filter, Response> {
        let level = match req.query.get("level") {
            Some(s) if !s.is_empty() => match Level::parse(s) {
                Some(l) => Some(l),
                None => return Err(bad_json("level must be debug|info|warning|error")),
            },
            _ => None,
        };
        let since_ms = match req.query.get("since") {
            Some(s) if !s.is_empty() => match parse_since(s) {
                Some(ms) => Some(ms),
                None => return Err(bad_json("since must be like 30m, 2h, 1d")),
            },
            _ => None,
        };
        Ok(Filter {
            level,
            grep: req.query.get("grep").cloned().filter(|s| !s.is_empty()),
            since_ms,
        })
    }

    fn parse_since(s: &str) -> Option<i64> {
        let t = s.trim().to_lowercase();
        let (num, unit) = t.split_at(t.len().saturating_sub(1));
        let n: i64 = num.trim().parse().ok()?;
        let secs = match unit {
            "s" => n,
            "m" => n * 60,
            "h" => n * 3600,
            "d" => n * 86_400,
            _ => return None,
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Some(now - secs * 1000)
    }

    /// Read the whole file, keep matching lines, return the last `tail`.
    fn tail_lines(path: &PathBuf, f: &Filter, tail: usize) -> Result<Vec<String>, String> {
        let mut text = String::new();
        std::fs::File::open(path)
            .map_err(|e| format!("open {}: {e}", path.display()))?
            .read_to_string(&mut text)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        let kept: Vec<String> = text
            .lines()
            .filter(|l| keep(l, f))
            .map(str::to_string)
            .collect();
        let start = kept.len().saturating_sub(tail);
        Ok(kept[start..].to_vec())
    }

    /// `GET /api/logs?source=agent|errors|gateway&level=&tail=200&grep=&since=`
    pub fn tail(app: &App, req: &Request) -> Response {
        let source = req
            .query
            .get("source")
            .map(String::as_str)
            .unwrap_or("agent");
        let filter = match parse_filter(req) {
            Ok(f) => f,
            Err(r) => return r,
        };
        let tail_n = query_usize(&req.query, "tail", 200).min(5000);
        let path = match log_path(app, source) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return json_ok(serde_json::json!({
                    "source": source, "lines": [], "note": "no log directory yet",
                }))
            }
            Err(e) => return bad_json(&e),
        };
        if !path.exists() {
            return json_ok(serde_json::json!({
                "source": source, "lines": [],
                "note": "log file not created yet. It appears on the first turn or warning",
            }));
        }
        match tail_lines(&path, &filter, tail_n) {
            Ok(lines) => json_ok(serde_json::json!({"source": source, "lines": lines})),
            Err(e) => err_json(500, "LOGS", &e),
        }
    }

    /// `GET /api/logs/stream?source=&level=`: SSE follow. Sends the current
    /// tail first, then new matching lines every 500ms, for up to 10 minutes
    /// or until the client disconnects (a write error ends the stream).
    pub fn stream(app: &App, req: &Request) -> Response {
        let source = req
            .query
            .get("source")
            .map(String::as_str)
            .unwrap_or("agent")
            .to_string();
        let filter = match parse_filter(req) {
            Ok(f) => f,
            Err(r) => return r,
        };
        let path = match log_path(app, &source) {
            Ok(Some(p)) => p,
            Ok(None) => return bad_json("no log directory yet"),
            Err(e) => return bad_json(&e),
        };
        let token_note = format!("following {source}");
        let run = move |sock: &TcpStream| {
            // Best-effort: every write error means the client went away.
            let send = |sock: &TcpStream, payload: &str| -> bool {
                let frame = format!("data: {}\n\n", payload);
                write_chunk(sock, frame.as_bytes()).is_ok()
            };
            let _ = send(sock, &serde_json::json!({"note": token_note}).to_string());
            let mut offset: u64 = 0;
            // Start at the tail: like the CLI's follow, we show what's there
            // and then stream.
            if let Ok(meta) = std::fs::metadata(&path) {
                offset = meta.len().saturating_sub(STREAM_TAIL_BYTES);
            }
            let mut pending = String::new();
            let deadline = std::time::Instant::now() + STREAM_MAX_AGE;
            loop {
                if std::time::Instant::now() > deadline {
                    break;
                }
                std::thread::sleep(STREAM_POLL);
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(offset);
                if len < offset {
                    offset = 0; // truncated/rotated under us
                    pending.clear();
                }
                if len == offset {
                    // Heartbeat so proxies don't kill an idle stream.
                    let _ = write_chunk(sock, b": ping\n\n");
                    continue;
                }
                let mut f = match std::fs::File::open(&path) {
                    Ok(f) => f,
                    Err(_) => continue,
                };
                use std::io::Seek;
                if f.seek(std::io::SeekFrom::Start(offset)).is_err() {
                    continue;
                }
                let mut buf = Vec::new();
                if f.read_to_end(&mut buf).is_err() {
                    continue;
                }
                offset = len;
                pending.push_str(&String::from_utf8_lossy(&buf));
                // Only whole lines: a partial trailing line is a write in
                // progress.
                let upto = match pending.rfind('\n') {
                    Some(p) => p + 1,
                    None => continue,
                };
                let chunk: String = pending.drain(..upto).collect();
                for line in chunk.lines() {
                    if !keep(line, &filter) {
                        continue;
                    }
                    let payload = serde_json::json!({"line": line}).to_string();
                    if !send(sock, &payload) {
                        return;
                    }
                }
            }
        };
        Response::ChunkedStream {
            content_type: "text/event-stream",
            stream: Box::new(run),
        }
    }
}
