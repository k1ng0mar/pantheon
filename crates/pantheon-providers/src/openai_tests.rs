//! Tests for `pantheon_providers::openai::tests` — sibling file so sources stay test-free.
use super::*;

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

#[test]
fn stream_flag_flips_body() {
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &[],
        true,
        pantheon_api::model::ReasoningLevel::Off,
        &TurnOptions::default(),
    );
    assert!(req.url.ends_with("/chat/completions"));
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["stream"], true);
    assert_eq!(v["stream_options"]["include_usage"], true);
    assert!(req.headers.iter().any(|(k, _)| k == "Authorization"));
}

#[test]
fn non_authorization_header_sends_the_raw_key() {
    // Xiaomi MiMo style: raw key in a vendor header, no Bearer prefix.
    let req = request(
        "http://x/v1",
        "k",
        "api-key",
        "m",
        &[Message::user("hi")],
        &[],
        false,
        pantheon_api::model::ReasoningLevel::Off,
        &TurnOptions::default(),
    );
    assert!(req.headers.iter().any(|(k, v)| k == "api-key" && v == "k"));
    assert!(!req.headers.iter().any(|(k, _)| k == "Authorization"));
}

struct Collect(std::cell::RefCell<Vec<ModelEvent>>);
impl ModelEventSink for Collect {
    fn emit(&self, event: ModelEvent) {
        self.0.borrow_mut().push(event);
    }
}
fn collector() -> Collect {
    Collect(std::cell::RefCell::new(vec![]))
}

#[test]
fn text_response_parses_and_emits() {
    let body = serde_json::json!({ "choices": [ { "message": { "role": "assistant", "content": "hi" }, "finish_reason": "stop" } ],
            "usage": { "prompt_tokens": 3, "completion_tokens": 2, "total_tokens": 5 } }).to_string();
    let c = collector();
    let turn = parse_response(&body, &c).unwrap();
    assert!(matches!(turn.outcome, TurnOutcome::Text { ref text, .. } if text == "hi"));
    assert_eq!(turn.usage.unwrap().total_tokens, 5);
    assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
    let evs = c.0.borrow();
    assert!(matches!(evs[0], ModelEvent::TextDelta { .. }));
}

#[test]
fn tool_calls_parse_and_emit() {
    let body = serde_json::json!({ "choices": [ { "message": { "role": "assistant", "content": "",
            "tool_calls": [ { "id": "call_1", "type": "function",
            "function": { "name": "shell", "arguments": "{\"cmd\":\"ls\"}" } } ] },
            "finish_reason": "tool_calls" } ] })
    .to_string();
    let c = collector();
    match parse_response(&body, &c).unwrap().outcome {
        TurnOutcome::Tools { calls, .. } => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].name, "shell");
            assert_eq!(calls[0].args, "{\"cmd\":\"ls\"}");
        }
        _ => panic!("expected tools"),
    }
    assert!(matches!(
        c.0.borrow()[0],
        ModelEvent::ToolCall { ref name, .. } if name == "shell"
    ));
}

#[test]
fn stream_assembles_text_from_chunks() {
    let c = collector();
    let mut s = OpenStream::default();
    s.push(
        r#"{"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        &c,
    )
    .unwrap();
    s.push(
        r#"{"choices":[{"delta":{"content":"Hello"},"finish_reason":null}]}"#,
        &c,
    )
    .unwrap();
    s.push(
        r#"{"choices":[{"delta":{"content":" there"},"finish_reason":null}]}"#,
        &c,
    )
    .unwrap();
    s.push(
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
            &c,
        )
        .unwrap();
    s.push("[DONE]", &c).unwrap();
    let turn = s.finish(&c).unwrap();
    assert!(matches!(turn.outcome, TurnOutcome::Text { ref text, .. } if text == "Hello there"));
    assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
    assert_eq!(turn.usage.unwrap().total_tokens, 3);
    let evs = c.0.borrow();
    let deltas: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["Hello", " there"]);
}

#[test]
fn stream_assembles_fragmented_tool_calls() {
    let c = collector();
    let mut s = OpenStream::default();
    s.push(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_9","function":{"name":"shell","arguments":""}}]},"finish_reason":null}]}"#,
            &c,
        )
        .unwrap();
    s.push(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"cmd\":"}}]},"finish_reason":null}]}"#,
            &c,
        )
        .unwrap();
    s.push(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":null}]}"#,
            &c,
        )
        .unwrap();
    s.push(
        r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
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
fn empty_content_is_empty_text() {
    let body = serde_json::json!({ "choices": [ { "message": { "role": "assistant", "content": null } } ] }).to_string();
    let c = collector();
    let turn = parse_response(&body, &c).unwrap();
    assert!(matches!(turn.outcome, TurnOutcome::Text { ref text, .. } if text.is_empty()));
    assert!(c.0.borrow().is_empty());
}

#[test]
fn reasoning_high_adds_effort_param() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &[],
        false,
        ReasoningLevel::High,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["reasoning_effort"], "high");
}

#[test]
fn reasoning_off_sends_no_effort_param() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &[],
        false,
        ReasoningLevel::Off,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(
        v.get("reasoning_effort").is_none(),
        "off is byte-identical to before"
    );
}

#[test]
fn reasoning_minimal_sends_gpt5_effort_string() {
    use pantheon_api::model::ReasoningLevel;
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &[],
        false,
        ReasoningLevel::Minimal,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["reasoning_effort"], "minimal");
}

fn opts_with(schema: serde_json::Value, choice: ToolChoice) -> TurnOptions {
    TurnOptions {
        response_schema: Some(schema),
        tool_choice: choice,
    }
}

fn some_tools() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "shell".into(),
        description: "run".into(),
        parameters: serde_json::json!({"type":"object","properties":{}}),
    }]
}

#[test]
fn response_schema_adds_json_schema_format() {
    let schema = serde_json::json!({
        "type": "object",
        "properties": {"city": {"type": "string"}},
        "required": ["city"],
    });
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &[],
        false,
        pantheon_api::model::ReasoningLevel::Off,
        &opts_with(schema.clone(), ToolChoice::Auto),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["response_format"]["type"], "json_schema");
    assert_eq!(v["response_format"]["json_schema"]["name"], "pantheon_structured");
    assert_eq!(v["response_format"]["json_schema"]["schema"], schema);
}

#[test]
fn no_schema_sends_no_response_format() {
    // Default options: byte-identical to before structured output existed.
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &[],
        false,
        pantheon_api::model::ReasoningLevel::Off,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(v.get("response_format").is_none());
}

#[test]
fn tool_choice_none_disables_tool_calls() {
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &some_tools(),
        false,
        pantheon_api::model::ReasoningLevel::Off,
        &TurnOptions {
            tool_choice: ToolChoice::None,
            ..Default::default()
        },
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert_eq!(v["tool_choice"], "none");
}

#[test]
fn tool_choice_required_and_named_map_to_openai_shapes() {
    let mk = |choice: ToolChoice| {
        let req = request(
            "http://x/v1",
            "k",
            "Authorization",
            "m",
            &[Message::user("hi")],
            &some_tools(),
            false,
            pantheon_api::model::ReasoningLevel::Off,
            &TurnOptions {
                tool_choice: choice,
                ..Default::default()
            },
        );
        serde_json::from_str::<serde_json::Value>(&req.body).unwrap()["tool_choice"].clone()
    };
    assert_eq!(mk(ToolChoice::Required)["type"], "required");
    let named = mk(ToolChoice::Named("shell".into()));
    assert_eq!(named["type"], "function");
    assert_eq!(named["function"]["name"], "shell");
}

#[test]
fn tool_choice_auto_omits_the_field() {
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &some_tools(),
        false,
        pantheon_api::model::ReasoningLevel::Off,
        &TurnOptions::default(),
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(v.get("tool_choice").is_none());
}

#[test]
fn tool_choice_omitted_when_there_are_no_tools() {
    // `required` with nothing to require must not reach the wire: the
    // provider would 400 on a dangling tool_choice.
    let req = request(
        "http://x/v1",
        "k",
        "Authorization",
        "m",
        &[Message::user("hi")],
        &[],
        false,
        pantheon_api::model::ReasoningLevel::Off,
        &TurnOptions {
            tool_choice: ToolChoice::Required,
            ..Default::default()
        },
    );
    let v: serde_json::Value = serde_json::from_str(&req.body).unwrap();
    assert!(v.get("tool_choice").is_none());
}

#[test]
fn usage_folds_cached_prompt_tokens_into_input() {
    let body = serde_json::json!({
        "choices": [{ "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop" }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "total_tokens": 110,
            "prompt_tokens_details": {"cached_tokens": 40, "audio_tokens": 0},
        },
    })
    .to_string();
    let c = collector();
    let turn = parse_response(&body, &c).unwrap();
    let usage = turn.usage.unwrap();
    assert_eq!(usage.input_tokens, 140, "prompt + cached");
    assert_eq!(usage.output_tokens, 10);
}

#[test]
fn usage_without_cache_details_is_unchanged() {
    let body = serde_json::json!({
        "choices": [{ "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop" }],
        "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110},
    })
    .to_string();
    let c = collector();
    let turn = parse_response(&body, &c).unwrap();
    assert_eq!(turn.usage.unwrap().input_tokens, 100);
}
