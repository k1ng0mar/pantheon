//! Behavioral tests for the web dashboard control plane.
//!
//! Spins up real gateway listeners on ephemeral loopback ports with a
//! dashboard-only mount and exercises auth, endpoint shapes, redaction,
//! and CRUD over raw TCP (the serve surface is std-only, so the test
//! client is too). The dashboard's own `spawn_test_server` shim is not
//! used: these tests go through `pantheon_gateway::http::spawn_test_server`
//! with a `ServerConfig` holding a `DashboardMount`, exactly the path the
//! CLI's unified listener uses.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use pantheon_api::events::Event;
use pantheon_dashboard::{App, DashboardMount};
use pantheon_storage::Ledger;

struct Resp {
    status: u16,
    body: String,
}

fn raw_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).expect("write");
    let mut reader = BufReader::new(s);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let mut len: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header");
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("content-length:") {
            len = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Content-Length:") {
            len = rest.trim().parse().unwrap_or(0);
        }
    }
    let mut body = String::new();
    if len > 0 {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).expect("body");
        body = String::from_utf8_lossy(&buf).into_owned();
    }
    Resp { status, body }
}

fn auth(token: &str) -> Vec<(String, String)> {
    vec![
        ("x-pantheon-token".into(), token.to_string()),
        ("origin".into(), "http://127.0.0.1".into()),
    ]
}

fn boot() -> (u16, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let token = pantheon_gateway::http::generate_token();
    let app = App {
        data_dir: dir.path().to_path_buf(),
        token,
        bind: "127.0.0.1".to_string(),
        bind_all: false,
        on_approval: None,
        send_locks: Default::default(),
        turn_children: Default::default(),
        swarm: std::sync::Arc::new(pantheon_runtime::swarm_exec::SwarmOrchestrator::new(
            std::sync::Arc::new(pantheon_runtime::swarm_exec::ScriptedWorker::new()),
            None,
        )),
    };
    let mount = DashboardMount::new(app);
    let auth = mount.auth_ctx();
    let cfg = pantheon_gateway::http::ServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        auth,
        mounts: vec![std::sync::Arc::new(mount)],
    };
    let (port, token) = pantheon_gateway::http::spawn_test_server(cfg);
    (port, token, dir)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("valid JSON")
}

#[test]
fn dashboard_unauthenticated_api_is_401() {
    let (port, _token, _dir) = boot();
    let none: Vec<(String, String)> = vec![];
    let r = raw_request(port, "GET", "/api/overview", &none, None);
    assert_eq!(r.status, 401, "missing token must be 401");
    let wrong = vec![("x-pantheon-token".into(), "wrong".into())];
    let r = raw_request(port, "GET", "/api/overview", &wrong, None);
    assert_eq!(r.status, 401, "wrong token must be 401");
    // Non-API routes are public.
    let r = raw_request(port, "GET", "/", &none, None);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Pantheon"));
}

#[test]
fn dashboard_overview_empty_and_host_guard() {
    let (port, token, _dir) = boot();
    let a = auth(&token);
    let r = raw_request(port, "GET", "/api/overview", &a, None);
    assert_eq!(r.status, 200);
    let v = json(&r.body);
    assert_eq!(v["approvals_pending"], 0);
    assert_eq!(v["runs"]["total"], 0);
    // A Host that does not match the bind address is rejected on mutation.
    // (The explicit host header is sent after the default one; last wins.)
    let bad = vec![
        ("x-pantheon-token".into(), token.to_string()),
        ("host".into(), "evil.example.com".into()),
    ];
    let r = raw_request(port, "POST", "/api/approvals/x/grant", &bad, Some("{}"));
    assert_eq!(r.status, 403, "bad Host must be rejected on mutation");
}

#[test]
fn dashboard_mutation_requires_same_origin() {
    let (port, token, _dir) = boot();
    let evil = vec![
        ("x-pantheon-token".into(), token.to_string()),
        ("origin".into(), "https://attacker.example".into()),
    ];
    let r = raw_request(port, "POST", "/api/approvals/nope/grant", &evil, Some("{}"));
    assert_eq!(r.status, 403, "cross-origin mutation must be rejected");
    // Same-origin mutation passes the origin check (then 404s on unknown scope).
    let r = raw_request(
        port,
        "POST",
        "/api/approvals/nope/grant",
        &auth(&token),
        Some("{}"),
    );
    assert_eq!(r.status, 404);
}

#[test]
fn dashboard_approval_grant_deny_round_trip() {
    let (port, token, dir) = boot();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    ledger
        .append(&Event::RunStarted {
            run_id: "eval-run-1".into(),
        })
        .expect("run started");
    ledger
        .append(&Event::ApprovalRequested {
            run_id: "eval-run-1".into(),
            scope: "shell:rm -rf /tmp/x".into(),
        })
        .expect("approval 1");
    ledger
        .append(&Event::ApprovalRequested {
            run_id: "eval-run-1".into(),
            scope: "tool:publish".into(),
        })
        .expect("approval 2");

    let a = auth(&token);
    let r = raw_request(port, "GET", "/api/approvals", &a, None);
    assert_eq!(r.status, 200);
    let v = json(&r.body);
    assert_eq!(v["approvals"].as_array().unwrap().len(), 2);

    let r = raw_request(
        port,
        "POST",
        "/api/approvals/shell%3Arm%20-rf%20%2Ftmp%2Fx/grant",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 200, "grant: {}", r.body);
    let r = raw_request(
        port,
        "POST",
        "/api/approvals/tool%3Apublish/deny",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 200, "deny: {}", r.body);

    let r = raw_request(port, "GET", "/api/approvals", &a, None);
    let v = json(&r.body);
    assert!(
        v["approvals"].as_array().unwrap().is_empty(),
        "both decided"
    );

    let decisions = ledger
        .replay("eval-run-1")
        .expect("replay")
        .into_iter()
        .filter(|e| {
            matches!(
                e.event,
                Event::ApprovalGranted { .. } | Event::ApprovalDenied { .. }
            )
        })
        .count();
    assert_eq!(decisions, 2, "decisions hit the ledger");
}

#[test]
fn dashboard_redact_never_leaks_secrets() {
    use pantheon_api::logging::redact;
    let secret = "sk-live-9f8e7d6c5b4a";
    let out = redact(&format!("api_key={secret}"));
    assert!(
        !out.contains(secret),
        "raw secret must not survive redaction: {out}"
    );
    // And the approvals endpoint returns valid JSON with an empty queue.
    let (port, token, _dir) = boot();
    let r = raw_request(port, "GET", "/api/approvals", &auth(&token), None);
    assert_eq!(r.status, 200);
    assert!(json(&r.body)["approvals"].as_array().unwrap().is_empty());
}

#[test]
fn dashboard_config_validation_failure_and_preview_flow() {
    let (port, token, dir) = boot();
    std::fs::write(
        dir.path().join("config.toml"),
        "[profile.default]\npolicy = \"open\"\n",
    )
    .expect("seed config");
    let a = auth(&token);

    let r = raw_request(port, "GET", "/api/config/schema", &a, None);
    assert_eq!(r.status, 200);
    let schema = json(&r.body);
    assert!(schema["fields"]
        .as_array()
        .unwrap()
        .iter()
        .any(|f| f["path"] == "profile.default.policy"));

    // Invalid policy preset is rejected with 400 on confirm.
    let r = raw_request(
        port,
        "PUT",
        "/api/config",
        &a,
        Some(r#"{"changes":{"profile.default.policy":"bogus-preset"},"confirm":true}"#),
    );
    assert_eq!(r.status, 400, "invalid preset must be rejected: {}", r.body);

    // Preview (no confirm) shows the changed names without applying or validating.
    let r = raw_request(
        port,
        "PUT",
        "/api/config",
        &a,
        Some(r#"{"changes":{"profile.default.policy":"coder_memory"},"confirm":false}"#),
    );
    assert_eq!(r.status, 200);
    let v = json(&r.body);
    assert!(v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c == "profile.default.policy"));

    // Confirm applies.
    let r = raw_request(
        port,
        "PUT",
        "/api/config",
        &a,
        Some(r#"{"changes":{"profile.default.policy":"coder_memory"},"confirm":true}"#),
    );
    assert_eq!(r.status, 200);
    let text = std::fs::read_to_string(dir.path().join("config.toml")).expect("read config");
    assert!(text.contains("coder_memory"), "applied value persisted");
}

#[test]
fn dashboard_env_redaction_and_atomic_update() {
    let (port, token, dir) = boot();
    std::fs::write(
        dir.path().join(".env"),
        "OPENAI_API_KEY=sk-secret-123\nPLAIN=hello\n",
    )
    .expect("seed env");
    let a = auth(&token);

    let r = raw_request(port, "GET", "/api/env", &a, None);
    assert_eq!(r.status, 200);
    assert!(
        !r.body.contains("sk-secret-123"),
        "raw value must never appear"
    );
    let v = json(&r.body);
    let keys: Vec<&str> = v["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains(&"OPENAI_API_KEY") && keys.contains(&"PLAIN"));

    // Preview shows names only, never values.
    let r = raw_request(
        port,
        "PUT",
        "/api/env",
        &a,
        Some(r#"{"upserts":{"OPENAI_API_KEY":"sk-rotated-456"},"confirm":false}"#),
    );
    assert_eq!(r.status, 200);
    assert!(!r.body.contains("sk-rotated-456"));

    // Confirm applies atomically, preserving the other key.
    let r = raw_request(
        port,
        "PUT",
        "/api/env",
        &a,
        Some(r#"{"upserts":{"OPENAI_API_KEY":"sk-rotated-456"},"confirm":true}"#),
    );
    assert_eq!(r.status, 200);
    let text = std::fs::read_to_string(dir.path().join(".env")).expect("read env");
    assert!(text.contains("sk-rotated-456"));
    assert!(text.contains("PLAIN=hello"), "other keys preserved");
}

#[test]
fn dashboard_schedule_crud_and_toggle() {
    let (port, token, _dir) = boot();
    let a = auth(&token);

    let r = raw_request(
        port,
        "POST",
        "/api/schedule/jobs",
        &a,
        Some(r#"{"name":"nightly","every":"30m","task":"summarize","confirm":true}"#),
    );
    assert_eq!(r.status, 200, "create: {}", r.body);
    let id = json(&r.body)["job"]["id"].as_str().unwrap().to_string();

    let r = raw_request(port, "GET", "/api/schedule/jobs", &a, None);
    assert_eq!(json(&r.body)["jobs"].as_array().unwrap().len(), 1);

    // Invalid schedule is rejected.
    let r = raw_request(
        port,
        "POST",
        "/api/schedule/jobs",
        &a,
        Some(r#"{"name":"bad","every":"whenever","task":"x","confirm":true}"#),
    );
    assert_eq!(r.status, 400);

    let r = raw_request(
        port,
        "PUT",
        &format!("/api/schedule/jobs/{id}"),
        &a,
        Some(r#"{"paused":true}"#),
    );
    assert_eq!(r.status, 200);
    assert!(json(&r.body)["job"]["paused"].as_bool().unwrap());

    let r = raw_request(
        port,
        "PUT",
        &format!("/api/schedule/jobs/{id}"),
        &a,
        Some(r#"{"paused":false}"#),
    );
    assert_eq!(r.status, 200);
    assert!(!json(&r.body)["job"]["paused"].as_bool().unwrap());

    let r = raw_request(
        port,
        "DELETE",
        &format!("/api/schedule/jobs/{id}"),
        &a,
        None,
    );
    assert_eq!(r.status, 200);
    let r = raw_request(port, "GET", "/api/schedule/jobs", &a, None);
    assert!(json(&r.body)["jobs"].as_array().unwrap().is_empty());
}

#[test]
fn dashboard_stats_shape_on_empty_ledger() {
    let (port, token, _dir) = boot();
    let r = raw_request(port, "GET", "/api/stats?days=7", &auth(&token), None);
    assert_eq!(r.status, 200);
    let v = json(&r.body);
    assert_eq!(v["totals"]["calls"], 0);
    assert!(v["by_model"].as_array().unwrap().is_empty());
    assert!(v["by_run"].as_array().unwrap().is_empty());
    assert!(v["by_day"].as_array().unwrap().is_empty());
}

#[test]
fn dashboard_runs_list_export_prune() {
    let (port, token, dir) = boot();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    ledger
        .append(&Event::RunStarted {
            run_id: "eval-export-1".into(),
        })
        .expect("started");
    ledger
        .append(&Event::RunCompleted {
            run_id: "eval-export-1".into(),
        })
        .expect("completed");
    let a = auth(&token);

    let r = raw_request(port, "GET", "/api/runs?limit=10", &a, None);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("eval-export-1"));

    let r = raw_request(
        port,
        "GET",
        "/api/runs/eval-export-1/export?format=json",
        &a,
        None,
    );
    assert_eq!(r.status, 200);
    assert!(r.body.contains("eval-export-1"));

    let r = raw_request(
        port,
        "DELETE",
        "/api/runs/eval-export-1?confirm=true",
        &a,
        None,
    );
    assert_eq!(r.status, 200);
    let r = raw_request(port, "GET", "/api/runs?limit=10", &a, None);
    assert!(!r.body.contains("eval-export-1"), "pruned run must be gone");
}

#[test]
fn dashboard_plugins_tool_plugin_approve_disable() {
    let (port, token, dir) = boot();
    let a = auth(&token);
    // Install a fake third-party tool plugin.
    let plug = dir.path().join("plugins").join("demo-tool");
    std::fs::create_dir_all(&plug).unwrap();
    std::fs::write(
        plug.join("manifest.yaml"),
        "name: demo-tool\nversion: 1.0.0\ndescription: demo\nenabled: true\n",
    )
    .unwrap();

    // List shows it as unapproved.
    let r = raw_request(port, "GET", "/api/plugins", &a, None);
    assert_eq!(r.status, 200);
    let tool = || {
        let v = json(&raw_request(port, "GET", "/api/plugins", &a, None).body);
        v["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "demo-tool")
            .cloned()
            .expect("demo-tool listed")
    };
    assert_eq!(r.status, 200);
    assert_eq!(tool()["kind"], "tool");
    assert_eq!(tool()["approved"], false);

    // Approve records it in the unified approval store.
    let r = raw_request(
        port,
        "POST",
        "/api/plugins/tool/demo-tool/approve",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 200, "approve: {}", r.body);
    assert_eq!(json(&r.body)["approved"], true);
    assert_eq!(tool()["approved"], true);

    // Disable revokes the approval and flips the manifest flag.
    let r = raw_request(
        port,
        "POST",
        "/api/plugins/tool/demo-tool/disable",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 200, "disable: {}", r.body);
    assert_eq!(tool()["approved"], false);
    assert_eq!(tool()["enabled"], false);

    // Unknown plugin -> 404, unsafe name -> 400, unknown kind -> 400.
    let r = raw_request(
        port,
        "POST",
        "/api/plugins/tool/nope/approve",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 404);
    let r = raw_request(
        port,
        "POST",
        "/api/plugins/tool/..%5cevil/approve",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 400, "unsafe name must be 400, got {}", r.status);
    let r = raw_request(
        port,
        "POST",
        "/api/plugins/bogus/demo-tool/approve",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 400);
}

#[test]
fn dashboard_plugins_hook_plugin_approve_revoke() {
    let (port, token, dir) = boot();
    let a = auth(&token);
    // Install a fake hook plugin.
    let plug = dir.path().join("extensions").join("demo-hook");
    std::fs::create_dir_all(&plug).unwrap();
    std::fs::write(
        plug.join("plugin.yaml"),
        "name: demo-hook\nversion: 0.1.0\ndescription: demo hook\n",
    )
    .unwrap();

    let hook = || {
        let v = json(&raw_request(port, "GET", "/api/plugins", &a, None).body);
        v["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "demo-hook")
            .cloned()
            .expect("demo-hook listed")
    };
    assert_eq!(hook()["kind"], "hook");
    assert_eq!(hook()["approved"], false);

    let r = raw_request(
        port,
        "POST",
        "/api/plugins/hook/demo-hook/approve",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 200, "approve hook: {}", r.body);
    assert_eq!(hook()["approved"], true);

    // Approving twice: no longer pending -> 404.
    let r = raw_request(
        port,
        "POST",
        "/api/plugins/hook/demo-hook/approve",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 404);

    // Disable revokes the approval; the plugin stays installed.
    let r = raw_request(
        port,
        "POST",
        "/api/plugins/hook/demo-hook/disable",
        &a,
        Some("{}"),
    );
    assert_eq!(r.status, 200, "disable hook: {}", r.body);
    assert_eq!(json(&r.body)["approved"], false);
    assert_eq!(hook()["approved"], false);
}

// ------------------------------------------------- nightly toggle ------
//
// Enable path 4 (dashboard / mobile-app toggle): `POST
// /api/nightly/enabled` writes the explicit `[nightly] enabled` flag
// through the shared config document — the same flag `/nightly on|off`
// and a manual config edit write. The test round-trips API → config
// file → `pantheon_api::config::nightly_enabled`.

#[test]
fn dashboard_nightly_toggle_round_trips() {
    let (port, token, dir) = boot();
    let h = auth(&token);
    let toggle =
        |payload: &str| raw_request(port, "POST", "/api/nightly/enabled", &h, Some(payload));

    // The two-phase convention: no confirm, no mutation.
    let r = toggle(r#"{"enabled": true}"#);
    assert_eq!(r.status, 400, "confirm required: {}", r.body);

    // Bad shape is rejected before touching the config.
    let r = toggle(r#"{"enabled": "yes", "confirm": true}"#);
    assert_eq!(r.status, 400, "enabled must be bool: {}", r.body);

    // Toggle on from a fresh data dir (no config.toml yet).
    let r = toggle(r#"{"enabled": true, "confirm": true}"#);
    assert_eq!(r.status, 200, "toggle on: {}", r.body);
    let v = json(&r.body);
    assert_eq!(v["enabled"].as_bool(), Some(true));
    assert_eq!(v["reason"].as_str(), Some("explicit flag on"));

    // The flag landed in the shared config file...
    let raw = std::fs::read_to_string(dir.path().join("config.toml")).expect("config written");
    assert!(raw.contains("enabled = true"), "flag in file: {raw}");
    // ...and the single enable rule resolves it.
    let cfg: pantheon_api::config::Config = toml::from_str(&raw).expect("config parses");
    let section = cfg.nightly.as_ref().expect("[nightly] present");
    assert!(pantheon_api::config::nightly_enabled(section));

    // The status endpoint — the mobile app's read surface — agrees.
    let r = raw_request(port, "GET", "/api/nightly/status", &h, None);
    assert_eq!(r.status, 200, "status: {}", r.body);
    let v = json(&r.body);
    assert_eq!(v["enabled"].as_bool(), Some(true));
    assert_eq!(v["explicit"].as_bool(), Some(true));
    assert_eq!(v["model_pin"].as_bool(), Some(false));

    // Toggle off: explicit false, resolved off, file updated.
    let r = toggle(r#"{"enabled": false, "confirm": true}"#);
    assert_eq!(r.status, 200, "toggle off: {}", r.body);
    assert_eq!(json(&r.body)["enabled"].as_bool(), Some(false));
    let raw = std::fs::read_to_string(dir.path().join("config.toml")).expect("config written");
    let cfg: pantheon_api::config::Config = toml::from_str(&raw).expect("config parses");
    let section = cfg.nightly.as_ref().expect("[nightly] present");
    assert!(!pantheon_api::config::nightly_enabled(section));

    // The legacy status routes report the resolved rule too, not the
    // raw TOML bool.
    let r = raw_request(port, "GET", "/api/reflect/status", &h, None);
    assert_eq!(r.status, 200);
    assert_eq!(json(&r.body)["enabled"].as_bool(), Some(false));
    let r = raw_request(port, "GET", "/api/consolidate/status", &h, None);
    assert_eq!(r.status, 200);
    assert_eq!(json(&r.body)["enabled"].as_bool(), Some(false));
}

/// P0 (2026-10-01): percent-encoded / empty-segment variants of `/api/*`
/// must hit the token gate exactly like the canonical path. Before the
/// fix `auth_group` matched the RAW request path, so `GET /%61pi/runs`
/// returned 200 with the full run list and
/// `PATCH /%61pi/runs/run-1/queue/0` rewrote the queue — no token, no
/// Host/Origin check. Real HTTP through the real gateway chain on a temp
/// ledger, the way the audit reproduced it.
#[test]
fn dashboard_encoded_api_prefix_requires_token() {
    let (port, token, dir) = boot();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    ledger
        .append(&Event::RunStarted {
            run_id: "run-1".into(),
        })
        .expect("run started");
    ledger
        .set_queued_message("run-1", Some("original"))
        .expect("seed queue");
    drop(ledger);

    let none: Vec<(String, String)> = vec![];
    // Every bypass shape: 401 without a token (mutations would be 403 on
    // a bad Host/Origin, but the missing token rejects them first).
    let bypasses: [(&str, &str, Option<&str>); 9] = [
        ("GET", "/%61pi/runs", None),
        ("GET", "//api/runs", None),
        ("GET", "//%61pi/runs", None),
        ("GET", "/%61pi//runs", None),
        ("GET", "/api//runs", None),
        (
            "PATCH",
            "/%61pi/runs/run-1/queue/0",
            Some(r#"{"text":"PWNED"}"#),
        ),
        (
            "PATCH",
            "/api/runs/run-1/%71ueue/0",
            Some(r#"{"text":"PWNED"}"#),
        ),
        ("DELETE", "//api/runs/run-1?confirm=true", None),
        ("DELETE", "/%61pi/runs/run-1?confirm=true", None),
    ];
    for (method, path, body) in bypasses {
        let r = raw_request(port, method, path, &none, body);
        assert_eq!(r.status, 401, "{method} {path} without token must be 401");
    }

    // Nothing was mutated through the bypass.
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    assert_eq!(
        ledger.queued_messages("run-1").expect("queue"),
        vec!["original".to_string()],
        "queue must be untouched by the bypass"
    );
    assert!(
        !ledger.replay("run-1").expect("replay").is_empty(),
        "run-1 must still exist after the bypass"
    );
    drop(ledger);

    // The canonical paths still work with a valid token (no regression).
    let a = auth(&token);
    let r = raw_request(port, "GET", "/api/runs", &a, None);
    assert_eq!(r.status, 200, "authed GET /api/runs: {}", r.body);
    let r = raw_request(
        port,
        "PATCH",
        "/api/runs/run-1/queue/0",
        &a,
        Some(r#"{"text":"edited"}"#),
    );
    assert_eq!(r.status, 200, "authed PATCH queue: {}", r.body);
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    assert_eq!(
        ledger.queued_messages("run-1").expect("queue"),
        vec!["edited".to_string()],
        "authed PATCH must still edit the queue"
    );
    drop(ledger);
    let r = raw_request(port, "DELETE", "/api/runs/run-1?confirm=true", &a, None);
    assert_eq!(r.status, 200, "authed DELETE run: {}", r.body);
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    assert!(
        ledger.replay("run-1").expect("replay").is_empty(),
        "authed DELETE must still prune the run"
    );
}

/// Round-1 closed shapes still fail safe after the P0 fix: paths that do
/// not decode to `["api", ...]` are never routed to a handler — 404 from
/// the Public group, or 401 when the first segment really is `api`.
#[test]
fn dashboard_closed_path_shapes_still_fail_safe() {
    let (port, token, _dir) = boot();
    let a = auth(&token);
    // With a valid token (so auth can't mask routing): all 404.
    for path in ["/API/runs", "/api%2f/runs", "/api/../runs", "/%2561pi/runs"] {
        let r = raw_request(port, "GET", path, &a, None);
        assert_eq!(r.status, 404, "GET {path} with token must be 404");
    }
    // Without a token: still never a 200, and never routed to a handler.
    let none: Vec<(String, String)> = vec![];
    for (path, want) in [
        ("/API/runs", 404),     // Public -> dispatch 404s
        ("/api%2f/runs", 404),  // one segment "api/runs": Public -> 404
        ("/%2561pi/runs", 404), // decodes once to "%61pi": Public -> 404
        ("/api/../runs", 401),  // first segment IS "api": token gate
    ] {
        let r = raw_request(port, "GET", path, &none, None);
        assert_eq!(r.status, want, "GET {path} without token must be {want}");
    }
}
