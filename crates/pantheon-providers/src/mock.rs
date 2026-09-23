//! Scripted mock transport for deterministic evals and tests.
//!
//! Implements `ChatTransport` by replaying a fixture file instead of
//! hitting the network. The fixture maps a substring of the request body
//! to a scripted OpenAI-style completion response; the first matching
//! entry wins, and a catch-all (`"match": "*"`) answers anything else.
//!
//! Fixture shape (JSON):
//!   {"responses": [
//!     {"match": "needle-in-request", "content": "text reply"},
//!     {"match": "*", "tool_calls": [
//!        {"name": "shell", "arguments": "{\"command\":\"echo hi\"}"}
//!      ]},
//!     {"match": "later", "content": "second reply"}
//!   ]}
//!
//! `match` checks the serialized request body (model name, message text,
//! tool schemas all appear there). Entries are consumed in order: each
//! response is used once, then the scan continues from the top for the
//! next request. This lets a fixture script multi-turn conversations:
//! first request matches entry 1, second matches entry 2, and so on.
//!
//! Also records every request body it sees, so tests/evals can assert on
//! what the runtime actually sent (tool schemas present, message count).
use crate::http::{ChatTransport, WireRequest};
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

pub(crate) fn perr(code: &str, cause: String, retryable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Provider,
        retryable,
        cause,
        "check the mock fixture",
        "",
    )
}

#[derive(Debug, Clone, Deserialize)]
pub struct MockResponse {
    /// Substring matched against the request body. "*" catches all.
    #[serde(rename = "match")]
    pub match_: String,
    /// Plain text reply.
    #[serde(default)]
    pub content: Option<String>,
    /// Tool calls to return instead of text.
    #[serde(default)]
    pub tool_calls: Vec<MockToolCall>,
    /// Simulated token usage.
    #[serde(default)]
    pub usage: Option<MockUsage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MockToolCall {
    pub name: String,
    /// Arguments as a JSON string (OpenAI wire format).
    pub arguments: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MockUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Deserialize)]
pub struct MockFixture {
    pub responses: Vec<MockResponse>,
}

/// Scripted transport. Clone-able; all clones share consumed-entry state
/// and the request journal.
#[derive(Clone)]
pub struct MockTransport {
    fixture: std::sync::Arc<MockFixture>,
    inner: std::sync::Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    /// Indices of fixture entries already consumed (each used once).
    used: Vec<usize>,
    /// Every request body seen, in order.
    journal: Vec<String>,
}

impl MockTransport {
    /// Load a fixture from a JSON file.
    pub fn from_file(path: &Path) -> Result<Self, PantheonError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| perr("MOCK_LOAD", format!("read {}: {e}", path.display()), false))?;
        Self::from_str(&raw)
    }

    pub fn from_str(raw: &str) -> Result<Self, PantheonError> {
        let fixture: MockFixture = serde_json::from_str(raw)
            .map_err(|e| perr("MOCK_PARSE", format!("fixture: {e}"), false))?;
        Ok(Self {
            fixture: std::sync::Arc::new(fixture),
            inner: std::sync::Arc::new(Mutex::new(MockState::default())),
        })
    }

    /// Requests seen so far, in order (for assertions).
    pub fn journal(&self) -> Vec<String> {
        self.inner.lock().unwrap().journal.clone()
    }
}

impl MockTransport {
    /// Pick the fixture entry for this request: first unconsumed entry whose
    /// `match` is "*" or appears in the body. Falls back to any unconsumed
    /// catch-all. Error when the script is exhausted.
    fn pick(&self, body: &str) -> Result<MockResponse, PantheonError> {
        let mut st = self.inner.lock().unwrap();
        st.journal.push(body.to_string());
        // Pass 1: specific matches (not "*") in fixture order.
        for (i, r) in self.fixture.responses.iter().enumerate() {
            if st.used.contains(&i) {
                continue;
            }
            if r.match_ != "*" && body.contains(&r.match_) {
                st.used.push(i);
                return Ok(r.clone());
            }
        }
        // Pass 2: catch-all.
        for (i, r) in self.fixture.responses.iter().enumerate() {
            if st.used.contains(&i) {
                continue;
            }
            if r.match_ == "*" {
                st.used.push(i);
                return Ok(r.clone());
            }
        }
        Err(perr(
            "MOCK_EXHAUSTED",
            format!(
                "fixture script exhausted: {} of {} entries used; unexpected request body starts with '{}'",
                st.used.len(),
                self.fixture.responses.len(),
                body.chars().take(120).collect::<String>()
            ),
            false,
        ))
    }

    /// Build an OpenAI chat-completions response body from a fixture entry.
    fn wire_response(entry: &MockResponse) -> String {
        let now = 1_700_000_000_000i64;
        let body = if entry.tool_calls.is_empty() {
            serde_json::json!({
                "id": "mockcmpl",
                "object": "chat.completion",
                "created": now,
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": entry.content.clone().unwrap_or_default()
                    },
                    "finish_reason": "stop"
                }],
                "usage": entry.usage.as_ref().map(|u| serde_json::json!({
                    "prompt_tokens": u.input_tokens,
                    "completion_tokens": u.output_tokens,
                    "total_tokens": u.input_tokens + u.output_tokens
                }))
            })
        } else {
            let calls: Vec<serde_json::Value> = entry
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, tc)| {
                    serde_json::json!({
                        "id": format!("mockcall_{i}"),
                        "type": "function",
                        "function": {"name": tc.name, "arguments": tc.arguments}
                    })
                })
                .collect();
            serde_json::json!({
                "id": "mockcmpl",
                "object": "chat.completion",
                "created": now,
                "model": "mock",
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": entry.content.clone().unwrap_or_default(),
                        "tool_calls": calls
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": entry.usage.as_ref().map(|u| serde_json::json!({
                    "prompt_tokens": u.input_tokens,
                    "completion_tokens": u.output_tokens,
                    "total_tokens": u.input_tokens + u.output_tokens
                }))
            })
        };
        body.to_string()
    }
}

impl ChatTransport for MockTransport {
    fn post(&self, req: &WireRequest) -> Result<String, PantheonError> {
        let entry = self.pick(&req.body)?;
        Ok(Self::wire_response(&entry))
    }

    fn post_stream(
        &self,
        req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError> {
        // Replay the same response as a few SSE chunks + [DONE].
        let entry = self.pick(&req.body)?;
        let body = Self::wire_response(&entry);
        // Naive chunking: split content into ~3 chunks for stream coverage.
        for _ in 0..3 {
            on_payload(&body)?;
        }
        on_payload("[DONE]")
    }
}
