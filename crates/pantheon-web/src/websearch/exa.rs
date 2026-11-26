//! Exa `POST .../search`. Key goes in `x-api-key`, not `Authorization`.
//! `type` must be one of `instant|fast|auto|deep-lite|deep|deep-reasoning`:
//! `neural` and `keyword` were removed server-side. We use `auto`, which is
//! good enough for agents without paying for deep search.
//!
//! Parent resolves the key and hands it in; no key in logs/errors/results.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// base URL, `/search` appended later
pub const DEFAULT_BASE_URL: &str = "https://api.exa.ai";
/// secret/env name the parent resolves the key from
pub const EXA_API_KEY: &str = "EXA_API_KEY";

pub struct ExaProvider {
    api_key: String,
    base_url: String,
}

impl ExaProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// local mirror / test server
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

impl SearchProvider for ExaProvider {
    fn name(&self) -> &str {
        "exa"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let mut body = serde_json::json!({
            "query": query,
            "numResults": opts.max_results,
            // neural/keyword are gone server-side
            "type": "auto",
        });
        if !opts.include_domains.is_empty() {
            body["includeDomains"] = serde_json::json!(opts.include_domains);
        }
        if !opts.exclude_domains.is_empty() {
            body["excludeDomains"] = serde_json::json!(opts.exclude_domains);
        }
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        let text = http::post_json("exa", &url, &[("x-api-key", &self.api_key)], body)?;
        parse_exa_response(&text)
    }
}

/// Parse the `/search` envelope. Any of `title`/`url`/`text`/`publishedDate`
/// may be missing or null; skip bad hits and hits with no URL, no array ->
/// `BadResponse`.
pub fn parse_exa_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| SearchError::BadResponse("response has no 'results' array".to_string()))?;
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
            snippet: opt_str(r, "text").unwrap_or_default(),
            published: opt_str(r, "publishedDate"),
            score: None,
        });
    }
    Ok(out)
}
