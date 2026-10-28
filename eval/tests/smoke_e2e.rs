//! End-to-end smoke suite: every major path driven start to finish, not
//! just its units. Run with `cargo test -p pantheon-eval --test smoke_e2e`.
//!
//! FAKE-PROVIDER MARKING (read before trusting a green run): tests whose
//! names contain `fake_provider` drive a REAL `Session::chat_turn` through
//! the REAL provider chain and HTTP transport, but the "provider" at the
//! other end is a local HTTP server in this file speaking the OpenAI SSE
//! wire format. That is the faithful seam the codebase offers: the SSRF
//! guard blocks loopback provider URLs by default, so these tests opt in
//! with `PANTHEON_ALLOW_LOOPBACK_PROVIDERS=1` (see the loopback-gate
//! unit tests in `pantheon-providers/src/http.rs`, which prove the gate
//! stays closed by default and still blocks LAN ranges when open). A
//! green fake-provider test proves the wiring, the ledger, the approval
//! gates and the resume machinery - it is NEVER a live-provider proof.
//! Live-provider proof is blocked-by-credential until a key is supplied;
//! see the credential list at the end of the sweep report.
//!
//! Browser/search corrections baked into the suite (Umar, 2026-10-28):
//! - Camofox needs NO API key (local build; only a proxy password when a
//!   proxy is used). Steel is the hosted/keyed one and is NOT recommended.
//! - Recommended browser backend is GSD (default): it needs `gsd-browser`
//!   on PATH + Chrome/Chromium on the host + the local
//!   `GSD_BROWSER_VAULT_KEY` vault value - not a provider API key.
//! - Recommended search provider is TinyFish: `TINYFISH_API_KEY` from the
//!   free dashboard, sent as `X-API-Key`. Exa/Tavily are not primary.
//! - NVIDIA is an OpenAI-compatible custom provider; its rate limits are
//!   deliberate and covered by explicit 429 smoke cases below.

#[path = "support.rs"]
mod support;

use pantheon_agent::LoopOutcome;
use pantheon_api::capability::{Capability, Policy};
use pantheon_api::error::PantheonError;
use pantheon_api::events::Event;
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_api::permission_mode::PermissionMode;
use pantheon_runtime::session::Session;
use pantheon_runtime::Supervisor;
use pantheon_secrets::SecretsBroker;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Local fake HTTP server (fake provider + TinyFish mirror)
// ---------------------------------------------------------------------------

/// Request handler: (method, path, raw headers, body, stream index)
/// -> response body. `stream_idx` is `usize::MAX` for non-stream requests
/// and counts only streaming POSTs otherwise (the title auxiliary posts
/// non-streaming, so it never disturbs a scripted turn sequence).
type Handler = Arc<dyn Fn(&str, &str, &str, &str, usize) -> String + Send + Sync>;

struct FakeServer {
    port: u16,
    stream_bodies: Arc<Mutex<Vec<String>>>,
}

fn start_server(handler: Handler) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fake server bind");
    let port = listener.local_addr().expect("local addr").port();
    let stream_bodies = Arc::new(Mutex::new(Vec::new()));
    let sb = Arc::clone(&stream_bodies);
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let h = Arc::clone(&handler);
                    let sb = Arc::clone(&sb);
                    std::thread::spawn(move || serve_conn(stream, &h, &sb));
                }
                Err(_) => break,
            }
        }
    });
    FakeServer {
        port,
        stream_bodies,
    }
}

fn serve_conn(stream: TcpStream, handler: &Handler, stream_bodies: &Mutex<Vec<String>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(15)));
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.is_empty() {
        return;
    }
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers_raw = String::new();
    let mut len: usize = 0;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.trim().is_empty() {
                    break;
                }
                if let Some(rest) = line
                    .strip_prefix("content-length:")
                    .or_else(|| line.strip_prefix("Content-Length:"))
                {
                    len = rest.trim().parse().unwrap_or(0);
                }
                headers_raw.push_str(&line);
            }
            Err(_) => return,
        }
    }
    let mut body = String::new();
    if len > 0 {
        let mut buf = vec![0u8; len];
        if reader.read_exact(&mut buf).is_err() {
            return;
        }
        body = String::from_utf8_lossy(&buf).into_owned();
    }
    let is_stream = body.contains("\"stream\":true");
    let idx = if is_stream {
        let mut sb = stream_bodies.lock().unwrap_or_else(|e| e.into_inner());
        sb.push(body.clone());
        sb.len() - 1
    } else {
        usize::MAX
    };
    let resp_body = handler(&method, &path, &headers_raw, &body, idx);
    let ctype = if is_stream {
        "text/event-stream"
    } else {
        "application/json"
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_body}",
        resp_body.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(resp.as_bytes());
    let _ = stream.flush();
}

/// One SSE text turn: content chunk, stop chunk, terminator.
fn sse_text(text: &str) -> String {
    let c1 = serde_json::json!({"choices":[{"delta":{"content": text},"finish_reason": null}]});
    let c2 = serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
    format!("data: {c1}\n\ndata: {c2}\n\ndata: [DONE]\n\n")
}

/// One SSE tool-call turn: the call fragment, then a `tool_calls` finish.
fn sse_tool(name: &str, args_json: &str) -> String {
    let c1 = serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_fake_1","type":"function","function":{"name": name,"arguments": args_json}}]},"finish_reason": null}]});
    let c2 = serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]});
    format!("data: {c1}\n\ndata: {c2}\n\ndata: [DONE]\n\n")
}

/// Single-shot (non-streaming) OpenAI body, used by the title auxiliary.
fn single_shot_text(text: &str) -> String {
    serde_json::json!({"choices":[{"message":{"content": text},"finish_reason":"stop"}]})
        .to_string()
}

/// Handler for a plain text fake provider: every streamed turn answers
/// `answer`; the title auxiliary gets a fixed title.
fn text_provider(answer: &str) -> Handler {
    let answer = answer.to_string();
    Arc::new(move |_m, _p, _h, _b, idx| {
        if idx == usize::MAX {
            single_shot_text("Smoke Title")
        } else {
            sse_text(&answer)
        }
    })
}

/// Handler for the approval tests: first streamed turn asks for the shell
/// call, every later streamed turn answers with `final_text`.
fn tool_provider(final_text: &str) -> Handler {
    let final_text = final_text.to_string();
    Arc::new(move |_m, _p, _h, _b, idx| {
        if idx == usize::MAX {
            single_shot_text("Smoke Title")
        } else if idx == 0 {
            let args = serde_json::json!({"command": "echo smoke-tool-ran"}).to_string();
            sse_tool("shell", &args)
        } else {
            sse_text(&final_text)
        }
    })
}

// ---------------------------------------------------------------------------
// Session helpers
// ---------------------------------------------------------------------------

fn fake_session(data_dir: &Path, port: u16, policy: Policy) -> Session {
    // Opt-ins only for these smoke tests; the gates stay closed by default
    // (unit-proven in pantheon-providers, and sandbox enforcement is
    // covered by the sandbox_runner/tools_builtins suites).
    std::env::set_var("PANTHEON_ALLOW_LOOPBACK_PROVIDERS", "1");
    // This VM has no bwrap, so the Container sandbox boundary is
    // unavailable; without the documented direct-fallback opt-in every
    // shell call would fail SANDBOX_UNAVAILABLE and the approval/config
    // wiring under test could never be proven end to end.
    std::env::set_var("PANTHEON_SANDBOX_FALLBACK", "allow");
    let model_policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: format!("http://127.0.0.1:{port}"),
            model: "fake-smoke".into(),
        },
        fallbacks: FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    };
    Session::new(
        data_dir.to_path_buf(),
        policy,
        model_policy,
        SecretsBroker::from_system_env(),
    )
    .expect("fake session")
}

/// Coder policy with ShellExecute parked for approval, so a harmless
/// `echo` call exercises the gate without touching git or the network.
fn approval_policy() -> Policy {
    Policy::coder().approval(Capability::ShellExecute)
}

fn answered(outcome: &LoopOutcome) -> String {
    match outcome {
        LoopOutcome::Answered { text, .. } => text.clone(),
        other => panic!("expected Answered, got {other:?}"),
    }
}

fn awaiting(outcome: &LoopOutcome) -> (String, String) {
    match outcome {
        LoopOutcome::AwaitingApproval { capability, scope } => {
            (format!("{capability:?}"), scope.clone())
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    }
}

fn replay(sup: &Supervisor, run: &str) -> Vec<pantheon_storage::LedgerEntry> {
    sup.replay(run).expect("replay")
}

fn has_tool_started(entries: &[pantheon_storage::LedgerEntry], tool: &str) -> bool {
    entries
        .iter()
        .any(|e| matches!(&e.event, Event::ToolStarted { tool: t, .. } if t == tool))
}

fn tool_message_texts(entries: &[pantheon_storage::LedgerEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::ToolMessage { message, .. } => Some(message.content.clone()),
            _ => None,
        })
        .collect()
}

fn assistant_texts(entries: &[pantheon_storage::LedgerEntry]) -> Vec<String> {
    entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::AssistantMessage { message, .. } => Some(message.content.clone()),
            _ => None,
        })
        .collect()
}

fn pct_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "-_.~".contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// (a) Full chat turn, fake provider
// ---------------------------------------------------------------------------

#[test]
fn smoke_fake_provider_full_chat_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = start_server(text_provider("smoke answer"));
    let session = fake_session(dir.path(), server.port, Policy::coder());
    let run = pantheon_runtime::new_run_id();
    let outcome = session.chat(&run, "hello smoke").expect("chat turn");
    assert_eq!(answered(&outcome), "smoke answer");
    assert_eq!(
        session
            .supervisor
            .ledger_status(&run)
            .expect("status")
            .as_deref(),
        Some("completed"),
        "a finished turn must land completed, not linger running"
    );
    // The prompt really reached the provider: the streamed request body
    // carries the user message, and the ledger kept the answer.
    let bodies = server.stream_bodies.lock().unwrap();
    assert!(!bodies.is_empty(), "provider saw no streamed request");
    assert!(
        bodies[0].contains("hello smoke"),
        "first streamed body must carry the prompt: {}",
        bodies[0]
    );
    let texts = assistant_texts(&replay(&session.supervisor, &run));
    assert!(
        texts.iter().any(|t| t.contains("smoke answer")),
        "ledger transcript must contain the answer: {texts:?}"
    );
}

// ---------------------------------------------------------------------------
// (b) Tool call with approval: approve -> executes -> result visible;
//     deny -> does not execute.
// ---------------------------------------------------------------------------

#[test]
fn smoke_fake_provider_tool_approve_executes_and_result_visible() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = start_server(tool_provider("done after tool"));
    let session = fake_session(dir.path(), server.port, approval_policy());
    let run = pantheon_runtime::new_run_id();
    let parked = session.chat(&run, "run the echo").expect("parked turn");
    let (cap, scope) = awaiting(&parked);
    assert!(
        cap.contains("ShellExecute"),
        "parked on {cap}, not ShellExecute"
    );
    assert_eq!(
        session.supervisor.pending_approvals(&run).expect("pending"),
        vec![scope.clone()],
        "exactly the parked scope must be pending"
    );
    assert!(
        !has_tool_started(&replay(&session.supervisor, &run), "shell"),
        "a parked call must not have started before approval"
    );
    session.supervisor.grant(&run, &scope).expect("grant");
    let resumed = session.chat(&run, "continue").expect("resumed turn");
    assert_eq!(answered(&resumed), "done after tool");
    let entries = replay(&session.supervisor, &run);
    assert!(
        has_tool_started(&entries, "shell"),
        "grant must lead to execution (ToolStarted in ledger)"
    );
    let results = tool_message_texts(&entries);
    assert!(
        results.iter().any(|t| t.contains("smoke-tool-ran")),
        "tool result must be visible in the transcript: {results:?}"
    );
}

#[test]
fn smoke_fake_provider_tool_deny_never_executes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let server = start_server(tool_provider("done after denial"));
    let session = fake_session(dir.path(), server.port, approval_policy());
    let run = pantheon_runtime::new_run_id();
    let parked = session.chat(&run, "run the echo").expect("parked turn");
    let (_cap, scope) = awaiting(&parked);
    session.supervisor.deny(&run, &scope).expect("deny");
    let resumed = session.chat(&run, "continue").expect("resumed turn");
    assert_eq!(answered(&resumed), "done after denial");
    let entries = replay(&session.supervisor, &run);
    let results = tool_message_texts(&entries);
    // The denial settlement itself emits a ToolStarted marker around
    // the denial result (pre-existing drive design); the invariant is
    // that the tool never EXECUTED: no echo output, only the denial.
    assert!(
        !results.iter().any(|t| t.contains("smoke-tool-ran")),
        "a denied call must never produce tool output: {results:?}"
    );
    assert!(
        results.iter().any(|t| t.contains("denied by operator")),
        "denial must settle as a visible tool result: {results:?}"
    );
}

// ---------------------------------------------------------------------------
// (c) Delegate / swarm spawn
// ---------------------------------------------------------------------------

#[test]
fn smoke_swarm_spawn_scripted_worker_settles() {
    use pantheon_runtime::swarm_exec::{ScriptedWorker, SwarmOrchestrator, SwarmStatus};
    let dir = tempfile::tempdir().expect("tempdir");
    let worker = Arc::new(ScriptedWorker::new());
    let orch = SwarmOrchestrator::with_data_dir(dir.path().to_path_buf(), worker.clone(), None);
    let created = orch.create("smoke swarm task", 2, false).expect("create");
    assert_eq!(created.status.agents.len(), 2, "two agents must spawn");
    let spawns = worker.spawns();
    assert_eq!(spawns.len(), 2, "worker must record both spawns");
    assert!(
        spawns.iter().all(|s| s.3 == "smoke swarm task"),
        "spawned agents must carry the task: {spawns:?}"
    );
    for agent in &created.status.agents {
        worker.complete(&agent.run_id, "agent done");
    }
    let status = orch.status(&created.id).expect("status");
    assert!(
        matches!(status.status, SwarmStatus::Complete),
        "settled swarm must be Complete, got {:?}",
        status.status
    );
    let transcript = orch.transcript(&created.id).expect("transcript");
    assert!(
        transcript.contains("agent done"),
        "agent results must surface in the swarm transcript"
    );
    // The manifest persists so `swarm list` sees this swarm after restart.
    let swarm_dir = dir.path().join("swarms");
    let persisted = std::fs::read_dir(&swarm_dir)
        .map(|mut r| r.next().is_some())
        .unwrap_or(false);
    assert!(persisted, "swarm manifest must persist under {swarm_dir:?}");
}

// ---------------------------------------------------------------------------
// (d) Scheduled job fire
// ---------------------------------------------------------------------------

#[test]
fn smoke_scheduled_job_fires_once_and_reports() {
    use pantheon_scheduler::{
        DurableClaimLedger, Job, RunOutcome, ScheduleKind, TickDecision, TickDriver,
    };
    use pantheon_storage::ClaimStore;
    let dir = tempfile::tempdir().expect("tempdir");
    // Persist the job first: a job that only exists in memory is not a
    // scheduled job. Save -> load round-trips the definition.
    let mut job = Job::new("smoke-job", ScheduleKind::OneShot { at_ms: 1_000 }, "nyx");
    job.task = "smoke scheduled task".to_string();
    let jobs = vec![pantheon_scheduler::ScheduledJob {
        job: job.clone(),
        last_run: None,
    }];
    pantheon_scheduler::save_jobs(dir.path(), &jobs).expect("save jobs");
    let loaded = pantheon_scheduler::load_jobs(dir.path()).expect("load jobs");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].job.id, "smoke-job");
    assert_eq!(loaded[0].job.task, "smoke scheduled task");
    // Then fire it through the real tick driver with a durable claim
    // ledger: exactly one fire, reported as Completed.
    let ledger = DurableClaimLedger::new(ClaimStore::open_in_memory().expect("claims"));
    let driver = Arc::new(TickDriver::new(ledger));
    let runs = Arc::new(AtomicUsize::new(0));
    let r = Arc::clone(&runs);
    let decision = driver.tick_job(
        &job,
        1_000,
        None,
        Arc::new(move || {
            r.fetch_add(1, Ordering::SeqCst);
        }),
    );
    let rx = match decision {
        TickDecision::Fired { completion } => completion,
        other => panic!("expected Fired, got {other:?}"),
    };
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).expect("completion"),
        RunOutcome::Completed
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1, "job must fire exactly once");
    // A second tick for the same occurrence must not double-execute.
    let again = driver.tick_job(&job, 1_000, Some(1_000), Arc::new(|| {}));
    assert!(
        !matches!(again, TickDecision::Fired { .. }),
        "second tick must not refire: {again:?}"
    );
    assert_eq!(runs.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// (e) Config change round-trip: write -> reload -> behaviour changes
// ---------------------------------------------------------------------------

#[test]
fn smoke_config_change_round_trip_changes_behaviour() {
    // `ask`: the shell call parks. `allow_all`: the same call runs
    // without parking. The only difference between the two sessions is
    // the config file on disk.
    let ask_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        ask_dir.path().join("config.toml"),
        "permission_mode = \"ask\"\n",
    )
    .unwrap();
    let loaded = pantheon_api::config::Config::load(ask_dir.path()).expect("load ask");
    assert_eq!(loaded.permission_mode.as_deref(), Some("ask"));
    // Shared server, scripted per streamed turn: the ask session's turn
    // is stream 0 (tool, parks) and the allow session's turn is stream 1
    // (tool, runs) followed by stream 2 (final text). A single shared
    // script keeps the two sessions' turns distinguishable.
    let server = start_server(Arc::new(move |_m, _p, _h, _b, idx| {
        if idx == usize::MAX {
            single_shot_text("Smoke Title")
        } else if idx <= 1 {
            let args = serde_json::json!({"command": "echo smoke-tool-ran"}).to_string();
            sse_tool("shell", &args)
        } else {
            sse_text("done after tool")
        }
    }));
    let ask = fake_session(ask_dir.path(), server.port, approval_policy());
    assert_eq!(ask.permission_mode(), PermissionMode::Ask);
    let run_ask = pantheon_runtime::new_run_id();
    let parked = ask.chat(&run_ask, "run the echo").expect("ask turn");
    let (_cap, _scope) = awaiting(&parked);

    let allow_dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        allow_dir.path().join("config.toml"),
        "permission_mode = \"allow_all\"\n",
    )
    .unwrap();
    let allow = fake_session(allow_dir.path(), server.port, approval_policy());
    assert_eq!(allow.permission_mode(), PermissionMode::AllowAll);
    let run_allow = pantheon_runtime::new_run_id();
    let outcome = allow.chat(&run_allow, "run the echo").expect("allow turn");
    assert_eq!(answered(&outcome), "done after tool");
    let entries = replay(&allow.supervisor, &run_allow);
    assert!(
        has_tool_started(&entries, "shell"),
        "allow_all must let the gated call run without parking"
    );
    assert!(
        tool_message_texts(&entries)
            .iter()
            .any(|t| t.contains("smoke-tool-ran")),
        "allow_all run must show the tool result"
    );
}

// ---------------------------------------------------------------------------
// (f) Dashboard approve + tool-use cycle
// ---------------------------------------------------------------------------

#[test]
fn smoke_dashboard_approve_then_tool_use_cycle() {
    let dash = support::boot();
    let server = start_server(tool_provider("done after dashboard approval"));
    let session = fake_session(dash.dir.path(), server.port, approval_policy());
    let run = pantheon_runtime::new_run_id();
    let parked = session.chat(&run, "run the echo").expect("parked turn");
    let (_cap, scope) = awaiting(&parked);
    // The dashboard sees the parked run over real HTTP.
    let listed = dash.get("/api/approvals");
    assert_eq!(listed.status, 200, "{}", listed.body);
    let v = listed.json();
    let found = v["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .any(|a| a["run_id"] == run && a["id"] == scope);
    assert!(
        found,
        "dashboard must list the parked scope: {}",
        listed.body
    );
    // Hold the run lease while deciding, so the dashboard's resume spawn
    // (a real `pantheon` subprocess, deliberately unexercised in eval)
    // is skipped by design and the decision itself is what is proven.
    // The lease must be acquired on the session's own supervisor: a
    // fresh Supervisor has a different lease id and can never assert
    // ownership of the parked turn's lease (LOST_LEASE).
    session
        .supervisor
        .acquire_lease(&run)
        .expect("acquire lease");
    let guard =
        pantheon_runtime::RunLeaseGuard::try_new(session.supervisor.clone(), run.clone())
            .expect("lease guard");
    let granted = dash.post(
        &format!("/api/approvals/{}/grant", pct_encode(&scope)),
        "{}",
    );
    assert_eq!(granted.status, 200, "{}", granted.body);
    assert_eq!(granted.json()["ok"], true);
    assert_eq!(granted.json()["granted"], true);
    drop(guard);
    // The resumed turn on the same ledger executes the tool.
    let resumed = session.chat(&run, "continue").expect("resumed turn");
    assert_eq!(answered(&resumed), "done after dashboard approval");
    let entries = replay(&session.supervisor, &run);
    assert!(has_tool_started(&entries, "shell"));
    assert!(
        tool_message_texts(&entries)
            .iter()
            .any(|t| t.contains("smoke-tool-ran")),
        "dashboard-approved tool result must be visible"
    );
}

// ---------------------------------------------------------------------------
// (f2) Dashboard approvals list fails closed, never empty, on a corrupt
// ledger row. A parked run hidden by a read failure would read as "no
// approvals pending" while a tool call waits on a human.
// ---------------------------------------------------------------------------

#[test]
fn smoke_dashboard_approvals_list_fails_closed_on_corrupt_ledger() {
    let dash = support::boot();
    let sup = Supervisor::open(dash.dir.path().to_path_buf()).expect("supervisor");
    let run = pantheon_runtime::new_run_id();
    sup.start_run(&run).expect("start");
    sup.emit(Event::ApprovalRequested {
        run_id: run.clone(),
        scope: "call_1:shell:{}".to_string(),
    })
    .expect("park");
    let listed = dash.get("/api/approvals");
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert!(
        listed.json()["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["run_id"] == run),
        "parked run must list before corruption: {}",
        listed.body
    );
    // Corrupt one event row: replay now fails, so the pending set is
    // unknowable - the honest answer is a 500, not an empty queue.
    {
        let conn =
            rusqlite::Connection::open(dash.dir.path().join("ledger.db")).expect("open ledger db");
        conn.execute(
            "UPDATE events SET event_json = 'not json' WHERE run_id = ?1",
            rusqlite::params![run],
        )
        .expect("corrupt row");
    }
    let after = dash.get("/api/approvals");
    assert_eq!(
        after.status, 500,
        "corrupt ledger must not read as 'no approvals': {}",
        after.body
    );
}

// ---------------------------------------------------------------------------
// (g) Resume cycle: run -> stop -> resume -> continues, not restarts
// ---------------------------------------------------------------------------

#[test]
fn smoke_fake_provider_resume_continues_not_restarts() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Streamed turn N answers "part N": the server can only answer the
    // second turn correctly if the first turn's transcript reaches it.
    let server = start_server(Arc::new(move |_m, _p, _h, b, idx| {
        if idx == usize::MAX {
            single_shot_text("Smoke Title")
        } else if idx == 0 {
            sse_text("first part")
        } else {
            // Continuation proof, checked below on the captured body:
            // the second request must already contain the first answer.
            let _ = b;
            sse_text("second part")
        }
    }));
    let session = fake_session(dir.path(), server.port, Policy::coder());
    let run = pantheon_runtime::new_run_id();
    let first = session.chat(&run, "first message").expect("first turn");
    assert_eq!(answered(&first), "first part");
    assert_eq!(
        session
            .supervisor
            .ledger_status(&run)
            .expect("status")
            .as_deref(),
        Some("completed")
    );
    // Resume: a new turn on the SAME run. It must continue the transcript
    // (both user messages, both answers), not restart from scratch.
    let second = session.chat(&run, "second message").expect("second turn");
    assert_eq!(answered(&second), "second part");
    // The bodies guard MUST drop before any later chat on this server:
    // the server thread needs the same mutex to record the next
    // request, so holding it across a chat deadlocks the turn.
    {
        let bodies = server.stream_bodies.lock().unwrap();
        assert!(bodies.len() >= 2, "expected two streamed turns");
        assert!(
            bodies[1].contains("first part") && bodies[1].contains("second message"),
            "resumed turn must carry prior transcript + new message: {}",
            bodies[1]
        );
    }
    let texts = assistant_texts(&replay(&session.supervisor, &run));
    assert!(
        texts.iter().any(|t| t.contains("first part"))
            && texts.iter().any(|t| t.contains("second part")),
        "transcript must keep both answers after resume: {texts:?}"
    );
    // Stop via cancel, then resume the canceled run the same way.
    let run2 = pantheon_runtime::new_run_id();
    session.supervisor.start_run(&run2).expect("start run2");
    session.cancel_current_run(&run2, "stop for smoke");
    assert_eq!(
        session
            .supervisor
            .ledger_status(&run2)
            .expect("status")
            .as_deref(),
        Some("canceled"),
        "stop must leave the run canceled, not failed"
    );
    session.reset_cancel();
    let resumed = session
        .chat(&run2, "hello again")
        .expect("resumed canceled run");
    assert_eq!(answered(&resumed), "second part");
    assert_eq!(
        session
            .supervisor
            .ledger_status(&run2)
            .expect("status")
            .as_deref(),
        Some("completed")
    );
}

// ---------------------------------------------------------------------------
// NVIDIA 429 handling (fake 429 provider, streamed turns like production)
// ---------------------------------------------------------------------------

struct Collect(std::cell::RefCell<Vec<pantheon_providers::model_event::ModelEvent>>);
impl pantheon_providers::model_event::ModelEventSink for Collect {
    fn emit(&self, event: pantheon_providers::model_event::ModelEvent) {
        self.0.borrow_mut().push(event);
    }
}

struct FakeClock {
    waits: Mutex<Vec<Duration>>,
}
impl pantheon_providers::chain::Sleeper for FakeClock {
    fn sleep(&self, dur: Duration) {
        self.waits.lock().unwrap().push(dur);
    }
    fn jitter(&self) -> f64 {
        1.0
    }
}

/// Fake OpenAI-compatible transport shaped like NVIDIA's endpoint:
/// `primary` 429s `fail_primary` times with a stamped Retry-After, then
/// answers; `fallback` always answers. No network is ever touched.
struct NvidiaFake {
    calls: AtomicUsize,
    fail_primary: usize,
}

impl pantheon_providers::http::ChatTransport for NvidiaFake {
    fn post(&self, _req: &pantheon_providers::http::WireRequest) -> Result<String, PantheonError> {
        unimplemented!("streamed turns only")
    }
    fn post_stream(
        &self,
        req: &pantheon_providers::http::WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<pantheon_providers::http::StreamEnd, PantheonError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let is_primary = req.body.contains("nvidia-primary");
        if is_primary && n < self.fail_primary {
            return Err(PantheonError::new(
                "PROVIDER_HTTP",
                pantheon_api::error::Layer::Provider,
                true,
                "https://integrate.api.nvidia.com/v1/chat/completions: HTTP 429 rate limited (retry-after: 2s)",
                "wait and retry",
                "",
            ));
        }
        let text = if is_primary {
            "nvidia recovered"
        } else {
            "fallback answered"
        };
        let c1 = serde_json::json!({"choices":[{"delta":{"content": text},"finish_reason": null}]});
        let c2 = serde_json::json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        on_payload(&c1.to_string())?;
        on_payload(&c2.to_string())?;
        Ok(pantheon_providers::http::StreamEnd::Done)
    }
}

fn nvidia_chain(
    transport: NvidiaFake,
    clock: Arc<FakeClock>,
    with_fallback: bool,
) -> (
    pantheon_providers::chain::ProviderChain<NvidiaFake>,
    Collect,
) {
    use pantheon_providers::chain::{ProviderChain, RetryConfig};
    let policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: "https://integrate.api.nvidia.com/v1".into(),
            model: "nvidia-primary".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: if with_fallback {
                vec![DefaultModel {
                    provider: "https://integrate.api.nvidia.com/v1".into(),
                    model: "nvidia-fallback".into(),
                }]
            } else {
                vec![]
            },
        },
        auxiliaries: vec![],
    };
    let chain = ProviderChain {
        policy,
        transport,
        tools: vec![],
        api_key: Some(SecretValue::new("fake-nvidia-key")),
        retry: RetryConfig::default(),
        sleeper: clock,
        last_resolved: std::cell::RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
        oauth_broker: None,
    };
    let sink = Collect(std::cell::RefCell::new(vec![]));
    (chain, sink)
}

use pantheon_providers::model_event::ModelEvent;
use pantheon_secrets::SecretValue;

#[test]
fn smoke_fake_provider_nvidia_429_retry_then_success() {
    // NVIDIA rate-limits are deliberate: a 429 with Retry-After must be
    // honoured (capped wait), retried, and then succeed - not hard-fail.
    let clock = Arc::new(FakeClock {
        waits: Mutex::new(vec![]),
    });
    let (chain, sink) = nvidia_chain(
        NvidiaFake {
            calls: AtomicUsize::new(0),
            fail_primary: 1,
        },
        Arc::clone(&clock),
        false,
    );
    let outcome = chain
        .turn_stream(&[pantheon_api::message::Message::user("go")], &sink)
        .expect("turn must recover after one 429");
    match outcome {
        pantheon_agent::TurnOutcome::Text { text, .. } => {
            assert_eq!(text, "nvidia recovered")
        }
        other => panic!("expected Text, got {other:?}"),
    }
    let events = sink.0.borrow();
    let retry_waits: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::RetryAttempt { wait_secs, .. } => Some(*wait_secs),
            _ => None,
        })
        .collect();
    assert_eq!(
        retry_waits,
        vec![2],
        "the retry must be announced with the 2s Retry-After floor: {retry_waits:?}"
    );
    let waits = clock.waits.lock().unwrap().clone();
    assert!(
        waits.iter().all(|w| *w >= Duration::from_secs(2)),
        "every wait must honour the Retry-After floor: {waits:?}"
    );
    assert_eq!(
        chain.transport.calls.load(Ordering::SeqCst),
        2,
        "one 429 + one successful retry"
    );
}

#[test]
fn smoke_fake_provider_nvidia_429_falls_back_cleanly() {
    // When the primary stays rate-limited, the turn must fall back to the
    // next entry and the user must see what happened (Fallback event +
    // AttemptFailed for the primary), not a bare failure.
    let clock = Arc::new(FakeClock {
        waits: Mutex::new(vec![]),
    });
    let (chain, sink) = nvidia_chain(
        NvidiaFake {
            calls: AtomicUsize::new(0),
            fail_primary: 100,
        },
        Arc::clone(&clock),
        true,
    );
    let outcome = chain
        .turn_stream(&[pantheon_api::message::Message::user("go")], &sink)
        .expect("fallback turn must succeed");
    match outcome {
        pantheon_agent::TurnOutcome::Text { text, .. } => {
            assert_eq!(text, "fallback answered")
        }
        other => panic!("expected Text, got {other:?}"),
    }
    let events = sink.0.borrow();
    assert!(
        events.iter().any(|e| matches!(
            e,
            ModelEvent::Fallback { from_model, to_model, .. }
                if from_model == "nvidia-primary" && to_model == "nvidia-fallback"
        )),
        "fallback transition must be user-visible in events"
    );
    assert!(
        events.iter().any(|e| matches!(
            e,
            ModelEvent::AttemptFailed { model, .. } if model == "nvidia-primary"
        )),
        "primary failures must be recorded, not swallowed"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ModelEvent::Exhausted { .. })),
        "a successful fallback must not report exhaustion"
    );
    // Retry-After itself is capped at the parse layer: a provider asking
    // for an hour earns at most 60s, never a wedged turn.
    assert_eq!(
        pantheon_providers::http::parse_retry_after("3600"),
        Some(60)
    );
}

// ---------------------------------------------------------------------------
// Recommended backends: GSD browser, TinyFish search (corrected 2026-10-28)
// ---------------------------------------------------------------------------

#[test]
fn smoke_browser_gsd_is_default_and_blocker_is_binary_not_key() {
    use pantheon_web::browser::registry::{
        all_backends, build_backend, BackendConfig, BackendKind,
    };
    // GSD is the default: first in picker order, parses from "gsd".
    assert_eq!(BackendKind::all()[0], BackendKind::Gsd);
    assert_eq!(BackendKind::parse("gsd"), Some(BackendKind::Gsd));
    let infos = all_backends();
    let gsd = infos
        .iter()
        .find(|i| i.kind == BackendKind::Gsd)
        .expect("gsd row");
    assert!(
        gsd.needs.contains("gsd-browser") && gsd.needs.contains("Chrome"),
        "GSD needs are binary + host browser, not a key: {}",
        gsd.needs
    );
    // Correction: Camofox is keyless (local build); Steel is the keyed one.
    let camo = infos
        .iter()
        .find(|i| i.kind == BackendKind::Camofox)
        .expect("camofox row");
    assert!(
        !camo.needs.contains("API_KEY"),
        "Camofox must not be presented as keyed: {}",
        camo.needs
    );
    let steel = infos
        .iter()
        .find(|i| i.kind == BackendKind::Steel)
        .expect("steel row");
    assert!(
        steel.needs.contains("STEEL_API_KEY"),
        "Steel is the hosted/keyed backend: {}",
        steel.needs
    );
    // Keyless construction: GSD builds with no vault key at all, and a
    // missing binary surfaces as a named BinaryMissing failure - the
    // blocker for live GSD proof is binary/host, never a missing key.
    let missing = std::env::temp_dir().join("pantheon-smoke-no-such-dir/gsd-browser");
    let cfg = BackendConfig {
        gsd_binary: Some(missing.clone()),
        gsd_vault_key: None,
        ..Default::default()
    };
    let backend = build_backend(BackendKind::Gsd, &cfg).expect("gsd builds keyless");
    let err = backend
        .invoke(
            &["navigate".to_string(), "https://example.com".to_string()],
            "smoke",
        )
        .expect_err("missing binary must fail");
    let dbg = format!("{err:?}");
    assert!(
        dbg.contains("gsd-browser") || dbg.contains("BinaryMissing"),
        "failure must name the missing binary, not a key: {dbg}"
    );
}

#[test]
fn smoke_search_tinyfish_is_recommended_default_with_x_api_key() {
    use pantheon_web::websearch::provider::{SearchOptions, SearchProvider};
    use pantheon_web::websearch::registry::{all_providers, ProviderAuth};
    use pantheon_web::websearch::{TinyFishProvider, TINYFISH_API_KEY};
    // Exactly one recommended provider, and it is TinyFish.
    let providers = all_providers();
    let recommended: Vec<_> = providers.iter().filter(|p| p.recommended).collect();
    assert_eq!(recommended.len(), 1, "exactly one recommended provider");
    assert_eq!(recommended[0].id, "tinyfish");
    match &recommended[0].auth {
        ProviderAuth::ApiKey { env_var } => assert_eq!(*env_var, TINYFISH_API_KEY),
        _ => panic!("TinyFish auth must be an API key env var"),
    }
    assert_eq!(TINYFISH_API_KEY, "TINYFISH_API_KEY");
    // Full search shape through a local TinyFish mirror (no SSRF guard on
    // the search HTTP layer, so a local base URL works): the request must
    // carry X-API-Key (not Authorization: Bearer) and hits must parse.
    let server = start_server(Arc::new(move |m, _p, headers, _b, _idx| {
        assert_eq!(m, "GET", "TinyFish searches are GETs");
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("x-api-key: fake-tinyfish-key"),
            "TinyFish auth must be X-API-Key: {headers}"
        );
        serde_json::json!({"results":[{"title":"Smoke Hit","url":"https://example.com/smoke","snippet":"smoke snippet"}]}).to_string()
    }));
    let provider = TinyFishProvider::with_base_url(
        "fake-tinyfish-key",
        format!("http://127.0.0.1:{}", server.port),
    );
    let hits = provider
        .search("smoke query", &SearchOptions::default())
        .expect("tinyfish mirror search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "Smoke Hit");
    assert_eq!(hits[0].url, "https://example.com/smoke");
}

