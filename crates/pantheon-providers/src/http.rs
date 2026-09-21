//! Live provider adapters (spec §5 + §14): OpenAI-compatible HTTP behind
//! `ModelTurn`, driven by the locked model policy (default + failure-only
//! fallbacks + auxiliaries). NO routing — the runtime picks, never the agent.
//!
//! Wire format follows the OpenAI chat-completions shape (studied from
//! Hermes): messages system|user|assistant|tool, tools as
//! {"type":"function","function":{...}}, tool_calls on the assistant row,
//! tool results on role:"tool" rows keyed by tool_call_id.

use pantheon_agent::{ToolCall, TurnOutcome};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::{Message, Role, ToolCallRef, ToolSchema};
use pantheon_core::model::{DefaultModel, ModelPolicy};
use std::time::Duration;

fn perr(code: &str, cause: String, retryable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Provider,
        retryable,
        cause,
        "check provider config, key, and network",
        "",
    )
}

/// Which model the turn resolved to (for the ledger).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider: String,
    pub model: String,
    /// Index in the chain: 0 = default, n>0 = n-th fallback.
    pub chain_index: usize,
}

/// One chat-completions HTTP call, provider-agnostic.
pub trait ChatTransport: Send + Sync {
    /// POST {base_url}/chat/completions with the assembled body.
    /// Returns the raw response body text.
    fn post_chat(&self, base_url: &str, api_key: &str, body: &str)
        -> Result<String, PantheonError>;
}

/// ureq-based transport. Sync, rustls, no tokio.
pub struct HttpTransport {
    pub timeout: Duration,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

impl ChatTransport for HttpTransport {
    fn post_chat(
        &self,
        base_url: &str,
        api_key: &str,
        body: &str,
    ) -> Result<String, PantheonError> {
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let resp = agent
            .post(&url)
            .set("Authorization", &format!("Bearer {api_key}"))
            .set("Content-Type", "application/json")
            .send_string(body)
            .map_err(|e| {
                let retryable = !matches!(e, ureq::Error::Status(400..=499, _));
                perr("PROVIDER_HTTP", format!("{url}: {e}"), retryable)
            })?;
        resp.into_string()
            .map_err(|e| perr("PROVIDER_READ", e.to_string(), true))
    }
}

/// Assemble the request body from canonical messages + tool schemas.
pub fn build_body(model: &str, messages: &[Message], tools: &[ToolSchema]) -> String {
    let mut msgs = Vec::with_capacity(messages.len());
    for m in messages {
        let mut row = serde_json::json!({ "role": role_str(m.role), "content": m.content });
        if !m.tool_calls.is_empty() {
            let calls: Vec<serde_json::Value> = m
                .tool_calls
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "id": c.id, "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments },
                    })
                })
                .collect();
            row["tool_calls"] = serde_json::Value::Array(calls);
        }
        if let Some(id) = &m.tool_call_id {
            row["tool_call_id"] = serde_json::Value::String(id.clone());
        }
        msgs.push(row);
    }
    let mut body = serde_json::json!({ "model": model, "messages": msgs });
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools.iter().map(|t| t.to_wire()).collect());
    }
    body.to_string()
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Parse a chat-completions response body into a TurnOutcome.
pub fn parse_response(body: &str) -> Result<TurnOutcome, PantheonError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| perr("PROVIDER_PARSE", e.to_string(), false))?;
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| perr("PROVIDER_PARSE", "no choices in response".into(), false))?;
    let msg = choice
        .get("message")
        .ok_or_else(|| perr("PROVIDER_PARSE", "no message in choice".into(), false))?;
    let tcs = msg.get("tool_calls").and_then(|t| t.as_array());
    match tcs {
        Some(calls) if !calls.is_empty() => {
            let mut out = Vec::with_capacity(calls.len());
            for c in calls {
                let _id = c
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("call_0")
                    .to_string();
                let name = c
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = c
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("{}")
                    .to_string();
                if name.is_empty() {
                    return Err(perr(
                        "PROVIDER_PARSE",
                        "tool_call missing name".into(),
                        false,
                    ));
                }
                out.push(ToolCall {
                    name,
                    capability: pantheon_core::capability::Capability::Other("tool".into()),
                    args,
                });
            }
            Ok(TurnOutcome::Tools(out))
        }
        _ => {
            let text = msg
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            if text.is_empty() {
                // Some providers return reasoning-only rows; treat as empty text.
                return Ok(TurnOutcome::Text(String::new()));
            }
            Ok(TurnOutcome::Text(text))
        }
    }
}

/// A `ModelTurn` backed by the policy chain: default first, then failure-only
/// fallbacks. Each attempt is recorded so the loop can log which model served.
pub struct ProviderChain<T: ChatTransport> {
    pub policy: ModelPolicy,
    pub transport: T,
    pub tools: Vec<ToolSchema>,
    pub api_key: String,
    /// Resolved after the last turn (default or fallback index).
    pub last_resolved: std::cell::RefCell<Option<ResolvedModel>>,
}

impl<T: ChatTransport> ProviderChain<T> {
    pub fn new(policy: ModelPolicy, transport: T, tools: Vec<ToolSchema>, api_key: String) -> Self {
        Self {
            policy,
            transport,
            tools,
            api_key,
            last_resolved: std::cell::RefCell::new(None),
        }
    }

    fn base_url_for(&self, m: &DefaultModel) -> String {
        // Provider-keyed env override first (PANTHEON_BASE_<PROVIDER>), then a
        // built-in default, then openai-compatible passthrough.
        let env_key = format!("PANTHEON_BASE_{}", m.provider.to_uppercase());
        if let Ok(u) = std::env::var(&env_key) {
            return u;
        }
        match m.provider.as_str() {
            "openai" => "https://api.openai.com/v1".into(),
            "deepseek" => "https://api.deepseek.com/v1".into(),
            "openrouter" => "https://openrouter.ai/api/v1".into(),
            "groq" => "https://api.groq.com/openai/v1".into(),
            "local" => "http://127.0.0.1:11434/v1".into(),
            _ => m.provider.clone(), // treat as full base URL
        }
    }

    fn key_for(&self, m: &DefaultModel) -> String {
        let env_key = format!("PANTHEON_KEY_{}", m.provider.to_uppercase());
        std::env::var(&env_key).unwrap_or_else(|_| self.api_key.clone())
    }

    /// Try the chain: default, then each fallback on retryable failure only.
    /// `failed` tracks the chain index that just failed; `None` = the default.
    fn turn_with_chain(&self, transcript: &[Message]) -> Result<TurnOutcome, PantheonError> {
        let mut failed: Option<usize> = None;
        loop {
            let (idx, model) = match failed {
                None => (0usize, &self.policy.default),
                // Default failed (failed == Some(0)) -> start the fallback list
                // from its head. A fallback at index i failed -> next one.
                Some(0) => match crate::on_retryable_failure(&self.policy, None) {
                    Some((ni, m)) => (ni + 1, m),
                    None => {
                        return Err(perr(
                            "PROVIDER_EXHAUSTED",
                            "default and all fallbacks failed".into(),
                            false,
                        ))
                    }
                },
                Some(i) => match crate::on_retryable_failure(&self.policy, Some(i - 1)) {
                    Some((ni, m)) => (ni + 1, m),
                    None => {
                        return Err(perr(
                            "PROVIDER_EXHAUSTED",
                            "default and all fallbacks failed".into(),
                            false,
                        ))
                    }
                },
            };
            let base = self.base_url_for(model);
            let key = self.key_for(model);
            let body = build_body(&model.model, transcript, &self.tools);
            match self
                .transport
                .post_chat(&base, &key, &body)
                .and_then(|b| parse_response(&b))
            {
                Ok(out) => {
                    *self.last_resolved.borrow_mut() = Some(ResolvedModel {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        chain_index: idx,
                    });
                    return Ok(out);
                }
                Err(e) if e.retryable => {
                    failed = Some(idx);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

impl<T: ChatTransport> pantheon_agent::ModelTurn for ProviderChain<T> {
    fn turn(&self, transcript: &[String]) -> Result<TurnOutcome, PantheonError> {
        // Legacy string transcript -> canonical messages (compat path).
        let msgs: Vec<Message> = transcript
            .iter()
            .map(|l| {
                if let Some(rest) = l.strip_prefix("user: ") {
                    Message::user(rest)
                } else if let Some(rest) = l.strip_prefix("assistant: ") {
                    Message::assistant(rest)
                } else if let Some(rest) = l.strip_prefix("tool") {
                    let id = l
                        .split('[')
                        .nth(1)
                        .and_then(|s| s.split(']').next())
                        .unwrap_or("t")
                        .to_string();
                    let _ = rest;
                    Message::tool(format!("call_{id}"), l)
                } else {
                    Message::user(l.clone())
                }
            })
            .collect();
        self.turn_with_chain(&msgs)
    }
}

/// Direct canonical-message turn (the wired loop uses this).
impl<T: ChatTransport> ProviderChain<T> {
    pub fn turn_messages(&self, messages: &[Message]) -> Result<TurnOutcome, PantheonError> {
        self.turn_with_chain(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeTransport {
        bodies: std::sync::Mutex<Vec<String>>,
        responses: std::sync::Mutex<Vec<Result<String, PantheonError>>>,
    }
    impl ChatTransport for FakeTransport {
        fn post_chat(&self, _b: &str, _k: &str, body: &str) -> Result<String, PantheonError> {
            self.bodies.lock().unwrap().push(body.into());
            self.responses.lock().unwrap().remove(0)
        }
    }

    fn policy() -> ModelPolicy {
        use pantheon_core::model::*;
        ModelPolicy {
            default: DefaultModel {
                provider: "openai".into(),
                model: "gpt-test".into(),
            },
            fallbacks: FallbackChain {
                fallbacks: vec![DefaultModel {
                    provider: "deepseek".into(),
                    model: "ds-test".into(),
                }],
            },
            auxiliaries: vec![],
        }
    }

    fn ok_body(text: &str) -> String {
        serde_json::json!({ "choices": [ { "message": { "role": "assistant", "content": text } } ] }).to_string()
    }
    fn tool_body() -> String {
        serde_json::json!({ "choices": [ { "message": { "role": "assistant", "content": "",
            "tool_calls": [ { "id": "call_1", "type": "function",
            "function": { "name": "shell", "arguments": "{\"cmd\":\"ls\"}" } } ] } } ] })
        .to_string()
    }

    #[test]
    fn text_response_parses() {
        assert_eq!(
            parse_response(&ok_body("hi")).unwrap(),
            TurnOutcome::Text("hi".into())
        );
    }

    #[test]
    fn tool_calls_parse() {
        match parse_response(&tool_body()).unwrap() {
            TurnOutcome::Tools(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "shell");
                assert_eq!(calls[0].args, "{\"cmd\":\"ls\"}");
            }
            _ => panic!("expected tools"),
        }
    }

    #[test]
    fn fallback_on_retryable_failure() {
        let t = FakeTransport {
            bodies: std::sync::Mutex::new(vec![]),
            responses: std::sync::Mutex::new(vec![
                Err(perr("PROVIDER_HTTP", "503".into(), true)),
                Ok(ok_body("from fallback")),
            ]),
        };
        let chain = ProviderChain::new(policy(), t, vec![], String::new());
        let out = chain.turn_messages(&[Message::user("go")]).unwrap();
        assert_eq!(out, TurnOutcome::Text("from fallback".into()));
        let r = chain.last_resolved.borrow().clone().unwrap();
        assert_eq!(r.chain_index, 1);
        assert_eq!(r.provider, "deepseek");
    }

    #[test]
    fn non_retryable_fails_fast() {
        let t = FakeTransport {
            bodies: std::sync::Mutex::new(vec![]),
            responses: std::sync::Mutex::new(vec![Err(perr(
                "PROVIDER_HTTP",
                "401 unauthorized".into(),
                false,
            ))]),
        };
        let chain = ProviderChain::new(policy(), t, vec![], String::new());
        let err = chain.turn_messages(&[Message::user("go")]).unwrap_err();
        assert_eq!(err.code, "PROVIDER_HTTP");
        assert!(!err.retryable);
    }

    #[test]
    fn chain_exhaustion_is_structured() {
        let t = FakeTransport {
            bodies: std::sync::Mutex::new(vec![]),
            responses: std::sync::Mutex::new(vec![
                Err(perr("PROVIDER_HTTP", "500".into(), true)),
                Err(perr("PROVIDER_HTTP", "500".into(), true)),
            ]),
        };
        let chain = ProviderChain::new(policy(), t, vec![], String::new());
        let err = chain.turn_messages(&[Message::user("go")]).unwrap_err();
        assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    }

    #[test]
    fn body_has_tools_and_messages() {
        let body = build_body(
            "m",
            &[Message::user("hi")],
            &[ToolSchema {
                name: "shell".into(),
                description: "run".into(),
                parameters: serde_json::json!({"type":"object","properties":{}}),
            }],
        );
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["tools"][0]["type"], "function");
        assert_eq!(v["tools"][0]["function"]["name"], "shell");
    }
}
