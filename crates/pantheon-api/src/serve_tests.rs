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
    let sup = pantheon_runtime::Supervisor::open(dir.clone()).unwrap();
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
