use super::*;

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
