//! The unified serve surface: one listener, one token, one port serves
//! the dashboard control plane (`/api/*` + PWA assets) AND the AG-UI
//! routes (`/agui/*`) AND the voice endpoints. Behavioral proof, over
//! real sockets, that the single listener `pantheon dashboard` and
//! `pantheon serve` start enforces every route group's pre-move auth
//! rules behind the one token.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use pantheon_dashboard::{App, DashboardMount};
use pantheon_gateway::http::{generate_token, spawn_test_server, ServerConfig};
use pantheon_runtime::agui_serve::{AguiMount, AguiServeConfig};

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

fn boot() -> (u16, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let token = generate_token();
    let dash = DashboardMount::new(App {
        data_dir: dir.path().to_path_buf(),
        token: token.clone(),
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
    });
    let agui = AguiMount {
        cfg: AguiServeConfig {
            data_dir: dir.path().to_path_buf(),
            host: "127.0.0.1".into(),
            port: 0,
            genui_base: "http://127.0.0.1:9/agui/blob".into(),
            auth_token: Some(token.clone()),
            voice: pantheon_gateway::voice::VoiceEdge::unconfigured(),
            live_voice: std::sync::Arc::new(
                pantheon_gateway::live_voice::LiveVoiceConfig::disabled(),
            ),
        },
    };
    // The single-token invariant, exactly as the CLI builds it: the
    // gateway's auth context comes from the dashboard mount's own
    // `auth_ctx()`, so both route groups enforce the same token.
    let auth = dash.auth_ctx();
    let cfg = ServerConfig {
        bind_addr: "127.0.0.1:0".into(),
        auth,
        mounts: vec![std::sync::Arc::new(dash), std::sync::Arc::new(agui)],
    };
    let (port, _tok) = spawn_test_server(cfg);
    (port, token, dir)
}

fn bearer(token: &str) -> Vec<(String, String)> {
    vec![("authorization".into(), format!("Bearer {token}"))]
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("valid JSON")
}

#[test]
fn unified_listener_serves_dashboard_and_api_behind_one_token() {
    let (port, token, _dir) = boot();
    let none: Vec<(String, String)> = vec![];
    // The PWA index is public, as before the move.
    let r = raw_request(port, "GET", "/", &none, None);
    assert_eq!(r.status, 200);
    assert!(r.body.contains("Pantheon"));
    // Dashboard API routes need the token.
    let r = raw_request(port, "GET", "/api/overview", &none, None);
    assert_eq!(r.status, 401, "API without token must be 401");
    let r = raw_request(
        port,
        "GET",
        &format!("/api/overview?token={token}"),
        &none,
        None,
    );
    assert_eq!(r.status, 200, "API with ?token= must pass: {}", r.body);
    let r = raw_request(
        port,
        "GET",
        "/api/overview",
        &[("x-pantheon-token".into(), token)],
        None,
    );
    assert_eq!(r.status, 200, "API with header token must pass");
}

#[test]
fn unified_dashboard_mutation_host_guard() {
    let (port, token, _dir) = boot();
    // A Host that does not match the bind address is rejected on mutation.
    // (The explicit host header is sent after the default one; last wins.)
    let bad = vec![
        ("x-pantheon-token".into(), token.to_string()),
        ("host".into(), "evil.example.com".into()),
    ];
    let r = raw_request(port, "POST", "/api/approvals/x/grant", &bad, Some("{}"));
    assert_eq!(r.status, 403, "mismatched Host must be 403");
}

#[test]
fn unified_agui_health_rpc_and_stream_behind_one_token() {
    let (port, token, _dir) = boot();
    let none: Vec<(String, String)> = vec![];
    // Health is public but keeps its origin check.
    let r = raw_request(port, "GET", "/agui/health", &none, None);
    assert_eq!(r.status, 200);
    assert_eq!(r.body, r#"{"ok":true}"#);

    // RPC without a token is 401 in the pre-move JSON shape.
    let rpc = r#"{"jsonrpc":"2.0","id":1,"method":"definitely.not.a.method"}"#;
    let r = raw_request(port, "POST", "/agui/rpc", &none, Some(rpc));
    assert_eq!(r.status, 401);
    assert!(r.body.contains("unauthorized"));

    // With `Authorization: Bearer` the request reaches the dispatcher: a
    // bad method returns the JSON-RPC method-not-found error.
    let r = raw_request(port, "POST", "/agui/rpc", &bearer(&token), Some(rpc));
    assert_eq!(r.status, 200, "dispatch: {}", r.body);
    let v = json(&r.body);
    assert_eq!(v["error"]["code"], -32601);
    assert!(v["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("method not found"));

    // ?token= on /agui/stream (EventSource cannot set headers) passes
    // auth; the missing ?run= then produces the handler's 400.
    let r = raw_request(
        port,
        "GET",
        &format!("/agui/stream?token={token}&run="),
        &none,
        None,
    );
    assert_eq!(r.status, 400);
    assert!(r.body.contains("missing ?run="), "body: {}", r.body);
}
