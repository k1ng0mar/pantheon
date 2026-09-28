//! Runs, overview, transcript export, and prune.
//!
//! All reads come from the real event ledger (`pantheon_storage::Ledger`);
//! the transcript is rebuilt with `pantheon_runtime::session`'s own
//! functions, so the dashboard shows exactly what the TUI would.

use crate::server::{Request, Response};
use crate::{bad_json, err_json, json_ok, query_usize, App};
use pantheon_api::events::Event;
use pantheon_api::logging::redact;
use pantheon_runtime::session::{rebuild_transcript, TranscriptItem};
use pantheon_storage::Ledger;
use std::collections::HashMap;

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
    }
    r
}

fn run_json(
    run_id: &str,
    status: &str,
    created_ms: i64,
    title: Option<&str>,
    r: &Rollup,
) -> serde_json::Value {
    serde_json::json!({
        "id": run_id,
        "status": status,
        "created_ms": created_ms,
        "title": title.unwrap_or(""),
        "model": r.model,
        "provider": r.provider,
        "input_tokens": r.input_tokens,
        "output_tokens": r.output_tokens,
        "cost_usd": (r.cost_usd * 100.0).round() / 100.0,
        "ended_ms": r.ended_ms,
        "turns": r.turns,
        "tool_calls": r.tool_calls,
        "approvals_pending": r.approvals_pending,
    })
}

/// `GET /api/overview`: KPI cards — totals, costs, pending approvals,
/// scheduler counts, gateway state. Empty ledger = zeros, not an error.
pub fn overview(app: &App) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let runs = ledger.list_runs(10_000).unwrap_or_default();
    let now_ms = now_ms();
    let day_ms: i64 = 86_400_000;
    let mut total = 0u64;
    let mut by_status: HashMap<String, u64> = HashMap::new();
    let mut cost_24h = 0.0f64;
    let mut tokens_24h = 0u64;
    let mut pending_approvals = 0u64;
    for (id, status, created_ms, _title) in &runs {
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
    let jobs = super::schedule::read_jobs(&app.data_dir).unwrap_or_default();
    let active_jobs = jobs.iter().filter(|j| !j.paused).count();
    json_ok(serde_json::json!({
        "runs": {"total": total, "by_status": by_status},
        "last_24h": {
            "cost_usd": (cost_24h * 100.0).round() / 100.0,
            "tokens": tokens_24h,
        },
        "approvals_pending": pending_approvals,
        "schedule": {"total": jobs.len(), "active": active_jobs},
    }))
}

/// `GET /api/runs?q=&status=&limit=`: run list with rollups.
pub fn list(app: &App, req: &Request) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let limit = query_usize(&req.query, "limit", 100).min(1000);
    let q = req.query.get("q").map(|s| s.to_lowercase());
    let status_filter = req.query.get("status").cloned();
    let mut out = Vec::new();
    for (id, status, created_ms, title) in ledger.list_runs(limit * 4).unwrap_or_default() {
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
        out.push(run_json(&id, &status, created_ms, title.as_deref(), &r));
        if out.len() >= limit {
            break;
        }
    }
    json_ok(serde_json::json!({"runs": out}))
}

/// A redacted timeline entry: tool arguments go through the same
/// [`redact`] pass the TUI's log views use, so a key pasted into a tool
/// arg never reaches the browser.
fn timeline_item(e: &pantheon_storage::LedgerEntry) -> serde_json::Value {
    let kind: &str = match &e.event {
        Event::RunStarted { .. } => "run_started",
        Event::RunCompleted { .. } => "run_completed",
        Event::RunFailed { .. } => "run_failed",
        Event::RunCanceled { .. } => "run_canceled",
        Event::TurnStarted { .. } => "turn_started",
        Event::TurnCompleted { .. } => "turn_completed",
        Event::TurnParked { .. } => "turn_parked",
        Event::ModelRequested { .. } => "model_requested",
        Event::ModelCompleted { .. } => "model_completed",
        Event::UsageRecorded { .. } => "usage",
        Event::ApprovalRequested { .. } => "approval_requested",
        Event::ApprovalGranted { .. } => "approval_granted",
        Event::ApprovalDenied { .. } => "approval_denied",
        Event::AgentSpawned { .. } => "agent_spawned",
        Event::AgentMessage { .. } => "agent_message",
        Event::AgentCompleted { .. } => "agent_completed",
        Event::SessionTitled { .. } => "titled",
        _ => "other",
    };
    let detail = match &e.event {
        Event::RunFailed { code, .. } => Some(code.clone()),
        Event::RunCanceled { reason, .. } => Some(reason.clone()),
        Event::TurnParked { reason, .. } => Some(reason.clone()),
        Event::TurnStarted { turn_id, .. } => Some(format!("turn {turn_id}")),
        Event::ModelRequested { model, .. } => Some(model.clone()),
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
        _ => None,
    };
    serde_json::json!({"seq": e.seq, "ts_ms": e.ts_ms, "kind": kind, "detail": detail})
}

/// `GET /api/runs/:id`: detail with transcript + redacted timeline.
pub fn detail(app: &App, run_id: &str) -> Response {
    let ledger = match ledger(app) {
        Ok(l) => l,
        Err(r) => return r,
    };
    let entries = match ledger.replay(run_id) {
        Ok(e) if !e.is_empty() => e,
        Ok(_) => return err_json(404, "NOT_FOUND", "unknown run"),
        Err(e) => return err_json(500, "LEDGER", &format!("replay: {e}")),
    };
    let r = rollup(&entries);
    let transcript: Vec<serde_json::Value> = rebuild_transcript(entries.clone())
        .into_iter()
        .map(|item| match item {
            TranscriptItem::Message(m) => serde_json::json!({
                "type": "message",
                "role": format!("{:?}", m.role).to_lowercase(),
                "content": redact(&m.content),
            }),
            TranscriptItem::Reasoning(t) => serde_json::json!({
                "type": "reasoning",
                "content": redact(&t),
            }),
        })
        .collect();
    let timeline: Vec<serde_json::Value> = entries.iter().map(timeline_item).collect();
    let header = ledger
        .list_runs(10_000)
        .unwrap_or_default()
        .into_iter()
        .find(|(id, _, _, _)| id == run_id);
    let (status, created_ms, title) = match header {
        Some((_, s, c, t)) => (s, c, t.unwrap_or_default()),
        None => ("unknown".to_string(), 0, String::new()),
    };
    let mut v = run_json(run_id, &status, created_ms, Some(&title), &r);
    v["transcript"] = serde_json::Value::Array(transcript);
    v["timeline"] = serde_json::Value::Array(timeline);
    json_ok(v)
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
                        md.push_str(&format!("## {:?}\n\n{}\n\n", m.role, redact(&m.content)));
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
                        "content": redact(&m.content),
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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
