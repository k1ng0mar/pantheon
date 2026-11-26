//! Firecrawl `POST .../v2/search`, `Authorization: Bearer <key>`. Responses
//! can carry scraped markdown — we keep snippets only, from
//! `metadata.description`, and never send `scrapeOptions`, so no full-page
//! scrapes by default. No domain filter params on v2 search, so
//! include/exclude are ignored here. Key is handed in by the parent, never
//! logged.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// base URL, `/v2/search` appended later
pub const DEFAULT_BASE_URL: &str = "https://api.firecrawl.dev";
/// secret/env name the parent resolves the key from
pub const FIRECRAWL_API_KEY: &str = "FIRECRAWL_API_KEY";

pub struct FirecrawlProvider {
    api_key: String,
    base_url: String,
}

impl FirecrawlProvider {
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

    /// base URL without `/v2/search`
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for FirecrawlProvider {
    fn name(&self) -> &str {
        "firecrawl"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        // no scrapeOptions: we want search hits, not full page scrapes
        let body = serde_json::json!({
            "query": query,
            "limit": opts.max_results,
        });
        let url = format!("{}/v2/search", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        let text = http::post_json("firecrawl", &url, &[("Authorization", &auth)], body)?;
        parse_firecrawl_response(&text)
    }
}

/// Parse the v2 envelope. Snippet is `metadata.description`, never the full
/// markdown; `published` from `metadata.publishedTime` (filling back to
/// `modifiedTime`). Skip bad hits / no-URL hits; no `data` array ->
/// `BadResponse`.
pub fn parse_firecrawl_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .get("data")
        .and_then(|r| r.as_array())
        .ok_or_else(|| SearchError::BadResponse("response has no 'data' array".to_string()))?;
    let mut out = Vec::with_capacity(results.len());
    for r in results {
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
            // snippet only, never the scraped markdown
            snippet: meta
                .and_then(|m| opt_str(m, "description"))
                .unwrap_or_default(),
            published: meta
                .and_then(|m| opt_str(m, "publishedTime").or_else(|| opt_str(m, "modifiedTime"))),
            score: None,
        });
    }
    Ok(out)
}
