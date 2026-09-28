//! Anthropic Messages adapter (spec §5): different wire shape, same
//! normalized `ModelEvent`s as the OpenAI-compat adapter.
//!
//! Mapping: system prompt rides top-level `system`; tools use
//! `input_schema`; assistant tool calls become `tool_use` content blocks;
//! tool results become `tool_result` blocks on user rows; streaming speaks
//! Anthropic's SSE event family (`message_start`, `content_block_delta`,
//! `message_delta`, `message_stop`, `ping`, `error`).

use crate::http::{perr, AdapterTurn, ChatTransport, ToolChoice, TurnOptions, WireRequest};
use crate::model_event::{ModelEvent, ModelEventSink, ModelUsage};
use pantheon_agent::{ToolCall, TurnOutcome};
use pantheon_api::capability::Capability;
use pantheon_api::error::PantheonError;
use pantheon_api::message::{Message, Role, ToolSchema};
use std::collections::BTreeMap;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Build the wire request for one attempt.
// Ten params for the same reason as the OpenAI builder: explicit wire
// arguments at every call site beat a bundled struct nobody else uses.
#[allow(clippy::too_many_arguments)]
pub fn request(
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSchema],
    stream: bool,
    max_tokens: u32,
    reasoning: pantheon_api::model::ReasoningLevel,
    reasoning_budget: Option<u32>,
    opts: &TurnOptions,
) -> WireRequest {
    let system: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect();
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for m in messages.iter().filter(|m| m.role != Role::System) {
        // Provenance envelope: same convention as the OpenAI adapter. Rows
        // with untrusted or memory-tier provenance carry a prefix so the
        // model can tell fetched data from instructions.
        let content = match &m.provenance {
            Some(p) if p.trust.rank() <= pantheon_api::provenance::TrustTier::Memory.rank() => {
                format!("{} {}", p.envelope_prefix(), m.content)
            }
            _ => m.content.clone(),
        };
        let blocks: Vec<serde_json::Value> = match m.role {
            Role::Assistant => {
                let mut b = Vec::new();
                if !m.content.is_empty() {
                    b.push(serde_json::json!({"type": "text", "text": m.content}));
                }
                for c in &m.tool_calls {
                    let input = serde_json::from_str(&c.arguments)
                        .unwrap_or_else(|_| serde_json::json!({}));
                    b.push(serde_json::json!({
                        "type": "tool_use", "id": c.id, "name": c.name, "input": input
                    }));
                }
                b
            }
            Role::Tool => vec![serde_json::json!({
                "type": "tool_result",
                "tool_use_id": m.tool_call_id.clone().unwrap_or_else(|| "call_0".into()),
                "content": content,
            })],
            _ => vec![serde_json::json!({"type": "text", "text": content})],
        };
        let role = if m.role == Role::Assistant {
            "assistant"
        } else {
            "user"
        };
        // Anthropic wants alternating roles; merge consecutive same-role
        // rows (tool results are user rows and often stack up).
        if let Some(last) = rows.last_mut() {
            if last["role"] == role {
                if let Some(arr) = last["content"].as_array_mut() {
                    arr.extend(blocks);
                    continue;
                }
            }
        }
        rows.push(serde_json::json!({ "role": role, "content": blocks }));
    }
    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": rows,
    });
    // Reasoning effort maps to a thinking budget. An explicit budget
    // wins over the level mapping; otherwise levels map to fixed budgets
    // (`Minimal` shares the API-minimum floor rather than sending a value
    // the API rejects). The API requires 1024 <= budget < max_tokens;
    // when the window cannot satisfy that, the param is skipped rather
    // than sending a body the API rejects. `Off` with no budget leaves
    // the body exactly as before.
    let level_budget = match reasoning {
        pantheon_api::model::ReasoningLevel::Off => None,
        pantheon_api::model::ReasoningLevel::Minimal => Some(1024),
        pantheon_api::model::ReasoningLevel::Low => Some(4096),
        pantheon_api::model::ReasoningLevel::Medium => Some(10_000),
        pantheon_api::model::ReasoningLevel::High => Some(20_000),
        pantheon_api::model::ReasoningLevel::Xhigh => Some(32_000),
        // Dynamic: whatever the window allows. The guard below drops it
        // when the window is too small to hold any legal budget.
        pantheon_api::model::ReasoningLevel::Max => max_tokens.checked_sub(1),
    };
    // An explicit budget wins over the level mapping; an explicit zero
    // disables (it reads as "no budget", not as a 0-token thinking block
    // the API would reject, and not as "fall back to the level").
    let want = match reasoning_budget {
        Some(0) => None,
        Some(b) => Some(b),
        None => level_budget,
    };
    if let Some(want) = want {
        if want >= 1024 && want < max_tokens {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": want,
            });
        }
    }
    if !system.is_empty() {
        body["system"] = serde_json::json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(
            tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.parameters,
                    })
                })
                .collect(),
        );
    }
    // Structured output: Anthropic's GA shape is `output_config.format`
    // (the beta-era `output_format` is deprecated and needs no beta
    // header since Feb 2026). Constrained decoding guarantees schema
    // conformance; the JSON still arrives as normal text content blocks,
    // so parsing is unchanged. Opt-in only: absent schema → no field.
    if let Some(schema) = &opts.response_schema {
        body["output_config"] = serde_json::json!({
            "format": { "type": "json_schema", "schema": schema },
        });
    }
    // Tool choice, same gating as the OpenAI adapter: provider default
    // unless the turn says otherwise, and never without tools.
    if !tools.is_empty() {
        if let Some(choice) = tool_choice_value(&opts.tool_choice) {
            body["tool_choice"] = choice;
        }
    }
    if stream {
        body["stream"] = serde_json::Value::Bool(true);
    }
    WireRequest {
        url: format!("{}/messages", base_url.trim_end_matches('/')),
        headers: vec![
            ("x-api-key".into(), api_key.to_string()),
            ("anthropic-version".into(), ANTHROPIC_VERSION.into()),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: body.to_string(),
    }
}

fn err_from(v: &serde_json::Value) -> PantheonError {
    let etype = v
        .pointer("/error/type")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown")
        .to_string();
    let msg = v
        .pointer("/error/message")
        .and_then(|x| x.as_str())
        .unwrap_or("anthropic stream error")
        .to_string();
    // Transient classes are fallback-eligible; the rest are config problems.
    let retryable = matches!(etype.as_str(), "overloaded_error" | "rate_limit_error");
    perr("PROVIDER_STREAM", format!("{etype}: {msg}"), retryable)
}

fn map_stop(reason: &str) -> String {
    match reason {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        other => other.to_string(),
    }
}

/// Anthropic `tool_choice` wire value. `None` = omit the field (provider
/// default, `auto`). `Required` maps to `any` — the closest equivalent
/// to OpenAI's `required`: the model must call some tool this turn.
fn tool_choice_value(choice: &ToolChoice) -> Option<serde_json::Value> {
    match choice {
        ToolChoice::Auto => None,
        ToolChoice::Required => Some(serde_json::json!({"type": "any"})),
        ToolChoice::None => Some(serde_json::json!({"type": "none"})),
        ToolChoice::Named(name) => Some(serde_json::json!({"type": "tool", "name": name})),
    }
}

fn usage_of(v: &serde_json::Value) -> Option<ModelUsage> {
    let u = v.get("usage")?;
    // Anthropic bills prompt-cache reads and cache writes separately from
    // `input_tokens`: fold all three into the input total so a cached
    // turn doesn't undercount context or cost.
    let input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    let cache_read = u
        .get("cache_read_input_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let cache_write = u
        .get("cache_creation_input_tokens")
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let input = input + cache_read + cache_write;
    let output = u.get("output_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
    Some(ModelUsage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        cost_usd: None,
    })
}

/// Parse a single-shot Messages response into an `AdapterTurn`.
pub fn parse_response(body: &str, sink: &dyn ModelEventSink) -> Result<AdapterTurn, PantheonError> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| perr("PROVIDER_PARSE", e.to_string(), false))?;
    if v.get("type").and_then(|t| t.as_str()) == Some("error") {
        return Err(err_from(&v));
    }
    let content = v
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| perr("PROVIDER_PARSE", "no content in response".into(), false))?;
    let mut text = String::new();
    let mut out = Vec::new();
    for block in content {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(|x| x.as_str()) {
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = block
                    .get("id")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call_{}", out.len()));
                let name = block
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = block
                    .get("input")
                    .map(|i| i.to_string())
                    .unwrap_or_else(|| "{}".into());
                if name.is_empty() {
                    return Err(perr(
                        "PROVIDER_PARSE",
                        "tool_use block missing name".into(),
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
            _ => {}
        }
    }
    let usage = usage_of(&v);
    let finish_reason = v.get("stop_reason").and_then(|x| x.as_str()).map(map_stop);
    let tokens = usage.as_ref().map(|u| u.total_tokens as u32).unwrap_or(0);
    let cost_cents = usage
        .as_ref()
        .and_then(|u| u.cost_usd)
        .map(|c| (c * 100.0) as u32)
        .unwrap_or(0);
    if !text.is_empty() {
        sink.emit(ModelEvent::TextDelta { text: text.clone() });
    }
    let outcome = if out.is_empty() {
        TurnOutcome::Text {
            text,
            tokens,
            cost_cents,
        }
    } else {
        TurnOutcome::Tools {
            calls: out,
            tokens,
            cost_cents,
        }
    };
    Ok(AdapterTurn {
        outcome,
        usage,
        finish_reason,
    })
}

/// Streaming state: accumulates Anthropic SSE event payloads into one turn.
#[derive(Default)]
pub struct AnthStream {
    text: String,
    tools: BTreeMap<u32, (String, String, String)>, // block index -> (id, name, partial json)
    usage_in: Option<u64>,
    usage_out: Option<u64>,
    finish_reason: Option<String>,
}

impl AnthStream {
    /// Feed one SSE `data:` payload (its `type` field selects the handler).
    pub fn push(&mut self, payload: &str, sink: &dyn ModelEventSink) -> Result<(), PantheonError> {
        let v: serde_json::Value = serde_json::from_str(payload)
            .map_err(|e| perr("PROVIDER_PARSE", format!("stream chunk: {e}"), false))?;
        match v.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                // Same cache accounting as `usage_of`: the stream's opening
                // usage block carries the cached variants separately.
                let input = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let cache_read = v
                    .pointer("/message/usage/cache_read_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                let cache_write = v
                    .pointer("/message/usage/cache_creation_input_tokens")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0);
                self.usage_in = Some(input + cache_read + cache_write);
            }
            Some("content_block_start") => {
                let block = v.get("content_block");
                if block.and_then(|b| b.get("type")).and_then(|t| t.as_str()) == Some("tool_use") {
                    let idx = v
                        .get("content_block_index")
                        .and_then(|i| i.as_u64())
                        .unwrap_or(0) as u32;
                    let id = block
                        .and_then(|b| b.get("id"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .and_then(|b| b.get("name"))
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    self.tools.insert(idx, (id, name, String::new()));
                }
            }
            Some("content_block_delta") => {
                let idx = v
                    .get("content_block_index")
                    .and_then(|i| i.as_u64())
                    .unwrap_or(0) as u32;
                match v.pointer("/delta/type").and_then(|t| t.as_str()) {
                    Some("text_delta") => {
                        if let Some(t) = v.pointer("/delta/text").and_then(|x| x.as_str()) {
                            if !t.is_empty() {
                                self.text.push_str(t);
                                sink.emit(ModelEvent::TextDelta {
                                    text: t.to_string(),
                                });
                            }
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(t) = v.pointer("/delta/thinking").and_then(|x| x.as_str()) {
                            if !t.is_empty() {
                                sink.emit(ModelEvent::ReasoningDelta {
                                    text: t.to_string(),
                                });
                            }
                        }
                    }
                    Some("input_json_delta") => {
                        let entry = self
                            .tools
                            .entry(idx)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(p) = v.pointer("/delta/partial_json").and_then(|x| x.as_str()) {
                            entry.2.push_str(p);
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                if let Some(r) = v.pointer("/delta/stop_reason").and_then(|x| x.as_str()) {
                    self.finish_reason = Some(map_stop(r));
                }
                if let Some(o) = v.pointer("/usage/output_tokens").and_then(|x| x.as_u64()) {
                    self.usage_out = Some(o);
                }
            }
            Some("message_stop") => {}
            Some("ping") => {}
            Some("error") => return Err(err_from(&v)),
            _ => {}
        }
        Ok(())
    }

    /// Assemble the final turn. Emits ToolCall events for completed blocks.
    pub fn finish(self, sink: &dyn ModelEventSink) -> Result<AdapterTurn, PantheonError> {
        let usage = match (self.usage_in, self.usage_out) {
            (None, None) => None,
            (i, o) => {
                let input = i.unwrap_or(0);
                let output = o.unwrap_or(0);
                Some(ModelUsage {
                    input_tokens: input,
                    output_tokens: output,
                    total_tokens: input + output,
                    cost_usd: None,
                })
            }
        };
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
                    format!("streamed tool_use block {idx} missing name"),
                    false,
                ));
            }
            let args = if args.is_empty() {
                "{}".to_string()
            } else {
                args
            };
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

/// One streaming attempt: deltas emit while events arrive.
pub fn stream(
    transport: &dyn ChatTransport,
    req: WireRequest,
    sink: &dyn ModelEventSink,
) -> Result<AdapterTurn, PantheonError> {
    let mut state = AnthStream::default();
    transport.post_stream(&req, &mut |payload| state.push(payload, sink))?;
    state.finish(sink)
}

#[cfg(test)]
#[path = "anthropic_tests.rs"]
mod tests;
