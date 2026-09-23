//! AG-UI serve: std-only HTTP shim for the interactive path.
//! GET /agui/stream (SSE replay + 25s long-poll), POST /agui/rpc (JSON-RPC),
//! GET /agui/blob/<task> (signed generative-UI bytes), GET /agui/health.
//! One TcpListener, one thread per connection, ledger polling for liveness.
use crate::rpc::Dispatcher;
use pantheon_gateway::{
    frames_for_entries, parse_last_event_id, valid_task_id, GenUiSigner, SseEncoder, UiFrame,
};
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
    fn effective_genui_base(&self) -> String {
        self.genui_base
            .replace("http://0.0.0.0", "http://127.0.0.1")
            .replace("http://[::]", "http://[::1]")
    }
    pub fn signer(&self) -> GenUiSigner {
        let secret = std::env::var("PANTHEON_GENUI_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| b"pantheon-dev-genui-secret".to_vec());
        GenUiSigner::new(self.effective_genui_base(), secret)
    }
    pub fn dispatcher(&self) -> Dispatcher {
        crate::agui::dispatcher_for_with_hint_and_host(
            self.data_dir.clone(),
            self.port,
            self.effective_genui_base(),
            &self.host,
        )
    }
}
/// Minimal, dependency-free AG-UI client. It is intentionally a smoke-test
/// surface rather than a framework: create a run, follow its SSE stream, and
/// answer approval frames through the same RPC endpoint.
pub const WEB_UI: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Pantheon AG-UI</title>
<style>
body{font:16px system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem}#log{white-space:pre-wrap;border:1px solid #ddd;padding:1rem;min-height:18rem}button{padding:.45rem .8rem;margin:.2rem}
</style>
<h1>Pantheon</h1>
<form id="send"><input id="text" required placeholder="Ask something" style="width:70%"><button>Send</button></form>
<button id="cancel" disabled>Cancel run</button><div id="actions"></div><pre id="log"></pre>
<script>
const $=s=>document.querySelector(s), log=s=>{$('#log').textContent+=s+'\n'};
let rpcId=1, run='', es;
async function rpc(method,params={}){let r=await fetch('/agui/rpc',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({jsonrpc:'2.0',id:rpcId++,method,params})});let j=await r.json();if(j.error)throw Error(j.error.message);return j.result}
function showActions(f){if(f.name!=='requested')return;const actions=$('#actions');actions.innerHTML='';for(const [label,answer] of [['Grant','grant'],['Deny','deny']]){const b=document.createElement('button');b.textContent=label;b.onclick=async()=>{try{await rpc('agui.'+answer,{run_id:run,scope:f.text});actions.innerHTML=''}catch(e){log(e.message)}};actions.appendChild(b)}}
$('#cancel').onclick=async()=>{if(!run)return;try{await rpc('agui.cancel',{run_id:run});$('#cancel').disabled=true;log('run canceled')}catch(e){log(e.message)}};
$('#send').onsubmit=async e=>{e.preventDefault();const text=$('#text').value;$('#text').value='';try{if(!run){const r=await rpc('agui.send',{text});run=r.run_id;$('#cancel').disabled=false;es=new EventSource('/agui/stream?run='+encodeURIComponent(run)+'&thread='+encodeURIComponent(r.thread_id));for(const kind of ['run','text','tool','state','genui'])es.addEventListener(kind,e=>{const f=JSON.parse(e.data);log(kind+': '+f.text)});es.addEventListener('approval',e=>showActions(JSON.parse(e.data)))}else{await rpc('agui.send',{run_id:run,text})}}catch(e){log(e.message)}};
</script>"##;

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
                if s == "completed" || s == "failed" || s == "canceled" {
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
    if !valid_task_id(&task) {
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
    let sup = match pantheon_runtime::Supervisor::open(cfg.data_dir.clone()) {
        Ok(sup) => sup,
        Err(_) => {
            respond(
                stream,
                500,
                "application/json",
                br#"{"error":"ledger unavailable"}"#,
            );
            return;
        }
    };
    match sup.artifact(&task) {
        Ok(Some(artifact)) => respond(stream, 200, &artifact.mime, &artifact.bytes),
        Ok(None) => respond(
            stream,
            404,
            "application/json",
            br#"{"error":"no such artifact"}"#,
        ),
        Err(_) => respond(
            stream,
            500,
            "application/json",
            br#"{"error":"artifact read failed"}"#,
        ),
    }
}
fn route_is(path: &str, route: &str) -> bool {
    path == route || path.starts_with(&format!("{route}?"))
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
    if method == "GET" && (path == "/" || path == "/agui" || path == "/agui/") {
        respond(&mut s, 200, "text/html; charset=utf-8", WEB_UI.as_bytes());
    } else if method == "GET" && route_is(&path, "/agui/stream") {
        handle_stream(&mut s, &cfg, &path, &headers);
    } else if method == "POST" && route_is(&path, "/agui/rpc") {
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
pub fn serve(mut cfg: ServeConfig) -> std::io::Result<()> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let listener = TcpListener::bind(&addr)?;
    let bound = listener.local_addr()?;
    if cfg.port == 0 {
        cfg.port = bound.port();
        cfg.genui_base = cfg
            .genui_base
            .replace(&format!(":0/"), &format!(":{}/", cfg.port));
    }
    eprintln!(
        "pantheon agui on http://{}:{}/agui/stream",
        cfg.host, cfg.port
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn request(cfg: ServeConfig, request: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(addr).unwrap();
        let server = listener.accept().unwrap().0;
        std::thread::spawn(move || handle_one(server, cfg));
        client.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn blob_route_requires_a_valid_signature() {
        let dir = std::env::temp_dir().join(format!("pantheon-blob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = pantheon_runtime::Supervisor::open(dir.clone()).unwrap();
        sup.put_artifact("task-1", "text/plain", b"hello").unwrap();
        let cfg = ServeConfig {
            data_dir: dir,
            host: "127.0.0.1".into(),
            port: 43219,
            genui_base: "http://127.0.0.1:43219/agui/blob".into(),
        };
        let signed = cfg.signer().sign("task-1", "text/plain", 60_000);
        let path = signed.url.split_once("http://127.0.0.1:43219").unwrap().1;
        let response = request(
            cfg.clone(),
            &format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"),
        );
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("hello"));
        let bad = request(
            cfg,
            "GET /agui/blob/task-1?exp=1&sig=00 HTTP/1.1\r\nHost: localhost\r\n\r\n",
        );
        assert!(bad.starts_with("HTTP/1.1 403 Forbidden"));
    }
}
