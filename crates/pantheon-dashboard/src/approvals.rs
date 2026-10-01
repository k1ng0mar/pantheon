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
/// colons via `splitn(3, …)`.
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
    let mut out = Vec::new();
    for (run_id, status, created_ms, title, _project) in
        ledger.list_runs(10_000).unwrap_or_default()
    {
        if status != "awaiting_approval" {
            continue;
        }
        let pending = supervisor.pending_approvals(&run_id).unwrap_or_default();
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
    for (id, status, _, _, _) in ledger.list_runs(10_000).unwrap_or_default() {
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
    // approval resolves, but no turn is alive to continue it — without a
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
                eprintln!("dashboard: failed to spawn resume turn for run {run_id}: {e}");
            }
        }
    }
    json_ok(serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "granted": granted,
    }))
}
