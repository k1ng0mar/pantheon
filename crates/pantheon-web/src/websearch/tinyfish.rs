//! TinyFish `GET https://api.search.tinyfish.ai/`. Auth is `X-API-Key`
//! (not `Authorization: Bearer`); keys are free, no card, from the dashboard
//! (`agent.tinyfish.ai`). Rank-stable, fast (p50 < 0.5s).
//!
//! Request (verified against the API docs): `GET {base}/?query=<urlencoded>`
//! with optional comma-separated `include_domains`/`exclude_domains`
//! (supported natively, mapped from [`SearchOptions`]). There's also
//! `purpose`/`location`/`language`/`domain_type`, date bounds, and `page`,
//! which we don't send. No result-count param; results are trimmed client-side
//! to `max_results`.
//!
//! Response: `{query, results: [{position, site_name, title, snippet, url}],
//! total_results, page}`. Some integrations add a `date` field when known;
//! map it to `published`, else `None` — never fabricated.
//!
//! Rate limit ~30 req/min on free tier (some plans report 5 req/min — back
//! off on 429). We pace ourselves with a conservative default of 2s between
//! requests; tune via [`TinyFishProvider::with_min_interval`] (zero disables).
//!
//! Key is handed in by the parent; never logged.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// base URL
pub const DEFAULT_BASE_URL: &str = "https://api.search.tinyfish.ai";
/// secret/env name the parent resolves the key from
pub const TINYFISH_API_KEY: &str = "TINYFISH_API_KEY";
/// 2s between requests ≈ 30 req/min. Used to be described as "the free-tier
/// rate limit", but limits vary by plan (some report 5 req/min); this is just
/// a default, and 429 `Retry-After` hints still get honoured on top.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_secs(2);

pub struct TinyFishProvider {
    api_key: String,
    base_url: String,
    min_interval: Duration,
    last_call: Mutex<Option<Instant>>,
}

impl TinyFishProvider {
    /// default endpoint, 2s pacing
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// local mirror / test server
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            min_interval: DEFAULT_MIN_INTERVAL,
            last_call: Mutex::new(None),
        }
    }

    /// override min interval; `Duration::ZERO` disables pacing (tests)
    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// sleep until [`Self::min_interval`] has passed since the last call, then
    /// record it. keeps us under ~30 req/min instead of tripping 429s
    fn pace(&self) {
        if self.min_interval.is_zero() {
            return;
        }
        let mut last = self.last_call.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < self.min_interval {
                std::thread::sleep(self.min_interval - elapsed);
            }
        }
        *last = Some(Instant::now());
    }
}

impl SearchProvider for TinyFishProvider {
    fn name(&self) -> &str {
        "tinyfish"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        self.pace();
        let mut url = format!(
            "{}/?query={}",
            self.base_url.trim_end_matches('/'),
            http::percent_encode(query),
        );
        // native TinyFish params, comma-separated
        if !opts.include_domains.is_empty() {
            url.push_str("&include_domains=");
            url.push_str(&http::percent_encode(&opts.include_domains.join(",")));
        }
        if !opts.exclude_domains.is_empty() {
            url.push_str("&exclude_domains=");
            url.push_str(&http::percent_encode(&opts.exclude_domains.join(",")));
        }
        let text = http::get_json("tinyfish", &url, &[("X-API-Key", &self.api_key)])?;
        let mut results = parse_tinyfish_response(&text)?;
        // no count param: trim client-side; 0 means one (see
        // SearchOptions::max_results)
        results.truncate(usize::from(opts.max_results.max(1)));
        Ok(results)
    }
}

/// Parse the envelope. Hits at `results`; `site_name` and `position` ignored.
/// `published` from `date` when present, else `None`. Skip bad hits / no-URL
/// hits; no `results` array -> `BadResponse`.
pub fn parse_tinyfish_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            published: opt_str(r, "date"),
            score: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tinyfish_parser_tests {
    use super::parse_tinyfish_response;

    const FIXTURE: &str = r#"{
        "query": "rust",
        "results": [
            {"position": 1, "site_name": "rust-lang", "title": "Rust", "snippet": "a language", "url": "https://www.rust-lang.org/", "date": "2024-05-01"},
            {"position": 2, "site_name": "x", "title": "No URL", "snippet": "skipped"},
            {"position": 3, "site_name": "y", "title": "Undated", "snippet": "s", "url": "https://undated.example/"}
        ],
        "total_results": 3,
        "page": 1
    }"#;

    #[test]
    fn parses_hits_dates_and_skips_url_less_rows() {
        let hits = parse_tinyfish_response(FIXTURE).expect("parse");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].published.as_deref(), Some("2024-05-01"));
        assert_eq!(hits[1].url, "https://undated.example/");
        assert_eq!(hits[1].published, None);
    }

    #[test]
    fn missing_results_array_is_bad_response() {
        assert!(parse_tinyfish_response(r#"{"query": "x"}"#).is_err());
        assert!(parse_tinyfish_response("not json").is_err());
    }
}
