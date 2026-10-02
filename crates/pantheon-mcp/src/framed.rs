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
    let v: Value = serde_json::from_str(&first).map_err(|e| FrameError::BadJson(e.to_string()))?;
    Ok(Some(v))
}

/// Read one `\n`-terminated line, consuming exactly up to and including the
/// newline - never over-reading into the next frame. Rejects anything over
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
    let s = String::from_utf8(buf).map_err(|e| FrameError::BadJson(format!("not UTF-8: {e}")))?;
    Ok(Some(s))
}

/// Read one line into `out` (used for the short header block).
fn read_line_into<R: BufRead>(
    reader: &mut R,
    out: &mut String,
    cap: usize,
) -> Result<(), FrameError> {
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
    let mut bytes = serde_json::to_vec(body).map_err(|e| FrameError::BadJson(e.to_string()))?;
    if bytes.len() + 1 > max_bytes {
        return Err(FrameError::Oversized(bytes.len() + 1));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::{BufReader, Cursor};

    /// A `BufRead` adapter that exposes at most `chunk` bytes through
    /// `fill_buf`, simulating a slow pipe that splits a frame across many
    /// reads.
    struct Chunked<R: BufRead> {
        inner: R,
        chunk: usize,
    }

    impl<R: BufRead> std::io::Read for Chunked<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.inner.read(buf)
        }
    }

    impl<R: BufRead> BufRead for Chunked<R> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let buf = self.inner.fill_buf()?;
            let n = buf.len().min(self.chunk);
            Ok(&buf[..n])
        }

        fn consume(&mut self, amt: usize) {
            self.inner.consume(amt);
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let bytes = encode(&msg, 1024).unwrap();
        assert!(bytes.ends_with(b"\n"));
        let mut r = BufReader::new(Cursor::new(bytes));
        assert_eq!(read_message(&mut r, 1024).unwrap(), Some(msg));
        // Clean EOF after the frame: no bytes before close -> Ok(None).
        assert_eq!(read_message(&mut r, 1024).unwrap(), None);
    }

    #[test]
    fn blank_lines_are_skipped_not_messages() {
        let mut r = BufReader::new(Cursor::new(b"\n\n  \n{\"id\": 9}\n".to_vec()));
        assert_eq!(read_message(&mut r, 1024).unwrap(), Some(json!({"id": 9})));
    }

    #[test]
    fn clean_eof_is_not_an_error() {
        let mut r = BufReader::new(Cursor::new(Vec::new()));
        assert_eq!(read_message(&mut r, 1024).unwrap(), None);
    }

    #[test]
    fn partial_reads_reassemble_a_split_frame() {
        let msg = json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}});
        let bytes = encode(&msg, 4096).unwrap();
        // Three bytes per fill_buf: the frame arrives in many fragments.
        let mut r = Chunked {
            inner: BufReader::new(Cursor::new(bytes)),
            chunk: 3,
        };
        assert_eq!(read_message(&mut r, 4096).unwrap(), Some(msg));
    }

    #[test]
    fn multiple_frames_in_one_buffer_decode_in_order() {
        let a = json!({"id": 1});
        let b = json!({"id": 2});
        let mut bytes = encode(&a, 1024).unwrap();
        bytes.extend(encode(&b, 1024).unwrap());
        let mut r = BufReader::new(Cursor::new(bytes));
        assert_eq!(read_message(&mut r, 1024).unwrap(), Some(a));
        assert_eq!(read_message(&mut r, 1024).unwrap(), Some(b));
        assert_eq!(read_message(&mut r, 1024).unwrap(), None);
    }

    #[test]
    fn content_length_framed_message_round_trips() {
        let msg = json!({"jsonrpc": "2.0", "id": 3, "result": "pong"});
        let body = serde_json::to_vec(&msg).unwrap();
        let mut wire = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        wire.extend(body);
        let mut r = BufReader::new(Cursor::new(wire));
        assert_eq!(read_message(&mut r, 4096).unwrap(), Some(msg));
    }

    #[test]
    fn malformed_json_line_is_bad_json() {
        let mut r = BufReader::new(Cursor::new(b"{not json}\n".to_vec()));
        assert!(matches!(
            read_message(&mut r, 1024),
            Err(FrameError::BadJson(_))
        ));
    }

    #[test]
    fn non_numeric_content_length_is_bad_header() {
        let mut r = BufReader::new(Cursor::new(b"Content-Length: abc\n\n{}\n".to_vec()));
        assert!(matches!(
            read_message(&mut r, 1024),
            Err(FrameError::BadHeader(_))
        ));
    }

    #[test]
    fn truncated_body_is_an_io_error() {
        // Declares 100 bytes, delivers 10.
        let mut wire = b"Content-Length: 100\n\n".to_vec();
        wire.extend(b"{\"id\":1}");
        let mut r = BufReader::new(Cursor::new(wire));
        assert!(matches!(read_message(&mut r, 1024), Err(FrameError::Io(_))));
    }

    #[test]
    fn oversized_line_is_rejected() {
        let big = "x".repeat(200);
        let mut r = BufReader::new(Cursor::new(format!("{big}\n").into_bytes()));
        assert!(matches!(
            read_message(&mut r, 64),
            Err(FrameError::Oversized(_))
        ));
    }

    #[test]
    fn content_length_over_the_cap_is_rejected() {
        let mut r = BufReader::new(Cursor::new(b"Content-Length: 99999\n\n".to_vec()));
        assert!(matches!(
            read_message(&mut r, 64),
            Err(FrameError::Oversized(_))
        ));
    }

    #[test]
    fn encode_over_the_cap_is_rejected() {
        let msg = json!({"data": "x".repeat(200)});
        assert!(matches!(encode(&msg, 64), Err(FrameError::Oversized(_))));
    }
}
