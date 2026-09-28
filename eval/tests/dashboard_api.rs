//! Behavioral tests for the web dashboard control plane.
//!
//! Spins up real dashboard servers on ephemeral loopback ports and
//! exercises auth, endpoint shapes, redaction, and CRUD over raw TCP
//! (the dashboard is std-only, so the test client is too).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use pantheon_api::events::Event;
use pantheon_dashboard::spawn_test_server;
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
    let (port, token) = spawn_test_server(dir.path().to_path_buf());
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
