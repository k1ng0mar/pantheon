//! Raw Perplexity Search API (ranked results, no LLM synthesis), not Sonar
//! `/chat/completions` or the Agent API. `POST .../search`,
//! `Authorization: Bearer <key>`. No free tier — prepaid credits only
//! ($5/1K requests).
//!
//! Best date fields in the set: `date` -> `published`, falling back to
//! `last_updated` when `date` is missing. `include_domains` maps to
//! `search_domain_filter`; no deny param exists, so `exclude_domains` is
//! ignored. Key handed in by the parent, never logged.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// base URL, `/search` appended later
pub const DEFAULT_BASE_URL: &str = "https://api.perplexity.ai";
/// secret/env name the parent resolves the key from
pub const PERPLEXITY_API_KEY: &str = "PERPLEXITY_API_KEY";

pub struct PerplexityProvider {
    api_key: String,
    base_url: String,
}

impl PerplexityProvider {
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

impl SearchProvider for PerplexityProvider {
    fn name(&self) -> &str {
        "perplexity"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let mut body = serde_json::json!({
            "query": query,
            "max_results": opts.max_results,
        });
        if !opts.include_domains.is_empty() {
            body["search_domain_filter"] = serde_json::json!(opts.include_domains);
        }
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        let text = http::post_json("perplexity", &url, &[("Authorization", &auth)], body)?;
        parse_perplexity_response(&text)
    }
}

/// Parse the envelope. `date` -> `published`, fallback `last_updated` — both
/// real provider fields, never fabricated. Skip bad hits / no-URL hits; no
/// `results` array -> `BadResponse`.
pub fn parse_perplexity_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            snippet: opt_str(r, "snippet").unwrap_or_default(),
            published: opt_str(r, "date").or_else(|| opt_str(r, "last_updated")),
            score: None,
        });
    }
    Ok(out)
}
