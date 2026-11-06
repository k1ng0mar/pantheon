//! Provider adapters: anthropic, openai, gemini in one file.

pub mod anthropic {
    //! Anthropic Messages adapter (spec §5): different wire shape, same
    //! normalized `ModelEvent`s as the OpenAI-compat adapter.
    //!
    //! Mapping: system prompt rides top-level `system`; tools use
    //! `input_schema`; assistant tool calls become `tool_use` content blocks;
    //! tool results become `tool_result` blocks on user rows; streaming speaks
    //! Anthropic's SSE event family (`message_start`, `content_block_delta`,
    //! `message_delta`, `message_stop`, `ping`, `error`).

    use crate::http::{
        perr, AdapterTurn, ChatTransport, StreamEnd, ToolChoice, TurnOptions, WireRequest,
    };
    use crate::model_event::{ModelEvent, ModelEventSink, ModelUsage};
    use pantheon_agent::{ToolCall, TurnOutcome};
    use pantheon_api::capability::Capability;
    use pantheon_api::error::PantheonError;
    use pantheon_api::message::{Message, Role, ToolSchema};
    use std::collections::BTreeMap;

    pub const ANTHROPIC_VERSION: &str = "2023-06-01";
    pub const DEFAULT_MAX_TOKENS: u32 = 4096;

    /// Tools whose presence depends on the turn's caller, not on the
    /// conversation: the collaboration/swarm tools `register_turn_tools`
    /// gates on the session's agent profile, policy, and reviewer flag.
    /// Their set can appear, disappear, or grow between turns of the same
    /// parent session (a profile attaches, a swarm reviewer spawns, a
    /// delegation ends) while the parent's own context is unchanged.
    pub const CALLER_TOOLS: &[&str] = &[
        "agent_inbox",
        "agent_message",
        "delegate",
        "delegate_async",
        "subagent_list",
        "subagent_read",
        "subagent_wait",
        "verdict",
    ];

    /// Split a tool list into the cacheable prefix and the caller-specific
    /// tail, each sorted by name so registry (HashMap) order can never churn
    /// the wire order. The first element is the stable set, the second the
    /// caller-specific set.
    fn split_tools(tools: &[ToolSchema]) -> (Vec<&ToolSchema>, Vec<&ToolSchema>) {
        let mut stable: Vec<&ToolSchema> = Vec::new();
        let mut caller: Vec<&ToolSchema> = Vec::new();
        for t in tools {
            if CALLER_TOOLS.contains(&t.name.as_str()) {
                caller.push(t);
            } else {
                stable.push(t);
            }
        }
        stable.sort_by(|a, b| a.name.cmp(&b.name));
        caller.sort_by(|a, b| a.name.cmp(&b.name));
        (stable, caller)
    }

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
        // System rows ride top-level `system`, which only takes text: image
        // parts on a system row degrade to explicit `[image: ...]` notes (the
        // pictures themselves ride on user rows in practice).
        let system: Vec<String> = messages
            .iter()
            .filter(|m| m.role == Role::System)
            .map(|m| {
                if m.images.is_empty() {
                    m.content.clone()
                } else {
                    m.text_with_image_notes()
                }
            })
            .collect();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        for m in messages.iter().filter(|m| m.role != Role::System) {
            // Provenance envelope: same convention as the OpenAI adapter. Rows
            // with untrusted or memory-tier provenance carry a prefix so the
            // model can tell fetched data from instructions.
            let text = match &m.provenance {
                Some(p) if p.trust.rank() <= pantheon_api::provenance::TrustTier::Memory.rank() => {
                    format!("{} {}", p.envelope_prefix(), m.content)
                }
                _ => m.content.clone(),
            };
            // Anthropic image block for one image part.
            let image_block = |img: &pantheon_api::message::ImagePart| {
                serde_json::json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": img.mime,
                        "data": img.data,
                    },
                })
            };
            let blocks: Vec<serde_json::Value> = match m.role {
                Role::Assistant => {
                    let mut b = Vec::new();
                    if !m.content.is_empty() {
                        b.push(serde_json::json!({"type": "text", "text": m.content}));
                    }
                    for img in &m.images {
                        b.push(image_block(img));
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
                // tool_result content is a string on this API: image parts on a
                // tool row degrade to explicit notes rather than vanishing.
                Role::Tool => {
                    let mut t = text;
                    for img in &m.images {
                        t.push('\n');
                        t.push_str(&img.note());
                    }
                    vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": m.tool_call_id.clone().unwrap_or_else(|| "call_0".into()),
                        "content": t,
                    })]
                }
                _ => {
                    let mut b = Vec::new();
                    // Text-only rows keep the unconditional text block
                    // (wire-identical to before); pictures-only rows skip the
                    // empty text block - bare image lists are valid here.
                    if !text.is_empty() || m.images.is_empty() {
                        b.push(serde_json::json!({"type": "text", "text": text}));
                    }
                    for img in &m.images {
                        b.push(image_block(img));
                    }
                    b
                }
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
        // Prompt caching: the system prompt and the tool definitions are re-sent
        // verbatim on every turn of a session, so they are the ideal cache
        // prefix. One breakpoint on the system block and one on the last tool
        // caches the whole static prefix (Anthropic caches everything up through
        // each marker); cache reads bill at roughly a tenth of base input, and
        // prefixes below the minimum cacheable length simply do not cache, so
        // there is no extra charge for short prompts. Default-on: no behavior
        // change, only cost.
        if !system.is_empty() {
            body["system"] = serde_json::json!([{
                "type": "text",
                "text": system.join("\n\n"),
                "cache_control": { "type": "ephemeral" },
            }]);
        }
        if !tools.is_empty() {
            // Context retention: only the stable tools sit inside the cached
            // prefix. The cache breakpoint goes on the last stable tool, and
            // caller-specific tools follow it uncached, in sorted order. A
            // caller-tool-only change (a delegation tool appears, a reviewer
            // tool goes away) therefore leaves the cached system + stable
            // prefix untouched instead of invalidating the whole prefix and
            // forcing a full re-read of the parent's context; the caller
            // tools themselves are simply re-sent, uncached, each turn. A
            // change to the stable set still invalidates the prefix, as
            // before, and never breaks the request.
            let (stable, caller) = split_tools(tools);
            let mut ordered = stable.clone();
            ordered.extend(caller);
            let n_stable = stable.len();
            body["tools"] = serde_json::Value::Array(
                ordered
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let mut tool = serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "input_schema": t.parameters,
                        });
                        if i + 1 == n_stable && n_stable > 0 {
                            tool["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                        }
                        tool
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
            use_sigv4: false,
            cancel: opts.cancel.clone(),
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
    /// default, `auto`). `Required` maps to `any` - the closest equivalent
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
    pub fn parse_response(
        body: &str,
        sink: &dyn ModelEventSink,
    ) -> Result<AdapterTurn, PantheonError> {
        let v: serde_json::Value =
            serde_json::from_str(body).map_err(|e| perr("PROVIDER_PARSE", e.to_string(), false))?;
        if v.get("type").and_then(|t| t.as_str()) == Some("error") {
            return Err(err_from(&v));
        }
        let content = v
            .get("content")
            .and_then(|c| c.as_array())
            .ok_or_else(|| perr("PROVIDER_PARSE", "no content in response".into(), false))?;
        let finish_reason = v.get("stop_reason").and_then(|x| x.as_str()).map(map_stop);
        let mut text = String::new();
        // Collect + validate everything first, THEN emit: emitting a
        // ToolCall event for an early block and failing on a later
        // missing-name block left a ledger tool card that a retry or
        // fallback would duplicate.
        let mut parsed: Vec<(String, String, String)> = Vec::new();
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
                        .unwrap_or_else(|| format!("call_{}", parsed.len()));
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
                    let args = crate::http::normalise_tool_args(&args, finish_reason.as_deref())?;
                    parsed.push((id, name, args));
                }
                _ => {}
            }
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
        let usage = usage_of(&v);
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
                text,
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

    /// Block index on the wire: Anthropic sends `index`, not
    /// `content_block_index`. Reading only the latter defaulted every
    /// block to 0, so a second `tool_use` overwrote the first in
    /// `tools[0]` and multi-tool streams silently dropped all but the
    /// last call. `content_block_index` stays as a fallback for
    /// gateways that rename the field.
    fn block_index(v: &serde_json::Value) -> u32 {
        v.get("index")
            .or_else(|| v.get("content_block_index"))
            .and_then(|i| i.as_u64())
            .unwrap_or(0) as u32
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
        pub fn push(
            &mut self,
            payload: &str,
            sink: &dyn ModelEventSink,
        ) -> Result<(), PantheonError> {
            // `[DONE]` is an OpenAI terminator, not an Anthropic event, but
            // a gateway may still send it: swallow it (trimmed) instead of
            // failing the whole stream with PROVIDER_PARSE.
            if payload.trim() == "[DONE]" {
                return Ok(());
            }
            let v: serde_json::Value = serde_json::from_str(payload.trim())
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
                    if block.and_then(|b| b.get("type")).and_then(|t| t.as_str())
                        == Some("tool_use")
                    {
                        let idx = block_index(&v);
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
                    let idx = block_index(&v);
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
                            if let Some(p) =
                                v.pointer("/delta/partial_json").and_then(|x| x.as_str())
                            {
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
            // With no `message_start` the input count is unknown, not zero:
            // reporting input_tokens=0 understated context and cost. Only
            // report usage when the opening block was seen.
            let usage = match (self.usage_in, self.usage_out) {
                (Some(i), o) => Some(ModelUsage {
                    input_tokens: i,
                    output_tokens: o.unwrap_or(0),
                    total_tokens: i + o.unwrap_or(0),
                    cost_usd: None,
                }),
                (None, _) => None,
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
            // Validate everything before emitting anything (see
            // parse_response): a late missing-name error must not leave
            // early ToolCall events behind for a retry to duplicate.
            let mut parsed: Vec<(String, String, String)> = Vec::with_capacity(self.tools.len());
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

    /// One streaming attempt: deltas emit while events arrive.
    pub fn stream(
        transport: &dyn ChatTransport,
        req: WireRequest,
        sink: &dyn ModelEventSink,
    ) -> Result<AdapterTurn, PantheonError> {
        let mut state = AnthStream::default();
        let end = transport.post_stream(&req, &mut |payload| state.push(payload, sink))?;
        if state.finish_reason.is_none() {
            // No stop_reason on ANY stream end: a terminator without one
            // is not a completed turn either — a proxy or provider can
            // end the stream after cutting it short, and accepting it
            // would present truncated output as finished. Fail retryable
            // so the chain retries or falls back instead of presenting a
            // silently truncated turn as complete.
            let cause = match end {
                StreamEnd::Done => "stream ended with terminator but no stop_reason".to_string(),
                StreamEnd::Eof => "stream ended without terminator and no stop_reason".to_string(),
            };
            return Err(perr("PROVIDER_TRUNCATED", cause, true));
        }
        state.finish(sink)
    }

    #[cfg(test)]
    mod anthropic_max_tokens_tests {
        use super::*;
        use crate::http::TurnOptions;

        /// Item 4: the resolved per-request output cap reaches the Anthropic
        /// wire body as `max_tokens` (this adapter already sent one; the
        /// test pins that the *resolved* value - not the old unwrap_or
        /// is what goes out).
        #[test]
        fn request_body_carries_resolved_max_tokens() {
            let req = request(
                "https://api.example.test/v1",
                "k",
                "test-model",
                &[],
                &[],
                false,
                4_321,
                pantheon_api::model::ReasoningLevel::Off,
                None,
                &TurnOptions::default(),
            );
            let body: serde_json::Value = serde_json::from_str(&req.body).expect("body is JSON");
            assert_eq!(body["max_tokens"], serde_json::json!(4_321));
        }
    }

    #[cfg(test)]
    mod stream_truncation_tests {
        use super::*;
        use crate::model_event::NoopModelSink;

        /// Scripted transport: feeds `payloads` to the callback, then ends
        /// with `[DONE]` (`StreamEnd::Done`) or bare EOF (`StreamEnd::Eof`),
        /// mirroring the real transport.
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
                    // Anthropic never emits `[DONE]` (its stream ends with
                    // `message_stop`); a scripted Done just ends here.
                    Ok(StreamEnd::Done)
                } else {
                    Ok(StreamEnd::Eof)
                }
            }
        }

        fn wire_req() -> WireRequest {
            WireRequest {
                url: "https://api.example.test/v1/messages".into(),
                headers: vec![],
                body: "{}".into(),
                use_sigv4: false,
                cancel: None,
            }
        }

        const TEXT_EVENT: &str = r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#;
        const STOP_EVENT: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#;

        #[test]
        fn eof_without_done_and_no_stop_reason_is_truncated() {
            let t = StubTransport {
                payloads: vec![TEXT_EVENT],
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
        fn eof_with_stop_reason_is_accepted() {
            let t = StubTransport {
                payloads: vec![TEXT_EVENT, STOP_EVENT],
                done: false,
            };
            let turn = stream(&t, wire_req(), &NoopModelSink).unwrap();
            assert!(matches!(turn.outcome, TurnOutcome::Text { .. }));
            assert_eq!(turn.finish_reason.as_deref(), Some("stop"));
        }

        #[test]
        fn done_terminator_still_accepted() {
            let t = StubTransport {
                payloads: vec![TEXT_EVENT, STOP_EVENT],
                done: true,
            };
            assert!(stream(&t, wire_req(), &NoopModelSink).is_ok());
        }

        #[test]
        fn done_without_stop_reason_is_truncated() {
            // A terminator with no stop_reason is not a completed turn:
            // a proxy can end the stream after cutting it short, and
            // accepting it would present truncated output as finished.
            let t = StubTransport {
                payloads: vec![TEXT_EVENT],
                done: true,
            };
            let err = match stream(&t, wire_req(), &NoopModelSink) {
                Ok(_) => panic!("expected PROVIDER_TRUNCATED on terminator without stop_reason"),
                Err(e) => e,
            };
            assert_eq!(err.code, "PROVIDER_TRUNCATED");
            assert!(err.retryable, "truncation must be retryable");
        }
    }

    #[cfg(test)]
    mod context_retention_tests {
        use super::*;
        use crate::http::TurnOptions;

        fn schema(name: &str) -> ToolSchema {
            ToolSchema {
                name: name.to_string(),
                description: format!("{name} tool"),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
            }
        }

        fn body_for(messages: &[Message], tools: &[ToolSchema]) -> serde_json::Value {
            let req = request(
                "https://api.example.test/v1",
                "k",
                "test-model",
                messages,
                tools,
                false,
                4096,
                pantheon_api::model::ReasoningLevel::Off,
                None,
                &TurnOptions::default(),
            );
            serde_json::from_str(&req.body).expect("body is JSON")
        }

        fn tool_names(body: &serde_json::Value) -> Vec<String> {
            body["tools"]
                .as_array()
                .map(|ts| {
                    ts.iter()
                        .map(|t| t["name"].as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default()
        }

        fn cached_names(body: &serde_json::Value) -> Vec<String> {
            body["tools"]
                .as_array()
                .map(|ts| {
                    ts.iter()
                        .filter(|t| t.get("cache_control").is_some())
                        .map(|t| t["name"].as_str().unwrap_or("").to_string())
                        .collect()
                })
                .unwrap_or_default()
        }

        #[test]
        fn caller_tool_change_keeps_stable_prefix_and_messages() {
            let messages = vec![Message::user("parent work in progress")];
            let before = body_for(
                &messages,
                &[schema("shell"), schema("delegate"), schema("read")],
            );
            let after = body_for(
                &messages,
                &[
                    schema("shell"),
                    schema("delegate"),
                    schema("agent_message"),
                    schema("read"),
                ],
            );
            // The parent's own context is untouched.
            assert_eq!(before["messages"], after["messages"]);
            // Stable tools, in order, with the breakpoint on the last one.
            assert_eq!(tool_names(&before), vec!["read", "shell", "delegate"]);
            assert_eq!(cached_names(&before), vec!["shell"]);
            assert_eq!(cached_names(&after), vec!["shell"]);
            // Caller tools ride after the breakpoint, uncached.
            assert_eq!(
                tool_names(&after),
                vec!["read", "shell", "agent_message", "delegate"]
            );
            let after_tools = after["tools"].as_array().unwrap();
            for t in &after_tools[2..] {
                assert!(
                    t.get("cache_control").is_none(),
                    "caller tool {} must not carry the cache breakpoint",
                    t["name"]
                );
            }
        }

        #[test]
        fn tool_order_is_deterministic_regardless_of_registry_order() {
            let messages = vec![Message::user("hi")];
            let a = body_for(
                &messages,
                &[schema("shell"), schema("verdict"), schema("read")],
            );
            let b = body_for(
                &messages,
                &[schema("read"), schema("shell"), schema("verdict")],
            );
            assert_eq!(a["tools"], b["tools"]);
            assert_eq!(tool_names(&a), vec!["read", "shell", "verdict"]);
        }

        #[test]
        fn genuine_message_change_changes_body_messages() {
            // Retention must not become stale-context pinning: new parent
            // content still reaches the provider verbatim.
            let tools = [schema("shell")];
            let before = body_for(&[Message::user("first")], &tools);
            let after = body_for(&[Message::user("second, after child finished")], &tools);
            assert_ne!(before["messages"], after["messages"]);
        }

        #[test]
        fn stable_tool_change_changes_the_cached_prefix() {
            let messages = vec![Message::user("hi")];
            let before = body_for(&messages, &[schema("shell"), schema("delegate")]);
            let after = body_for(
                &messages,
                &[schema("shell"), schema("read"), schema("delegate")],
            );
            assert_ne!(before["tools"], after["tools"]);
            assert_eq!(cached_names(&after), vec!["shell"]);
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
        fn multi_tool_stream_keeps_blocks_by_wire_index() {
            let mut s = AnthStream::default();
            let sink = NoopModelSink;
            for p in [
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_a","name":"first"}}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_b","name":"second"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"y\":2}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}"#,
                r#"{"type":"message_start","message":{"usage":{"input_tokens":10}}}"#,
            ] {
                s.push(p, &sink).unwrap();
            }
            let turn = s.finish(&sink).unwrap();
            match turn.outcome {
                TurnOutcome::Tools { calls, .. } => {
                    assert_eq!(calls.len(), 2);
                    assert_eq!(calls[0].name, "first");
                    assert_eq!(calls[0].args, "{\"x\":1}");
                    assert_eq!(calls[1].name, "second");
                    assert_eq!(calls[1].args, "{\"y\":2}");
                }
                other => panic!("expected Tools, got {other:?}"),
            }
        }

        #[test]
        fn content_block_index_fallback_still_works() {
            let mut s = AnthStream::default();
            let sink = NoopModelSink;
            s.push(
            r#"{"type":"content_block_start","content_block_index":3,"content_block":{"type":"tool_use","id":"c","name":"only"}}"#,
            &sink,
        )
        .unwrap();
            s.push(
            r#"{"type":"content_block_delta","content_block_index":3,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            &sink,
        )
        .unwrap();
            let turn = s.finish(&sink).unwrap();
            assert!(matches!(turn.outcome, TurnOutcome::Tools { .. }));
        }

        #[test]
        fn done_terminator_swallowed_with_whitespace() {
            let mut s = AnthStream::default();
            let sink = NoopModelSink;
            assert!(s.push(" [DONE] ", &sink).is_ok());
            assert!(s.push("[DONE]", &sink).is_ok());
        }

        #[test]
        fn parse_response_emits_nothing_on_late_missing_name() {
            let sink = RecordingSink {
                tool_calls: std::sync::Mutex::new(vec![]),
            };
            let body = r#"{"type":"message","content":[
            {"type":"tool_use","id":"a","name":"good","input":{}},
            {"type":"tool_use","id":"b","input":{}}
        ],"stop_reason":"tool_use"}"#;
            assert!(parse_response(body, &sink).is_err());
            assert!(
                sink.tool_calls.lock().unwrap().is_empty(),
                "no ToolCall event may precede a parse failure"
            );
        }

        #[test]
        fn streamed_empty_args_normalise_to_empty_object() {
            let mut s = AnthStream::default();
            let sink = NoopModelSink;
            s.push(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"c","name":"bare"}}"#,
            &sink,
        )
        .unwrap();
            let turn = s.finish(&sink).unwrap();
            match turn.outcome {
                TurnOutcome::Tools { calls, .. } => assert_eq!(calls[0].args, "{}"),
                other => panic!("expected Tools, got {other:?}"),
            }
        }

        #[test]
        fn missing_message_start_reports_no_usage_not_zero_input() {
            let mut s = AnthStream::default();
            let sink = NoopModelSink;
            s.push(
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}"#,
            &sink,
        )
        .unwrap();
            let turn = s.finish(&sink).unwrap();
            assert!(turn.usage.is_none(), "unknown input must not report 0");
        }
    }
}
pub mod openai {
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
    pub fn body_value(
        model: &str,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> serde_json::Value {
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
            cancel: opts.cancel.clone(),
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
    pub fn parse_response(
        body: &str,
        sink: &dyn ModelEventSink,
    ) -> Result<AdapterTurn, PantheonError> {
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
        pub fn push(
            &mut self,
            payload: &str,
            sink: &dyn ModelEventSink,
        ) -> Result<(), PantheonError> {
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
                cancel: None,
            }
        }

        const TEXT_CHUNK: &str =
            r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#;
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
}
pub mod gemini {
    //! Native Gemini adapter: generateContent shape, not OpenAI compat.
    //!
    //! This module only builds the wire request shape marker. Full streaming
    //! and tool call translation lives in the OpenAI compat path until a
    //! live Gemini key is available to verify against. The marker here
    //! proves the request is shaped for the native endpoint, and the
    //! catalog now has a distinct api_mode for it.

    use crate::http::{TurnOptions, WireRequest};

    pub fn request(
        base_url: &str,
        api_key: &str,
        model: &str,
        messages: &[pantheon_api::message::Message],
        _tools: &[pantheon_api::message::ToolSchema],
        stream: bool,
        max_tokens: u32,
        _opts: &TurnOptions,
    ) -> WireRequest {
        let _ = messages;
        let action = if stream {
            "streamGenerateContent"
        } else {
            "generateContent"
        };
        let _ = api_key;
        WireRequest {
            url: format!(
                "{}/models/{}:{action}?alt=sse",
                base_url.trim_end_matches('/'),
                model
            ),
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: format!(
                "{{\"contents\":[],\"generationConfig\":{{\"maxOutputTokens\":{}}}}}",
                max_tokens
            ),
            use_sigv4: false,
            cancel: _opts.cancel.clone(),
        }
    }

    pub fn parse_response(
        body: &str,
        sink: &dyn crate::model_event::ModelEventSink,
    ) -> Result<crate::http::AdapterTurn, pantheon_api::error::PantheonError> {
        use crate::http::{perr, AdapterTurn};
        use crate::model_event::{ModelEvent, ModelUsage};
        let v: serde_json::Value =
            serde_json::from_str(body).map_err(|e| perr("PROVIDER_PARSE", e.to_string(), false))?;
        let candidates = v
            .get("candidates")
            .and_then(|c| c.as_array())
            .ok_or_else(|| {
                perr(
                    "PROVIDER_PARSE",
                    "no candidates in Gemini response".into(),
                    false,
                )
            })?;
        let cand = candidates
            .first()
            .ok_or_else(|| perr("PROVIDER_PARSE", "empty candidates".into(), false))?;
        let parts = cand
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|p| p.as_array())
            .cloned()
            .unwrap_or_default();
        let mut text = String::new();
        let mut calls = Vec::new();
        for part in &parts {
            if let Some(t) = part.get("text").and_then(|x| x.as_str()) {
                text.push_str(t);
                sink.emit(ModelEvent::TextDelta {
                    text: t.to_string(),
                });
            }
            if let Some(fc) = part.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = fc
                    .get("args")
                    .map(|a| a.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                calls.push(pantheon_agent::ToolCall {
                    name,
                    capability: pantheon_api::capability::Capability::Other("gemini.tool".into()),
                    args,
                });
                sink.emit(ModelEvent::ToolCall {
                    id: String::new(),
                    name: calls.last().unwrap().name.clone(),
                    arguments: calls.last().unwrap().args.clone(),
                });
            }
        }
        let finish = cand
            .get("finishReason")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let usage = v.get("usageMetadata").map(|u| ModelUsage {
            input_tokens: u
                .get("promptTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            output_tokens: u
                .get("candidatesTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            total_tokens: u
                .get("totalTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0),
            cost_usd: None,
        });
        let tokens = usage.as_ref().map(|u| u.total_tokens as u32).unwrap_or(0);
        let outcome = if calls.is_empty() {
            pantheon_agent::TurnOutcome::Text {
                text,
                tokens,
                cost_cents: 0,
            }
        } else {
            pantheon_agent::TurnOutcome::Tools {
                calls,
                text,
                tokens,
                cost_cents: 0,
            }
        };
        Ok(AdapterTurn {
            outcome,
            usage,
            finish_reason: finish,
        })
    }

    pub fn complete(
        transport: &dyn crate::http::ChatTransport,
        req: WireRequest,
        sink: &dyn crate::model_event::ModelEventSink,
    ) -> Result<crate::http::AdapterTurn, pantheon_api::error::PantheonError> {
        let body = transport.post(&req)?;
        parse_response(&body, sink)
    }

    pub fn stream(
        transport: &dyn crate::http::ChatTransport,
        req: WireRequest,
        sink: &dyn crate::model_event::ModelEventSink,
    ) -> Result<crate::http::AdapterTurn, pantheon_api::error::PantheonError> {
        // Native streaming shares the SSE shape, but full translation needs a live key to verify.
        // Fail loud and let the chain fall back to the OpenAI compat google provider.
        let _ = (transport, req, sink);
        Err(crate::http::perr(
            "PROVIDER_NOT_IMPLEMENTED",
            "native Gemini streaming is not verified yet, use google openai provider".to_string(),
            false,
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn native_request_uses_generate_content_action() {
            let opts = TurnOptions::default();
            let req = request(
                "https://generativelanguage.googleapis.com/v1beta",
                "k",
                "gemini-2.5-flash",
                &[],
                &[],
                false,
                100,
                &opts,
            );
            assert!(req.url.contains("generateContent"));
            assert!(!req.url.contains("/openai"));
        }
    }
}
