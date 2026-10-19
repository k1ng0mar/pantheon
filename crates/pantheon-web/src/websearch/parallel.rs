//! Anonymous Parallel search via its public MCP endpoint.
//!
//! Keyless: `POST https://search.parallel.ai/mcp` with a JSON-RPC
//! `tools/call` for `web_search` (`objective`, `search_queries`,
//! `session_id`). No auth header. The public endpoint runs on a shared
//! quota (expect throttling), so this row is a fallback, never the
//! recommended default. The tool answers with JSON text shaped
//! `{results:[{url,title,excerpts[]}]}`.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default MCP endpoint URL.
pub const DEFAULT_BASE_URL: &str = "https://search.parallel.ai/mcp";

/// Anonymous Parallel search provider. Keyless - no constructor key.
pub struct ParallelFreeProvider {
    base_url: String,
}

impl ParallelFreeProvider {
    /// Production constructor: the public keyless endpoint.
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the endpoint URL.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for ParallelFreeProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Per-process correlation id for the free tier (rate limiting only,
/// never persisted). Timestamp + pid hex: unique enough without a uuid
/// dependency.
fn session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}-{:x}", nanos, std::process::id())
}

impl SearchProvider for ParallelFreeProvider {
    fn name(&self) -> &str {
        "parallel_free"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search",
                "arguments": {
                    "objective": query,
                    "search_queries": [query],
                    "session_id": session_id(),
                },
            },
        });
        let text = http::post_json(
            "parallel_free",
            self.base_url.trim_end_matches('/'),
            &[
                ("Accept", "application/json, text/event-stream"),
                ("Content-Type", "application/json"),
            ],
            body,
        )?;
        let payload = http::mcp_text_payload(&text)?;
        parse_parallel_results(&payload, opts.max_results)
    }
}

/// Parse Parallel's JSON result text. Pure and unit-testable. Hits live
/// at `results` with `url`/`title` and joined `excerpts`. A hit without
/// a usable URL is skipped; a missing or non-array `results` is a
/// `BadResponse`.
pub fn parse_parallel_results(body: &str, limit: u8) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| SearchError::BadResponse("response has no 'results' array".to_string()))?;
    let mut out = Vec::new();
    for r in results {
        if limit > 0 && out.len() >= limit as usize {
            break;
        }
        let Some(url) = opt_str(r, "url") else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        let excerpts = r
            .get("excerpts")
            .and_then(|e| e.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|e| e.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        out.push(SearchResult {
            title: opt_str(r, "title").unwrap_or_default(),
            url,
            snippet: excerpts,
            published: None,
            score: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_results_with_limit() {
        let body = r#"{"results":[
            {"url":"https://a.example","title":"A","excerpts":["one","two"]},
            {"url":"https://b.example","title":"B","excerpts":[]},
            {"url":"","title":"skip"}
        ]}"#;
        let hits = parse_parallel_results(body, 5).expect("parse");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].snippet, "one two");
        let capped = parse_parallel_results(body, 1).expect("parse");
        assert_eq!(capped.len(), 1);
    }

    #[test]
    fn mcp_envelope_plain_and_sse() {
        let plain = r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"results\":[]}"}]}}"#;
        assert_eq!(
            http::mcp_text_payload(plain).expect("plain"),
            r#"{"results":[]}"#
        );
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n\n";
        assert_eq!(http::mcp_text_payload(sse).expect("sse"), "hi");
    }

    #[test]
    fn mcp_errors_surface() {
        let rpc_err = r#"{"jsonrpc":"2.0","id":1,"error":{"message":"rate limited"}}"#;
        assert!(http::mcp_text_payload(rpc_err).is_err());
        let tool_err = r#"{"jsonrpc":"2.0","id":1,"result":{"isError":true,"content":[{"type":"text","text":"slow down"}]}}"#;
        let err = http::mcp_text_payload(tool_err).expect_err("isError");
        assert!(format!("{err:?}").contains("slow down"));
    }
}
