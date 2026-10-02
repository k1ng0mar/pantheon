//! Behavioral tests for real vision support (image parts in the provider plane).
//!
//! - OpenAI-compatible serialization: `content` parts with `image_url` data URLs.
//! - Anthropic serialization: base64 `image` content blocks.
//! - Vision gate: a non-vision model refuses image parts loudly at the chain
//!   layer (catalog-resolved), before any network attempt.
//! - Base64 encoding of a real fixture image (`eval/fixtures/red-1x1.png`).
//! - End-to-end through the upload path: `POST /api/uploads` -> disk ->
//!   resolve -> image part -> provider wire JSON.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use pantheon_agent::TurnOutcome;
use pantheon_api::error::PantheonError;
use pantheon_api::message::{ImagePart, Message};
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy, ReasoningLevel};
use pantheon_dashboard::{uploads, App, DashboardMount};
use pantheon_providers::chain::ProviderChain;
use pantheon_providers::http::{ChatTransport, TurnOptions, WireRequest};
use pantheon_providers::{anthropic, openai};
use pantheon_secrets::SecretValue;

fn fixture_png() -> Vec<u8> {
    std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/red-1x1.png"))
        .expect("fixture PNG must exist")
}

fn image_part() -> ImagePart {
    ImagePart::from_bytes("red-1x1.png", &fixture_png()).expect("fixture must sniff as PNG")
}

fn vision_policy(provider: &str, model: &str) -> ModelPolicy {
    ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: provider.into(),
            model: model.into(),
        },
        fallbacks: FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    }
}

/// Stub transport: the vision-gate test must fail before any attempt, so
/// this panics if it is ever called - proving the gate fires first.
struct NeverTransport;
impl ChatTransport for NeverTransport {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        panic!("transport must not be reached: the vision gate fires first")
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<pantheon_providers::http::StreamEnd, PantheonError> {
        panic!("transport must not be reached: the vision gate fires first")
    }
}

#[test]
fn openai_wire_carries_image_url_data_url() {
    let body = openai::body_value(
        "gpt-4o-mini",
        &[Message::user("what color is this?").with_images(vec![image_part()])],
        &[],
    );
    let parts = body["messages"][0]["content"]
        .as_array()
        .expect("image message must serialize as content parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[0]["text"], "what color is this?");
    assert_eq!(parts[1]["type"], "image_url");
    let url = parts[1]["image_url"]["url"].as_str().unwrap();
    assert!(
        url.starts_with("data:image/png;base64,"),
        "must be a PNG data URL, got {url:.48}"
    );
    // The payload is the fixture file itself, base64-encoded.
    let encoded = &url["data:image/png;base64,".len()..];
    assert_eq!(encoded, image_part().data);
    assert_eq!(encoded.len(), fixture_png().len().div_ceil(3) * 4);
}

#[test]
fn openai_text_only_rows_stay_plain_strings() {
    let body = openai::body_value("gpt-4o-mini", &[Message::user("hi")], &[]);
    assert_eq!(body["messages"][0]["content"], "hi");
}

#[test]
fn anthropic_wire_carries_base64_image_block() {
    let req = anthropic::request(
        "https://api.anthropic.com",
        "k",
        "claude-sonnet-4",
        &[Message::user("what color is this?").with_images(vec![image_part()])],
        &[],
        false,
        64,
        ReasoningLevel::Off,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    let blocks = v["messages"][0]["content"]
        .as_array()
        .expect("image message must serialize as content blocks");
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[0]["text"], "what color is this?");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
    assert_eq!(blocks[1]["source"]["media_type"], "image/png");
    assert_eq!(
        blocks[1]["source"]["data"].as_str().unwrap(),
        image_part().data
    );
}

#[test]
fn vision_gate_refuses_non_vision_model_before_any_attempt() {
    // deepseek-chat: catalog vision=false. NeverTransport panics if the
    // chain gets as far as building a request - the gate must fire first.
    let chain = ProviderChain::new(
        vision_policy("deepseek", "deepseek-chat"),
        NeverTransport,
        vec![],
        SecretValue::new("k"),
    );
    let msg = Message::user("see this").with_images(vec![image_part()]);
    let err = chain
        .turn_messages(&[msg])
        .expect_err("non-vision model must refuse image parts loudly");
    assert_eq!(err.code, "VISION_UNSUPPORTED");
    assert!(
        !err.retryable,
        "refusal must not be retryable: no fallback may mask a capability mismatch"
    );
    assert!(
        err.cause.contains("deepseek/deepseek-chat"),
        "error must name the model: {}",
        err.cause
    );
    assert!(
        err.remediation.contains("vision-capable"),
        "error must say what to do instead: {}",
        err.remediation
    );
}

#[test]
fn fixture_png_base64_matches_independent_encoding() {
    // Cross-checked with `python3 -c base64.b64encode`: the encoder must
    // agree with an independent implementation byte for byte.
    let part = image_part();
    assert_eq!(part.mime, "image/png");
    let expected = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVQI12P4z8AAAAADAAEABf7UAAAAAElFTkSuQmCC";
    assert_eq!(part.data, expected);
}

// --- End-to-end: upload -> disk -> image part -> provider wire JSON ---

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
        if line.trim().is_empty() {
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
fn upload_attach_serialize_end_to_end() {
    let (port, token, dir) = boot();
    let auth = vec![
        ("x-pantheon-token".into(), token),
        ("origin".into(), "http://127.0.0.1".into()),
    ];
    // 1. Attach: POST the fixture PNG through the real upload endpoint.
    let data_b64 = image_part().data;
    let body = format!(r#"{{"name": "red-1x1.png", "mime": "image/png", "data": "{data_b64}"}}"#);
    let r = raw_request(port, "POST", "/api/uploads", &auth, Some(&body));
    assert_eq!(r.status, 201, "upload must succeed: {}", r.body);
    let id = serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. Resolve from disk (no App handle - the dir-based path the turn
    //    child would use) and build the image part.
    let info = uploads::resolve_in(&dir.path().join("uploads"), &id)
        .expect("upload must resolve from disk");
    let part = uploads::image_part(&info).expect("uploaded PNG must become an image part");
    assert_eq!(part.mime, "image/png");
    assert_eq!(part.data, data_b64);

    // 3. Serialize onto the outgoing provider request (both wire modes).
    let msg = Message::user("what color is this?").with_images(vec![part]);
    let oai = openai::body_value("gpt-4o-mini", &[msg.clone()], &[]);
    let oai_parts = oai["messages"][0]["content"].as_array().unwrap();
    assert_eq!(oai_parts[1]["type"], "image_url");
    assert!(oai_parts[1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));

    let areq = anthropic::request(
        "https://api.anthropic.com",
        "k",
        "claude-sonnet-4",
        &[msg],
        &[],
        false,
        64,
        ReasoningLevel::Off,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&areq.body).unwrap();
    let ablocks = v["messages"][0]["content"].as_array().unwrap();
    assert_eq!(ablocks[1]["type"], "image");
    assert_eq!(ablocks[1]["source"]["media_type"], "image/png");
    assert_eq!(ablocks[1]["source"]["data"].as_str().unwrap(), data_b64);
}

/// Mock-provider vision round trip: an image part goes out on the wire as
/// an `image_url` data URL and the model's description comes back.
///
/// No live key needed - the stub transport captures the request body and
/// returns a canned chat-completion. This exercises the full chain path
/// (vision gate -> adapter serialization -> response parsing), which the
/// unit-level `body_value` tests above do not. A live variant (real pixels
/// to a real vision model) still wants a key in hand; the sandbox has no
/// `NVIDIA_API_KEY` and no direct egress, so that stays a manual step
/// (see docs/live-test-report-nim.md for the harness used last time).
struct CaptureTransport {
    body: Arc<Mutex<Option<String>>>,
}
impl ChatTransport for CaptureTransport {
    fn post(&self, req: &WireRequest) -> Result<String, PantheonError> {
        *self.body.lock().unwrap() = Some(req.body.clone());
        Ok(r#"{"choices":[{"message":{"content":"A single red pixel."},"finish_reason":"stop"}],"usage":{"prompt_tokens":259,"completion_tokens":8,"total_tokens":267}}"#.to_string())
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<pantheon_providers::http::StreamEnd, PantheonError> {
        unimplemented!("single-shot test")
    }
}

#[test]
fn vision_turn_sends_image_and_returns_description() {
    // gpt-4o-mini: catalog vision=true, so the chain's vision gate passes;
    // the stub transport means no network ever happens.
    let captured = Arc::new(Mutex::new(None));
    let chain = ProviderChain::new(
        vision_policy("openai", "gpt-4o-mini"),
        CaptureTransport {
            body: captured.clone(),
        },
        vec![],
        SecretValue::new("k"),
    );
    let msg = Message::user("what color is this?").with_images(vec![image_part()]);
    let outcome = chain
        .turn_messages(&[msg])
        .expect("mock vision turn must succeed");
    let text = match outcome {
        TurnOutcome::Text { text, .. } => text,
        other => panic!("expected a text answer, got {other:?}"),
    };
    assert_eq!(text, "A single red pixel.");
    // And the wire request actually carried the picture, not just the path.
    let body = captured
        .lock()
        .unwrap()
        .clone()
        .expect("request must be captured");
    let v: serde_json::Value = serde_json::from_str(&body).expect("wire body is JSON");
    let parts = v["messages"][0]["content"]
        .as_array()
        .expect("image message must serialize as content parts");
    assert_eq!(parts[1]["type"], "image_url");
    let url = parts[1]["image_url"]["url"].as_str().unwrap();
    assert!(
        url.starts_with("data:image/png;base64,"),
        "wire image must be a PNG data URL, got {url:.48}"
    );
    assert!(
        url.ends_with(&image_part().data),
        "wire payload must be the fixture image bytes"
    );
}
