//! Hosted Ollama search API (free account key), not the local model daemon.
//! `POST .../api/web_search`, `Authorization: Bearer <key>`. No date fields,
//! so `published` stays `None`. Key handed in by the parent, never logged.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// base URL, `/api/web_search` appended later
pub const DEFAULT_BASE_URL: &str = "https://ollama.com";
/// secret/env name the parent resolves the key from
pub const OLLAMA_API_KEY: &str = "OLLAMA_API_KEY";

pub struct OllamaProvider {
    api_key: String,
    base_url: String,
}

impl OllamaProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// local mirror / test server, or a local daemon on
    /// http://localhost:11434 that proxies the same path when signed in
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    /// base URL without `/api/web_search`
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        // API caps max_results at 10
        let body = serde_json::json!({
            "query": query,
            "max_results": opts.max_results.min(10),
        });
        let url = format!("{}/api/web_search", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        let text = http::post_json("ollama", &url, &[("Authorization", &auth)], body)?;
        parse_ollama_response(&text)
    }
}

/// Parse the envelope. Hits at `results`; no date fields so `published` stays
/// `None`. Skip bad hits / no-URL hits; no `results` array -> `BadResponse`.
pub fn parse_ollama_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
