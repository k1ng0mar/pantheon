//! MCP over HTTP: the two remote transports, sharing the [`McpConn`]
//! pairing rules with the stdio client.
//!
//! * `http` - streamable HTTP (2025-03-26): plain JSON-RPC POSTs to one
//!   endpoint. A response is a single JSON object, or an SSE stream when
//!   the server batches. A `Mcp-Session-Id` response header, when present,
//!   is echoed back on later requests.
//! * `sse` - legacy SSE (2024-11-05): one long-lived GET event stream,
//!   plus a per-session POST endpoint the server announces in an
//!   `endpoint` event. Responses to POSTs arrive as `message` events on
//!   the stream.
//!
//! Sync, via ureq (the same crate the provider layer uses). For SSE the
//! stream is read on a background thread and fanned into an mpsc channel,
//! mirroring the stdio client's reader thread; `request` pairs by id with
//! a deadline. Server-initiated requests on the SSE stream are ignored
//! they cannot wedge the pairing because responses are matched by id.
//!
//! Shutdown note: the SSE reader thread blocks on the socket, so after
//! `shutdown` it can linger until the stream agent's idle timeout (one
//! hour) or process exit. Request pairing is unaffected.

use crate::{
    classify, McpConn, McpError, McpToolDef, Wire, MCP_PROTOCOL_VERSION,
    SUPPORTED_PROTOCOL_VERSIONS,
};
use serde_json::Value;
use std::io::BufRead;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Which remote transport to speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpTransport {
    /// Legacy SSE (2024-11-05).
    Sse,
    /// Streamable HTTP (2025-03-26 and later).
    Streamable,
}

/// Idle ceiling for the long-lived SSE GET stream. When it elapses with
/// no server traffic the reader thread reports EOF and the manager
/// reconnects transparently.
const SSE_STREAM_TIMEOUT: Duration = Duration::from_secs(3600);

/// One parsed SSE event.
struct SseEvent {
    event: String,
    data: String,
}

/// Read one SSE event from a buffered stream. `Ok(None)` is clean EOF.
fn next_sse_event<R: BufRead>(r: &mut R) -> Result<Option<SseEvent>, String> {
    let mut event = String::new();
    let mut data: Vec<String> = Vec::new();
    loop {
        let mut line = String::new();
        match r.read_line(&mut line) {
            Ok(0) => {
                if event.is_empty() && data.is_empty() {
                    return Ok(None);
                }
                break;
            }
            Ok(_) => {}
            Err(e) => return Err(e.to_string()),
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break; // blank line dispatches the event
        }
        if line.starts_with(':') {
            continue; // comment / heartbeat
        }
        let (field, value) = match line.find(':') {
            Some(i) => (&line[..i], line[i + 1..].trim_start()),
            None => (line, ""),
        };
        match field {
            "event" => event = value.to_string(),
            "data" => data.push(value.to_string()),
            _ => {}
        }
    }
    if event.is_empty() && data.is_empty() {
        return Ok(None);
    }
    Ok(Some(SseEvent {
        event,
        data: data.join("\n"),
    }))
}

/// Resolve the `endpoint` event payload against the SSE base URL. The
/// spec allows a relative path; absolutize against the base's origin.
fn resolve_endpoint(base: &str, endpoint: &str) -> String {
    if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
        return endpoint.to_string();
    }
    let origin = base.split('/').take(3).collect::<Vec<_>>().join("/");
    if let Some(path) = endpoint.strip_prefix('/') {
        format!("{origin}/{path}")
    } else {
        format!("{origin}/{endpoint}")
    }
}

fn ureq_err(e: ureq::Error, method: &str) -> McpError {
    match e {
        ureq::Error::Status(code, resp) => McpError::Rpc {
            code: i64::from(code),
            message: format!("http {code}: {}", resp.status_text()),
        },
        ureq::Error::Transport(t) => {
            if is_timeout_transport(&t) {
                McpError::Timeout {
                    method: method.to_string(),
                }
            } else {
                McpError::Io(format!("http: {t}"))
            }
        }
    }
}

/// This ureq version reports timeouts as `ErrorKind::Io` with an
/// underlying `io::Error` of kind `TimedOut`: walk the source chain.
fn is_timeout_transport(t: &ureq::Transport) -> bool {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(t);
    while let Some(e) = cur {
        if let Some(ioe) = e.downcast_ref::<std::io::Error>() {
            if ioe.kind() == std::io::ErrorKind::TimedOut {
                return true;
            }
        }
        cur = std::error::Error::source(e);
    }
    false
}

/// `Response::into_json` fails with `std::io::Error`, not `ureq::Error`.
fn json_err(e: std::io::Error, method: &str) -> McpError {
    if e.kind() == std::io::ErrorKind::TimedOut {
        McpError::Timeout {
            method: method.to_string(),
        }
    } else {
        McpError::Framing(format!("bad json response: {e}"))
    }
}

/// Messages from the SSE reader thread.
enum StreamMsg {
    Endpoint(String),
    Message(Value),
    Eof,
    Error(String),
}

/// An MCP client over SSE or streamable HTTP. Not `Sync`: requests are
/// issued one at a time from the owning thread. `Send` so the manager can
/// hold it behind a mutex.
pub struct HttpMcpClient {
    server: String,
    transport: HttpTransport,
    agent: ureq::Agent,
    post_url: String,
    session_id: Option<String>,
    sse_rx: Option<mpsc::Receiver<StreamMsg>>,
    next_id: u64,
    timeout: Duration,
    /// Extra headers sent with every POST (auth for remote endpoints).
    headers: Vec<(String, String)>,
    /// Protocol version the server answered in `initialize`.
    pub negotiated_version: String,
    /// The server's self-reported version, when it sends one.
    pub server_version: Option<String>,
    dead: bool,
}

impl HttpMcpClient {
    /// Connect and run the `initialize` handshake.
    ///
    /// `url` is the SSE stream URL for [`HttpTransport::Sse`] and the
    /// JSON-RPC endpoint for [`HttpTransport::Streamable`].
    pub fn connect(
        server: &str,
        transport: HttpTransport,
        url: &str,
        timeout: Duration,
        headers: &[(String, String)],
    ) -> Result<Self, McpError> {
        let agent = ureq::AgentBuilder::new().timeout(timeout).build();
        let (post_url, sse_rx) = match transport {
            HttpTransport::Streamable => (url.to_string(), None),
            HttpTransport::Sse => {
                let stream_agent = ureq::AgentBuilder::new()
                    .timeout(SSE_STREAM_TIMEOUT)
                    .build();
                let resp = stream_agent
                    .get(url)
                    .set("Accept", "text/event-stream")
                    .call()
                    .map_err(|e| ureq_err(e, "sse connect"))?;
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(resp.into_reader());
                    loop {
                        match next_sse_event(&mut reader) {
                            Ok(None) => {
                                let _ = tx.send(StreamMsg::Eof);
                                break;
                            }
                            Ok(Some(ev)) => {
                                let msg = match ev.event.as_str() {
                                    "endpoint" => Some(StreamMsg::Endpoint(ev.data)),
                                    "message" => match serde_json::from_str::<Value>(&ev.data) {
                                        Ok(v) => Some(StreamMsg::Message(v)),
                                        Err(e) => Some(StreamMsg::Error(format!(
                                            "bad sse message json: {e}"
                                        ))),
                                    },
                                    _ => None,
                                };
                                if let Some(m) = msg {
                                    if tx.send(m).is_err() {
                                        break;
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(StreamMsg::Error(e));
                                break;
                            }
                        }
                    }
                });
                // The first thing a well-behaved server sends is the
                // endpoint event. Wait for it with the request timeout.
                let deadline = Instant::now() + timeout;
                let endpoint = loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    match rx.recv_timeout(remaining) {
                        Ok(StreamMsg::Endpoint(u)) => break u,
                        Ok(StreamMsg::Message(_)) => continue,
                        Ok(StreamMsg::Eof) => {
                            return Err(McpError::Closed);
                        }
                        Ok(StreamMsg::Error(e)) => {
                            return Err(McpError::Framing(format!("sse stream: {e}")));
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            return Err(McpError::Timeout {
                                method: "sse endpoint".to_string(),
                            });
                        }
                        Err(RecvTimeoutError::Disconnected) => return Err(McpError::Closed),
                    }
                };
                (resolve_endpoint(url, &endpoint), Some(rx))
            }
        };
        let mut client = Self {
            server: server.to_string(),
            transport,
            agent,
            post_url,
            session_id: None,
            sse_rx,
            next_id: 1,
            timeout,
            headers: headers.to_vec(),
            negotiated_version: String::new(),
            server_version: None,
            dead: false,
        };
        client.initialize()?;
        Ok(client)
    }

    /// POST one JSON-RPC message. Returns the raw response for callers
    /// that parse it themselves (streamable responses).
    fn post(&self, body: &Value) -> Result<ureq::Response, McpError> {
        let body_str =
            serde_json::to_string(body).map_err(|e| McpError::Framing(format!("encode: {e}")))?;
        let mut req = self
            .agent
            .post(&self.post_url)
            .set("Content-Type", "application/json")
            .set("Accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.set(k, v);
        }
        if let Some(sid) = &self.session_id {
            req = req.set("Mcp-Session-Id", sid);
        }
        req.send_string(&body_str).map_err(|e| ureq_err(e, "post"))
    }

    /// Fire-and-forget notification (no id, no response expected).
    fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        if self.dead {
            return Err(McpError::Closed);
        }
        let body = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
        // A 202 with an empty body is the happy path; any 2xx is fine.
        // Drain the body so the connection can be reused.
        match self.post(&body) {
            Ok(resp) => {
                let _ = resp.into_string();
                Ok(())
            }
            Err(e) => {
                self.dead = true;
                Err(e)
            }
        }
    }

    fn initialize(&mut self) -> Result<(), McpError> {
        let result = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "pantheon", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        let version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                McpError::Protocol("initialize response has no protocolVersion".into())
            })?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return Err(McpError::Protocol(format!(
                "server protocol version {version:?} not in supported {:?}",
                SUPPORTED_PROTOCOL_VERSIONS
            )));
        }
        self.negotiated_version = version.to_string();
        self.server_version = result
            .get("serverInfo")
            .and_then(|i| i.get("version"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Mandatory initialized notification; a failure here kills the
        // client like the stdio path does.
        if let Err(e) = self.notify("notifications/initialized", serde_json::json!({})) {
            self.dead = true;
            return Err(e);
        }
        Ok(())
    }

    /// Send one request and wait for the response with the matching id.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        if self.dead {
            return Err(McpError::Closed);
        }
        let id = self.next_id;
        self.next_id += 1;
        let want = Value::from(id);
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        });
        match self.transport {
            HttpTransport::Streamable => self.request_streamable(&body, &want, method),
            HttpTransport::Sse => self.request_sse(&body, &want, method),
        }
    }

    fn request_streamable(
        &mut self,
        body: &Value,
        want: &Value,
        method: &str,
    ) -> Result<Value, McpError> {
        let resp = self.post(body).inspect_err(|_| self.dead = true)?;
        if self.session_id.is_none() {
            if let Some(sid) = resp.header("Mcp-Session-Id") {
                self.session_id = Some(sid.to_string());
            }
        }
        let is_sse = resp
            .header("Content-Type")
            .is_some_and(|ct| ct.contains("text/event-stream"));
        if !is_sse {
            let v: Value = resp.into_json().map_err(|e| {
                self.dead = true;
                json_err(e, method)
            })?;
            return match_response(&v, want);
        }
        // SSE-batched response: scan events for our id. The agent timeout
        // bounds the whole read; a timeout surfaces as a read error.
        let mut reader = std::io::BufReader::new(resp.into_reader());
        loop {
            match next_sse_event(&mut reader) {
                Ok(None) => {
                    self.dead = true;
                    return Err(McpError::Closed);
                }
                Ok(Some(ev)) => {
                    if ev.event != "message" {
                        continue;
                    }
                    let v: Value = serde_json::from_str(&ev.data)
                        .map_err(|e| McpError::Framing(format!("bad sse message json: {e}")))?;
                    match classify(&v) {
                        Wire::Response {
                            id: rid,
                            result,
                            error,
                        } if rid == *want => {
                            if let Some((code, message)) = error {
                                return Err(McpError::Rpc { code, message });
                            }
                            return Ok(result.unwrap_or(Value::Null));
                        }
                        _ => continue,
                    }
                }
                Err(e) => {
                    self.dead = true;
                    return Err(match_stream_err(&e, method));
                }
            }
        }
    }

    fn request_sse(&mut self, body: &Value, want: &Value, method: &str) -> Result<Value, McpError> {
        // The POST itself carries no response body on this transport; the
        // answer arrives on the GET stream.
        match self.post(body) {
            Ok(resp) => {
                let _ = resp.into_string();
            }
            Err(e) => {
                self.dead = true;
                return Err(e);
            }
        }
        let rx = self.sse_rx.as_ref().ok_or(McpError::Closed)?;
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Err(RecvTimeoutError::Timeout) => {
                    self.dead = true;
                    return Err(McpError::Timeout {
                        method: method.to_string(),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.dead = true;
                    return Err(McpError::Closed);
                }
                Ok(StreamMsg::Eof) => {
                    self.dead = true;
                    return Err(McpError::Closed);
                }
                Ok(StreamMsg::Error(e)) => {
                    self.dead = true;
                    return Err(McpError::Framing(format!("sse stream: {e}")));
                }
                Ok(StreamMsg::Endpoint(_)) => continue,
                Ok(StreamMsg::Message(v)) => match classify(&v) {
                    Wire::Response {
                        id: rid,
                        result,
                        error,
                    } if rid == *want => {
                        if let Some((code, message)) = error {
                            return Err(McpError::Rpc { code, message });
                        }
                        return Ok(result.unwrap_or(Value::Null));
                    }
                    // Server-initiated requests are out of scope (same as
                    // the stdio client); anything else is not our answer.
                    _ => continue,
                },
            }
        }
    }

    /// `tools/list`.
    pub fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
        let result = self.request("tools/list", serde_json::json!({}))?;
        tools_from(&result, &self.server)
    }

    /// `tools/call`. No capability gate on this type: the manager holds
    /// the single gate at the `ToolRegistry` boundary, like the stdio
    /// client's manager path.
    pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
        let result = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        )?;
        if result.get("isError").and_then(|b| b.as_bool()) == Some(true) {
            return Err(McpError::ServerToolError(crate::result_text(&result)));
        }
        Ok(result)
    }

    /// Cheap liveness: no traffic. A dead stream is noticed on the next
    /// request or health probe.
    pub fn alive(&mut self) -> bool {
        !self.dead
    }

    /// Mark dead and drop the stream handle. Idempotent. (The SSE reader
    /// thread may linger until the stream timeout; see the module docs.)
    fn kill(&mut self) {
        self.dead = true;
        self.sse_rx = None;
    }
}

impl Drop for HttpMcpClient {
    fn drop(&mut self) {
        self.kill();
    }
}

impl McpConn for HttpMcpClient {
    fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
        HttpMcpClient::list_tools(self)
    }

    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
        HttpMcpClient::call_tool(self, name, args)
    }

    fn alive(&mut self) -> bool {
        HttpMcpClient::alive(self)
    }

    fn shutdown(&mut self) {
        self.kill();
    }

    fn negotiated_version(&self) -> &str {
        &self.negotiated_version
    }

    fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }
}

/// Match one decoded response value against our request id.
fn match_response(v: &Value, want: &Value) -> Result<Value, McpError> {
    match classify(v) {
        Wire::Response {
            id: rid,
            result,
            error,
        } if rid == *want => {
            if let Some((code, message)) = error {
                return Err(McpError::Rpc { code, message });
            }
            Ok(result.unwrap_or(Value::Null))
        }
        _ => Err(McpError::Protocol(
            "streamable response did not carry our request id".into(),
        )),
    }
}

fn tools_from(result: &Value, server: &str) -> Result<Vec<McpToolDef>, McpError> {
    let tools = result
        .get("tools")
        .and_then(|t| t.as_array())
        .ok_or_else(|| McpError::Protocol("tools/list response has no tools array".into()))?;
    tools
        .iter()
        .map(|t| {
            let name = t
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| McpError::Protocol("tool without a name".into()))?;
            Ok(McpToolDef {
                server: server.to_string(),
                name: name.to_string(),
                description: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// Map a stream read failure to timeout vs framing. ureq surfaces its own
/// timeout as an `io::Error` with kind TimedOut on the response reader.
fn match_stream_err(e: &str, method: &str) -> McpError {
    if e.contains("timed out") || e.contains("TimedOut") {
        McpError::Timeout {
            method: method.to_string(),
        }
    } else {
        McpError::Framing(format!("sse stream: {e}"))
    }
}
