//! Ideas backend behavioral evals: the nightly-gated proactive
//! suggestions pipeline end to end.
//!
//! - disabled nightly mints nothing (the real `run_one_pass` entry gate)
//! - an enabled `run_pass` mints general ideas from repeated failures,
//!   exactly one batch per day
//! - daily refresh: new batches on new days, stale pending ideas roll off
//! - dismissed topics are downranked out of generation
//! - HTTP: accept (general) spawns a run, accept (scheduled_task) creates
//!   a scheduled job, dismiss/feedback record signals
//!
//! The ledger, memory store, ideas.db, and schedule.json are all real.
//! Scripted eval/replay runners stand in for `pantheon-eval` targets.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use pantheon_api::capability::Policy;
use pantheon_api::events::Event;
use pantheon_api::model::{DefaultModel, ModelPolicy};
use pantheon_api::provenance::Provenance;
use pantheon_dashboard::{App, DashboardMount};
use pantheon_nightly::{
    run_ideas_phase, run_pass, EvalOutcome, EvalRunner, NightlyConfig, NightlyDeps, ReplayRunner,
    ReplayTask, Signal, TurnRef,
};
use pantheon_storage::{IdeaKind, IdeaStore, Ledger, NewIdea, ScheduleSpec};

// ---------------------------------------------------------------------------
// Nightly harness (mirrors eval/tests/nightly.rs)
// ---------------------------------------------------------------------------

struct Harness {
    dir: tempfile::TempDir,
    ledger: Ledger,
    backend: Arc<dyn pantheon_memory::MemoryBackend>,
    policy: Policy,
    model_policy: ModelPolicy,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    let backend = pantheon_memory::open_selected(dir.path()).unwrap();
    Harness {
        dir,
        ledger,
        backend,
        policy: Policy::coder_with_memory(),
        model_policy: ModelPolicy {
            default: DefaultModel {
                provider: "chat-provider".into(),
                model: "chat-model".into(),
            },
            fallbacks: Default::default(),
            auxiliaries: vec![],
            reasoning: Default::default(),
            reasoning_budget: None,
        },
    }
}

struct PassEval;
impl EvalRunner for PassEval {
    fn run_eval(&self, _target: &str, _timeout: Duration) -> EvalOutcome {
        EvalOutcome::Pass
    }
}

struct NoopReplay;
impl ReplayRunner for NoopReplay {
    fn run_transcript(
        &self,
        _task: &ReplayTask,
        _with_proposal: Option<&pantheon_nightly::Proposal>,
    ) -> Result<String, String> {
        Ok("noop".into())
    }
}

fn config(dir: &Path) -> NightlyConfig {
    NightlyConfig {
        data_dir: dir.to_path_buf(),
        ..NightlyConfig::default()
    }
}

fn deps<'a>(
    h: &'a Harness,
    eval: &'a dyn EvalRunner,
    replay: &'a dyn ReplayRunner,
) -> NightlyDeps<'a> {
    NightlyDeps {
        ledger: &h.ledger,
        backend: h.backend.as_ref(),
        capability_policy: &h.policy,
        model_policy: &h.model_policy,
        eval_runner: eval,
        replay_runner: replay,
        llm: None,
        repair: None,
    }
}

/// One run whose turn fails with `tool` as the last-started tool: feeds
/// the RepeatedFailure signal.
fn failing_run(ledger: &Ledger, run_id: &str, tool: &str) {
    ledger
        .append(&Event::RunStarted {
            run_id: run_id.into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnStarted {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
        })
        .unwrap();
    ledger
        .append(&Event::ToolStarted {
            run_id: run_id.into(),
            call_id: "call_0".into(),
            tool: tool.into(),
            args: "{}".into(),
            provenance: Provenance::system("eval"),
        })
        .unwrap();
    ledger
        .append(&Event::TurnFailed {
            run_id: run_id.into(),
            turn_id: "turn_0".into(),
            code: "TOOL_ERROR".into(),
        })
        .unwrap();
}

fn turn_ref(ts_ms: i64) -> TurnRef {
    TurnRef {
        run_id: "r".into(),
        turn_id: "t".into(),
        ts_ms,
    }
}

#[test]
fn nightly_disabled_mints_no_ideas() {
    // No config.toml in the temp dir: the nightly master switch is off.
    let dir = tempfile::tempdir().unwrap();
    let err = pantheon_tui::nightly_cli::run_one_pass(dir.path(), false).unwrap_err();
    assert!(
        err.contains("disabled"),
        "disabled nightly must refuse the pass, got: {err}"
    );
    // Generation never ran: no ideas database was even created.
    assert!(
        !dir.path().join("ideas.db").exists(),
        "disabled nightly must not mint ideas"
    );
}

#[test]
fn run_pass_mints_general_idea_from_repeated_failures() {
    let h = harness();
    for i in 1..=3 {
        failing_run(&h.ledger, &format!("run_{i}"), "exec");
    }
    let eval = PassEval;
    let replay = NoopReplay;
    let cfg = config(h.dir.path());
    let mut d = deps(&h, &eval, &replay);
    let out = run_pass(&cfg, &mut d).unwrap();
    assert!(out.ideas_minted >= 1, "expected at least one idea");
    let store = IdeaStore::open(h.dir.path()).unwrap();
    let ideas = store.list().unwrap();
    assert!(
        ideas
            .iter()
            .any(|i| i.kind == IdeaKind::General && i.topic == "fail:exec"),
        "expected a general idea for the failing exec tool"
    );
    // A second pass the same day mints nothing new (daily refresh gate).
    let mut d = deps(&h, &eval, &replay);
    let out2 = run_pass(&cfg, &mut d).unwrap();
    assert_eq!(out2.ideas_minted, 0, "one batch per day");
    assert_eq!(store.list().unwrap().len(), ideas.len());
}

#[test]
fn dry_run_mints_nothing() {
    let h = harness();
    for i in 1..=3 {
        failing_run(&h.ledger, &format!("run_{i}"), "exec");
    }
    let eval = PassEval;
    let replay = NoopReplay;
    let mut cfg = config(h.dir.path());
    cfg.dry_run = true;
    let mut d = deps(&h, &eval, &replay);
    let out = run_pass(&cfg, &mut d).unwrap();
    assert_eq!(out.ideas_minted, 0);
    // The store may not even exist; if it does it must be empty.
    if h.dir.path().join("ideas.db").exists() {
        let store = IdeaStore::open(h.dir.path()).unwrap();
        assert!(store.list().unwrap().is_empty());
    }
}

fn failure_signal(tool: &str) -> Signal {
    Signal::RepeatedFailure {
        tool: tool.into(),
        count: 3,
        at: vec![],
    }
}

#[test]
fn daily_refresh_and_rolloff() {
    let store = IdeaStore::open_in_memory().unwrap();
    // A stale unanswered idea from five days ago.
    store
        .mint(&NewIdea {
            id: "stale".into(),
            title: "stale".into(),
            description: "".into(),
            includes: vec![],
            kind: IdeaKind::General,
            topic: "stale:topic".into(),
            created_day: "2026-09-25".into(),
            schedule: None,
        })
        .unwrap();
    // Day one: stale rolls off, the failure signal mints one idea.
    let n = run_ideas_phase(
        &store,
        &[failure_signal("exec")],
        &[],
        false,
        "2026-09-30",
        0,
    )
    .unwrap();
    assert_eq!(n, 1);
    assert!(store.get("stale").unwrap().is_none(), "stale rolls off");
    let day1: Vec<_> = store.list().unwrap();
    assert_eq!(day1.len(), 1);
    assert_eq!(day1[0].created_day, "2026-09-30");
    // Same day again: no duplicate batch.
    let n = run_ideas_phase(
        &store,
        &[failure_signal("exec")],
        &[],
        false,
        "2026-09-30",
        0,
    )
    .unwrap();
    assert_eq!(n, 0);
    // Next day, a different topic: a fresh batch is minted.
    let n = run_ideas_phase(
        &store,
        &[failure_signal("read")],
        &[],
        false,
        "2026-10-01",
        0,
    )
    .unwrap();
    assert_eq!(n, 1);
    let days: Vec<String> = store
        .list()
        .unwrap()
        .into_iter()
        .map(|i| i.created_day)
        .collect();
    assert!(days.contains(&"2026-09-30".to_string()));
    assert!(days.contains(&"2026-10-01".to_string()));
}

#[test]
fn dismissed_topics_are_downranked() {
    let store = IdeaStore::open_in_memory().unwrap();
    store.bump_topic("fail:exec", false).unwrap();
    store.bump_topic("fail:exec", false).unwrap();
    let n = run_ideas_phase(
        &store,
        &[failure_signal("exec")],
        &[],
        false,
        "2026-09-30",
        0,
    )
    .unwrap();
    assert_eq!(n, 0, "twice-dismissed topic must not be proposed again");
    assert!(store.list().unwrap().is_empty());
    // One dismissal is not enough to downrank.
    let store = IdeaStore::open_in_memory().unwrap();
    store.bump_topic("fail:exec", false).unwrap();
    let n = run_ideas_phase(
        &store,
        &[failure_signal("exec")],
        &[],
        false,
        "2026-09-30",
        0,
    )
    .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn scheduled_task_idea_from_multi_day_habit() {
    let store = IdeaStore::open_in_memory().unwrap();
    // read → exec on three separate days around 07:xx UTC.
    let hits = vec![
        turn_ref(1_790_579_600_000),
        turn_ref(1_790_666_800_000),
        turn_ref(1_790_752_800_000),
    ];
    let signal = Signal::RepeatedSequence {
        tools: vec!["read".into(), "exec".into()],
        hits,
    };
    let n = run_ideas_phase(&store, &[signal], &[], false, "2026-09-30", 0).unwrap();
    assert_eq!(n, 1);
    let idea = &store.list().unwrap()[0];
    assert_eq!(idea.kind, IdeaKind::ScheduledTask);
    let spec = idea.schedule.as_ref().expect("schedule spec");
    assert_eq!(spec.cron, "0 7 * * *");
    assert_eq!(spec.deliver, "home");
    assert!(spec.prompt.contains("read → exec"));
    // A one-day burst is not a habit: no idea.
    let store = IdeaStore::open_in_memory().unwrap();
    let burst = Signal::RepeatedSequence {
        tools: vec!["read".into(), "exec".into()],
        hits: vec![
            turn_ref(1_790_752_800_000),
            turn_ref(1_790_752_860_000),
            turn_ref(1_790_752_920_000),
        ],
    };
    let n = run_ideas_phase(&store, &[burst], &[], false, "2026-09-30", 0).unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------------------
// HTTP: dashboard mount over a real loopback listener
// ---------------------------------------------------------------------------

struct Resp {
    status: u16,
    body: String,
}

fn raw_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let body = body.unwrap_or("");
    let mut req = format!("{method} {path} HTTP/1.1\r\nhost: 127.0.0.1\r\nconnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    if !body.is_empty() {
        req.push_str(&format!("content-length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes()).expect("write");
    let mut reader = BufReader::new(s);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).expect("status line");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    let mut len: usize = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header");
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.strip_prefix("content-length:") {
            len = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("Content-Length:") {
            len = rest.trim().parse().unwrap_or(0);
        }
    }
    let mut body = String::new();
    if len > 0 {
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).expect("body");
        body = String::from_utf8_lossy(&buf).into_owned();
    }
    Resp { status, body }
}

fn auth(token: &str) -> Vec<(String, String)> {
    vec![
        ("x-pantheon-token".into(), token.to_string()),
        ("origin".into(), "http://127.0.0.1".into()),
    ]
}

fn boot() -> (u16, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let token = pantheon_gateway::http::generate_token();
    let app = App {
        data_dir: dir.path().to_path_buf(),
        token,
        bind: "127.0.0.1".to_string(),
        bind_all: false,
        on_approval: None,
        send_locks: Default::default(),
        turn_children: Default::default(),
        // Scripted worker + no judge: spawns never leave the process.
        swarm: Arc::new(pantheon_runtime::swarm_exec::SwarmOrchestrator::new(
            Arc::new(pantheon_runtime::swarm_exec::ScriptedWorker::new()),
            None,
        )),
    };
    let mount = DashboardMount::new(app);
    let auth_ctx = mount.auth_ctx();
    let cfg = pantheon_gateway::http::ServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        auth: auth_ctx,
        mounts: vec![std::sync::Arc::new(mount)],
        label: "pantheon eval".to_string(),
    };
    let (port, token) = pantheon_gateway::http::spawn_test_server(cfg);
    (port, token, dir)
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("valid JSON")
}

fn seed_idea(dir: &tempfile::TempDir, id: &str, kind: IdeaKind, schedule: Option<ScheduleSpec>) {
    let store = IdeaStore::open(dir.path()).unwrap();
    store
        .mint(&NewIdea {
            id: id.into(),
            title: format!("Test idea {id}"),
            description: "do the thing".into(),
            includes: vec!["step one".into(), "step two".into()],
            kind,
            topic: format!("test:{id}"),
            created_day: "2026-09-30".into(),
            schedule,
        })
        .unwrap();
}

#[test]
fn ideas_list_shape_over_http() {
    let (port, token, dir) = boot();
    seed_idea(&dir, "g1", IdeaKind::General, None);
    seed_idea(
        &dir,
        "s1",
        IdeaKind::ScheduledTask,
        Some(ScheduleSpec {
            cron: "0 7 * * *".into(),
            deliver: "log".into(),
            prompt: "morning check".into(),
        }),
    );
    let r = raw_request(port, "GET", "/api/ideas", &auth(&token), None);
    assert_eq!(r.status, 200);
    let v = json(&r.body);
    let ideas = v["ideas"].as_array().unwrap();
    assert_eq!(ideas.len(), 2);
    // Pending first; both are pending here, so check each shape.
    for i in ideas {
        assert_eq!(i["status"], "pending");
        assert_eq!(i["created_day"], "2026-09-30");
        assert!(i["includes"].as_array().unwrap().len() == 2);
        if i["kind"] == "scheduled_task" {
            assert_eq!(i["schedule"]["cron"], "0 7 * * *");
            assert_eq!(i["schedule"]["deliver"], "log");
            assert_eq!(i["schedule"]["prompt"], "morning check");
        } else {
            assert!(i.get("schedule").is_none(), "general has no schedule");
        }
    }
}

#[test]
fn accept_general_idea_spawns_run() {
    let (port, token, dir) = boot();
    seed_idea(&dir, "g1", IdeaKind::General, None);
    let r = raw_request(port, "POST", "/api/ideas/g1/accept", &auth(&token), None);
    assert_eq!(r.status, 200, "accept body: {}", r.body);
    let v = json(&r.body);
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "accepted");
    let run_id = v["run_id"].as_str().expect("run_id");
    assert!(!run_id.is_empty());
    // The run was admitted durably: it is in the ledger.
    // (The turn itself is handed to a `pantheon run` subprocess, which in
    // this test is the test binary re-invoked with CLI args; libtest
    // rejects the unknown flags and exits - nothing is executed.)
    let ledger = Ledger::open(&dir.path().join("ledger.db")).unwrap();
    assert!(
        ledger
            .list_runs(100)
            .unwrap()
            .iter()
            .any(|(id, ..)| id == run_id),
        "accepted idea must admit a run"
    );
    // The idea left pending.
    let r = raw_request(port, "GET", "/api/ideas", &auth(&token), None);
    let v = json(&r.body);
    let idea = v["ideas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "g1")
        .unwrap();
    assert_eq!(idea["status"], "accepted");
    // Accepting twice is a 409.
    let r = raw_request(port, "POST", "/api/ideas/g1/accept", &auth(&token), None);
    assert_eq!(r.status, 409);
}

#[test]
fn accept_scheduled_task_idea_creates_job() {
    let (port, token, dir) = boot();
    seed_idea(
        &dir,
        "s1",
        IdeaKind::ScheduledTask,
        Some(ScheduleSpec {
            cron: "0 7 * * *".into(),
            deliver: "log".into(),
            prompt: "morning check".into(),
        }),
    );
    let r = raw_request(port, "POST", "/api/ideas/s1/accept", &auth(&token), None);
    assert_eq!(r.status, 200, "accept body: {}", r.body);
    let v = json(&r.body);
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "accepted");
    let schedule_id = v["schedule_id"].as_str().expect("schedule_id");
    assert!(schedule_id.starts_with("job_"));
    // The job landed in schedule.json with the proposed spec.
    let jobs = pantheon_scheduler::load_jobs(dir.path()).expect("load jobs");
    let job = jobs
        .iter()
        .find(|j| j.job.id == schedule_id)
        .expect("created job present");
    assert!(job.job.task.contains("morning check"));
    match &job.job.kind {
        pantheon_scheduler::ScheduleKind::Cron { expr } => assert_eq!(expr, "0 7 * * *"),
        other => panic!("expected cron kind, got {other:?}"),
    }
    assert_eq!(job.job.deliver.as_deref(), Some("log"));
}

#[test]
fn dismiss_and_feedback_record_signals() {
    let (port, token, dir) = boot();
    seed_idea(&dir, "g1", IdeaKind::General, None);
    seed_idea(&dir, "g2", IdeaKind::General, None);
    // Feedback first: more + less on g1.
    let r = raw_request(
        port,
        "POST",
        "/api/ideas/g1/feedback",
        &auth(&token),
        Some(r#"{"signal":"more"}"#),
    );
    assert_eq!(r.status, 200);
    assert_eq!(json(&r.body)["ok"], true);
    let r = raw_request(
        port,
        "POST",
        "/api/ideas/g1/feedback",
        &auth(&token),
        Some(r#"{"signal":"less"}"#),
    );
    assert_eq!(r.status, 200);
    let r = raw_request(
        port,
        "POST",
        "/api/ideas/g1/feedback",
        &auth(&token),
        Some(r#"{"signal":"bogus"}"#),
    );
    assert_eq!(r.status, 400, "unknown signal must be rejected");
    // Dismiss g2.
    let r = raw_request(port, "POST", "/api/ideas/g2/dismiss", &auth(&token), None);
    assert_eq!(r.status, 200);
    assert_eq!(json(&r.body)["status"], "dismissed");
    // The store carries the signals durably.
    let store = IdeaStore::open(dir.path()).unwrap();
    let g1 = store.get("g1").unwrap().unwrap();
    assert_eq!((g1.more, g1.less), (1, 1));
    let g2 = store.get("g2").unwrap().unwrap();
    assert_eq!(g2.status, pantheon_storage::IdeaStatus::Dismissed);
    let ts = store.topic_signals("test:g2").unwrap();
    assert_eq!(ts.dismissed, 1, "dismiss bumps the topic signal");
    let ts = store.topic_signals("test:g1").unwrap();
    assert_eq!((ts.more, ts.less), (1, 1));
    // Unknown idea: 404s.
    let r = raw_request(port, "POST", "/api/ideas/nope/dismiss", &auth(&token), None);
    assert_eq!(r.status, 404);
    let r = raw_request(
        port,
        "POST",
        "/api/ideas/nope/feedback",
        &auth(&token),
        Some(r#"{"signal":"more"}"#),
    );
    assert_eq!(r.status, 404);
}
