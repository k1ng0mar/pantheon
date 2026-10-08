//! LSP diagnostics: mock-server tests for the JSON-RPC wire path,
//! publishDiagnostics flattening, the diagnostics cache, and language
//! / server resolution.
use pantheon_exec::lsp::{language_for, path_uri, server_for, LspClient};
use std::path::PathBuf;
use std::time::Duration;

fn tempdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon_lsp_{}_{:?}",
        std::process::id(),
        std::time::SystemTime::now()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A minimal mock LSP server in Python: it reads LSP frames off stdin,
/// answers `initialize`, and emits a `publishDiagnostics` when it sees
/// `textDocument/didOpen`. Python handles the byte framing correctly
/// where a bash `read` loop does not.
fn write_mock_server(dir: &std::path::Path) -> PathBuf {
    let script = dir.join("mock_lsp.py");
    let body = r#"#!/usr/bin/env python3
import sys, json

def read_frame(r):
    headers = b""
    while True:
        b = r.read(1)
        if not b:
            return None
        headers += b
        if headers.endswith(b"\r\n\r\n"):
            break
    ln = 0
    for line in headers.decode("latin1").split("\r\n"):
        if line.lower().startswith("content-length:"):
            ln = int(line.split(":", 1)[1].strip())
    if ln == 0:
        return None
    body = r.read(ln)
    return json.loads(body.decode("utf-8"))

def write_frame(w, obj):
    data = json.dumps(obj).encode("utf-8")
    w.write(b"Content-Length: %d\r\n\r\n" % len(data))
    w.write(data)
    w.flush()

r = sys.stdin.buffer
w = sys.stdout.buffer
while True:
    msg = read_frame(r)
    if msg is None:
        break
    if msg.get("method") == "initialize":
        write_frame(w, {"jsonrpc":"2.0","id":msg.get("id"),"result":{"capabilities":{}}})
    elif msg.get("method") == "textDocument/didOpen":
        note = {"jsonrpc":"2.0","method":"publishDiagnostics","params":{
            "uri":"file:///tmp/lsp_test/main.rs",
            "diagnostics":[{"range":{"start":{"line":3,"character":1},"end":{"line":3,"character":5}},
                            "severity":1,"message":"mismatched types"}]}}
        write_frame(w, note)
"#;
    std::fs::write(&script, body).unwrap();
    script
}

#[test]
fn path_uri_roundtrip() {
    let p = PathBuf::from("/home/u/project/main.rs");
    assert_eq!(path_uri(&p), "file:///home/u/project/main.rs");
    // Windows-style separators normalize to forward slashes.
    let w = PathBuf::from("C:\\proj\\x.rs");
    assert!(path_uri(&w).starts_with("file://"));
}

#[test]
fn language_detection() {
    assert_eq!(language_for("rs"), Some("rust"));
    assert_eq!(language_for("ts"), Some("typescript"));
    assert_eq!(language_for("GO"), Some("go"));
    assert_eq!(language_for("wat"), None);
}

#[test]
fn server_resolution() {
    let (prog, _) = server_for("rust").expect("rust server");
    assert!(prog.contains("rust-analyzer"));
    let (prog, args) = server_for("go").expect("go server");
    assert!(prog.contains("gopls") || prog == "gopls");
    assert!(args.iter().any(|a| a.contains("stdio")));
    assert!(server_for("astrologer").is_none());
}

#[test]
fn mock_server_diagnostics_flow() {
    let d = tempdir();
    let server = write_mock_server(&d);

    // Start the client against the mock server. The mock answers the
    // initialize handshake and keeps reading frames, so start() should
    // complete.
    let client = LspClient::start(
        "python3",
        &[server.to_string_lossy().to_string()],
        "mock-lsp",
        "file:///tmp/lsp_test",
        Duration::from_secs(5),
    )
    .expect("client should start against the mock server");

    // Open a document; the mock will emit a publishDiagnostics for it.
    client
        .open_document(
            &PathBuf::from("/tmp/lsp_test/main.rs"),
            "rust",
            "fn main() {}\n",
        )
        .expect("open document");

    // Poll the cache for the diagnostics the mock emitted.
    let got = client
        .wait_diagnostics("file:///tmp/lsp_test/main.rs", Duration::from_secs(3))
        .expect("wait should not error");
    let diag = got.expect("mock should have emitted diagnostics");
    assert_eq!(diag.count, 1);
    assert!(
        diag.lines[0].contains("mismatched types"),
        "line: {:?}",
        diag.lines
    );
    assert!(
        diag.lines[0].contains("error"),
        "severity should map to 'error'"
    );

    // The cache also exposes it by readback.
    assert!(client.diagnostics("file:///tmp/lsp_test/main.rs").is_some());
    assert!(client
        .all_diagnostics()
        .iter()
        .any(|x| x.uri == "file:///tmp/lsp_test/main.rs"));

    // Clean shutdown.
    client.shutdown();
}
