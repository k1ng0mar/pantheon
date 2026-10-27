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
//! `shutdown` it can linger until the stream agent's overall timeout (one
//! hour) or process exit. Request pairing is unaffected.

use crate::client::ElicitationHandler;
use crate::{
    classify, McpConn, McpError, McpPrompt, McpPromptResult, McpResource, McpResourceContent,
    McpResourceTemplate, McpToolDef, Wire, MCP_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS,
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

/// Overall ceiling for the long-lived SSE GET stream: one budget covering
/// connect through every stream read (ureq's global-timeout scope). When
/// it elapses the reader thread reports a stream error and the manager
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

/// ureq agent with one overall timeout (connect through body read, the
/// scope of ureq's `timeout_global`, matching the ureq 2 `.timeout()`
/// budget). HTTP statuses surface as responses, not errors, so call sites
/// read headers/bodies and map statuses themselves.
fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .max_redirects(5)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

fn ureq_err(e: ureq::Error, method: &str) -> McpError {
    if is_timeout_error(&e) {
        McpError::Timeout {
            method: method.to_string(),
        }
    } else {
        McpError::Io(format!("http: {e}"))
    }
}

/// ureq 3 reports timeouts as `Error::Timeout`. Errors funneled through
/// an `io::Error` keep the `TimedOut` kind, so cover both.
fn is_timeout_error(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Io(ioe) => ioe.kind() == std::io::ErrorKind::TimedOut,
        _ => false,
    }
}

/// Map an HTTP status >= 400 to the RPC error the ureq 2 status-error
/// path produced. The reason phrase stands in for ureq 2's status-line
/// text; unknown codes render an empty reason.
fn status_err(resp: &ureq::http::Response<ureq::Body>) -> McpError {
    let code = resp.status().as_u16();
    McpError::Rpc {
        code: i64::from(code),
        message: format!(
            "http {code}: {}",
            resp.status().canonical_reason().unwrap_or("")
        ),
    }
}

/// `Body::read_json` fails with `ureq::Error`.
fn json_err(e: ureq::Error, method: &str) -> McpError {
    if is_timeout_error(&e) {
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

/// Build the JSON-RPC response to one server-initiated request. Only
/// `elicitation/create` is implemented; everything else is a clean
/// method-not-found so a misbehaving server cannot wedge a request.
fn server_request_response(
    handler: Option<&ElicitationHandler>,
    server: &str,
    id: &Value,
    method: &str,
    params: &Value,
) -> Value {
    if method != "elicitation/create" {
        return serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": "method not found"},
        });
    }
    match handler {
        Some(handler) => match handler(server, params) {
            Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32603, "message": e.to_string()},
            }),
        },
        None => serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": "elicitation is not supported by this host"},
        }),
    }
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
    elicitation: Option<ElicitationHandler>,
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
        Self::connect_with(server, transport, url, timeout, headers, None)
    }

    /// [`HttpMcpClient::connect`] plus an optional server-initiated
    /// elicitation handler.
    pub fn connect_with(
        server: &str,
        transport: HttpTransport,
        url: &str,
        timeout: Duration,
        headers: &[(String, String)],
        elicitation: Option<ElicitationHandler>,
    ) -> Result<Self, McpError> {
        let agent = http_agent(timeout);
        let (post_url, sse_rx) = match transport {
            HttpTransport::Streamable => (url.to_string(), None),
            HttpTransport::Sse => {
                let stream_agent = http_agent(SSE_STREAM_TIMEOUT);
                let resp = stream_agent
                    .get(url)
                    .header("Accept", "text/event-stream")
                    .call()
                    .map_err(|e| ureq_err(e, "sse connect"))?;
                if resp.status().as_u16() >= 400 {
                    return Err(status_err(&resp));
                }
                let (tx, rx) = mpsc::channel();
                std::thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(resp.into_body().into_reader());
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
            elicitation,
        };
        client.initialize()?;
        Ok(client)
    }

    /// POST one JSON-RPC message. Returns the raw response for callers
    /// that parse it themselves (streamable responses).
    fn post(&self, body: &Value) -> Result<ureq::http::Response<ureq::Body>, McpError> {
        let body_str =
            serde_json::to_string(body).map_err(|e| McpError::Framing(format!("encode: {e}")))?;
        let mut req = self
            .agent
            .post(&self.post_url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        let resp = req
            .send(body_str.as_str())
            .map_err(|e| ureq_err(e, "post"))?;
        if resp.status().as_u16() >= 400 {
            return Err(status_err(&resp));
        }
        Ok(resp)
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
                let _ = resp.into_body().read_to_string();
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
            if let Some(sid) = resp
                .headers()
                .get("Mcp-Session-Id")
                .and_then(|v| v.to_str().ok())
            {
                self.session_id = Some(sid.to_string());
            }
        }
        let is_sse = resp
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.contains("text/event-stream"));
        if !is_sse {
            let v: Value = resp.into_body().read_json().map_err(|e| {
                self.dead = true;
                json_err(e, method)
            })?;
            return match_response(&v, want);
        }
        // SSE-batched response: scan events for our id. The agent timeout
        // bounds the whole read; a timeout surfaces as a read error.
        let mut reader = std::io::BufReader::new(resp.into_body().into_reader());
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
                        Wire::Request {
                            id: rid,
                            method: smethod,
                            params,
                        } => {
                            let response = server_request_response(
                                self.elicitation.as_ref(),
                                &self.server,
                                &rid,
                                &smethod,
                                &params,
                            );
                            if let Err(e) = self.post(&response) {
                                self.dead = true;
                                return Err(e);
                            }
                            continue;
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
                let _ = resp.into_body().read_to_string();
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
                    Wire::Request {
                        id: rid,
                        method: smethod,
                        params,
                    } => {
                        let response = server_request_response(
                            self.elicitation.as_ref(),
                            &self.server,
                            &rid,
                            &smethod,
                            &params,
                        );
                        self.post(&response)?;
                        continue;
                    }
                    // Anything else is not our answer.
                    _ => continue,
                },
            }
        }
    }

    /// `tools/list`, following `nextCursor` pages. A repeated cursor or
    /// 1,000 pages ends the walk defensively.
    pub fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..1000 {
            let params = match &cursor {
                Some(c) => serde_json::json!({"cursor": c}),
                None => serde_json::json!({}),
            };
            let result = self.request("tools/list", params)?;
            out.extend(tools_from(&result, &self.server)?);
            match crate::next_cursor(&result) {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
        Err(McpError::Protocol(
            "tools/list exceeded 1,000 pages".to_string(),
        ))
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

    /// `resources/list`, following `nextCursor` pages. A repeated cursor or
    /// 1,000 pages ends the walk defensively.
    pub fn list_resources(&mut self) -> Result<Vec<McpResource>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..1000 {
            let params = match &cursor {
                Some(c) => serde_json::json!({"cursor": c}),
                None => serde_json::json!({}),
            };
            let result = self.request("resources/list", params)?;
            out.extend(crate::resources_from(&result)?);
            match crate::next_cursor(&result) {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
        Err(McpError::Protocol(
            "resources/list exceeded 1,000 pages".to_string(),
        ))
    }

    pub fn list_resource_templates(&mut self) -> Result<Vec<McpResourceTemplate>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..1000 {
            let params = match &cursor {
                Some(c) => serde_json::json!({"cursor": c}),
                None => serde_json::json!({}),
            };
            let result = self.request("resources/templates/list", params)?;
            out.extend(crate::resource_templates_from(&result)?);
            match crate::next_cursor(&result) {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
        Err(McpError::Protocol(
            "resources/templates/list exceeded 1,000 pages".to_string(),
        ))
    }

    pub fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
        let result = self.request("resources/read", serde_json::json!({"uri": uri}))?;
        crate::resource_contents_from(&result)
    }

    pub fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..1000 {
            let params = match &cursor {
                Some(c) => serde_json::json!({"cursor": c}),
                None => serde_json::json!({}),
            };
            let result = self.request("prompts/list", params)?;
            out.extend(crate::prompts_from(&result)?);
            match crate::next_cursor(&result) {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                _ => return Ok(out),
            }
        }
        Err(McpError::Protocol(
            "prompts/list exceeded 1,000 pages".to_string(),
        ))
    }

    pub fn get_prompt(&mut self, name: &str, args: &Value) -> Result<McpPromptResult, McpError> {
        let result = self.request(
            "prompts/get",
            serde_json::json!({"name": name, "arguments": args}),
        )?;
        crate::prompt_result_from(&result)
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

    fn list_resources(&mut self) -> Result<Vec<McpResource>, McpError> {
        HttpMcpClient::list_resources(self)
    }

    fn list_resource_templates(&mut self) -> Result<Vec<McpResourceTemplate>, McpError> {
        HttpMcpClient::list_resource_templates(self)
    }

    fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
        HttpMcpClient::read_resource(self, uri)
    }

    fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpError> {
        HttpMcpClient::list_prompts(self)
    }

    fn get_prompt(&mut self, name: &str, args: &Value) -> Result<McpPromptResult, McpError> {
        HttpMcpClient::get_prompt(self, name, args)
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
/// timeouts on the response reader as an `io::Error` wrapping
/// `Error::Timeout`, which displays as `timeout: <phase>`; a raw socket
/// timeout keeps the `timed out` message.
fn match_stream_err(e: &str, method: &str) -> McpError {
    if e.contains("timed out") || e.contains("TimedOut") || e.contains("timeout: ") {
        McpError::Timeout {
            method: method.to_string(),
        }
    } else {
        McpError::Framing(format!("sse stream: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn tools_list_follows_next_cursor_over_streamable_http() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(mut s) = stream else { continue };
                let mut buf = Vec::new();
                let mut byte = [0u8; 1];
                while !buf.ends_with(b"\r\n\r\n") {
                    match s.read(&mut byte) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => buf.push(byte[0]),
                    }
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                let len = text
                    .lines()
                    .find_map(|l| l.strip_prefix("Content-Length:").or_else(|| l.strip_prefix("content-length:")))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                if len > 0 {
                    let _ = s.read_exact(&mut body);
                }
                let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
                let (status, payload): (u16, String) = match method.as_str() {
                    "initialize" => (
                        200,
                        serde_json::json!({"jsonrpc": "2.0", "id": req.get("id"), "result": {"protocolVersion": crate::MCP_PROTOCOL_VERSION, "serverInfo": {"version": "http-1"}}}).to_string(),
                    ),
                    "tools/list" => {
                        let cursor = req.get("params").and_then(|p| p.get("cursor")).and_then(|c| c.as_str());
                        let result = if cursor == Some("p2") {
                            serde_json::json!({"tools": [{"name": "second", "inputSchema": {"type": "object"}}]})
                        } else {
                            serde_json::json!({"tools": [{"name": "first", "inputSchema": {"type": "object"}}], "nextCursor": "p2"})
                        };
                        (200, serde_json::json!({"jsonrpc": "2.0", "id": req.get("id"), "result": result}).to_string())
                    }
                    _ => (202, String::new()),
                };
                let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = s.write_all(resp.as_bytes());
            }
        });
        let mut client = HttpMcpClient::connect(
            "fake-http",
            HttpTransport::Streamable,
            &format!("http://{addr}/mcp"),
            Duration::from_secs(10),
            &[],
        )
        .expect("connect fake http server");
        let tools = client.list_tools().expect("tools/list");
        assert_eq!(
            tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        drop(client);
        let _ = handle.join();
    }
}
