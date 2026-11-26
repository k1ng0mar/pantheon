//! SearXNG `GET {instance}/search?q=...&format=json`. Self-hosted only:
//! instance URL comes from config `[websearch] base_url` or the
//! `SEARXNG_URL` env override (env wins). Do NOT point this at a public
//! instance — they bot-gate the JSON API (403/429) and silently break. No
//! auth sent.
//!
//! No date fields, so `published` stays `None`. No result-count request
//! parameter; the instance's own page size applies.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// env override for the instance URL (wins over config file)
pub const SEARXNG_URL: &str = "SEARXNG_URL";
/// last-resort default: stock SearXNG docker-compose port on localhost
pub const DEFAULT_INSTANCE_URL: &str = "http://localhost:8080";

/// self-hosted, takes an instance URL, no key
pub struct SearxngProvider {
    base_url: String,
}

impl SearxngProvider {
    pub fn new(instance_url: impl Into<String>) -> Self {
        Self {
            base_url: instance_url.into(),
        }
    }

    /// instance base URL without `/search`
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for SearxngProvider {
    fn name(&self) -> &str {
        "searxng"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let url = format!(
            "{}/search?q={}&format=json",
            self.base_url.trim_end_matches('/'),
            http::percent_encode(query),
        );
        let text = http::get_json("searxng", &url, &[])?;
        let mut results = parse_searxng_response(&text)?;
        // no count param, trim client-side. 0 means one (see
        // SearchOptions::max_results) — truncating to 0 gives an empty page.
        results.truncate(usize::from(opts.max_results.max(1)));
        Ok(results)
    }
}

/// Parse the `/search?format=json` envelope. Hits at `results`
/// (`title`/`url`/`content`, `engine` ignored); no date fields so
/// `published` stays `None`. Skip bad hits / no-URL hits; no `results`
/// array -> `BadResponse`.
pub fn parse_searxng_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            snippet: opt_str(r, "content").unwrap_or_default(),
            published: None,
            score: None,
        });
    }
    Ok(out)
}
