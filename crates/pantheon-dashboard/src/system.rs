//! Gateway status/restart, reflection status/trigger, consolidation
//! status/trigger.
//!
//! Status reads are the real APIs (`pantheon_gateway::service::status`,
//! the reflect audit's last-run summary, the consolidation state file).
//! Triggers call back into the real CLI (`spawn_pantheon`) instead of
//! reimplementing pass machinery — the dashboard is a control plane, not
//! a second runtime.

use crate::server::{Request, Response};
use crate::{bad_json, body_json, json_ok, spawn_pantheon, App};

fn config_bool(data_dir: &std::path::Path, section: &str, key: &str) -> bool {
    let text = std::fs::read_to_string(data_dir.join("config.toml")).unwrap_or_default();
    text.parse::<toml::Value>()
        .ok()
        .and_then(|v| v.get(section)?.get(key)?.as_bool())
        .unwrap_or(false)
}

/// `GET /api/gateway/status`
pub fn gateway_status(_app: &App) -> Response {
    let s = pantheon_gateway::service::status();
    json_ok(serde_json::json!({
        "detected": s.detected.as_str(),
        "installed": s.installed.map(|m| m.as_str()),
        "running": s.running,
    }))
}

/// `POST /api/gateway/restart` — `{confirm: true}`. Restarts via the real
/// `pantheon gateway restart` path, in the background (202).
pub fn gateway_restart(_app: &App, req: &Request) -> Response {
    if !req_body_confirmed(req) {
        return bad_json("restart requires confirm: true");
    }
    spawn_pantheon(&["gateway", "restart"]);
    json_ok(serde_json::json!({"ok": true, "accepted": true}))
}

/// `GET /api/reflect/status`
pub fn reflect_status(app: &App) -> Response {
    json_ok(serde_json::json!({
        "enabled": config_bool(&app.data_dir, "reflect", "enabled"),
        "last_run": pantheon_reflect::audit::last_run_summary(&app.data_dir),
    }))
}

/// `POST /api/reflect/run` — `{dry_run?, confirm: true}`. Runs the real
/// `pantheon reflect` CLI in the background (202).
pub fn reflect_run(_app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return bad_json("run requires confirm: true");
    }
    let dry_run = body
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if dry_run {
        spawn_pantheon(&["reflect", "--dry-run"]);
    } else {
        spawn_pantheon(&["reflect"]);
    }
    json_ok(serde_json::json!({"ok": true, "accepted": true, "dry_run": dry_run}))
}

/// `GET /api/consolidate/status`
pub fn consolidate_status(app: &App) -> Response {
    let state = pantheon_consolidate::state::load(&app.data_dir).unwrap_or_default();
    json_ok(serde_json::json!({
        "enabled": config_bool(&app.data_dir, "consolidation", "enabled"),
        "last_run_ms": state.last_run_ms,
        "last_summary": state.last_summary,
        "promoted_keys": state.promoted_keys.len(),
    }))
}

/// `POST /api/consolidate/run` — `{dry_run?, confirm: true}`.
pub fn consolidate_run(_app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return bad_json("run requires confirm: true");
    }
    let dry_run = body
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if dry_run {
        spawn_pantheon(&["consolidate", "--dry-run"]);
    } else {
        spawn_pantheon(&["consolidate"]);
    }
    json_ok(serde_json::json!({"ok": true, "accepted": true, "dry_run": dry_run}))
}

fn req_body_confirmed(req: &Request) -> bool {
    body_json(req)
        .ok()
        .and_then(|v| v.get("confirm")?.as_bool())
        .unwrap_or(false)
}
