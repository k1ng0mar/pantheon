//! OpenAI-compatible chat-completions adapter (spec §5 + §14).
//!
//! Wire format follows the OpenAI chat-completions shape (studied from
//! Hermes): messages system|user|assistant|tool, tools as
//! {"type":"function","function":{...}}, tool_calls on the assistant row,
//! tool results on role:"tool" rows keyed by tool_call_id.
//!
//! Both paths (single-shot and SSE) emit the same normalized `ModelEvent`s.
//! Policy events (Attempt/Usage/Completed/Fallback) belong to the chain.

use crate::http::{perr, AdapterTurn, ChatTransport, WireRequest};
use pantheon_agent::{ToolCall, TurnOutcome};
use pantheon_core::capability::Capability;
use pantheon_core::error::PantheonError;
use pantheon_core::message::{Message, Role, ToolSchema};
use pantheon_core::model_event::{ModelEvent, ModelEventSink, ModelUsage};
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
        let content = match &m.provenance {
            Some(p) if p.trust.rank() <= pantheon_core::provenance::TrustTier::Memory.rank() => {
                format!("{} {}", p.envelope_prefix(), m.content)
            }
            _ => m.content.clone(),
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
pub fn request(
    base_url: &str,
    api_key: &str,
    key_header: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSchema],
    stream: bool,
) -> WireRequest {
    let mut body = body_value(model, messages, tools);
    if stream {
        body["stream"] = serde_json::Value::Bool(true);
        // Ask for the trailing usage chunk (OpenAI standard); without it,
        // usage in streams is provider-dependent.
        body["stream_options"] = serde_json::json!({ "include_usage": true });
    }
    let auth_value = if key_header.trim().eq_ignore_ascii_case("authorization") {
        format!("Bearer {api_key}")
    } else {
        api_key.to_string()
    };
    let header_name = if key_header.trim().is_empty() {
        "Authorization".to_string()
    } else {
        key_header.trim().to_string()
    };
    WireRequest {
        url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
        headers: vec![
            (header_name, auth_value),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: body.to_string(),
    }
}

fn usage_of(v: &serde_json::Value) -> Option<ModelUsage> {
    let u = v.get("usage")?;
    if !u.is_null() {
        return Some(ModelUsage {
            input_tokens: u.get("prompt_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
            output_tokens: u
                .get("completion_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            total_tokens: u.get("total_tokens").and_then(|x| x.as_u64()).unwrap_or(0),
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
    let tcs = msg.get("tool_calls").and_then(|t| t.as_array());
    match tcs {
        Some(calls) if !calls.is_empty() => {
            let mut out = Vec::with_capacity(calls.len());
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
            let text = msg
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
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
        if payload == "[DONE]" {
            return Ok(());
        }
        let v: serde_json::Value = serde_json::from_str(payload)
            .map_err(|e| perr("PROVIDER_PARSE", format!("stream chunk: {e}"), false))?;
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
        let mut out = Vec::with_capacity(self.tools.len());
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
    transport.post_stream(&req, &mut |payload| state.push(payload, sink))?;
    state.finish(sink)
}

#[cfg(test)]
#[path = "openai_tests.rs"]
mod tests;
