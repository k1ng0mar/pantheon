//! Anthropic Messages adapter (spec §5): different wire shape, same
//! normalized `ModelEvent`s as the OpenAI-compat adapter.
//!
//! Mapping: system prompt rides top-level `system`; tools use
//! `input_schema`; assistant tool calls become `tool_use` content blocks;
//! tool results become `tool_result` blocks on user rows; streaming speaks
//! Anthropic's SSE event family (`message_start`, `content_block_delta`,
//! `message_delta`, `message_stop`, `ping`, `error`).

use crate::http::{perr, AdapterTurn, ChatTransport, WireRequest};
use pantheon_agent::{ToolCall, TurnOutcome};
use pantheon_core::capability::Capability;
use pantheon_core::error::PantheonError;
use pantheon_core::message::{Message, Role, ToolSchema};
use pantheon_core::model_event::{ModelEvent, ModelEventSink, ModelUsage};
use std::collections::BTreeMap;

pub const ANTHROPIC_VERSION: &str = "2023-06-01";
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Build the wire request for one attempt.
pub fn request(
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolSchema],
    stream: bool,
    max_tokens: u32,
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
            Some(p) if p.trust.rank() <= pantheon_core::provenance::TrustTier::Memory.rank() => {
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

fn usage_of(v: &serde_json::Value) -> Option<ModelUsage> {
    let u = v.get("usage")?;
    let input = u.get("input_tokens").and_then(|x| x.as_u64()).unwrap_or(0);
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
    if !text.is_empty() {
        sink.emit(ModelEvent::TextDelta { text: text.clone() });
    }
    let outcome = if out.is_empty() {
        TurnOutcome::Text(text)
    } else {
        TurnOutcome::Tools(out)
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
                if let Some(u) = v.pointer("/message/usage/input_tokens") {
                    self.usage_in = u.as_u64();
                }
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
            return Ok(AdapterTurn {
                outcome: TurnOutcome::Text(self.text),
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
            outcome: TurnOutcome::Tools(out),
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
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Collect(RefCell<Vec<ModelEvent>>);
    impl ModelEventSink for Collect {
        fn emit(&self, event: ModelEvent) {
            self.0.borrow_mut().push(event);
        }
    }
    fn collector() -> Collect {
        Collect(RefCell::new(vec![]))
    }

    #[test]
    fn request_maps_system_tools_and_alternation() {
        let msgs = vec![
            Message::system("be brief"),
            Message::user("hi"),
            Message::assistant_tool_calls(vec![pantheon_core::message::ToolCallRef {
                id: "call_1".into(),
                name: "shell".into(),
                arguments: "{\"cmd\":\"ls\"}".into(),
            }]),
            Message::tool("call_1", "file.txt"),
            Message::user("thanks"),
        ];
        let tools = vec![ToolSchema {
            name: "shell".into(),
            description: "run".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let req = request(
            "https://api.anthropic.com/v1",
            "sk-test",
            "claude-sonnet-4",
            &msgs,
            &tools,
            false,
            1024,
        );
        assert!(req.url.ends_with("/messages"));
        assert!(req
            .headers
            .iter()
            .any(|(k, v)| k == "x-api-key" && v == "sk-test"));
        let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(v["system"], "be brief");
        assert_eq!(v["max_tokens"], 1024);
        assert_eq!(v["tools"][0]["name"], "shell");
        assert_eq!(v["tools"][0]["input_schema"]["type"], "object");
        // user -> assistant -> tool-result(user) merged with trailing user
        let roles: Vec<&str> = v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        // merged user row holds tool_result followed by the plain text row
        let user_content = &v["messages"][2]["content"];
        assert_eq!(user_content[0]["type"], "tool_result");
        assert_eq!(user_content[1]["type"], "text");
        assert_eq!(v["stream"], serde_json::Value::Null);
    }

    #[test]
    fn response_parses_text_and_usage() {
        let body = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 7, "output_tokens": 4}
        })
        .to_string();
        let c = collector();
        let turn = parse_response(&body, &c).unwrap();
        assert_eq!(turn.outcome, TurnOutcome::Text("hello".into()));
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
        let u = turn.usage.unwrap();
        assert_eq!((u.input_tokens, u.total_tokens), (7, 11));
        assert!(matches!(
            c.0.borrow()[0],
            ModelEvent::TextDelta { ref text } if text == "hello"
        ));
    }

    #[test]
    fn response_parses_tool_use() {
        let body = serde_json::json!({
            "content": [
                {"type": "text", "text": "running it"},
                {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {"cmd": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 2}
        })
        .to_string();
        let c = collector();
        match parse_response(&body, &c).unwrap().outcome {
            TurnOutcome::Tools(calls) => {
                assert_eq!(calls[0].name, "shell");
                assert_eq!(calls[0].args, "{\"cmd\":\"ls\"}");
            }
            _ => panic!("expected tools"),
        }
        assert!(matches!(
            c.0.borrow()[0],
            ModelEvent::ToolCall { ref id, .. } if id == "toolu_1"
        ));
    }

    #[test]
    fn stream_assembles_text_thinking_and_usage() {
        let c = collector();
        let mut s = AnthStream::default();
        s.push(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":9}}}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"content_block_start","content_block_index":0,"content_block":{"type":"text","text":""}}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"content_block_delta","content_block_index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"content_block_delta","content_block_index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"content_block_stop","content_block_index":0}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}"#,
            &c,
        )
        .unwrap();
        s.push(r#"{"type":"message_stop"}"#, &c).unwrap();
        let turn = s.finish(&c).unwrap();
        assert_eq!(turn.outcome, TurnOutcome::Text("Hi".into()));
        assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
        let u = turn.usage.unwrap();
        assert_eq!(
            (u.input_tokens, u.output_tokens, u.total_tokens),
            (9, 5, 14)
        );
        let evs = c.0.borrow();
        assert!(evs
            .iter()
            .any(|e| matches!(e, ModelEvent::ReasoningDelta { .. })));
        assert!(evs.iter().any(|e| matches!(
            e,
            ModelEvent::TextDelta { ref text } if text == "Hi"
        )));
    }

    #[test]
    fn stream_assembles_tool_use_from_partial_json() {
        let c = collector();
        let mut s = AnthStream::default();
        s.push(
            r#"{"type":"content_block_start","content_block_index":0,"content_block":{"type":"tool_use","id":"toolu_7","name":"shell"}}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"content_block_delta","content_block_index":0,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":"}}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"content_block_delta","content_block_index":0,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"content_block_stop","content_block_index":0}"#,
            &c,
        )
        .unwrap();
        s.push(
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            &c,
        )
        .unwrap();
        match s.finish(&c).unwrap().outcome {
            TurnOutcome::Tools(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].args, "{\"cmd\":\"ls\"}");
            }
            _ => panic!("expected tools"),
        }
    }

    #[test]
    fn stream_error_event_is_structured() {
        let c = collector();
        let mut s = AnthStream::default();
        let err = s
            .push(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"busy"}}"#,
                &c,
            )
            .unwrap_err();
        assert_eq!(err.code, "PROVIDER_STREAM");
        assert!(err.retryable, "overloaded must be fallback-eligible");
    }
}
