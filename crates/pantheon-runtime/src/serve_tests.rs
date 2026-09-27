//! Tests for `pantheon_api::serve::tests` — sibling file so sources stay test-free.
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
    let sup = crate::Supervisor::open(dir.clone()).unwrap();
    sup.put_artifact("task-1", "text/plain", b"hello").unwrap();
    let cfg = ServeConfig {
        data_dir: dir,
        host: "127.0.0.1".into(),
        port: 43219,
        genui_base: "http://127.0.0.1:43219/agui/blob".into(),
        auth_token: None,
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

fn auth_cfg(token: &str) -> ServeConfig {
    ServeConfig {
        data_dir: std::env::temp_dir().join(format!("pantheon-auth-{}", std::process::id())),
        host: "127.0.0.1".into(),
        port: 0,
        genui_base: "http://127.0.0.1:9/agui/blob".into(),
        auth_token: Some(token.into()),
    }
}

#[test]
fn rpc_requires_the_token_when_configured() {
    let cfg = auth_cfg("sekrit");
    let no_token = request(
        cfg.clone(),
        "POST /agui/rpc HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert!(no_token.starts_with("HTTP/1.1 401"), "{no_token}");
    let with_token = request(
            cfg,
            "POST /agui/rpc HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer sekrit\r\nContent-Length: 2\r\n\r\n{}",
        );
    assert!(!with_token.starts_with("HTTP/1.1 401"), "{with_token}");
}

#[test]
fn health_stays_open_but_stream_needs_token() {
    let cfg = auth_cfg("t2");
    let health = request(
        cfg.clone(),
        "GET /agui/health HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(health.contains("\"ok\":true"), "{health}");
    let stream = request(
        cfg,
        "GET /agui/stream?run=x HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(stream.starts_with("HTTP/1.1 401"), "{stream}");
}

#[test]
fn origin_validation_allows_loopback_and_non_browser_clients() {
    assert!(origin_allowed(None)); // curl, scripts: no Origin header
    assert!(origin_allowed(Some("http://127.0.0.1:18789")));
    assert!(origin_allowed(Some("http://localhost:3000")));
    assert!(origin_allowed(Some("http://[::1]:18789")));
    assert!(origin_allowed(Some("https://127.0.0.1/")));
    assert!(origin_allowed(Some("  http://localhost  ")));
}

#[test]
fn origin_validation_rejects_cross_origin_and_garbage() {
    assert!(!origin_allowed(Some("https://evil.com")));
    assert!(!origin_allowed(Some("http://127.0.0.1.evil.com")));
    assert!(!origin_allowed(Some("http://evil127.0.0.1.com")));
    assert!(!origin_allowed(Some("http://localhost.evil.com:80")));
    assert!(!origin_allowed(Some("null")));
    assert!(!origin_allowed(Some("")));
    assert!(!origin_allowed(Some("127.0.0.1"))); // no scheme
    assert!(!origin_allowed(Some("file:///etc/passwd")));
}

#[test]
fn cross_origin_rpc_is_rejected_before_auth() {
    let cfg = auth_cfg("sekrit");
    // Even with a valid token, a cross-origin browser request dies at 403.
    let evil = request(
        cfg.clone(),
        "POST /agui/rpc HTTP/1.1\r\nHost: localhost\r\nOrigin: https://evil.com\r\nAuthorization: Bearer sekrit\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert!(evil.starts_with("HTTP/1.1 403"), "{evil}");
    // A loopback Origin passes the gate and reaches auth as before.
    let ok = request(
        cfg,
        "POST /agui/rpc HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:18789\r\nAuthorization: Bearer sekrit\r\nContent-Length: 2\r\n\r\n{}",
    );
    assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
}

#[test]
fn token_injection_is_a_quoted_js_literal() {
    let page = inject_token(WEB_UI, Some("abc"));
    assert!(page.contains("const TOKEN=\"abc\";"), "{page}");
}

#[test]
fn token_injection_cannot_break_out_of_script() {
    // A hostile token (e.g. hand-set via env) must not terminate the
    // <script> block or inject JS.
    let evil = "x\";alert(1);//</script><script>alert(2)";
    let page = inject_token(WEB_UI, Some(evil));
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
fn generated_tokens_are_unique_base64url() {
    let a = generate_token();
    let b = generate_token();
    assert_eq!(a.len(), 32);
    assert_eq!(b.len(), 32);
    assert_ne!(a, b);
    assert!(a
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
}

#[test]
fn oversized_headers_are_rejected() {
    let cfg = auth_cfg("t3");
    let big = "x".repeat(40 * 1024);
    let resp = request(
        cfg,
        &format!("GET /agui/health HTTP/1.1\r\nHost: localhost\r\nX-Pad: {big}\r\n\r\n"),
    );
    assert!(resp.starts_with("HTTP/1.1 431"), "{resp}");
}
