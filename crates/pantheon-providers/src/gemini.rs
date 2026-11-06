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
