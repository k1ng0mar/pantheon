//! Tests for `pantheon_providers::anthropic::tests` — sibling file so sources stay test-free.
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
        Message::assistant_tool_calls(vec![pantheon_api::message::ToolCallRef {
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
        pantheon_api::model::ReasoningLevel::Off,
        None,
        &TurnOptions::default(),
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
    assert!(matches!(turn.outcome, TurnOutcome::Text { ref text, .. } if text == "hello"));
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
        TurnOutcome::Tools { calls, .. } => {
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
    assert!(matches!(turn.outcome, TurnOutcome::Text { ref text, .. } if text == "Hi"));
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
        TurnOutcome::Tools { calls, .. } => {
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

#[test]
fn reasoning_high_adds_thinking_budget() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        200_000,
        ReasoningLevel::High,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["thinking"]["type"], "enabled");
    assert_eq!(v["thinking"]["budget_tokens"], 20_000);
}

#[test]
fn reasoning_off_sends_no_thinking_block() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        200_000,
        ReasoningLevel::Off,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(
        v.get("thinking").is_none(),
        "off is byte-identical to before"
    );
}

#[test]
fn thinking_skipped_when_window_cannot_fit_budget() {
    use pantheon_api::model::ReasoningLevel;
    // max_tokens 1024 cannot satisfy 1024 <= budget < max_tokens, so the
    // param is skipped rather than sending a body the API rejects.
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        1024,
        ReasoningLevel::High,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(v.get("thinking").is_none());
}

#[test]
fn explicit_budget_overrides_the_level_mapping() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        200_000,
        ReasoningLevel::Low,
        Some(16_000),
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["thinking"]["budget_tokens"], 16_000);
}

#[test]
fn explicit_zero_budget_disables_thinking() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        200_000,
        ReasoningLevel::High,
        Some(0),
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(
        v.get("thinking").is_none(),
        "0 reads as no budget, not a 0-token block"
    );
}

#[test]
fn minimal_shares_the_api_minimum_floor() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        200_000,
        ReasoningLevel::Minimal,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["thinking"]["budget_tokens"], 1024);
}

#[test]
fn xhigh_takes_the_top_fixed_budget() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        200_000,
        ReasoningLevel::Xhigh,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["thinking"]["budget_tokens"], 32_000);
}

#[test]
fn max_fills_whatever_the_window_allows() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        200_000,
        ReasoningLevel::Max,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["thinking"]["budget_tokens"], 199_999);
}

#[test]
fn max_skipped_when_no_legal_budget_fits() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &[],
        false,
        1024,
        ReasoningLevel::Max,
        None,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(
        v.get("thinking").is_none(),
        "1023 is below the minimum and not < max"
    );
}

fn anth_tools() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "shell".into(),
        description: "run".into(),
        parameters: serde_json::json!({"type":"object"}),
    }]
}

fn anth_req(choice: ToolChoice, schema: Option<serde_json::Value>) -> serde_json::Value {
    let req = request(
        "https://api.anthropic.com/v1",
        "sk-test",
        "claude-sonnet-4",
        &[Message::user("hi")],
        &anth_tools(),
        false,
        1024,
        pantheon_api::model::ReasoningLevel::Off,
        None,
        &TurnOptions {
            response_schema: schema,
            tool_choice: choice,
        },
    );
    serde_json::from_str(&req.body).unwrap()
}

#[test]
fn response_schema_uses_output_config_format() {
    // Anthropic's GA structured-output shape (no beta header needed):
    // `output_config.format`, not OpenAI's `response_format`.
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"],
    });
    let v = anth_req(ToolChoice::Auto, Some(schema.clone()));
    assert_eq!(v["output_config"]["format"]["type"], "json_schema");
    assert_eq!(v["output_config"]["format"]["schema"], schema);
    assert!(v.get("response_format").is_none());
}

#[test]
fn no_schema_sends_no_output_config() {
    let v = anth_req(ToolChoice::Auto, None);
    assert!(v.get("output_config").is_none());
}

#[test]
fn tool_choice_maps_to_anthropic_shapes() {
    // `none` disables, `any` is the closest to OpenAI's `required`,
    // a named tool uses the `tool` variant.
    assert_eq!(anth_req(ToolChoice::None, None)["tool_choice"]["type"], "none");
    assert_eq!(
        anth_req(ToolChoice::Required, None)["tool_choice"]["type"],
        "any"
    );
    let named = anth_req(ToolChoice::Named("shell".into()), None)["tool_choice"].clone();
    assert_eq!(named["type"], "tool");
    assert_eq!(named["name"], "shell");
    // Auto omits the field entirely.
    assert!(anth_req(ToolChoice::Auto, None).get("tool_choice").is_none());
}

#[test]
fn usage_sums_cache_read_and_creation_tokens() {
    // Anthropic bills cache reads/writes separately from input_tokens:
    // all three fold into the input total.
    let body = serde_json::json!({
        "content": [{"type": "text", "text": "hello"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 100,
            "cache_read_input_tokens": 800,
            "cache_creation_input_tokens": 50,
            "output_tokens": 4,
        },
    })
    .to_string();
    let c = collector();
    let turn = parse_response(&body, &c).unwrap();
    let usage = turn.usage.unwrap();
    assert_eq!(usage.input_tokens, 950, "input + cache_read + cache_write");
    assert_eq!(usage.output_tokens, 4);
    assert_eq!(usage.total_tokens, 954);
}

#[test]
fn stream_usage_sums_cache_tokens_from_message_start() {
    let c = collector();
    let mut s = AnthStream::default();
    s.push(
        r#"{"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":800,"cache_creation_input_tokens":50}}}"#,
        &c,
    )
    .unwrap();
    s.push(
        r#"{"type":"content_block_delta","content_block_index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
        &c,
    )
    .unwrap();
    s.push(
        r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
        &c,
    )
    .unwrap();
    s.push(r#"{"type":"message_stop"}"#, &c).unwrap();
    let turn = s.finish(&c).unwrap();
    let usage = turn.usage.unwrap();
    assert_eq!(usage.input_tokens, 950);
    assert_eq!(usage.total_tokens, 954);
}
