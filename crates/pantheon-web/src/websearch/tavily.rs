//! Tavily-backed [`SearchProvider`]: one `POST /search` REST call, good
//! snippet quality, structured JSON. Sync via ureq, matching the sync tool
//! closures in `pantheon-tools`.
//!
//! The API key is handed to the constructor by the caller (resolved from the
//! secrets broker or the env by the parent). It is placed in the JSON request
//! body and never appears in any log line, error, or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{parse_tavily_response, SearchOptions, SearchProvider, SearchResult};

pub const DEFAULT_BASE_URL: &str = "https://api.tavily.com";

/// Tavily search provider.
pub struct TavilyProvider {
    api_key: String,
    base_url: String,
}

impl TavilyProvider {
    /// Production constructor: default Tavily endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the API base URL (e.g. a
    /// local stub server). The path is still `/search`.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    /// The base URL in use (without the `/search` path).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for TavilyProvider {
    fn name(&self) -> &str {
        "tavily"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let mut body = serde_json::json!({
            "api_key": self.api_key,
            "query": query,
            "search_depth": "basic",
            "max_results": opts.max_results,
            "include_answer": false,
            "include_raw_content": false,
        });
        if !opts.include_domains.is_empty() {
            body["include_domains"] = serde_json::json!(opts.include_domains);
        }
        if !opts.exclude_domains.is_empty() {
            body["exclude_domains"] = serde_json::json!(opts.exclude_domains);
        }
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        // The request body carries the API key: the shared core reports the
        // status plus a truncated slice of the response body, never the key.
        let text = http::post_json("tavily", &url, &[], body)?;
        parse_tavily_response(&text)
    }
}
