//! Agent Client Protocol (ACP) server over stdio.
//!
//! `pantheon serve acp` speaks newline-delimited JSON-RPC 2.0 on stdin/stdout,
//! which is what Zed and other ACP clients expect. It maps one ACP session to
//! one Pantheon run:
//!
//! * `initialize` advertises protocol v1 and the capabilities we implement;
//! * `session/new` / `session/load` create or reopen a run and set the
//!   session workspace to the client's `cwd`;
//! * `session/prompt` streams `session/update` notifications while the model
//!   and tools work, then returns an ACP `stopReason`;
//! * approval-gated tool calls become `session/request_permission` requests;
//! * UI approval cards (`render_ui` parks) become the same
//!   `session/request_permission` requests, resolved through the runtime's
//!   revision-pinned `submit_ui_action`; document content degrades to
//!   markdown for every client, and the raw `UiDocument` rides the
//!   `_pantheon/uiDocument` extension notification for clients that opt in;
//!   `ask_user` questions become an assistant message and are answered by the
//!   next `session/prompt` (the run is parked, so the text is routed through
//!   `Supervisor::answer_input`, not treated as a fresh user turn).
//!
//! The server is deliberately one prompt at a time. `Session` already owns a
//! single run lease and process-global working directory; pretending to run
//! concurrent ACP prompts would let two clients race those resources.
//!
//! This module is transport and translation only. Tool gating, approvals,
//! ledger persistence, and the actual turn loop remain in the runtime.

use crate::config;
use pantheon_api::events::Event;
use pantheon_api::mode::AgentMode;
use pantheon_runtime::session::Session;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

const PROTOCOL_VERSION: u64 = 1;
const PERMISSION_TIMEOUT: Duration = Duration::from_secs(300);

type PendingReply = mpsc::Sender<Value>;

struct Shared {
    out: Mutex<std::io::Stdout>,
    pending: Mutex<HashMap<String, PendingReply>>,
    next_id: AtomicU64,
    /// ACP session id -> Pantheon run id.
    sessions: Mutex<HashMap<String, String>>,
    /// The run currently owned by a prompt worker, if any.
    active: Mutex<Option<String>>,
    /// A parked `ask_user` keyed by ACP session id: (call id, options).
    pending_input: Mutex<HashMap<String, (String, Vec<String>)>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            out: Mutex::new(std::io::stdout()),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
            active: Mutex::new(None),
            pending_input: Mutex::new(HashMap::new()),
        }
    }

    fn send(&self, value: Value) {
        if let Ok(mut out) = self.out.lock() {
            let _ = writeln!(out, "{value}");
            let _ = out.flush();
        }
    }

    fn respond(&self, id: Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn error(&self, id: Value, code: i64, message: impl Into<String>) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message.into()}
        }));
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn update(&self, session_id: &str, update: Value) {
        self.notify(
            "session/update",
            json!({"sessionId": session_id, "update": update}),
        );
    }

    /// Rich-UI documents: markdown fallback for every ACP client (the
    /// protocol's own degradation rule), plus an extension notification with
    /// the raw `UiDocument` for clients that opt into native rendering.
    /// Extension delivery is additive: standard clients ignore the method.
    fn ui_document(
        &self,
        session_id: &str,
        ui_id: &str,
        revision: u64,
        document: &pantheon_api::ui::UiDocument,
    ) {
        let markdown = ui_document_markdown(document);
        if !markdown.is_empty() {
            self.text_chunk(session_id, &markdown);
        }
        if let Ok(doc) = serde_json::to_value(document) {
            self.notify(
                "_pantheon/uiDocument",
                json!({"sessionId": session_id, "uiId": ui_id, "revision": revision, "document": doc}),
            );
        }
    }

    fn tool_call(&self, session_id: &str, call_id: &str, tool: &str, args: &str, status: &str) {
        self.update(
            session_id,
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id,
                "title": tool,
                "kind": tool_kind(tool),
                "status": status,
                "content": [{"type": "content", "content": {"type": "text", "text": args}}],
                "rawInput": parse_raw_input(args),
            }),
        );
    }

    fn tool_update(&self, session_id: &str, call_id: &str, status: &str, text: Option<&str>) {
        let mut update = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": call_id,
            "status": status,
        });
        if let Some(text) = text {
            update["content"] = json!([{
                "type": "content",
                "content": {"type": "text", "text": text}
            }]);
        }
        self.update(session_id, update);
    }

    fn text_chunk(&self, session_id: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.update(
            session_id,
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            }),
        );
    }

    /// Send a request to the client and wait for its response. The reader
    /// loop stays free while the prompt worker blocks here, so a permission
    /// answer (or a cancel) can always be read.
    fn request_client(&self, method: &str, params: Value) -> Option<Value> {
        let id = format!(
            "pantheon-{}-{}",
            method.replace('/', "-"),
            self.next_id.fetch_add(1, Ordering::SeqCst)
        );
        let (tx, rx) = mpsc::channel();
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(id.clone(), tx);
        }
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        match rx.recv_timeout(PERMISSION_TIMEOUT) {
            Ok(response) => response.get("result").cloned(),
            Err(_) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&id);
                }
                None
            }
        }
    }

    fn resolve_response(&self, response: &Value) -> bool {
        let Some(id) = response.get("id") else {
            return false;
        };
        let key = match id {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            _ => return false,
        };
        let tx = self.pending.lock().ok().and_then(|mut p| p.remove(&key));
        if let Some(tx) = tx {
            let _ = tx.send(response.clone());
            true
        } else {
            false
        }
    }
}

/// Entry point for `pantheon serve acp`.
pub fn run_stdio() {
    let data_dir = crate::terminal::data_dir();
    let session = match open_session(&data_dir) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("acp: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("pantheon acp: protocol v{PROTOCOL_VERSION} on stdio");
    let shared = Arc::new(Shared::new());
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => {
                eprintln!("acp: read stdin: {e}");
                break;
            }
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                shared.error(Value::Null, -32700, format!("parse error: {e}"));
                continue;
            }
        };
        // Responses to requests we sent (permission prompts).
        if message.get("method").is_none()
            && (message.get("result").is_some() || message.get("error").is_some())
        {
            shared.resolve_response(&message);
            continue;
        }
        let Some(method) = message.get("method").and_then(|m| m.as_str()) else {
            continue;
        };
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
        match method {
            "initialize" => {
                if let Some(id) = id {
                    shared.respond(
                        id,
                        json!({
                            "protocolVersion": PROTOCOL_VERSION,
                            "agentCapabilities": {
                                "loadSession": true,
                                "promptCapabilities": {
                                    "image": false,
                                    "audio": false,
                                    "embeddedContext": true
                                },
                                "mcpCapabilities": {"http": false, "sse": false}
                            },
                            "authMethods": [],
                            "agentInfo": {
                                "name": "pantheon",
                                "version": env!("CARGO_PKG_VERSION")
                            }
                        }),
                    );
                }
            }
            "authenticate" => {
                if let Some(id) = id {
                    shared.respond(id, json!({}));
                }
            }
            "session/new" => {
                let Some(id) = id else { continue };
                let cwd = params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from)
                    .or_else(|| std::env::current_dir().ok())
                    .unwrap_or_else(|| PathBuf::from("."));
                if let Err(e) = session.set_workspace_root(&cwd) {
                    shared.error(id, -32000, format!("set cwd: {e}"));
                    continue;
                }
                let run_id = pantheon_runtime::new_run_id();
                if let Ok(mut sessions) = shared.sessions.lock() {
                    sessions.insert(run_id.clone(), run_id.clone());
                }
                shared.respond(
                    id,
                    json!({
                        "sessionId": run_id,
                        "modes": {
                            "currentModeId": agent_mode_id(session.mode()),
                            "availableModes": [
                                {"id": "build", "name": "Build", "description": "Edit and run code."},
                                {"id": "plan", "name": "Plan", "description": "Read-only planning."}
                            ]
                        }
                    }),
                );
            }
            "session/load" => {
                let Some(id) = id else { continue };
                let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) else {
                    shared.error(id, -32602, "session/load requires sessionId");
                    continue;
                };
                let cwd = params
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(PathBuf::from);
                if let Some(cwd) = cwd {
                    if let Err(e) = session.set_workspace_root(&cwd) {
                        shared.error(id, -32000, format!("set cwd: {e}"));
                        continue;
                    }
                }
                let run_id = session_id.to_string();
                if let Ok(mut sessions) = shared.sessions.lock() {
                    sessions.insert(session_id.to_string(), run_id.clone());
                }
                replay_run(&shared, session_id, &run_id, &session);
                shared.respond(id, json!({}));
            }
            "session/prompt" => {
                let Some(id) = id else { continue };
                let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) else {
                    shared.error(id, -32602, "session/prompt requires sessionId");
                    continue;
                };
                let run_id = shared
                    .sessions
                    .lock()
                    .ok()
                    .and_then(|s| s.get(session_id).cloned())
                    .unwrap_or_else(|| session_id.to_string());
                if shared.active.lock().map(|a| a.is_some()).unwrap_or(false) {
                    shared.error(id, -32000, "another prompt is already running");
                    continue;
                }
                let text = prompt_text(&params);
                if text.trim().is_empty() {
                    shared.error(id, -32602, "prompt contains no text");
                    continue;
                }
                if let Ok(mut active) = shared.active.lock() {
                    *active = Some(run_id.clone());
                }
                let shared = Arc::clone(&shared);
                let session = Arc::clone(&session);
                let session_id = session_id.to_string();
                std::thread::spawn(move || {
                    let result = run_prompt(&shared, &session, &session_id, &run_id, &text);
                    // The worker owns clearing `active`; do it before the
                    // response so a client can immediately send another turn.
                    if let Ok(mut active) = shared.active.lock() {
                        *active = None;
                    }
                    match result {
                        Ok(stop) => shared.respond(id, json!({"stopReason": stop})),
                        Err(e) => shared.error(id, -32000, e),
                    }
                });
            }
            "session/cancel" => {
                if let Some(session_id) = params.get("sessionId").and_then(|v| v.as_str()) {
                    let run_id = shared
                        .sessions
                        .lock()
                        .ok()
                        .and_then(|s| s.get(session_id).cloned())
                        .unwrap_or_else(|| session_id.to_string());
                    session.cancel_current_run(&run_id, "ACP client cancelled");
                }
            }
            "session/set_mode" => {
                if let Some(id) = id {
                    let mode = params.get("modeId").and_then(|v| v.as_str());
                    session.set_mode(match mode {
                        Some("plan") => AgentMode::Plan,
                        _ => AgentMode::Build,
                    });
                    shared.respond(id, json!({}));
                }
            }
            "session/set_model" => {
                if let Some(id) = id {
                    let raw = params.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                    let (provider, model) = raw.split_once('/').unwrap_or(("", raw));
                    if provider.is_empty() || model.is_empty() {
                        shared.error(id, -32602, "modelId must be provider/model");
                    } else {
                        match session.switch_model(provider, model) {
                            Ok(()) => shared.respond(id, json!({})),
                            Err(e) => shared.error(id, -32000, e.to_string()),
                        }
                    }
                }
            }
            "session/close" => {
                if let Some(id) = id {
                    shared.respond(id, json!({}));
                }
            }
            _ => {
                if let Some(id) = id {
                    shared.error(id, -32601, format!("method not found: {method}"));
                }
            }
        }
    }
    session.cancel_current_run("", "ACP client disconnected");
}

fn open_session(data_dir: &Path) -> Result<Session, String> {
    let cfg = config::Config::load(data_dir).ok();
    let model_policy = config::build_model_policy(cfg.as_ref(), None, None);
    let policy = crate::config_schema::policy_for_config(&cfg);
    let secrets = config::chat_secrets(cfg.as_ref());
    let session = Session::new(data_dir.to_path_buf(), policy, model_policy, secrets)
        .map_err(|e| format!("open session: {e}"))?;
    config::apply_tool_enablement(&session, cfg.as_ref());
    config::apply_budget_tiers(&session, cfg.as_ref());
    Ok(session)
}

/// One prompt, including approval/input resume loops. Returns an ACP
/// `stopReason` on success.
fn run_prompt(
    shared: &Arc<Shared>,
    session: &Session,
    session_id: &str,
    run_id: &str,
    text: &str,
) -> Result<&'static str, String> {
    session.reset_cancel();
    let mut next_text = text.to_string();
    // A parked ask_user is answered by the next prompt instead of starting a
    // new model turn. The runtime emits the answer as the tool result.
    let pending = shared
        .pending_input
        .lock()
        .ok()
        .and_then(|mut p| p.remove(session_id));
    if let Some((call_id, _options)) = pending {
        session
            .supervisor
            .answer_input(run_id, &call_id, text)
            .map_err(|e| format!("answer question: {e}"))?;
        shared.notify(
            "_pantheon/input_resolved",
            json!({"sessionId": session_id, "requestId": call_id}),
        );
        next_text.clear();
    }

    for _ in 0..12 {
        let saw_delta = Arc::new(AtomicBool::new(false));
        let approvals = Arc::new(Mutex::new(Vec::<String>::new()));
        let stream = {
            let shared = Arc::clone(shared);
            let saw_delta = Arc::clone(&saw_delta);
            let approvals = Arc::clone(&approvals);
            let session_id = session_id.to_string();
            let run_id = run_id.to_string();
            session
                .supervisor
                .register_observer(Arc::new(move |event: &Event| match event {
                    Event::ModelDelta { run_id: r, delta } if r == &run_id => {
                        saw_delta.store(true, Ordering::Relaxed);
                        shared.text_chunk(&session_id, delta);
                    }
                    Event::AssistantMessage { run_id: r, message } if r == &run_id => {
                        if !saw_delta.load(Ordering::Relaxed) && !message.content.is_empty() {
                            shared.text_chunk(&session_id, &message.content);
                        }
                    }
                    Event::ToolStarted {
                        run_id: r,
                        call_id,
                        tool,
                        args,
                        ..
                    } if r == &run_id => {
                        shared.tool_call(&session_id, call_id, tool, args, "in_progress")
                    }
                    Event::ToolMessage { run_id: r, message } if r == &run_id => {
                        if let Some(call_id) = &message.tool_call_id {
                            shared.tool_update(
                                &session_id,
                                call_id,
                                "completed",
                                Some(&message.content),
                            );
                        }
                    }
                    Event::ApprovalRequested { run_id: r, scope } if r == &run_id => {
                        if let Ok(mut a) = approvals.lock() {
                            a.push(scope.clone());
                        }
                    }
                    Event::UiContent {
                        run_id: r,
                        ui_id,
                        document,
                        ..
                    } if r == &run_id => {
                        shared.ui_document(&session_id, ui_id, 0, document);
                    }
                    Event::UiUpdated {
                        run_id: r,
                        ui_id,
                        revision,
                        document,
                        ..
                    } if r == &run_id => {
                        shared.ui_document(&session_id, ui_id, *revision, document);
                    }
                    _ => {}
                }))
        };

        let outcome = session.chat_turn(run_id, "", &next_text);
        drop(stream);
        let outcome = match outcome {
            Ok(o) => o,
            Err(e) if e.code == "RUN_PARKED" => {
                // The run already rejected the turn because it was parked.
                // Fall through to the pending-approval loop below instead of
                // surfacing a transport error.
                let scopes = session
                    .supervisor
                    .pending_approvals(run_id)
                    .unwrap_or_default();
                if scopes.is_empty() {
                    return Err(e.to_string());
                }
                for scope in scopes {
                    if !resolve_approval(shared, session, session_id, run_id, &scope)? {
                        return Ok("cancelled");
                    }
                }
                next_text.clear();
                continue;
            }
            Err(e) => return Err(e.to_string()),
        };

        match outcome {
            pantheon_agent::LoopOutcome::Answered { .. } => return Ok("end_turn"),
            pantheon_agent::LoopOutcome::Canceled { .. } => return Ok("cancelled"),
            pantheon_agent::LoopOutcome::Denied { .. } => return Ok("refusal"),
            pantheon_agent::LoopOutcome::BudgetExhausted { cap } => {
                return Ok(if cap.contains("token") {
                    "max_tokens"
                } else {
                    "max_turn_requests"
                });
            }
            pantheon_agent::LoopOutcome::AwaitingInput {
                call_id,
                question,
                options,
            } => {
                shared.text_chunk(session_id, &question);
                // First-class ask-user: the notification carries the question
                // and its options so a client can render its question form;
                // the answer arrives as the next prompt (see run_prompt).
                shared.notify(
                    "_pantheon/input_request",
                    json!({
                        "sessionId": session_id,
                        "requestId": call_id,
                        "request": {
                            "kind": "questions",
                            "questions": [{
                                "id": "answer",
                                "question": question,
                                "isOther": true,
                                "options": options
                                    .iter()
                                    .map(|option| json!({"label": option}))
                                    .collect::<Vec<_>>(),
                            }],
                        },
                    }),
                );
                if let Ok(mut pending) = shared.pending_input.lock() {
                    pending.insert(session_id.to_string(), (call_id, options));
                }
                return Ok("end_turn");
            }
            pantheon_agent::LoopOutcome::AwaitingUiAction { .. } => {
                // A UI approval card parked the turn. Surface it as the same
                // client permission request a gated tool call gets; the
                // decision resumes the run through the runtime's own
                // revision-pinned submission path, and the loop drains the
                // resumed turn below.
                resolve_ui_action(shared, session, session_id, run_id)?;
                next_text.clear();
            }
            pantheon_agent::LoopOutcome::AwaitingApproval { scope, .. } => {
                let scopes = {
                    let mut all = session
                        .supervisor
                        .pending_approvals(run_id)
                        .unwrap_or_default();
                    if !all.iter().any(|s| s == &scope) {
                        all.push(scope);
                    }
                    all
                };
                for scope in scopes {
                    if !resolve_approval(shared, session, session_id, run_id, &scope)? {
                        return Ok("cancelled");
                    }
                }
                next_text.clear();
            }
            pantheon_agent::LoopOutcome::Delegated { .. } => return Ok("end_turn"),
        }
    }
    Err("turn parked too many times without resolving".into())
}

/// Collect every node of a serialized `UiDocument` as `(id, node)` pairs.
/// Works on JSON so the walker survives component-enum additions without
/// changes here; the runtime validates documents before they reach events.
fn ui_nodes(value: &serde_json::Value, out: &mut Vec<(String, serde_json::Value)>) {
    match value {
        serde_json::Value::Array(items) => {
            for item in items {
                ui_nodes(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            if let (Some(id), Some(tag)) = (
                map.get("id").and_then(|v| v.as_str()),
                map.get("type").and_then(|v| v.as_str()),
            ) {
                out.push((id.to_string(), value.clone()));
                let _ = tag;
            }
            for nested in map.values() {
                ui_nodes(nested, out);
            }
        }
        _ => {}
    }
}

/// The protocol's own degradation rule: unknown renderers must still leave
/// the content readable. Markdown and tables render directly; every other
/// component becomes a tagged placeholder; approval cards state their
/// decision explicitly.
fn ui_document_markdown(document: &pantheon_api::ui::UiDocument) -> String {
    let Ok(value) = serde_json::to_value(document) else {
        return String::new();
    };
    let mut nodes = Vec::new();
    ui_nodes(&value, &mut nodes);
    let mut parts: Vec<String> = Vec::new();
    for (_id, node) in nodes {
        match node["type"].as_str().unwrap_or_default() {
            "markdown" => {
                if let Some(title) = node["title"].as_str() {
                    parts.push(format!("### {title}"));
                }
                if let Some(text) = node["text"].as_str() {
                    parts.push(text.to_string());
                }
            }
            "table" => {
                let headers: Vec<&str> = node["headers"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|h| h.as_str()).collect())
                    .unwrap_or_default();
                if headers.is_empty() {
                    continue;
                }
                parts.push(format!("| {} |", headers.join(" | ")));
                parts.push(format!("|{}|", "| --- ".repeat(headers.len())));
                for row in node["rows"].as_array().into_iter().flatten() {
                    let cells: Vec<String> = row
                        .as_array()
                        .map(|c| {
                            c.iter()
                                .map(|x| x.as_str().unwrap_or_default().to_string())
                                .collect()
                        })
                        .unwrap_or_default();
                    parts.push(format!("| {} |", cells.join(" | ")));
                }
            }
            "approval" => {
                let title = node["title"].as_str().unwrap_or("Approval");
                let detail = node["detail"].as_str().unwrap_or_default();
                let approve = node["approve"]["label"].as_str().unwrap_or("approve");
                let deny = node["deny"]["label"].as_str().unwrap_or("deny");
                parts.push(format!(
                    "**{title}** - {detail}\n\nThe client decides: **{approve}** or {deny}."
                ));
            }
            other if !other.is_empty() => parts.push(format!("\u{ab}{other}\u{bb}")),
            _ => {}
        }
    }
    parts.join("\n\n")
}

fn fresh_ui_nonce() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "acp-{}-{}",
        nanos,
        SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
    )
}

/// Resolve a parked UI approval card through the client's standard
/// permission request. The decision is submitted as a real
/// `UiActionSubmission` against the displayed revision, so park resolution,
/// audit, and replay follow the runtime's own rules instead of an ACP
/// side channel.
fn resolve_ui_action(
    shared: &Shared,
    session: &Session,
    session_id: &str,
    run_id: &str,
) -> Result<(), String> {
    let Some((ui_id, call_id)) = session
        .pending_ui_action(run_id)
        .map_err(|e| format!("pending ui action: {e}"))?
    else {
        return Ok(());
    };
    // Latest document for this ui_id: replays carry the full replacement.
    let entries = session
        .supervisor
        .replay(run_id)
        .map_err(|e| format!("replay: {e}"))?;
    let mut latest: Option<(u64, &pantheon_api::ui::UiDocument)> = None;
    for entry in &entries {
        match &entry.event {
            Event::UiContent {
                ui_id: u, document, ..
            } if u == &ui_id => {
                if latest.as_ref().is_none_or(|(rev, _)| *rev < 1) {
                    latest = Some((0, document));
                }
            }
            Event::UiUpdated {
                ui_id: u,
                revision,
                document,
                ..
            } if u == &ui_id && latest.as_ref().is_none_or(|(rev, _)| *revision > *rev) => {
                latest = Some((*revision, document));
            }
            _ => {}
        }
    }
    let Some((revision, document)) = latest else {
        return Ok(());
    };
    let Ok(doc_value) = serde_json::to_value(document) else {
        return Ok(());
    };
    let mut nodes = Vec::new();
    ui_nodes(&doc_value, &mut nodes);
    // The card's approve ref carries decision "grant", deny carries "deny"
    // (`ActionKind::Approval` is internally tagged, so `decision` is a
    // sibling of the `kind` string in JSON).
    let mut approve: Option<(&str, &str, &str)> = None; // (node_id, action_id, label)
    let mut deny: Option<(&str, &str, &str)> = None;
    let mut title = "Approve?";
    let mut detail = String::new();
    for (node_id, node) in &nodes {
        if node["type"].as_str() != Some("approval") {
            continue;
        }
        if let Some(t) = node["title"].as_str() {
            title = t;
        }
        if let Some(d) = node["detail"].as_str() {
            detail = d.to_string();
        }
        for (key, decision) in [("approve", "grant"), ("deny", "deny")] {
            if node[key]["decision"].as_str() != Some(decision) {
                continue;
            }
            let found = (
                node_id.as_str(),
                node[key]["action_id"].as_str().unwrap_or_default(),
                node[key]["label"].as_str().unwrap_or(decision),
            );
            if key == "approve" {
                approve = Some(found);
            } else {
                deny = Some(found);
            }
        }
    }
    let (
        Some((approve_node, approve_action, approve_label)),
        Some((deny_node, deny_action, deny_label)),
    ) = (approve, deny)
    else {
        // Card without a resolvable approve/deny pair: degrade to the text
        // path so the park stays operator-resolvable in a full client.
        shared.text_chunk(
            session_id,
            "This turn is waiting for a structured UI decision. Respond in the client UI, then continue the session.",
        );
        return Ok(());
    };
    let request = json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": call_id,
            "title": "render_ui",
            "kind": "read",
            "status": "pending",
            "content": [{"type": "content", "content": {"type": "text", "text": format!("{title}\n{detail}")}}],
        },
        "options": [
            {"optionId": "allow_once", "name": approve_label, "kind": "allow_once"},
            {"optionId": "reject_once", "name": deny_label, "kind": "reject_once"}
        ]
    });
    let Some(response) = shared.request_client("session/request_permission", request) else {
        // No client answer: record the deny decision so the park resolves
        // instead of hanging the session.
        let chosen = (deny_node, deny_action);
        return submit_ui_decision(session, run_id, &ui_id, revision, chosen);
    };
    let approved = response
        .get("outcome")
        .and_then(|o| o.get("outcome"))
        .and_then(|o| o.as_str())
        == Some("selected")
        && response
            .get("outcome")
            .and_then(|o| o.get("optionId"))
            .and_then(|o| o.as_str())
            == Some("allow_once");
    let chosen = if approved {
        (approve_node, approve_action)
    } else {
        (deny_node, deny_action)
    };
    submit_ui_decision(session, run_id, &ui_id, revision, chosen)
}

fn submit_ui_decision(
    session: &Session,
    run_id: &str,
    ui_id: &str,
    revision: u64,
    (node_id, action_id): (&str, &str),
) -> Result<(), String> {
    let submission = pantheon_api::ui::UiActionSubmission {
        ui_id: ui_id.to_string(),
        revision,
        node_id: node_id.to_string(),
        action_id: action_id.to_string(),
        params: Default::default(),
        nonce: fresh_ui_nonce(),
    };
    session
        .submit_ui_action(run_id, &submission)
        .map(|_| ())
        .map_err(|e| format!("ui action submit: {e}"))
}

fn resolve_approval(
    shared: &Shared,
    session: &Session,
    session_id: &str,
    run_id: &str,
    scope: &str,
) -> Result<bool, String> {
    let (call_id, tool, args) = split_scope(scope);
    let request = json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": call_id,
            "title": tool,
            "kind": tool_kind(&tool),
            "status": "pending",
            "content": [{"type": "content", "content": {"type": "text", "text": args}}],
            "rawInput": parse_raw_input(&args),
        },
        "options": [
            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
            {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}
        ]
    });
    let Some(response) = shared.request_client("session/request_permission", request) else {
        // No client answer: deny rather than silently running the call.
        let _ = session.supervisor.deny(run_id, scope);
        return Ok(false);
    };
    let outcome = response.get("outcome").and_then(|o| o.get("outcome"));
    let option = response
        .get("outcome")
        .and_then(|o| o.get("optionId"))
        .and_then(|o| o.as_str());
    match (outcome.and_then(|o| o.as_str()), option) {
        (Some("selected"), Some("allow_once")) => {
            session
                .supervisor
                .grant(run_id, scope)
                .map_err(|e| format!("grant: {e}"))?;
            Ok(true)
        }
        (Some("selected"), Some("reject_once")) | (Some("cancelled"), _) => {
            session
                .supervisor
                .deny(run_id, scope)
                .map_err(|e| format!("deny: {e}"))?;
            Ok(outcome.and_then(|o| o.as_str()) != Some("cancelled"))
        }
        _ => {
            let _ = session.supervisor.deny(run_id, scope);
            Ok(false)
        }
    }
}

fn replay_run(shared: &Shared, session_id: &str, run_id: &str, session: &Session) {
    let entries = match session.supervisor.replay(run_id) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        match entry.event {
            Event::UserMessage { text, .. } => shared.update(
                session_id,
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": text}
                }),
            ),
            Event::AssistantMessage { message, .. } if !message.content.is_empty() => {
                shared.text_chunk(session_id, &message.content)
            }
            Event::ToolStarted {
                call_id,
                tool,
                args,
                ..
            } => shared.tool_call(session_id, &call_id, &tool, &args, "completed"),
            Event::ToolMessage { message, .. } => {
                if let Some(call_id) = message.tool_call_id {
                    shared.tool_update(session_id, &call_id, "completed", Some(&message.content));
                }
            }
            _ => {}
        }
    }
}

fn split_scope(scope: &str) -> (String, String, String) {
    let mut parts = scope.splitn(3, ':');
    let call = parts.next().unwrap_or(scope).to_string();
    let tool = parts.next().unwrap_or("tool").to_string();
    let args = parts.next().unwrap_or("").to_string();
    (call, tool, args)
}

fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read_file" | "list_dir" | "grep" | "glob" | "codegraph_search" | "session_search" => {
            "read"
        }
        "write_file" | "apply_files" | "apply_staged" | "edit" | "patch" => "edit",
        "delete_file" | "remove_file" => "delete",
        "move_file" | "rename_file" => "move",
        "shell" | "terminal_open" | "terminal_write" | "terminal_read" | "terminal_close" => {
            "execute"
        }
        "websearch" | "webfetch" | "browser" => "fetch",
        "todo" => "think",
        _ => "other",
    }
}

fn parse_raw_input(args: &str) -> Value {
    serde_json::from_str(args).unwrap_or_else(|_| json!(args))
}

fn prompt_text(params: &Value) -> String {
    let Some(prompt) = params.get("prompt").and_then(|p| p.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for block in prompt {
        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

fn agent_mode_id(mode: AgentMode) -> &'static str {
    match mode {
        AgentMode::Plan => "plan",
        AgentMode::Build => "build",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_joins_text_blocks_and_ignores_other_kinds() {
        let params = json!({
            "prompt": [
                {"type": "text", "text": "first"},
                {"type": "resource_link", "uri": "file:///x"},
                {"type": "text", "text": "second"}
            ]
        });
        assert_eq!(prompt_text(&params), "first\nsecond");
    }

    #[test]
    fn scope_splits_into_call_tool_and_verbatim_args() {
        let (call, tool, args) = split_scope("call_1_0:shell:{\"command\":\"ls\"}");
        assert_eq!(call, "call_1_0");
        assert_eq!(tool, "shell");
        assert_eq!(args, "{\"command\":\"ls\"}");
    }

    #[test]
    fn tool_kinds_cover_the_documented_editor_categories() {
        assert_eq!(tool_kind("read_file"), "read");
        assert_eq!(tool_kind("apply_files"), "edit");
        assert_eq!(tool_kind("shell"), "execute");
        assert_eq!(tool_kind("unknown"), "other");
    }

    #[test]
    fn agent_mode_ids_are_stable() {
        assert_eq!(agent_mode_id(AgentMode::Build), "build");
        assert_eq!(agent_mode_id(AgentMode::Plan), "plan");
    }
}
