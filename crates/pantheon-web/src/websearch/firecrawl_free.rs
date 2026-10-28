//! Anonymous Firecrawl search via its public cloud API.
//!
//! Keyless: `POST https://api.firecrawl.dev/v2/search` with
//! `{"query","limit"}` and no auth header. The public endpoint runs on
//! a shared quota (expect throttling), so this row is a fallback, never
//! the recommended default. The response shape matches the keyed
//! Firecrawl API, so the shared [`parse_firecrawl_response`] parser
//! reads it.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default public API base URL (no trailing path).
pub const DEFAULT_BASE_URL: &str = "https://api.firecrawl.dev";

/// Anonymous Firecrawl search provider. Keyless - no constructor key.
pub struct FirecrawlFreeProvider {
    base_url: String,
}

impl FirecrawlFreeProvider {
    /// Production constructor: the public keyless endpoint.
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the API base URL.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for FirecrawlFreeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchProvider for FirecrawlFreeProvider {
    fn name(&self) -> &str {
        "firecrawl_free"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let url = format!("{}/v2/search", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "query": query,
            "limit": opts.max_results.max(1),
        });
        let text = http::post_json("firecrawl_free", &url, &[], body)?;
        parse_firecrawl_free_response(&text)
    }
}

/// Parse the keyless `/v2/search` envelope. Pure and unit-testable.
/// Unlike the keyed API (`data` array), the public endpoint nests hits
/// under `data.web` (falling back to `data.results`, then a bare `data`
/// array, then top-level `web`/`results`). A hit without a usable URL
/// is skipped; no list anywhere is a `BadResponse`.
pub fn parse_firecrawl_free_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let data = v.get("data");
    // Fall through on an empty list, not just on a missing key:
    // `data.web: []` must not block the `data.results` fallback the
    // docstring promises (or_else only fires on None).
    let candidates = [
        data.and_then(|d| d.get("web")),
        data.and_then(|d| d.get("results")),
        data.filter(|d| d.is_array()),
        v.get("web"),
        v.get("results"),
    ];
    let arrays: Vec<&Vec<serde_json::Value>> = candidates
        .into_iter()
        .flatten()
        .filter_map(|r| r.as_array())
        .collect();
    // First non-empty list wins; if every present list is empty the
    // result is genuinely empty, not a malformed envelope.
    let list = arrays
        .iter()
        .copied()
        .find(|a| !a.is_empty())
        .or_else(|| arrays.first().copied())
        .ok_or_else(|| SearchError::BadResponse("response has no result list".to_string()))?;
    let mut out = Vec::with_capacity(list.len());
    for r in list {
        let Some(url) = opt_str(r, "url") else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        let meta = r.get("metadata");
        out.push(SearchResult {
            title: opt_str(r, "title").unwrap_or_default(),
            url,
            snippet: meta
                .and_then(|m| opt_str(m, "description"))
                .or_else(|| opt_str(r, "description"))
                .unwrap_or_default(),
            published: meta
                .and_then(|m| opt_str(m, "publishedTime").or_else(|| opt_str(m, "modifiedTime"))),
            score: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_live_data_web_shape() {
        let body = r#"{"success":true,"data":{"web":[
            {"url":"https://www.rust-lang.org/en-US","title":"Rust Programming Language","description":"blazingly fast","position":1},
            {"url":"","title":"skip"}
        ]}}"#;
        let hits = parse_firecrawl_free_response(body).expect("parse");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "blazingly fast");
    }

    #[test]
    fn empty_web_falls_through_to_results() {
        let body = r#"{"data":{"web":[],"results":[{"url":"https://a.example","title":"A"}]}}"#;
        let hits = parse_firecrawl_free_response(body).expect("parse");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://a.example");
    }

    #[test]
    fn falls_back_across_shapes() {
        let body = r#"{"data":{"results":[{"url":"https://a.example","title":"A"}]}}"#;
        assert_eq!(parse_firecrawl_free_response(body).expect("parse").len(), 1);
        assert!(parse_firecrawl_free_response(r#"{"success":true}"#).is_err());
    }
}
