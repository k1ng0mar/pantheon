//! Brave web search, `GET .../res/v1/web/search`. Auth goes in
//! `X-Subscription-Token` (not `Authorization`). Hits are at `web.results`;
//! `age` is a relative string ("2 days ago") and gets passed through as-is.
//! No domain filters on the API, so include/exclude are ignored here.
//!
//! The parent resolves the key and passes it in; it never lands in a log,
//! error or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// default endpoint, no trailing path
pub const DEFAULT_BASE_URL: &str = "https://api.search.brave.com";
/// secret/env name the parent resolves the key from
pub const BRAVE_API_KEY: &str = "BRAVE_API_KEY";

pub struct BraveProvider {
    api_key: String,
    base_url: String,
}

impl BraveProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// point at a local mirror / test server
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    /// base URL without the `/res/v1/web/search` path
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for BraveProvider {
    fn name(&self) -> &str {
        "brave"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let url = format!(
            "{}/res/v1/web/search?q={}&count={}",
            self.base_url.trim_end_matches('/'),
            http::percent_encode(query),
            opts.max_results,
        );
        let text = http::get_json("brave", &url, &[("X-Subscription-Token", &self.api_key)])?;
        parse_brave_response(&text)
    }
}

/// Parse the envelope. One bad hit is skipped, not fatal; no usable URL or
/// no `web.results` array means skip / `BadResponse` respectively.
pub fn parse_brave_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
        .ok_or_else(|| {
            SearchError::BadResponse("response has no 'web.results' array".to_string())
        })?;
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let Some(url) = opt_str(r, "url") else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        out.push(SearchResult {
            title: opt_str(r, "title").unwrap_or_default(),
            url,
            snippet: opt_str(r, "description").unwrap_or_default(),
            published: opt_str(r, "age").or_else(|| opt_str(r, "page_age")),
            score: None,
        });
    }
    Ok(out)
}
