//! SearXNG-backed [`SearchProvider`]:
//! `GET {instance}/search?q=…&format=json`.
//!
//! Self-hosted only: the instance base URL is user-configured (config
//! file `[websearch] searxng_url`, or the `SEARXNG_URL` env override).
//! **Do not point this at a public instance** — public instances bot-gate
//! the JSON API (403/429) and silently break. No auth is sent.
//!
//! Responses carry no date fields: `published` is always `None` rather
//! than fabricated. SearXNG has no result-count request parameter; the
//! instance's own page size applies.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Env var the parent checks (winning over the config file) for the
/// SearXNG instance base URL.
pub const SEARXNG_URL: &str = "SEARXNG_URL";
/// Last-resort default when no instance URL is configured anywhere: the
/// stock SearXNG docker-compose port on localhost.
pub const DEFAULT_INSTANCE_URL: &str = "http://localhost:8080";

/// SearXNG search provider. Self-hosted — takes an instance URL, no key.
pub struct SearxngProvider {
    base_url: String,
}

impl SearxngProvider {
    /// Production constructor: the configured self-hosted instance URL.
    pub fn new(instance_url: impl Into<String>) -> Self {
        Self {
            base_url: instance_url.into(),
        }
    }

    /// The instance base URL in use (without the `/search` path).
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
        // SearXNG has no count parameter: trim client-side.
        results.truncate(usize::from(opts.max_results));
        Ok(results)
    }
}

/// Parse a SearXNG `/search?format=json` envelope. Pure and unit-testable.
///
/// Hits live at `results` with `title`/`url`/`content` (plus `engine`).
/// SearXNG returns no date fields, so `published` is always `None`. A
/// single malformed hit is skipped, never fatal; a hit without a usable
/// URL is skipped. A missing or non-array `results` field is a
/// `BadResponse`.
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
