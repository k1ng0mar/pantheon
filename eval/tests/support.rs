//! Shared scaffolding for dashboard HTTP eval tests.
//!
//! Boots a real gateway listener on an ephemeral loopback port with a
//! [`pantheon_dashboard::DashboardMount`] - exactly the path the CLI's
//! unified listener uses - and drives it with a std-only raw TCP client
//! (the serve surface is std-only, so the test client is too).
//!
//! Each consumer includes this file with:
//! `#[path = "support.rs"] mod support;`
//!
//! Note: this file is also compiled as its own (empty) test target by
//! cargo's auto-discovery; that is harmless.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use pantheon_dashboard::{App, DashboardMount};
use pantheon_runtime::swarm_exec::{ScriptedWorker, SwarmOrchestrator};

/// A buffered HTTP response.
pub struct Resp {
    pub status: u16,
    pub body: String,
}

impl Resp {
    /// Parse the body as JSON (panics when it is not valid JSON).
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).expect("response body is JSON")
    }
}

/// A booted dashboard server plus handles the tests need.
pub struct Dash {
    pub port: u16,
    pub token: String,
    /// Temp data dir; held so it outlives the test.
    pub dir: tempfile::TempDir,
    /// The swarm orchestrator wired into the App, so tests can assert
    /// on spawn state (e.g. fail-closed paths spawned nothing).
    pub swarm: Arc<SwarmOrchestrator>,
}

/// Boot a dashboard-only gateway listener on 127.0.0.1:0 with a fresh
/// temp data dir and a scripted (in-process, no-subprocess) swarm worker.
pub fn boot() -> Dash {
    let dir = tempfile::tempdir().expect("tempdir");
    let token = pantheon_gateway::http::generate_token();
    let swarm = Arc::new(SwarmOrchestrator::new(
        Arc::new(ScriptedWorker::new()),
        None,
    ));
    let app = App {
        data_dir: dir.path().to_path_buf(),
        token,
        bind: "127.0.0.1".to_string(),
        bind_all: false,
        on_approval: None,
        send_locks: Default::default(),
        turn_children: Default::default(),
        swarm: swarm.clone(),
    };
    let mount = DashboardMount::new(app);
    let auth = mount.auth_ctx();
    let cfg = pantheon_gateway::http::ServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        auth,
        mounts: vec![Arc::new(mount)],
        label: "pantheon eval".to_string(),
    };
    let (port, token) = pantheon_gateway::http::spawn_test_server(cfg);
    Dash {
        port,
        token,
        dir,
        swarm,
    }
}

fn raw_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(std::time::Duration::from_secs(15)))
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
        if let Some(rest) = line
            .strip_prefix("content-length:")
            .or_else(|| line.strip_prefix("Content-Length:"))
        {
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

impl Dash {
    fn auth_headers(&self) -> Vec<(String, String)> {
        vec![
            ("x-pantheon-token".into(), self.token.clone()),
            // Mutations additionally require a matching Origin.
            ("origin".into(), "http://127.0.0.1".into()),
        ]
    }

    /// One authenticated request. `body` of `None` sends no body.
    pub fn request(&self, method: &str, path: &str, body: Option<&str>) -> Resp {
        raw_request(self.port, method, path, &self.auth_headers(), body)
    }

    pub fn get(&self, path: &str) -> Resp {
        self.request("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &str) -> Resp {
        self.request("POST", path, Some(body))
    }

    pub fn put(&self, path: &str, body: &str) -> Resp {
        self.request("PUT", path, Some(body))
    }

    pub fn patch(&self, path: &str, body: &str) -> Resp {
        self.request("PATCH", path, Some(body))
    }

    pub fn delete(&self, path: &str) -> Resp {
        self.request("DELETE", path, None)
    }

    /// Write a file directly into the dashboard data dir (e.g.
    /// `config.toml`, or a hand-crafted gallery file).
    pub fn write_data_file(&self, name: &str, contents: &str) {
        let path = self.dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
    }
}
