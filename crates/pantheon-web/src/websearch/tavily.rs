//! Tavily `POST /search`. Good snippet quality, structured JSON. Sync via
//! ureq to match the sync tool closures in `pantheon-tools`.
//!
//! The key rides in the JSON request body and must never show up in a log,
//! error or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{parse_tavily_response, SearchOptions, SearchProvider, SearchResult};

pub const DEFAULT_BASE_URL: &str = "https://api.tavily.com";

pub struct TavilyProvider {
    api_key: String,
    base_url: String,
}

impl TavilyProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// local stub server / test mirror, still calls `/search`
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    /// base URL without `/search`
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
        // body carries the API key: shared core reports status + truncated
        // response slice, never the key
        let text = http::post_json("tavily", &url, &[], body)?;
        parse_tavily_response(&text)
    }
}
