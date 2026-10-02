//! Behavioral evals for provider error + fallback display state.
//!
//! Covers: a retryable provider failure followed by a fallback renders a
//! `BlockKind::ProviderError` card naming the failed provider/model, the
//! classified error kind + message, and the engaged fallback model;
//! rate-limit failures classify distinctly (`RateLimited`) from auth and
//! other failures; an exhausted chain reports honestly that no fallback
//! was available; a non-retryable failure reports that no fallback was
//! attempted; stacked-key rotation renders a status line, not an error
//! card; and the dashboard run timeline exposes `model_fallback`,
//! `model_attempt_failed`, and `model_exhausted` kinds with labels in the
//! served web UI.
//!
//! Run with `cargo test -p pantheon-eval`.

use pantheon_providers::error_kind::ProviderErrorKind;
use pantheon_providers::model_event::ModelEvent;
use pantheon_tui::session::{BlockKind, FallbackOutcome, TuiState};

// ------------------------------------------------------------ TUI ---

fn attempt(provider: &str, model: &str, chain_index: usize) -> ModelEvent {
    ModelEvent::Attempt {
        provider: provider.into(),
        model: model.into(),
        chain_index,
        streaming: false,
    }
}

fn failed(provider: &str, model: &str, code: &str, retryable: bool, cause: &str) -> ModelEvent {
    ModelEvent::AttemptFailed {
        provider: provider.into(),
        model: model.into(),
        chain_index: 0,
        code: code.into(),
        retryable,
        cause: cause.into(),
    }
}

fn fallback(to_provider: &str, to_model: &str) -> ModelEvent {
    ModelEvent::Fallback {
        from_index: 0,
        from_provider: "openai".into(),
        from_model: "gpt-4o".into(),
        from_code: "PROVIDER_HTTP".into(),
        from_cause: "https://api.openai.com/v1: HTTP 429 slow down (retry-after: 12s)".into(),
        to_index: 1,
        to_provider: to_provider.into(),
        to_model: to_model.into(),
    }
}

fn state() -> TuiState {
    TuiState::new("run_eval_fb".to_string(), "gpt-4o".to_string(), 200_000)
}

/// The last block, expected to be a provider error card, destructured.
fn last_card(s: &TuiState) -> (String, String, ProviderErrorKind, String, FallbackOutcome) {
    match &s.blocks.last().expect("a block must exist").kind {
        BlockKind::ProviderError {
            provider,
            model,
            kind,
            message,
            fallback,
        } => (
            provider.clone(),
            model.clone(),
            *kind,
            message.clone(),
            fallback.clone(),
        ),
        other => panic!("expected a ProviderError card, got {other:?}"),
    }
}

/// How many provider error cards are on screen.
fn card_count(s: &TuiState) -> usize {
    s.blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::ProviderError { .. }))
        .count()
}

#[test]
fn fallback_engagement_renders_error_card() {
    let mut s = state();
    s.handle_model_event(attempt("openai", "gpt-4o", 0));
    let blocks_before = s.blocks.len();
    s.handle_model_event(failed(
        "openai",
        "gpt-4o",
        "PROVIDER_HTTP",
        true,
        "https://api.openai.com/v1: HTTP 429 slow down (retry-after: 12s)",
    ));
    assert_eq!(
        s.blocks.len(),
        blocks_before,
        "a retryable failure renders nothing until the chain's verdict"
    );
    s.handle_model_event(fallback("deepseek", "deepseek-chat"));
    assert_eq!(s.blocks.len(), blocks_before + 1);
    let (provider, model, kind, message, fb) = last_card(&s);
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
    assert_eq!(kind, ProviderErrorKind::RateLimited);
    assert!(message.contains("HTTP 429"), "message: {message}");
    assert_eq!(
        fb,
        FallbackOutcome::Engaged {
            provider: "deepseek".into(),
            model: "deepseek-chat".into()
        },
        "the card must name the now-active fallback model"
    );
    assert_eq!(s.model, "deepseek-chat", "fallback model becomes active");
    // The Attempt for the fallback model must not print the old
    // "routing:" status line on top of the card.
    s.handle_model_event(attempt("deepseek", "deepseek-chat", 1));
    assert_eq!(
        s.blocks.len(),
        blocks_before + 1,
        "no duplicate routing status after the fallback card"
    );
}

#[test]
fn rate_limit_kind_is_distinct_from_auth_failure() {
    // 429, retryable, fallback engages: the rate-limit kind.
    let mut s = state();
    s.handle_model_event(failed(
        "openai",
        "gpt-4o",
        "PROVIDER_HTTP",
        true,
        "https://x/v1: HTTP 429 slow down",
    ));
    s.handle_model_event(fallback("deepseek", "deepseek-chat"));
    let (_, _, kind, _, _) = last_card(&s);
    assert!(kind.is_rate_limited(), "429 must be the distinct kind");
    assert_eq!(kind.label(), "rate limited");

    // 401, not retryable: immediate card, auth kind, no retries — but
    // the chain still walks on, so a later verdict finalizes the card
    // in place rather than pushing a second one.
    let mut s = state();
    s.handle_model_event(failed(
        "openai",
        "gpt-4o",
        "PROVIDER_HTTP",
        false,
        "https://x/v1: HTTP 401 bad key",
    ));
    let (provider, model, kind, message, fb) = last_card(&s);
    assert_eq!(kind, ProviderErrorKind::Auth);
    assert!(!kind.is_rate_limited());
    assert_eq!(provider, "openai");
    assert_eq!(model, "gpt-4o");
    assert!(message.contains("HTTP 401"), "message: {message}");
    assert_eq!(fb, FallbackOutcome::NotRetryable);
    // Non-retryable walks to the fallback: the verdict finalizes the
    // same card — no retries were emitted, no second card appears.
    let cards_before = card_count(&s);
    s.handle_model_event(fallback("deepseek", "deepseek-chat"));
    assert_eq!(card_count(&s), cards_before, "fallback updates the card");
    let (_, _, _, _, fb) = last_card(&s);
    assert!(matches!(fb, FallbackOutcome::Engaged { .. }));

    // Non-retryable and nothing left: Exhausted flips the same card.
    let mut s = state();
    s.handle_model_event(failed(
        "openai",
        "gpt-4o",
        "PROVIDER_HTTP",
        false,
        "https://x/v1: HTTP 401 bad key",
    ));
    let cards_before = card_count(&s);
    s.handle_model_event(ModelEvent::Exhausted {
        code: "PROVIDER_EXHAUSTED".into(),
    });
    assert_eq!(card_count(&s), cards_before, "exhaustion updates the card");
    let (_, _, _, _, fb) = last_card(&s);
    assert_eq!(fb, FallbackOutcome::Exhausted);
}

#[test]
fn exhausted_chain_reports_no_fallback_available() {
    let mut s = state();
    s.handle_model_event(failed(
        "openai",
        "gpt-4o",
        "PROVIDER_HTTP",
        true,
        "https://x/v1: HTTP 500 boom",
    ));
    s.handle_model_event(ModelEvent::Exhausted {
        code: "PROVIDER_EXHAUSTED".into(),
    });
    let (_, _, kind, _, fb) = last_card(&s);
    assert_eq!(kind, ProviderErrorKind::Server);
    assert_eq!(
        fb,
        FallbackOutcome::Exhausted,
        "exhaustion must say no fallback was available"
    );
}

#[test]
fn stacked_key_rotation_renders_status_line_not_card() {
    let mut s = state();
    s.handle_model_event(attempt("openai", "gpt-4o", 0));
    s.handle_model_event(failed(
        "openai",
        "gpt-4o",
        "PROVIDER_HTTP:key1",
        true,
        "https://x/v1: HTTP 429 slow down",
    ));
    // No Fallback: the next Attempt is the same model on the next key.
    s.handle_model_event(attempt("openai", "gpt-4o", 0));
    assert!(
        s.blocks
            .iter()
            .all(|b| !matches!(b.kind, BlockKind::ProviderError { .. })),
        "key rotation must not render an error card"
    );
    let status = match &s.blocks.last().unwrap().kind {
        BlockKind::Status(t) => t.clone(),
        other => panic!("expected a status line, got {other:?}"),
    };
    assert!(status.contains("trying next key"), "status: {status}");
    assert!(status.contains("stacked key 1"), "status: {status}");
}

// ------------------------------------------------------ dashboard ---

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;

use pantheon_api::events::Event;
use pantheon_dashboard::{App, DashboardMount};
use pantheon_storage::Ledger;

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
        swarm: std::sync::Arc::new(pantheon_runtime::swarm_exec::SwarmOrchestrator::new(
            std::sync::Arc::new(pantheon_runtime::swarm_exec::ScriptedWorker::new()),
            None,
        )),
    };
    let mount = DashboardMount::new(app);
    let auth = mount.auth_ctx();
    let cfg = pantheon_gateway::http::ServerConfig {
        bind_addr: "127.0.0.1:0".to_string(),
        auth,
        mounts: vec![std::sync::Arc::new(mount)],
        label: "pantheon eval".to_string(),
    };
    let (port, token) = pantheon_gateway::http::spawn_test_server(cfg);
    (port, token, dir)
}

#[test]
fn dashboard_timeline_shows_fallback_and_error_kinds() {
    let (port, token, dir) = boot();
    let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("open ledger");
    let run = "eval-fb-1";
    ledger
        .append(&Event::RunStarted { run_id: run.into() })
        .expect("started");
    // The exact shapes `ModelEvent::to_event` projects: fallback move,
    // failed attempt, exhausted chain.
    ledger
        .append(&Event::RunProgress {
            run_id: run.into(),
            detail: "fallback openai/gpt-4o (PROVIDER_HTTP: HTTP 429 slow down) -> deepseek/deepseek-chat (chain index 1)".into(),
        })
        .expect("fallback");
    ledger
        .append(&Event::RunProgress {
            run_id: run.into(),
            detail: "model attempt failed: PROVIDER_HTTP (openai/gpt-4o): HTTP 429 slow down"
                .into(),
        })
        .expect("attempt failed");
    ledger
        .append(&Event::RunProgress {
            run_id: run.into(),
            detail: "provider chain exhausted: PROVIDER_EXHAUSTED".into(),
        })
        .expect("exhausted");

    let a = auth(&token);
    let r = raw_request(port, "GET", &format!("/api/runs/{run}"), &a, None);
    assert_eq!(r.status, 200);
    let v: serde_json::Value = serde_json::from_str(&r.body).expect("valid JSON");
    let timeline: Vec<&serde_json::Value> = v["timeline"].as_array().unwrap().iter().collect();
    // Skip run_started: assert on the three provider-plane rows.
    let kinds: Vec<&str> = timeline[1..]
        .iter()
        .map(|t| t["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["model_fallback", "model_attempt_failed", "model_exhausted"],
        "timeline kinds: {kinds:?}"
    );
    let details: Vec<&str> = timeline[1..]
        .iter()
        .map(|t| t["detail"].as_str().unwrap())
        .collect();
    assert!(
        details[0].contains("deepseek/deepseek-chat"),
        "fallback detail names the engaged model: {}",
        details[0]
    );
    assert!(
        details[1].contains("HTTP 429"),
        "attempt detail names the failure: {}",
        details[1]
    );

    // The served web UI labels the new kinds instead of rendering a
    // generic "Event" row.
    let r = raw_request(port, "GET", "/app.js", &[], None);
    assert_eq!(r.status, 200);
    for label in [
        "model_fallback",
        "model_attempt_failed",
        "model_exhausted",
        "Model fallback",
        "Model attempt failed",
        "Provider chain exhausted",
    ] {
        assert!(r.body.contains(label), "app.js must define {label}");
    }
}
