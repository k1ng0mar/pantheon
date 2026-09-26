//! External harness backend over the Agent Client Protocol (§6).
//!
//! Both Hermes (`hermes acp`) and OMP (`omp acp`) ship an ACP server on
//! stdio. That is the interop seam §6's "another harness can be a
//! specialized execution backend" needs: no adapter code per harness, just
//! one client speaking the protocol.
//!
//! What this module does today: spawn the server, frame JSON-RPC 2.0
//! messages both ways, and perform the `initialize` handshake (protocol
//! version negotiation + capability echo). Session/prompt methods are next;
//! until they exist this is a handshake probe, not an execution backend,
//! and it says so in [`AcpBackend::status`].
//!
//! Framing: ACP travels over stdio as JSON-RPC 2.0. The reader accepts both
//! `Content-Length`-framed messages and bare JSON lines, because servers
//! differ and guessing wrong means hanging on the first read. The writer
//! always sends `Content-Length` framing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

/// Protocol version this client speaks. Sent in `initialize`; a server that
/// answers a different version is reported, not assumed compatible.
pub const ACP_PROTOCOL_VERSION: u32 = 1;

/// One JSON-RPC 2.0 message.
#[derive(Debug, Clone)]
pub enum RpcMessage {
    Request {
        id: serde_json::Value,
        method: String,
        params: serde_json::Value,
    },
    Response {
        id: serde_json::Value,
        result: Option<serde_json::Value>,
        error: Option<RpcError>,
    },
    Notification {
        method: String,
        params: serde_json::Value,
    },
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// Encode one message with `Content-Length` framing.
pub fn encode(body: &serde_json::Value) -> Vec<u8> {
    let text = serde_json::to_string(body).unwrap_or_else(|_| "{}".into());
    format!("Content-Length: {}\r\n\r\n{text}", text.len()).into_bytes()
}

/// Read one message from `reader`: `Content-Length`-framed, or a bare JSON
/// line. `None` on clean EOF; `Err` on corrupt framing or timeout.
pub fn decode_one<R: BufRead>(
    reader: &mut R,
    timeout: Duration,
) -> Result<Option<serde_json::Value>, String> {
    let deadline = Instant::now() + timeout;
    let mut line = String::new();
    // Skip blank lines: servers log them, and a blank line is not a message.
    let first = loop {
        if Instant::now() > deadline {
            return Err("acp: read timeout waiting for message".into());
        }
        line.clear();
        let n = reader
            .read_line(&mut line)
            .map_err(|e| format!("acp: read failed: {e}"))?;
        if n == 0 {
            return Ok(None);
        }
        let t = line.trim();
        if !t.is_empty() {
            break t.to_string();
        }
    };
    if let Some(rest) = first.strip_prefix("Content-Length:") {
        let len: usize = rest
            .trim()
            .parse()
            .map_err(|_| format!("acp: bad Content-Length {rest:?}"))?;
        // Consume the blank line after the header block.
        let mut blank = String::new();
        reader
            .read_line(&mut blank)
            .map_err(|e| format!("acp: read failed: {e}"))?;
        let mut buf = vec![0u8; len];
        reader
            .read_exact(&mut buf)
            .map_err(|e| format!("acp: short body: {e}"))?;
        let v: serde_json::Value =
            serde_json::from_slice(&buf).map_err(|e| format!("acp: bad JSON body: {e}"))?;
        return Ok(Some(v));
    }
    // Bare JSON line.
    let v: serde_json::Value =
        serde_json::from_str(&first).map_err(|e| format!("acp: bad JSON line: {e}"))?;
    Ok(Some(v))
}

fn to_message(v: serde_json::Value) -> Result<RpcMessage, String> {
    let obj = v.as_object().ok_or("acp: message is not an object")?;
    if obj.get("jsonrpc").and_then(|j| j.as_str()) != Some("2.0") {
        return Err("acp: missing jsonrpc 2.0 marker".into());
    }
    let id = obj.get("id").cloned();
    let method = obj
        .get("method")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    match (id, method) {
        (Some(id), Some(method)) => Ok(RpcMessage::Request {
            id,
            method,
            params: obj
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        (Some(id), None) => {
            if let Some(err) = obj.get("error") {
                Ok(RpcMessage::Response {
                    id,
                    result: None,
                    error: Some(RpcError {
                        code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
                        message: err
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                    }),
                })
            } else {
                Ok(RpcMessage::Response {
                    id,
                    result: obj.get("result").cloned(),
                    error: None,
                })
            }
        }
        (None, Some(method)) => Ok(RpcMessage::Notification {
            method,
            params: obj
                .get("params")
                .cloned()
                .unwrap_or(serde_json::Value::Null),
        }),
        (None, None) => Err("acp: message has neither id nor method".into()),
    }
}

/// A spawned ACP server: stdin writer + framed stdout reader.
pub struct AcpBackend {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: i64,
    /// Server's answered protocol version (from `initialize`), if handshook.
    pub server_version: Option<u32>,
    /// Capability names the server advertised, if handshook.
    pub server_capabilities: Vec<String>,
}

impl AcpBackend {
    /// Spawn `program [args...]` with piped stdio. Fails fast when the
    /// program does not exist, so a missing harness reads as "not installed"
    /// rather than a hung handshake.
    pub fn spawn(program: &str, args: &[&str]) -> Result<Self, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("acp: spawn {program}: {e}"))?;
        let stdin = child.stdin.take().ok_or("acp: no stdin")?;
        let stdout = child.stdout.take().ok_or("acp: no stdout")?;
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            server_version: None,
            server_capabilities: Vec::new(),
        })
    }

    fn send(&mut self, body: &serde_json::Value) -> Result<(), String> {
        let bytes = encode(body);
        self.stdin
            .write_all(&bytes)
            .map_err(|e| format!("acp: write failed: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("acp: flush failed: {e}"))?;
        Ok(())
    }

    fn recv(&mut self, timeout: Duration) -> Result<RpcMessage, String> {
        match decode_one(&mut self.stdout, timeout)? {
            Some(v) => to_message(v),
            None => Err("acp: server closed stdout".into()),
        }
    }

    /// `initialize` handshake: send our version, expect the server's version
    /// plus capabilities. A version mismatch is an error, not a guess.
    pub fn initialize(&mut self, timeout: Duration) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {},
            },
        }))?;
        match self.recv(timeout)? {
            RpcMessage::Response { result, error, .. } => {
                if let Some(e) = error {
                    return Err(format!(
                        "acp: initialize failed: {} ({})",
                        e.message, e.code
                    ));
                }
                let r = result.unwrap_or(serde_json::Value::Null);
                let v = r
                    .get("protocolVersion")
                    .and_then(|x| x.as_u64())
                    .ok_or("acp: initialize response has no protocolVersion")?;
                if v as u32 != ACP_PROTOCOL_VERSION {
                    return Err(format!(
                        "acp: protocol mismatch (server {v}, client {})",
                        ACP_PROTOCOL_VERSION
                    ));
                }
                self.server_version = Some(v as u32);
                if let Some(caps) = r.get("capabilities").and_then(|c| c.as_object()) {
                    self.server_capabilities = caps.keys().cloned().collect();
                    self.server_capabilities.sort();
                }
                Ok(())
            }
            other => Err(format!("acp: expected initialize response, got {other:?}")),
        }
    }

    /// What this backend is: honest about session/prompt being unbuilt.
    pub fn status(&self) -> HashMap<String, String> {
        HashMap::from([
            ("transport".into(), "acp-stdio".into()),
            (
                "server_version".into(),
                self.server_version
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "no-handshake".into()),
            ),
            ("session_prompt".into(), "unbuilt".into()),
        ])
    }
}

impl Drop for AcpBackend {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
#[path = "acp_tests.rs"]
mod tests;
