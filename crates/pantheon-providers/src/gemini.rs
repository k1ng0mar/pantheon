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
