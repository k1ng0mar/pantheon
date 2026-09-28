use super::*;
use std::io::Write;

// A fake ACP server: reads one framed request, answers initialize, exits.
// Written to a temp file so no fixture is committed.
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

#[test]
fn framed_and_bare_messages_both_decode() {
    // Framed.
    let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {}});
    let bytes = encode(&body);
    let mut cur = std::io::Cursor::new(bytes);
    let got = decode_one(&mut cur, Duration::from_secs(2))
        .unwrap()
        .unwrap();
    assert_eq!(got, body);
    // Bare line.
    let mut cur2 = std::io::Cursor::new(b"{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n".to_vec());
    let got2 = decode_one(&mut cur2, Duration::from_secs(2))
        .unwrap()
        .unwrap();
    assert_eq!(got2["method"], "ping");
    // Blank lines are skipped, not parsed.
    let mut cur3 =
        std::io::Cursor::new(b"\n\n{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}\n".to_vec());
    assert!(decode_one(&mut cur3, Duration::from_secs(2))
        .unwrap()
        .is_some());
    // Clean EOF is None, not an error.
    let mut cur4 = std::io::Cursor::new(Vec::new());
    assert!(decode_one(&mut cur4, Duration::from_secs(2))
        .unwrap()
        .is_none());
    // Corrupt framing errors (never hangs).
    let mut cur5 = std::io::Cursor::new(b"Content-Length: nope\r\n\r\n".to_vec());
    assert!(decode_one(&mut cur5, Duration::from_secs(2)).is_err());
}
