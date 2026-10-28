//! OpenAI-compatible chat-completions adapter (spec §5 + §14).
//!
//! Wire format follows the OpenAI chat-completions shape:
//! messages system|user|assistant|tool, tools as
//! {"type":"function","function":{...}}, tool_calls on the assistant row,
//! tool results on role:"tool" rows keyed by tool_call_id.
//!
//! Both paths (single-shot and SSE) emit the same normalized `ModelEvent`s.
//! Policy events (Attempt/Usage/Completed/Fallback) belong to the chain.

use crate::http::{
    perr, AdapterTurn, ChatTransport, StreamEnd, ToolChoice, TurnOptions, WireRequest,
};
use crate::model_event::{ModelEvent, ModelEventSink, ModelUsage};
use pantheon_agent::{ToolCall, TurnOutcome};
use pantheon_api::capability::Capability;
use pantheon_api::error::PantheonError;
use pantheon_api::message::{Message, Role, ToolSchema};
use std::collections::BTreeMap;

/// Assemble the request body value from canonical messages + tool schemas.
///
/// Provenance envelope: tool rows and any row carrying untrusted or
/// memory-tier provenance get a `[provenance: ...]` prefix so the model
/// can distinguish fetched data from instructions. System/User tiers are
/// authoritative and get no prefix (a prefix on authoritative content
/// would only invite the model to distrust the wrong things).
pub fn body_value(model: &str, messages: &[Message], tools: &[ToolSchema]) -> serde_json::Value {
    let mut msgs = Vec::with_capacity(messages.len());
    for m in messages {
        let text = match &m.provenance {
            Some(p) if p.trust.rank() <= pantheon_api::provenance::TrustTier::Memory.rank() => {
                format!("{} {}", p.envelope_prefix(), m.content)
            }
            _ => m.content.clone(),
        };
        // Vision models get OpenAI-style content parts; text-only rows keep
        // the plain string shape byte-identically to before. (The chain
        // refuses image parts for non-vision models before this runs, so a
        // parts array here always means the model can see pictures.)
        let content = if m.images.is_empty() {
            serde_json::Value::String(text)
        } else {
            let mut parts = vec![serde_json::json!({"type": "text", "text": text})];
            for img in &m.images {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": { "url": img.data_url() },
                }));
            }
            serde_json::Value::Array(parts)
        };
        let mut row = serde_json::json!({ "role": role_str(m.role), "content": content });
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
    body
}

/// Assemble the request body from canonical messages + tool schemas.
pub fn build_body(model: &str, messages: &[Message], tools: &[ToolSchema]) -> String {
    body_value(model, messages, tools).to_string()
}

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

/// Build the wire request for one attempt. `stream` flips SSE mode on.
/// `key_header` names the HTTP header carrying the key: `Authorization`
/// sends `Bearer <key>`; any other name (e.g. Xiaomi MiMo's `api-key`)
/// sends the raw key.
// Ten params: the nine the body needs plus per-turn options
// (structured output, tool choice), which stay explicit wire arguments
// at every call site so each reads as what goes on the wire.
#[allow(clippy::too_many_arguments)]
pub fn request(
    base_url: &str,
    api_key: &str,
    key_header: &str,
    provider_id: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSchema],
    stream: bool,
    max_tokens: u32,
    reasoning: pantheon_api::model::ReasoningLevel,
    opts: &TurnOptions,
) -> WireRequest {
    let mut body = body_value(model, messages, tools);
    // Per-request output cap (session /tokens > [budget].max_tokens >
    // the model's known maximum, resolved by the chain). Sent on every
    // chat request: previously the OpenAI wire path never sent one, so
    // the directive was dead here even when set.
    body["max_tokens"] = serde_json::Value::Number(max_tokens.into());
    // Reasoning effort is opt-in only: `Off` leaves the body exactly as
    // before, so endpoints that reject unknown fields never see one.
    // Mapped to OpenAI's `reasoning_effort` verbatim (low|medium|high).
    // A model the catalog *knows* cannot take reasoning never gets
    // the field: sending it earns a non-retryable 400, and the chain
    // then burns every fallback with the same poisoned body.
    // Uncatalogued models keep the old behaviour (unknown, not known-bad).
    let known_no_reasoning = crate::catalog::model_known(provider_id, model)
        && !crate::catalog::model_meta(provider_id, model).reasoning;
    if !matches!(reasoning, pantheon_api::model::ReasoningLevel::Off) && !known_no_reasoning {
        let level = reasoning.as_str();
        let mapped = crate::catalog::provider(provider_id)
            .and_then(|p| p.reasoning_map.get(level).cloned())
            .unwrap_or_else(|| level.to_string());
        body["reasoning_effort"] = serde_json::Value::String(mapped);
    }
    if stream {
        body["stream"] = serde_json::Value::Bool(true);
        // Ask for the trailing usage chunk (OpenAI standard); without it,
        // usage in streams is provider-dependent.
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    // Structured output is opt-in: no schema → the field is absent and the
    // body is byte-identical to before.
    if let Some(schema) = &opts.response_schema {
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": { "name": "pantheon_structured", "schema": schema },
        });
    }
    // Tool choice is provider-default (`auto`) unless the turn says
    // otherwise; with no tools there is nothing to choose between, so the
    // field stays absent either way.
    if !tools.is_empty() {
        if let Some(choice) = tool_choice_value(&opts.tool_choice) {
            body["tool_choice"] = choice;
        }
    }
    // Filter sampling params by the provider's allowed list. Providers that
    // reject unknown fields (Azure, newer Anthropic) need this to avoid 400s.
    // `model`, `messages`, and `tools` are never filtered: they are the
    // request itself, not sampling params, and an allowed list that omits
    // them (as Azure's once did) must not strip the body down to a bare
    // sampling dict that every endpoint rejects. `stream` and
    // `stream_options` are likewise never filtered: they are wire
    // protocol, not sampling params, and stripping them silently drops
    // the trailing usage chunk on every attempt - including retries,
    // which is how token usage was lost on retry.
    if let Some(p) = crate::catalog::provider(provider_id) {
        if !p.allowed_params.is_empty() {
            let allowed: std::collections::HashSet<&str> =
                p.allowed_params.iter().map(|s| s.as_str()).collect();
            body.as_object_mut().unwrap().retain(|k, _| {
                matches!(
                    k.as_str(),
                    "model" | "messages" | "tools" | "stream" | "stream_options"
                ) || allowed.contains(k.as_str())
            });
        }
    }
    let (header_name, auth_value) = crate::http::auth_header_pair(key_header, api_key);
    WireRequest {
        url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        headers: vec![
            (header_name, auth_value),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: body.to_string(),
        use_sigv4: crate::catalog::provider(provider_id)
            .map(|p| p.sigv4)
            .unwrap_or(false),
    }
}

/// OpenAI `tool_choice` wire value. `None` = omit the field (provider
/// default, `auto`).
fn tool_choice_value(choice: &ToolChoice) -> Option<serde_json::Value> {
    match choice {
        ToolChoice::Auto => None,
        ToolChoice::Required => Some(serde_json::json!({"type": "required"})),
        ToolChoice::None => Some(serde_json::json!("none")),
        ToolChoice::Named(name) => {
            Some(serde_json::json!({"type": "function", "function": {"name": name}}))
        }
    }
}

/// Build a structured error from a mid-stream `{"error": ...}` SSE payload.
/// Mirrors the Anthropic `error` event path: rate-limit/overloaded-class
/// errors are fallback-eligible, the rest are not.
fn stream_err_from(v: &serde_json::Value) -> PantheonError {
    let etype = v
        .pointer("/error/type")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let code = v
        .pointer("/error/code")
        .map(|x| {
            x.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| x.to_string())
        })
        .unwrap_or_default();
    let msg = v
        .pointer("/error/message")
        .and_then(|x| x.as_str())
        .unwrap_or("provider stream error")
        .to_string();
    let tag = if code.is_empty() {
        etype.as_str()
    } else {
        code.as_str()
    };
    let retryable = matches!(
        tag,
        "overloaded_error"
            | "rate_limit_error"
            | "rate_limit_exceeded"
            | "rate_limit_reached"
            | "429"
    );
    perr("PROVIDER_STREAM", format!("{etype}: {msg}"), retryable)
}

fn usage_of(v: &serde_json::Value) -> Option<ModelUsage> {
    let u = v.get("usage")?;
    if !u.is_null() {
        // OpenAI documents `prompt_tokens` as already inclusive of
        // cached tokens; adding `cached_tokens` on top double-counted
        // every cached turn. Input is `prompt_tokens` verbatim.
        let prompt = u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
        let output = u
            .get("completion_tokens")
            .and_then(|x| x.as_u64())
            .unwrap_or(0);
        let total = u
            .get("total_tokens")
            .and_then(|x| x.as_u64())
            .filter(|t| *t > 0)
            .unwrap_or(prompt + output);
        return Some(ModelUsage {
            input_tokens: prompt,
            output_tokens: output,
            total_tokens: total,
            cost_usd: None,
        });
    }
    None
}

/// Parse a single-shot chat-completions response into an `AdapterTurn`,
/// emitting normalized events (text / reasoning / tool calls).
pub fn parse_response(body: &str, sink: &dyn ModelEventSink) -> Result<AdapterTurn, PantheonError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| perr("PROVIDER_PARSE", e.to_string(), false))?;
    let choice = v
        .get("choices")
        .and_then(|c| c.get(0))
        .ok_or_else(|| perr("PROVIDER_PARSE", "no choices in response".into(), false))?;
    let msg = choice
        .get("message")
        .ok_or_else(|| perr("PROVIDER_PARSE", "no message in choice".into(), false))?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|x| x.as_str())
        .map(str::to_string);
    let usage = usage_of(&v);
    if let Some(r) = msg.get("reasoning_content").and_then(|x| x.as_str()) {
        if !r.is_empty() {
            sink.emit(ModelEvent::ReasoningDelta {
                text: r.to_string(),
            });
        }
    }
    let text = msg
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let tcs = msg.get("tool_calls").and_then(|t| t.as_array());
    match tcs {
        Some(calls) if !calls.is_empty() => {
            // Collect + validate first, then emit (see the Anthropic
            // parser): a late missing-name error must not leave early
            // ToolCall events behind for a retry to duplicate.
            let mut parsed: Vec<(String, String, String)> = Vec::with_capacity(calls.len());
            for (i, c) in calls.iter().enumerate() {
                let id = c
                    .get("id")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{i}"));
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
                let args = crate::http::normalise_tool_args(&args, finish_reason.as_deref())?;
                parsed.push((id, name, args));
            }
            let mut out = Vec::with_capacity(parsed.len());
            for (id, name, args) in parsed {
                sink.emit(ModelEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: args.clone(),
                });
                out.push(ToolCall {
                    name,
                    capability: Capability::Other("tool".into()),
                    args,
                });
            }
            if !text.is_empty() {
                sink.emit(ModelEvent::TextDelta { text: text.clone() });
            }
            Ok(AdapterTurn {
                outcome: TurnOutcome::Tools {
                    calls: out,
                    text,
                    tokens: usage.as_ref().map(|u| u.total_tokens as u32).unwrap_or(0),
                    cost_cents: usage
                        .as_ref()
                        .and_then(|u| u.cost_usd)
                        .map(|c| (c * 100.0) as u32)
                        .unwrap_or(0),
                },
                usage,
                finish_reason,
            })
        }
        _ => {
            if !text.is_empty() {
                sink.emit(ModelEvent::TextDelta { text: text.clone() });
            }
            Ok(AdapterTurn {
                outcome: TurnOutcome::Text {
                    text,
                    tokens: usage.as_ref().map(|u| u.total_tokens as u32).unwrap_or(0),
                    cost_cents: usage
                        .as_ref()
                        .and_then(|u| u.cost_usd)
                        .map(|c| (c * 100.0) as u32)
                        .unwrap_or(0),
                },
                usage,
                finish_reason,
            })
        }
    }
}

/// Streaming state: accumulates OpenAI chunk payloads into one turn.
#[derive(Default)]
pub struct OpenStream {
    text: String,
    tools: BTreeMap<u32, (String, String, String)>, // index -> (id, name, args)
    usage: Option<ModelUsage>,
    finish_reason: Option<String>,
}

impl OpenStream {
    /// Feed one SSE `data:` payload. Emits TextDelta/ReasoningDelta as
    /// chunks arrive; tool fragments accumulate until `finish`.
    pub fn push(&mut self, payload: &str, sink: &dyn ModelEventSink) -> Result<(), PantheonError> {
        // Fully trimmed: `data: [DONE] ` (trailing space) previously
        // fell through to JSON parsing instead of terminating.
        if payload.trim() == "[DONE]" {
            return Ok(());
        }
        let v: serde_json::Value = serde_json::from_str(payload.trim())
            .map_err(|e| perr("PROVIDER_PARSE", format!("stream chunk: {e}"), false))?;
        // Some gateways/proxies send mid-stream errors as SSE `data:` events
        // with HTTP 200. Surface them as structured errors (mirrors the
        // Anthropic `error` event path) so chain fallback can engage.
        if v.get("error").is_some() {
            return Err(stream_err_from(&v));
        }
        if let Some(u) = usage_of(&v) {
            self.usage = Some(u);
        }
        let choice = match v.get("choices").and_then(|c| c.get(0)) {
            Some(c) => c,
            None => return Ok(()),
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|x| x.as_str()) {
            if !fr.is_empty() {
                self.finish_reason = Some(fr.to_string());
            }
        }
        let delta = match choice.get("delta") {
            Some(d) => d,
            None => return Ok(()),
        };
        if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
            if !content.is_empty() {
                self.text.push_str(content);
                sink.emit(ModelEvent::TextDelta {
                    text: content.to_string(),
                });
            }
        }
        for key in ["reasoning_content", "reasoning", "thinking"] {
            if let Some(r) = delta.get(key).and_then(|x| x.as_str()) {
                if !r.is_empty() {
                    sink.emit(ModelEvent::ReasoningDelta {
                        text: r.to_string(),
                    });
                }
            }
        }
        if let Some(frag) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in frag {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                let entry = self
                    .tools
                    .entry(idx)
                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                if let Some(id) = tc.get("id").and_then(|x| x.as_str()) {
                    if !id.is_empty() {
                        entry.0 = id.to_string();
                    }
                }
                if let Some(f) = tc.get("function") {
                    if let Some(n) = f.get("name").and_then(|x| x.as_str()) {
                        if !n.is_empty() {
                            entry.1 = n.to_string();
                        }
                    }
                    if let Some(a) = f.get("arguments").and_then(|x| x.as_str()) {
                        entry.2.push_str(a);
                    }
                }
            }
        }
        Ok(())
    }

    /// Assemble the final turn. Emits ToolCall events for completed calls
    /// (text deltas were already emitted chunk-by-chunk).
    pub fn finish(self, sink: &dyn ModelEventSink) -> Result<AdapterTurn, PantheonError> {
        let usage = self.usage;
        let finish_reason = self.finish_reason;
        if self.tools.is_empty() {
            let tokens = usage.as_ref().map(|u| u.total_tokens as u32).unwrap_or(0);
            let cost_cents = usage
                .as_ref()
                .and_then(|u| u.cost_usd)
                .map(|c| (c * 100.0) as u32)
                .unwrap_or(0);
            return Ok(AdapterTurn {
                outcome: TurnOutcome::Text {
                    text: self.text,
                    tokens,
                    cost_cents,
                },
                usage,
                finish_reason,
            });
        }
        // Validate everything before emitting anything (see the
        // single-shot parser), and normalise empty args to `"{}"`.
        let mut parsed: Vec<(String, String, String)> = Vec::with_capacity(self.tools.len());
        for (idx, (mut id, name, args)) in self.tools {
            if id.is_empty() {
                id = format!("call_{idx}");
            }
            if name.is_empty() {
                return Err(perr(
                    "PROVIDER_PARSE",
                    format!("streamed tool_call {idx} missing name"),
                    false,
                ));
            }
            let args = crate::http::normalise_tool_args(&args, finish_reason.as_deref())?;
            parsed.push((id, name, args));
        }
        let mut out = Vec::with_capacity(parsed.len());
        for (id, name, args) in parsed {
            sink.emit(ModelEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: args.clone(),
            });
            out.push(ToolCall {
                name,
                capability: Capability::Other("tool".into()),
                args,
            });
        }
        Ok(AdapterTurn {
            outcome: TurnOutcome::Tools {
                calls: out,
                text: self.text,
                tokens: usage.as_ref().map(|u| u.total_tokens as u32).unwrap_or(0),
                cost_cents: usage
                    .as_ref()
                    .and_then(|u| u.cost_usd)
                    .map(|c| (c * 100.0) as u32)
                    .unwrap_or(0),
            },
            usage,
            finish_reason,
        })
    }
}

/// One single-shot attempt.
pub fn complete(
    transport: &dyn ChatTransport,
    req: WireRequest,
    sink: &dyn ModelEventSink,
) -> Result<AdapterTurn, PantheonError> {
    let body = transport.post(&req)?;
    parse_response(&body, sink)
}

/// One streaming attempt: deltas emit while chunks arrive.
pub fn stream(
    transport: &dyn ChatTransport,
    req: WireRequest,
    sink: &dyn ModelEventSink,
) -> Result<AdapterTurn, PantheonError> {
    let mut state = OpenStream::default();
    let end = transport.post_stream(&req, &mut |payload| state.push(payload, sink))?;
    if state.finish_reason.is_none() {
        // No finish_reason on ANY stream end: a `[DONE]` terminator
        // without one is not a completed turn either — a proxy or
        // provider can emit `[DONE]` after cutting the stream short,
        // and accepting it would present truncated output as finished.
        // Fail retryable so the chain retries or falls back instead
        // of presenting a silently truncated turn as complete.
        let cause = match end {
            StreamEnd::Done => "stream ended with [DONE] but no finish_reason".to_string(),
            StreamEnd::Eof => "stream ended without [DONE] and no finish_reason".to_string(),
        };
        return Err(perr("PROVIDER_TRUNCATED", cause, true));
    }
    state.finish(sink)
}

#[cfg(test)]
mod openai_max_tokens_tests {
    use super::*;
    use crate::http::TurnOptions;

    /// Item 6: `stream` and `stream_options` survive the allowed-params
    /// filter. (Before the fix, any provider with a non-empty
    /// allowed_params list silently lost both on every attempt -
    /// including retries - and the trailing usage chunk with them.)
    #[test]
    fn request_keeps_stream_options_under_allowed_params_filter() {
        crate::catalog::register_custom_provider(crate::catalog::ProviderMeta {
            id: "dec-rt-stream-allowed".to_string(),
            label: "dec-rt-stream-allowed".to_string(),
            base_url: "https://api.example.test/v1".to_string(),
            api_mode: crate::catalog::ApiMode::OpenAi,
            auth_command: None,
            auth_refresh_ms: None,
            reserved: false,
            allowed_params: vec!["temperature".to_string()],
            oauth_apps: vec![],
            sigv4: false,
            reasoning_map: Default::default(),
            base_env: String::new(),
            key_env: String::new(),
            key_header: "Authorization".into(),
            models: vec![],
            prominent: false,
            recommended: false,
            dev: false,
            tag: String::new(),
        });
        let req = request(
            "https://api.example.test/v1",
            "k",
            "Authorization",
            "dec-rt-stream-allowed",
            "test-model",
            &[],
            &[],
            true,
            4_321,
            pantheon_api::model::ReasoningLevel::Off,
            &TurnOptions::default(),
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body is JSON");
        assert_eq!(body["stream"], serde_json::json!(true));
        assert_eq!(
            body["stream_options"]["include_usage"],
            serde_json::json!(true)
        );
    }

    /// Item 4: the resolved per-request output cap reaches the OpenAI
    /// wire body as `max_tokens`. (Before the fix this adapter sent no
    /// max_tokens at all, so the directive was dead on this path.)
    #[test]
    fn request_body_carries_resolved_max_tokens() {
        let req = request(
            "https://api.example.test/v1",
            "k",
            "Authorization",
            "test-provider",
            "test-model",
            &[],
            &[],
            false,
            4_321,
            pantheon_api::model::ReasoningLevel::Off,
            &TurnOptions::default(),
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body is JSON");
        assert_eq!(body["max_tokens"], serde_json::json!(4_321));
    }

    /// Cluster 1 (g): Azure's `allowed_params` filter must never strip
    /// the request itself. Before the fix the catalog list omitted
    /// `model`, `messages`, and `tools`, so `retain` deleted them and
    /// every Azure chat body went out as a bare sampling dict (400 /
    /// fallback-exhaust). The filter now always keeps those three,
    /// and the catalog list names them too.
    #[test]
    fn azure_request_body_keeps_model_messages_tools() {
        let tools = vec![pantheon_api::message::ToolSchema {
            name: "read_file".to_string(),
            description: "read".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }];
        let req = request(
            "https://example.openai.azure.com/openai/v1",
            "k",
            "api-key",
            "azure",
            "my-deployment",
            &[pantheon_api::message::Message::user("hi")],
            &tools,
            false,
            1_000,
            pantheon_api::model::ReasoningLevel::Off,
            &TurnOptions::default(),
        );
        let body: serde_json::Value = serde_json::from_str(&req.body).expect("body is JSON");
        assert_eq!(body["model"], serde_json::json!("my-deployment"));
        assert!(
            body["messages"].as_array().is_some_and(|m| !m.is_empty()),
            "messages stripped: {body}"
        );
        assert!(
            body["tools"].as_array().is_some_and(|t| !t.is_empty()),
            "tools stripped: {body}"
        );
        assert_eq!(body["max_tokens"], serde_json::json!(1_000));
        // And the catalog itself must name the three, so a future
        // reader of allowed_params is not misled about what is sent.
        let azure = crate::catalog::provider("azure").expect("azure in catalog");
        for required in ["model", "messages", "tools"] {
            assert!(
                azure.allowed_params.iter().any(|p| p == required),
                "azure allowed_params omits {required}"
            );
        }
    }
}

#[cfg(test)]
mod stream_truncation_tests {
    use super::*;
    use crate::model_event::NoopModelSink;

    /// Scripted transport: feeds `payloads` to the callback, then ends
    /// the way the script says - with `[DONE]` (`StreamEnd::Done`) or
    /// bare EOF (`StreamEnd::Eof`), mirroring the real transport.
    struct StubTransport {
        payloads: Vec<&'static str>,
        done: bool,
    }

    impl ChatTransport for StubTransport {
        fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
            Err(perr(
                "STUB",
                "single-shot unused in this stub".into(),
                false,
            ))
        }
        fn post_stream(
            &self,
            _req: &WireRequest,
            on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
        ) -> Result<StreamEnd, PantheonError> {
            for p in &self.payloads {
                on_payload(p)?;
            }
            if self.done {
                on_payload("[DONE]")?;
                Ok(StreamEnd::Done)
            } else {
                Ok(StreamEnd::Eof)
            }
        }
    }

    fn wire_req() -> WireRequest {
        WireRequest {
            url: "https://api.example.test/v1/chat/completions".into(),
            headers: vec![],
            body: "{}".into(),
            use_sigv4: false,
        }
    }

    const TEXT_CHUNK: &str = r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#;
    const STOP_CHUNK: &str = r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#;

    #[test]
    fn eof_without_done_and_no_finish_reason_is_truncated() {
        let t = StubTransport {
            payloads: vec![TEXT_CHUNK],
            done: false,
        };
        let err = match stream(&t, wire_req(), &NoopModelSink) {
            Ok(_) => panic!("expected PROVIDER_TRUNCATED on truncated stream"),
            Err(e) => e,
        };
        assert_eq!(err.code, "PROVIDER_TRUNCATED");
        assert!(
            err.retryable,
            "truncation must be retryable so the chain can retry"
        );
    }

    #[test]
    fn eof_with_finish_reason_is_accepted() {
        let t = StubTransport {
            payloads: vec![TEXT_CHUNK, STOP_CHUNK],
            done: false,
        };
        let turn = stream(&t, wire_req(), &NoopModelSink).unwrap();
        assert!(matches!(turn.outcome, TurnOutcome::Text { .. }));
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn done_without_finish_reason_is_truncated() {
        // A `[DONE]` terminator with no finish_reason is not a
        // completed turn: a proxy can emit it after cutting the
        // stream short, and accepting it would present truncated
        // output as finished.
        let t = StubTransport {
            payloads: vec![TEXT_CHUNK],
            done: true,
        };
        let err = match stream(&t, wire_req(), &NoopModelSink) {
            Ok(_) => panic!("expected PROVIDER_TRUNCATED on DONE without finish_reason"),
            Err(e) => e,
        };
        assert_eq!(err.code, "PROVIDER_TRUNCATED");
        assert!(err.retryable, "truncation must be retryable");
    }

    #[test]
    fn done_terminator_still_accepted() {
        let t = StubTransport {
            payloads: vec![TEXT_CHUNK, STOP_CHUNK],
            done: true,
        };
        assert!(stream(&t, wire_req(), &NoopModelSink).is_ok());
    }
}

#[cfg(test)]
mod fix_model_tests {
    use super::*;
    use crate::model_event::NoopModelSink;

    struct RecordingSink {
        tool_calls: std::sync::Mutex<Vec<String>>,
    }
    impl ModelEventSink for RecordingSink {
        fn emit(&self, event: ModelEvent) {
            if let ModelEvent::ToolCall { name, .. } = event {
                self.tool_calls.lock().unwrap().push(name);
            }
        }
    }

    #[test]
    fn push_trims_done_terminator() {
        let mut s = OpenStream::default();
        let sink = NoopModelSink;
        assert!(s.push(" [DONE] ", &sink).is_ok());
        assert!(s.push("[DONE] ", &sink).is_ok());
    }

    #[test]
    fn streamed_empty_args_normalise_to_empty_object() {
        let mut s = OpenStream::default();
        let sink = NoopModelSink;
        s.push(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"bare","arguments":""}}]},"finish_reason":null}]}"#,
            &sink,
        )
        .unwrap();
        s.finish_reason = Some("tool_calls".into());
        let turn = s.finish(&sink).unwrap();
        match turn.outcome {
            TurnOutcome::Tools { calls, .. } => assert_eq!(calls[0].args, "{}"),
            other => panic!("expected Tools, got {other:?}"),
        }
    }

    #[test]
    fn truncated_tool_args_are_truncation_not_tool_error() {
        let mut s = OpenStream::default();
        let sink = NoopModelSink;
        s.push(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c","function":{"name":"t","arguments":"{\"x\":"}}]},"finish_reason":"length"}]}"#,
            &sink,
        )
        .unwrap();
        let err = match s.finish(&sink) {
            Ok(_) => panic!("cut-off args must fail"),
            Err(e) => e,
        };
        assert_eq!(err.code, "PROVIDER_TRUNCATED");
    }

    #[test]
    fn text_beside_tool_calls_survives_in_outcome() {
        let body = r#"{"choices":[{"message":{"content":"working on it","tool_calls":[{"id":"c","type":"function","function":{"name":"read","arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#;
        let turn = parse_response(body, &NoopModelSink).unwrap();
        match turn.outcome {
            TurnOutcome::Tools { text, calls, .. } => {
                assert_eq!(text, "working on it");
                assert_eq!(calls.len(), 1);
            }
            other => panic!("expected Tools, got {other:?}"),
        }
    }

    #[test]
    fn parse_response_emits_nothing_on_late_missing_name() {
        let sink = RecordingSink {
            tool_calls: std::sync::Mutex::new(vec![]),
        };
        let body = r#"{"choices":[{"message":{"tool_calls":[
            {"id":"a","type":"function","function":{"name":"good","arguments":"{}"}},
            {"id":"b","type":"function","function":{"arguments":"{}"}}
        ]},"finish_reason":"tool_calls"}]}"#;
        assert!(parse_response(body, &sink).is_err());
        assert!(sink.tool_calls.lock().unwrap().is_empty());
    }

    #[test]
    fn usage_does_not_double_count_cached_tokens() {
        let body = r#"{"choices":[{"message":{"content":"hi"},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":40},"completion_tokens":5,"total_tokens":105}}"#;
        let turn = parse_response(body, &NoopModelSink).unwrap();
        let u = turn.usage.expect("usage present");
        assert_eq!(u.input_tokens, 100);
        assert_eq!(u.total_tokens, 105);
    }
}
