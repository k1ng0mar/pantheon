//! Behavioral tests for the home session backend.
//!
//! The home session is the permanent, pinned session with the well-known id
//! `"home"`: auto-created on first access, never deletable, first in every
//! session list, and the default delivery target for scheduled jobs without
//! an explicit `--deliver` target (plus `--deliver mobile`).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use pantheon_api::events::Event;
use pantheon_dashboard::{App, DashboardMount};
use pantheon_storage::{Ledger, HOME_SESSION_ID};

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
        // Scripted worker + no judge: spawns never leave the process.
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
        label: "pantheon eval".to_string(),
    };
    let (port, token) = pantheon_gateway::http::spawn_test_server(cfg);
    (port, token, dir)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("valid JSON")
}

fn start_run(ledger: &Ledger, run_id: &str) {
    ledger
        .append(&Event::RunStarted {
            run_id: run_id.to_string(),
        })
        .unwrap();
}

#[test]
fn home_session_auto_created_on_first_access() {
    let ledger = Ledger::open_in_memory().unwrap();
    assert!(ledger.status(HOME_SESSION_ID).unwrap().is_none());
    // Access surfaces call ensure before reading; after it, home exists.
    ledger.ensure_home_session().unwrap();
    assert_eq!(
        ledger.status(HOME_SESSION_ID).unwrap().as_deref(),
        Some("running")
    );
    assert_eq!(
        ledger.run_title(HOME_SESSION_ID).unwrap().as_deref(),
        Some("Home")
    );
}

#[test]
fn home_session_pinned_first_despite_recency() {
    let ledger = Ledger::open_in_memory().unwrap();
    // Home exists first, then a strictly newer ordinary run.
    ledger.ensure_home_session().unwrap();
    start_run(&ledger, "run-newer");
    let recency = ledger.list_runs(10).unwrap();
    assert_eq!(
        recency[0].0, "run-newer",
        "sanity: recency order is newest first"
    );
    let pinned = Ledger::pin_home_first(recency);
    assert_eq!(pinned[0].0, HOME_SESSION_ID);
    assert_eq!(pinned[1].0, "run-newer");
}

#[test]
fn home_session_delete_is_rejected() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger.ensure_home_session().unwrap();
    let err = ledger.delete_run(HOME_SESSION_ID).unwrap_err();
    assert_eq!(err.code, "LEDGER_HOME_PROTECTED");
    // The session is still there afterwards.
    assert!(ledger.status(HOME_SESSION_ID).unwrap().is_some());
}

#[test]
fn dashboard_runs_list_auto_creates_pins_and_flags_home() {
    let (port, token, dir) = boot();
    // Seed a newer ordinary run before the first list call.
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    start_run(&ledger, "run-newer");
    drop(ledger);

    let r = raw_request(port, "GET", "/api/runs", &auth(&token), None);
    assert_eq!(r.status, 200, "body: {}", r.body);
    let runs = json(&r.body)["runs"]
        .as_array()
        .expect("runs array")
        .clone();
    assert!(runs.len() >= 2, "home + the seeded run");
    // Pinned first despite being older than run-newer...
    assert_eq!(runs[0]["id"], HOME_SESSION_ID);
    // ...carrying the is_home flag on the serialized form.
    assert_eq!(runs[0]["is_home"], serde_json::Value::Bool(true));
    assert_eq!(runs[0]["title"], serde_json::Value::String("Home".into()));
    assert_eq!(runs[1]["id"], serde_json::Value::String("run-newer".into()));
    assert_eq!(runs[1]["is_home"], serde_json::Value::Bool(false));
}

#[test]
fn dashboard_prune_rejects_home_deletion() {
    let (port, token, _dir) = boot();
    // Make sure home exists, then try to delete it with confirmation.
    let r = raw_request(port, "GET", "/api/runs", &auth(&token), None);
    assert_eq!(r.status, 200);
    let r = raw_request(
        port,
        "DELETE",
        "/api/runs/home?confirm=true",
        &auth(&token),
        None,
    );
    assert_eq!(r.status, 403, "body: {}", r.body);
    assert!(r.body.contains("HOME_PROTECTED"), "body: {}", r.body);
    // And it is still listed afterwards.
    let r = raw_request(port, "GET", "/api/runs", &auth(&token), None);
    let runs = json(&r.body)["runs"]
        .as_array()
        .expect("runs array")
        .clone();
    assert!(runs.iter().any(|v| v["id"] == HOME_SESSION_ID));
}

#[test]
fn default_delivery_routes_job_results_to_home() {
    use pantheon_gateway::schedule_delivery::{deliver_to_home_session, routes_via_home_session};
    // Routing decision: no explicit target and "mobile" go to home.
    assert!(routes_via_home_session(None));
    assert!(routes_via_home_session(Some("mobile")));
    assert!(!routes_via_home_session(Some("telegram")));
    assert!(!routes_via_home_session(Some("log")));

    // The delivery itself lands in the home session's ledger.
    let dir = tempfile::tempdir().expect("tempdir");
    let err = deliver_to_home_session(dir.path(), "nightly-job", "all quiet");
    assert!(err.is_none(), "delivery must not fail: {err:?}");
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    let entries = ledger.replay(HOME_SESSION_ID).unwrap();
    let found = entries.iter().any(|e| match &e.event {
        Event::RunProgress { detail, .. } => {
            detail.contains("nightly-job") && detail.contains("all quiet")
        }
        _ => false,
    });
    assert!(
        found,
        "job summary must be replayable from the home session"
    );
    // Home is pinned first in the listing the user opens.
    let pinned = Ledger::pin_home_first(ledger.list_runs(10).unwrap());
    assert_eq!(pinned[0].0, HOME_SESSION_ID);
}

#[test]
fn home_session_is_never_reported_stuck() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    // The run_leases table is created by RunLeaseStore in production
    // (Supervisor::open); without it stuck_runs cannot evaluate leases.
    let _leases = pantheon_storage::RunLeaseStore::open(&db).unwrap();
    ledger.ensure_home_session().unwrap();
    start_run(&ledger, "run-stuck");
    let ids: Vec<String> = ledger
        .stuck_runs()
        .unwrap()
        .into_iter()
        .map(|(id, ..)| id)
        .collect();
    assert!(ids.contains(&"run-stuck".to_string()));
    assert!(
        !ids.contains(&HOME_SESSION_ID.to_string()),
        "the permanent home session sits at running by design and is never a corpse"
    );
}
