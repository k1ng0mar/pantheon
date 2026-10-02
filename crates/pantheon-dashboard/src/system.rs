//! Gateway status/restart, reflection status/trigger, consolidation
//! status/trigger.
//!
//! Status reads are the real APIs (`pantheon_gateway::service::status`,
//! the reflect audit's last-run summary, the consolidation state file).
//! Triggers call back into the real CLI (`spawn_pantheon`) instead of
//! reimplementing pass machinery - the dashboard is a control plane, not
//! a second runtime.

use crate::util::atomic_write;
use crate::{bad_json, body_json, err_json, json_ok, spawn_pantheon, App};
use pantheon_gateway::http::{Request, Response};
use std::path::Path;

/// Resolved nightly enable state from the on-disk config: the single
/// enable rule ([`pantheon_api::config::nightly_enabled`]) - explicit
/// flag wins, else the `[nightly.model]` pin (or
/// `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env) implies
/// on. Replaces the old raw `nightly.enabled` TOML bool read, which
/// cannot express the `Option<bool>` flag or the pin-implied default.
fn nightly_enabled_resolved(data_dir: &Path) -> bool {
    pantheon_api::config::Config::load(data_dir)
        .ok()
        .and_then(|c| c.nightly)
        .is_some_and(|s| pantheon_api::config::nightly_enabled(&s))
}

/// Next scheduled nightly run in epoch ms, if any: the earliest next
/// fire among the non-paused `pantheon schedule nightly` jobs (the
/// [`pantheon_scheduler::NIGHTLY_TASK_MARKER`] task). `None` when no
/// nightly job is scheduled.
fn next_nightly_run_ms(data_dir: &Path) -> Option<i64> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    pantheon_scheduler::load_jobs(data_dir)
        .ok()
        .unwrap_or_default()
        .iter()
        .filter(|s| s.job.task == pantheon_scheduler::NIGHTLY_TASK_MARKER)
        .filter_map(|s| s.job.next_fire_ms(now_ms, s.last_run))
        .min()
}

/// `GET /api/nightly/status` - the nightly enable state behind the
/// dashboard toggle and the mobile app's toggle. The shared surface for
/// enable path 4 (dashboard / mobile-app toggle):
///
/// ```json
/// {
///   "enabled": true,            // resolved: the single enable rule
///   "reason": "explicit flag on", // why: explicit flag on|off,
///                               // on via [nightly.model] pin,
///                               // on via PANTHEON_NIGHTLY_* env,
///                               // off (no flag, no model pin)
///   "explicit": true,           // the raw `enabled` flag (null = absent)
///   "model_pin": false,         // `[nightly.model]` table present
///   "next_run_ms": 1790449200000, // next `schedule nightly` fire, if any
///   "last_run_ms": 0,
///   "last_summary": "..."
/// }
/// ```
///
/// The write half is `POST /api/nightly/enabled`.
pub fn nightly_status(app: &App) -> Response {
    let section = pantheon_api::config::Config::load(&app.data_dir)
        .ok()
        .and_then(|c| c.nightly);
    let enabled = section
        .as_ref()
        .is_some_and(pantheon_api::config::nightly_enabled);
    let reason = section
        .as_ref()
        .map(pantheon_api::config::nightly_enabled_reason)
        .unwrap_or("off (no flag, no model pin)");
    let state = pantheon_nightly::NightlyState::load(&app.data_dir).unwrap_or_default();
    json_ok(serde_json::json!({
        "enabled": enabled,
        "reason": reason,
        "explicit": section.as_ref().and_then(|s| s.enabled),
        "model_pin": section.as_ref().is_some_and(pantheon_api::config::nightly_model_pin_present),
        "next_run_ms": next_nightly_run_ms(&app.data_dir),
        "last_run_ms": state.last_run_ms,
        "last_summary": pantheon_nightly::last_run_summary(&app.data_dir),
        "last_repairs_fixed": state.last_repairs_fixed,
        "last_repairs_contained": state.last_repairs_contained,
    }))
}

/// `POST /api/nightly/enabled` - `{enabled: bool, confirm: true}`.
/// The dashboard / mobile-app toggle (enable path 4): writes the
/// explicit `[nightly] enabled` flag - the same flag `/nightly on|off`
/// and a manual config edit write. There is no parallel store; the
/// toggle round-trips API → config file →
/// [`pantheon_api::config::nightly_enabled`].
///
/// The write is a raw TOML table merge (parse to `toml::Value`, set the
/// `nightly.enabled` key, write back) - deliberately NOT a
/// `Config::load` → mutate → `Config::save` round-trip, which silently
/// deletes keys the typed document does not declare (D-3). Unknown keys
/// are warned about at load; a toggle must never delete them.
///
/// Without `confirm: true` the request is rejected (same two-phase
/// convention as the other dashboard mutations). The response is the
/// fresh `GET /api/nightly/status` document, so the client renders the
/// toggle from the resolved rule without a second read.
pub fn nightly_set_enabled(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return bad_json("nightly enable requires confirm: true");
    }
    let enabled = match body.get("enabled").and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => return bad_json("body must be {enabled: bool, confirm: true}"),
    };
    // A missing config.toml starts from an empty document (equivalent to
    // the defaults for this write); a present-but-unparseable one is a
    // 500, never a silent default.
    let path = app.data_dir.join("config.toml");
    let mut doc: toml::Value = if path.exists() {
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                return err_json(500, "CONFIG", &format!("read {}: {e}", path.display()));
            }
        };
        match raw.parse() {
            Ok(d) => d,
            Err(e) => {
                return err_json(500, "CONFIG", &format!("parse {}: {e}", path.display()));
            }
        }
    } else {
        toml::Value::Table(toml::map::Map::new())
    };
    let table = match doc.as_table_mut() {
        Some(t) => t,
        None => return err_json(500, "CONFIG", "config.toml root is not a table"),
    };
    let nightly = table
        .entry("nightly".to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let nightly_table = match nightly.as_table_mut() {
        Some(t) => t,
        None => return bad_json("[nightly] exists but is not a table"),
    };
    nightly_table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    let text = toml::to_string_pretty(&doc).unwrap_or_default();
    if let Err(e) = atomic_write(&path, &text) {
        return err_json(500, "CONFIG", &format!("write {}: {e}", path.display()));
    }
    nightly_status(app)
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

/// `POST /api/gateway/restart` - `{confirm: true}`. Restarts via the real
/// `pantheon gateway restart` path, in the background (200).
///
/// Probes first: on a standalone `pantheon dashboard` there is no
/// installed service, so the restart would silently no-op - that is a
/// plain 409 saying so, not an "accepted" toast. Spawn failures are
/// surfaced (500) instead of swallowed with `let _ =`.
pub fn gateway_restart(_app: &App, req: &Request) -> Response {
    if !req_body_confirmed(req) {
        return bad_json("restart requires confirm: true");
    }
    let st = pantheon_gateway::service::status();
    let installed = st.installed.map(|m| m.as_str().to_string());
    let Some(mechanism) = installed.clone() else {
        return err_json(
            409,
            "GATEWAY",
            "no gateway service installed: nothing to restart (this dashboard was started standalone, not under `pantheon serve`)",
        );
    };
    match spawn_pantheon(&["gateway", "restart"]) {
        Ok(()) => {
            json_ok(serde_json::json!({"ok": true, "accepted": true, "mechanism": mechanism}))
        }
        Err(e) => err_json(
            500,
            "GATEWAY",
            &format!("spawn `pantheon gateway restart`: {e}"),
        ),
    }
}

/// `GET /api/reflect/status` - kept as a legacy route name; reports the
/// unified nightly pass status.
pub fn reflect_status(app: &App) -> Response {
    json_ok(serde_json::json!({
        "enabled": nightly_enabled_resolved(&app.data_dir),
        "last_run": pantheon_nightly::last_run_summary(&app.data_dir),
    }))
}

/// `POST /api/reflect/run` - `{dry_run?, confirm: true}`. Runs the real
/// `pantheon nightly` CLI in the background (200).
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
        let _ = spawn_pantheon(&["nightly", "--dry-run"]);
    } else {
        let _ = spawn_pantheon(&["nightly"]);
    }
    json_ok(serde_json::json!({"ok": true, "accepted": true, "dry_run": dry_run}))
}

/// `GET /api/consolidate/status` - kept as a legacy route name; reports
/// the unified nightly pass state.
pub fn consolidate_status(app: &App) -> Response {
    let state = pantheon_nightly::NightlyState::load(&app.data_dir).unwrap_or_default();
    json_ok(serde_json::json!({
        "enabled": nightly_enabled_resolved(&app.data_dir),
        "last_run_ms": state.last_run_ms,
        "last_summary": pantheon_nightly::last_run_summary(&app.data_dir),
        "last_repairs_fixed": state.last_repairs_fixed,
        "last_repairs_contained": state.last_repairs_contained,
        "pending": state.last_pending,
    }))
}

/// `POST /api/consolidate/run` - `{dry_run?, confirm: true}`. Runs the
/// real `pantheon nightly` CLI in the background (200).
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
        let _ = spawn_pantheon(&["nightly", "--dry-run"]);
    } else {
        let _ = spawn_pantheon(&["nightly"]);
    }
    json_ok(serde_json::json!({"ok": true, "accepted": true, "dry_run": dry_run}))
}

fn req_body_confirmed(req: &Request) -> bool {
    body_json(req)
        .ok()
        .and_then(|v| v.get("confirm")?.as_bool())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_app() -> (App, std::path::PathBuf) {
        let n = DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-system-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        let app = App {
            data_dir: dir.clone(),
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            turn_children: Default::default(),
            swarm: swarm::orchestrator_for(&dir),
        };
        (app, dir)
    }

    fn json_req(body: &str) -> Request {
        Request {
            method: "POST".to_string(),
            path: "/api/nightly/enabled".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    fn buffered(resp: Response) -> (u16, Vec<u8>) {
        match resp {
            Response::Buffered { status, body, .. } => (status, body),
            _ => panic!("expected a buffered response"),
        }
    }

    /// D-3: the nightly toggle must flip `nightly.enabled` without
    /// dropping keys the typed Config document does not declare - the
    /// old `Config::load` / mutate / `save` path silently erased them.
    #[test]
    fn nightly_toggle_preserves_unknown_keys() {
        let (app, dir) = test_app();
        std::fs::write(
            dir.join("config.toml"),
            "[nightly]\nenabled = true\n\n[custom]\nlegacy_key = \"keep me\"\n",
        )
        .expect("seed config");
        let (status, body) = buffered(nightly_set_enabled(
            &app,
            &json_req(r#"{"enabled":false,"confirm":true}"#),
        ));
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let raw = std::fs::read_to_string(dir.join("config.toml")).expect("read back config");
        let doc: toml::Value = raw.parse().expect("config still parses");
        assert_eq!(
            doc.get("nightly").and_then(|t| t.get("enabled")),
            Some(&toml::Value::Boolean(false))
        );
        assert_eq!(
            doc.get("custom").and_then(|t| t.get("legacy_key")),
            Some(&toml::Value::String("keep me".to_string())),
            "unknown key must survive the toggle"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing config.toml starts from an empty document; the toggle
    /// must create `[nightly] enabled = ...` rather than 500.
    #[test]
    fn nightly_toggle_on_missing_config() {
        let (app, dir) = test_app();
        let (status, body) = buffered(nightly_set_enabled(
            &app,
            &json_req(r#"{"enabled":true,"confirm":true}"#),
        ));
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        let raw = std::fs::read_to_string(dir.join("config.toml")).expect("read back config");
        let doc: toml::Value = raw.parse().expect("config parses");
        assert_eq!(
            doc.get("nightly").and_then(|t| t.get("enabled")),
            Some(&toml::Value::Boolean(true))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
