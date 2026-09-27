//! Stdio framing for JSON-RPC 2.0 messages.
//!
//! The MCP stdio transport delimits messages with newlines; some servers
//! instead send `Content-Length` headers (LSP style). The reader accepts
//! both, like the ACP backend does. The writer always sends newline-
//! delimited JSON, which is what the MCP spec describes for stdio.

use serde_json::Value;
use std::io::BufRead;

/// Cap on the header block of a `Content-Length`-framed message.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// What went wrong reading one framed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// `Content-Length` header missing, unparseable, or (with `usize`) over
    /// the configured message cap.
    BadHeader(String),
    /// The framed body is not valid JSON.
    BadJson(String),
    /// A newline-delimited line (or declared body) exceeds the cap.
    Oversized(usize),
    /// Underlying I/O failure.
    Io(String),
}

/// Read one message from `reader`: `Content-Length`-framed, or a bare JSON
/// line. Returns `Ok(None)` on clean EOF (no bytes before close).
/// Every byte counted against `max_bytes`; anything over is rejected.
pub fn read_message<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Option<Value>, FrameError> {
    // Skip blank lines: servers emit them, and a blank line is not a message.
    // Iterative, so a flood of blank lines cannot grow the stack.
    let first = loop {
        let line = match read_line_capped(reader, max_bytes)? {
            None => return Ok(None),
            Some(l) => l,
        };
        let t = line.trim().to_string();
        if !t.is_empty() {
            break t;
        }
    };
    if let Some(rest) = first.strip_prefix("Content-Length:") {
        let len: usize = rest
            .trim()
            .parse()
            .map_err(|_| FrameError::BadHeader(format!("bad Content-Length {rest:?}")))?;
        if len > max_bytes {
            return Err(FrameError::Oversized(len));
        }
        // Consume the blank line separating headers from the body.
        let mut blank = String::new();
        read_line_into(reader, &mut blank, MAX_HEADER_BYTES)?;
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| FrameError::Io(format!("short body: {e}")))?;
        let v: Value =
            serde_json::from_slice(&buf).map_err(|e| FrameError::BadJson(e.to_string()))?;
        return Ok(Some(v));
    }
    // Bare JSON line.
    let v: Value =
        serde_json::from_str(&first).map_err(|e| FrameError::BadJson(e.to_string()))?;
    Ok(Some(v))
}

/// Read one `\n`-terminated line, consuming exactly up to and including the
/// newline — never over-reading into the next frame. Rejects anything over
/// `cap` bytes. Returns `Ok(None)` only on EOF with no bytes read.
fn read_line_capped<R: BufRead>(reader: &mut R, cap: usize) -> Result<Option<String>, FrameError> {
    let mut buf = Vec::new();
    loop {
        let chunk = reader
            .fill_buf()
            .map_err(|e| FrameError::Io(e.to_string()))?;
        if chunk.is_empty() {
            if buf.is_empty() {
                return Ok(None);
            }
            break;
        }
        match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => {
                let take = i + 1;
                if buf.len() + take > cap {
                    return Err(FrameError::Oversized(buf.len() + take));
                }
                buf.extend_from_slice(&chunk[..take]);
                reader.consume(take);
                break;
            }
            None => {
                if buf.len() + chunk.len() > cap {
                    return Err(FrameError::Oversized(buf.len() + chunk.len()));
                }
                let n = chunk.len();
                buf.extend_from_slice(chunk);
                reader.consume(n);
            }
        }
    }
    let s =
        String::from_utf8(buf).map_err(|e| FrameError::BadJson(format!("not UTF-8: {e}")))?;
    Ok(Some(s))
}

/// Read one line into `out` (used for the short header block).
fn read_line_into<R: BufRead>(reader: &mut R, out: &mut String, cap: usize) -> Result<(), FrameError> {
    match read_line_capped(reader, cap)? {
        Some(l) => {
            *out = l;
            Ok(())
        }
        None => Err(FrameError::Io("EOF inside header block".into())),
    }
}

/// Encode one message as newline-delimited JSON for the wire.
pub fn encode(body: &Value, max_bytes: usize) -> Result<Vec<u8>, FrameError> {
    let mut bytes =
        serde_json::to_vec(body).map_err(|e| FrameError::BadJson(e.to_string()))?;
    if bytes.len() + 1 > max_bytes {
        return Err(FrameError::Oversized(bytes.len() + 1));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Cursor};

    fn cur(s: &[u8]) -> BufReader<Cursor<Vec<u8>>> {
        BufReader::new(Cursor::new(s.to_vec()))
    }

    #[test]
    fn reads_bare_json_line() {
        let mut r = cur(b"{\"jsonrpc\":\"2.0\",\"id\":1}\n");
        let v = read_message(&mut r, 1024).unwrap().unwrap();
        assert_eq!(v["id"], 1);
    }

    #[test]
    fn skips_blank_lines() {
        let mut r = cur(b"\n\n{\"a\":1}\n");
        let v = read_message(&mut r, 1024).unwrap().unwrap();
        assert_eq!(v["a"], 1);
    }

    #[test]
    fn reads_content_length_framing() {
        let body = r#"{"jsonrpc":"2.0","id":2}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let mut r = cur(raw.as_bytes());
        let v = read_message(&mut r, 1024).unwrap().unwrap();
        assert_eq!(v["id"], 2);
    }

    #[test]
    fn clean_eof_is_none() {
        let mut r = cur(b"");
        assert!(read_message(&mut r, 1024).unwrap().is_none());
    }

    #[test]
    fn oversized_line_rejected() {
        let mut r = cur(b"{\"a\":\"xxxxxxxxxx\"}\n");
        assert_eq!(
            read_message(&mut r, 8).unwrap_err(),
            FrameError::Oversized(19)
        );
    }

    #[test]
    fn oversized_content_length_rejected_before_body() {
        let raw = b"Content-Length: 999999\r\n\r\n";
        let mut r = cur(raw);
        assert_eq!(
            read_message(&mut r, 1024).unwrap_err(),
            FrameError::Oversized(999999)
        );
    }

    #[test]
    fn bad_json_is_structured() {
        let mut r = cur(b"not json\n");
        assert!(matches!(
            read_message(&mut r, 1024).unwrap_err(),
            FrameError::BadJson(_)
        ));
    }

    #[test]
    fn encode_is_newline_delimited() {
        let b = encode(&serde_json::json!({"a": 1}), 1024).unwrap();
        assert_eq!(b, b"{\"a\":1}\n");
    }
}
