//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral — SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem — lives here and
//! runs via `cargo test -p pantheon-eval`.

use pantheon_exec::acp::*;
use std::io::Write;
use std::time::Duration;

#[test]
fn handshake_with_fake_server_succeeds() {
    let py = fake_server_py();
    let mut b = AcpBackend::spawn("python3", &[py.to_str().unwrap()]).unwrap();
    b.initialize(Duration::from_secs(10)).unwrap();
    assert_eq!(b.server_version, Some(ACP_PROTOCOL_VERSION));
    assert_eq!(b.server_capabilities, vec!["prompt".to_string()]);
    assert_eq!(b.status()["session_prompt"], "unbuilt");
}

#[test]
fn spawn_failure_names_the_missing_harness() {
    let e: String = match AcpBackend::spawn("pantheon-no-such-harness-xyz", &[]) {
        Ok(_) => panic!("a missing harness must not spawn"),
        Err(e) => e,
    };
    assert!(e.contains("spawn"), "{e}");
}

fn fake_server_py() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pantheon-acp-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("fake_acp.py");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(
        f,
        r#"import sys, json
def read_msg():
    headers = {{}}
    while True:
        line = sys.stdin.buffer.readline().decode()
        if not line:
            return None
        line = line.strip()
        if not line:
            continue
        if ':' in line:
            k, v = line.split(':', 1)
            headers[k.strip().lower()] = v.strip()
            continue
        # bare JSON line (no headers seen)
        if not headers:
            return json.loads(line)
    # unreachable in this fake: framed path below
def main():
    # read headers
    length = None
    while True:
        line = sys.stdin.buffer.readline().decode()
        if not line:
            return
        line = line.strip()
        if not line:
            break
        if line.lower().startswith('content-length:'):
            length = int(line.split(':', 1)[1].strip())
    body = sys.stdin.buffer.read(length or 0)
    req = json.loads(body)
    resp = {{
        "jsonrpc": "2.0",
        "id": req.get("id"),
        "result": {{
            "protocolVersion": 1,
            "capabilities": {{"prompt": {{}}}},
        }},
    }}
    out = json.dumps(resp).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(out) + out)
    sys.stdout.buffer.flush()
main()
"#
    )
    .unwrap();
    path
}
