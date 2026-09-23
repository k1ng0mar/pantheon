//! AG-UI serve: std-only HTTP shim for the interactive path.
//! GET /agui/stream (SSE replay + 25s long-poll), POST /agui/rpc (JSON-RPC),
//! GET /agui/blob/<task> (signed generative-UI bytes), GET /agui/health.
//! One TcpListener, one thread per connection, ledger polling for liveness.
use crate::rpc::Dispatcher;
use pantheon_gateway::{frames_for_entries, parse_last_event_id, GenUiSigner, SseEncoder, UiFrame};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub data_dir: PathBuf,
    pub host: String,
    pub port: u16,
    pub genui_base: String,
}
impl ServeConfig {
    pub fn signer(&self) -> GenUiSigner {
        let secret = std::env::var("PANTHEON_GENUI_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| b"pantheon-dev-genui-secret".to_vec());
        GenUiSigner::new(self.genui_base.clone(), secret)
    }
    pub fn dispatcher(&self) -> Dispatcher {
        crate::agui::dispatcher_for(self.data_dir.clone())
    }
}
fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Error",
        _ => "Error",
    }
}
fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reason(code), body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}
fn query_map(path: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = path.split_once('?').map(|(_, q)| q) {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}
fn thread_for(data_dir: &PathBuf, run_id: &str, fallback: &str) -> String {
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    let p = data_dir.join("threads.json");
    if let Ok(raw) = std::fs::read_to_string(&p) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) {
            if let Some(t) = map.get(run_id) {
                return t.clone();
            }
        }
    }
    format!("cli:{run_id}")
}
pub fn remember_thread(data_dir: &PathBuf, run_id: &str, thread_id: &str) {
    let p = data_dir.join("threads.json");
    let mut map: HashMap<String, String> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    map.insert(run_id.to_string(), thread_id.to_string());
    if let Ok(raw) = serde_json::to_string(&map) {
        let _ = std::fs::create_dir_all(data_dir);
        let _ = std::fs::write(&p, raw);
    }
}
pub fn snapshot_frames(data_dir: &PathBuf, run_id: &str, thread: &str, after: i64) -> Vec<UiFrame> {
    let sup = match pantheon_runtime::Supervisor::open(data_dir.clone()) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let entries = sup.replay(run_id).unwrap_or_default();
    let thread_id = thread_for(data_dir, run_id, thread);
    let mut frames = frames_for_entries(&entries, &thread_id);
    frames.retain(|f| f.id > after || f.id == 0);
    frames
}
fn handle_stream(
    stream: &mut TcpStream,
    cfg: &ServeConfig,
    path: &str,
    headers: &HashMap<String, String>,
) {
    let q = query_map(path);
    let run_id = q.get("run").cloned().unwrap_or_default();
    let thread = q.get("thread").cloned().unwrap_or_default();
    if run_id.is_empty() {
        respond(
            stream,
            400,
            "application/json",
            br#"{"error":"missing ?run="}"#,
        );
        return;
    }
    let mut after: i64 = q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    if after == 0 {
        if let Some(h) = headers.get("last-event-id") {
            if let Some(n) = parse_last_event_id(h) {
                after = n;
            }
        }
    }
    let enc = SseEncoder;
    let _ = stream.write_all(enc.head().as_bytes());
    let mut sent = after;
    for f in snapshot_frames(&cfg.data_dir, &run_id, &thread, after) {
        if f.id > sent {
            sent = f.id;
        }
        let _ = stream.write_all(enc.frame(&f).as_bytes());
    }
    let _ = stream.flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let fresh = snapshot_frames(&cfg.data_dir, &run_id, &thread, sent);
        let mut progressed = false;
        for f in fresh {
            if f.id == 0 {
                continue;
            }
            if f.id > sent {
                sent = f.id;
                progressed = true;
                if stream.write_all(enc.frame(&f).as_bytes()).is_err() {
                    return;
                }
            }
        }
        if progressed && stream.flush().is_err() {
            return;
        }
        if let Ok(sup) = pantheon_runtime::Supervisor::open(cfg.data_dir.clone()) {
            if let Ok(Some(s)) = sup.ledger_status(&run_id) {
                if s == "completed" || s == "failed" {
                    break;
                }
            }
        }
    }
}
fn handle_rpc(stream: &mut TcpStream, cfg: &ServeConfig, body: &str) {
    let d = cfg.dispatcher();
    let mut out = Vec::new();
    for resp in d.handle_line(body.trim()) {
        out.push(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()));
    }
    respond(stream, 200, "application/json", out.join("\n").as_bytes());
}
fn handle_blob(stream: &mut TcpStream, cfg: &ServeConfig, path: &str) {
    let without_q = path.split('?').next().unwrap_or(path);
    let task = without_q.trim_start_matches("/agui/blob/").to_string();
    let q = query_map(path);
    let exp: i64 = q.get("exp").and_then(|v| v.parse().ok()).unwrap_or(0);
    let sig = q.get("sig").cloned().unwrap_or_default();
    if task.is_empty() || task.contains('/') || task.contains('.') {
        respond(stream, 400, "application/json", br#"{"error":"bad task"}"#);
        return;
    }
    if !cfg.signer().verify(&task, exp, &sig) {
        respond(
            stream,
            403,
            "application/json",
            br#"{"error":"bad signature or expired"}"#,
        );
        return;
    }
    match std::fs::read(cfg.data_dir.join("genui").join(&task)) {
        Ok(bytes) => respond(stream, 200, "application/octet-stream", &bytes),
        Err(_) => respond(
            stream,
            404,
            "application/json",
            br#"{"error":"no such artifact"}"#,
        ),
    }
}
fn handle_one(stream: TcpStream, cfg: ServeConfig) {
    let mut s = stream;
    let (method, path, headers, body) = {
        let mut reader = BufReader::new(s.try_clone().unwrap());
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("/").to_string();
        let mut headers = HashMap::new();
        let mut content_len = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                return;
            }
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_lowercase();
                let v = v.trim().to_string();
                if k == "content-length" {
                    content_len = v.parse().unwrap_or(0);
                }
                headers.insert(k, v);
            }
        }
        let mut body = vec![0u8; content_len];
        if content_len > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        (
            method,
            path,
            headers,
            String::from_utf8_lossy(&body).to_string(),
        )
    };
    if method == "GET" && path.starts_with("/agui/stream") {
        handle_stream(&mut s, &cfg, &path, &headers);
    } else if method == "POST" && path.starts_with("/agui/rpc") {
        handle_rpc(&mut s, &cfg, &body);
    } else if method == "GET" && path.starts_with("/agui/blob/") {
        handle_blob(&mut s, &cfg, &path);
    } else if method == "GET" && path == "/agui/health" {
        respond(&mut s, 200, "application/json", br#"{"ok":true}"#);
    } else {
        respond(
            &mut s,
            404,
            "application/json",
            br#"{"error":"unknown agui route"}"#,
        );
    }
}
/// Blocking serve loop: one thread per connection.
pub fn serve(cfg: ServeConfig) -> std::io::Result<()> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = TcpListener::bind(&addr)?;
    eprintln!("pantheon agui on http://{addr}/agui/stream");
    let cfg = Arc::new(cfg);
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let c = ServeConfig {
                    data_dir: cfg.data_dir.clone(),
                    host: cfg.host.clone(),
                    port: cfg.port,
                    genui_base: cfg.genui_base.clone(),
                };
                std::thread::spawn(move || handle_one(s, c));
            }
            Err(e) => eprintln!("agui accept: {e}"),
        }
    }
    Ok(())
}
