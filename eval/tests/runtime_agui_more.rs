//! AG-UI serve surface tests: the mount's pure route-dispatch decisions,
//! exercised in-process through the public `HttpMount` interface.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral lives here and runs
//! via `cargo test -p pantheon-eval`.
//!
//! Socket-level auth behavior (one token for dashboard + AG-UI) lives in
//! `unified_serve.rs`; the AG-UI RPC command behavior lives in
//! `runtime_agui.rs`; voice edge behavior lives in `gateway_voice.rs`.
//! This file covers what those don't: which paths the mount claims, the
//! token-injected UI (including the hostile-token XSS guard), 404 routing,
//! the SSE wire shape, the live-voice socket takeover, signed-blob serving,
//! threads.json bookkeeping, and the serve-hint port.

use pantheon_gateway::http::{parse_query, AuthGroup, HttpMount, Request, Response};
use pantheon_runtime::agui::dispatcher_for_with_hint;
use pantheon_runtime::agui_serve::{remember_thread, snapshot_frames, AguiMount, AguiServeConfig};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;

fn test_cfg(data_dir: PathBuf) -> AguiServeConfig {
    AguiServeConfig {
        data_dir,
        host: "127.0.0.1".into(),
        port: 0,
        genui_base: "http://127.0.0.1:9/agui/blob".into(),
        auth_token: Some("sekrit".into()),
        voice: pantheon_gateway::voice::VoiceEdge::unconfigured(),
        live_voice: std::sync::Arc::new(pantheon_gateway::live_voice::LiveVoiceConfig::disabled()),
    }
}

fn mount() -> (AguiMount, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let m = AguiMount {
        cfg: test_cfg(dir.path().to_path_buf()),
    };
    (m, dir)
}

fn req(method: &str, target: &str, body: &str) -> Request {
    // Mirror the real parser: path is the raw target before '?', query is
    // parsed separately. A test that leaves the query in `path` would not
    // exercise what the server actually produces.
    let (path, q) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    Request {
        method: method.into(),
        path: path.into(),
        query: parse_query(q),
        headers: HashMap::new(),
        body: body.as_bytes().to_vec(),
    }
}

fn buffered(resp: Response) -> (u16, &'static str, Vec<u8>) {
    match resp {
        Response::Buffered {
            status,
            content_type,
            body,
            ..
        } => (status, content_type, body),
        _ => panic!("expected buffered response"),
    }
}

#[test]
fn mount_claims_agui_paths_and_root_only() {
    let (m, _dir) = mount();
    for p in [
        "/",
        "/agui",
        "/agui/",
        "/agui/stream",
        "/agui/rpc",
        "/agui/health",
    ] {
        assert_eq!(
            m.auth_group(&req("GET", p, "")),
            Some(AuthGroup::Agui),
            "{p}"
        );
    }
    for p in ["/api/runs", "/dashboard", "/other"] {
        assert_eq!(m.auth_group(&req("GET", p, "")), None, "{p}");
    }
}

#[test]
fn root_serves_the_token_injected_ui() {
    let (m, _dir) = mount();
    for p in ["/", "/agui", "/agui/"] {
        let (status, ctype, body) = buffered(m.handle(&req("GET", p, "")));
        assert_eq!(status, 200);
        assert_eq!(ctype, "text/html; charset=utf-8");
        let page = String::from_utf8(body).unwrap();
        assert!(page.contains("const TOKEN=\"sekrit\";"), "{p}");
    }
}

#[test]
fn hostile_token_cannot_break_out_of_script() {
    // A hostile token (e.g. hand-set via env) must not terminate the
    // <script> block or inject JS. Driven through the public mount so the
    // escaping contract is tested where it is actually served.
    let evil = "x\";alert(1);//</script><script>alert(2)";
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_cfg(dir.path().to_path_buf());
    cfg.auth_token = Some(evil.into());
    let m = AguiMount { cfg };
    let (_, _, body) = buffered(m.handle(&req("GET", "/", "")));
    let page = String::from_utf8(body).unwrap();
    assert!(!page.contains("</script><script>"), "script breakout");
    // Every quote is backslash-escaped, so the token cannot terminate the
    // JS string literal: no unescaped `";alert(1)` may appear.
    let raw = page.matches("\";alert(1)").count();
    let escaped = page.matches("\\\";alert(1)").count();
    assert!(raw > 0 && raw == escaped, "string breakout");
    // The token round-trips through JSON decoding instead.
    let line = page
        .lines()
        .find(|l| l.starts_with("const TOKEN="))
        .unwrap();
    let lit = line
        .trim_start_matches("const TOKEN=")
        .trim_end_matches(';');
    let decoded: String = serde_json::from_str(lit).unwrap();
    assert_eq!(decoded, evil);
}

#[test]
fn unknown_agui_route_is_404_json() {
    let (m, _dir) = mount();
    for (method, path) in [
        ("GET", "/agui/nope"),
        ("POST", "/agui/unknown"),
        ("DELETE", "/agui/rpc"),
    ] {
        let (status, ctype, body) = buffered(m.handle(&req(method, path, "")));
        assert_eq!(status, 404, "{method} {path}");
        assert_eq!(ctype, "application/json");
        assert_eq!(body, br#"{"error":"unknown agui route"}"#.as_slice());
    }
}

#[test]
fn stream_with_run_is_raw_non_chunked() {
    // Wire behavior: connection-close terminated, NOT chunked.
    let (m, _dir) = mount();
    match m.handle(&req("GET", "/agui/stream?run=x", "")) {
        Response::RawStream { content_type, .. } => {
            assert_eq!(content_type, "text/event-stream");
        }
        _ => panic!("expected RawStream"),
    }
}

#[test]
fn live_voice_route_takes_over_the_socket() {
    let (m, _dir) = mount();
    match m.handle(&req("GET", "/agui/voice/live", "")) {
        Response::Takeover { .. } => {}
        _ => panic!("expected Takeover"),
    }
}

#[test]
fn blob_route_requires_a_valid_signature() {
    let dir = tempfile::tempdir().unwrap();
    let sup = pantheon_runtime::Supervisor::open(dir.path().to_path_buf()).unwrap();
    sup.put_artifact("task-1", "text/plain", b"hello").unwrap();
    let m = AguiMount {
        cfg: test_cfg(dir.path().to_path_buf()),
    };
    let signed = m.cfg.signer().sign("task-1", "text/plain", 60_000);
    let path = signed.url.split_once("http://127.0.0.1:9").unwrap().1;
    let (status, ctype, body) = buffered(m.handle(&req("GET", path, "")));
    assert_eq!(status, 200);
    assert_eq!(ctype, "text/plain");
    assert_eq!(body, b"hello");
    let (status, _, _) = buffered(m.handle(&req("GET", "/agui/blob/task-1?exp=1&sig=00", "")));
    assert_eq!(status, 403);
    let (status, _, _) = buffered(m.handle(&req("GET", "/agui/blob/../x?exp=1&sig=00", "")));
    assert_eq!(status, 400);
}

#[test]
fn thread_mapping_persists_and_prefers_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let sup = pantheon_runtime::Supervisor::open(dir.path().to_path_buf()).unwrap();
    sup.start_run("r1").unwrap();
    sup.emit(pantheon_api::events::Event::ApprovalRequested {
        run_id: "r1".into(),
        scope: "call_0".into(),
    })
    .unwrap();
    // No mapping yet: falls back to the cli: prefix.
    let frames = snapshot_frames(dir.path(), "r1", "", 0);
    assert!(!frames.is_empty());
    assert!(frames.iter().all(|f| f.thread_id == "cli:r1"));
    // remember_thread persists; an empty explicit thread resolves to it.
    remember_thread(&dir.path().to_path_buf(), "r1", "t-9");
    let frames = snapshot_frames(dir.path(), "r1", "", 0);
    assert!(frames.iter().all(|f| f.thread_id == "t-9"));
    // An explicit thread wins over the remembered one.
    let frames = snapshot_frames(dir.path(), "r1", "explicit", 0);
    assert!(frames.iter().all(|f| f.thread_id == "explicit"));
    // A second run's mapping does not clobber the first.
    remember_thread(&dir.path().to_path_buf(), "r2", "t-10");
    let frames = snapshot_frames(dir.path(), "r1", "", 0);
    assert!(frames.iter().all(|f| f.thread_id == "t-9"));
}

#[test]
fn snapshot_frames_is_empty_for_an_unknown_run() {
    let dir = tempfile::tempdir().unwrap();
    assert!(snapshot_frames(dir.path(), "no-such-run", "", 0).is_empty());
}

#[test]
fn serve_hint_uses_configured_port() {
    let dir = tempfile::tempdir().unwrap();
    let d = dispatcher_for_with_hint(dir.path().to_path_buf(), 43219);
    let req = pantheon_runtime::rpc::Request {
        jsonrpc: "2.0".into(),
        id: pantheon_runtime::rpc::Id::Number(1),
        method: "agui.serve_hint".into(),
        params: Some(json!({"port": 1})),
    };
    let resp = d.dispatch(&req).unwrap();
    assert!(resp.is_success(), "{resp:?}");
    let v = resp.result.unwrap();
    assert_eq!(v["sse"], "http://127.0.0.1:43219/agui/stream");
}
