//! TinyFish-backed [`SearchProvider`]: `GET https://api.search.tinyfish.ai/`.
//!
//! Auth is the `X-API-Key` header (NOT `Authorization: Bearer`). Keys are
//! free with no credit card from the TinyFish dashboard
//! (`agent.tinyfish.ai`). The API is rank-stable and fast (p50 < 0.5s).
//!
//! Request shape (verified against <https://docs.tinyfish.ai/api-reference/search-the-web>):
//! `GET {base}/?query=<urlencoded>` with optional `include_domains` /
//! `exclude_domains` (comma-separated — supported natively, mapped from
//! [`SearchOptions`]), plus `purpose`, `location`, `language`,
//! `domain_type`, date bounds, and `page` which this provider does not
//! send. There is no result-count parameter: results are trimmed
//! client-side to `max_results`.
//!
//! Response shape: `{query, results: [{position, site_name, title,
//! snippet, url}], total_results, page}`. Third-party integrations also
//! report a `date` field on results when known; it is mapped into
//! `published` when present, otherwise `published` is `None` — never
//! fabricated.
//!
//! Rate limit: ~30 req/min on the free tier (some integrations report
//! 5 req/min on the default plan — back off on 429). This provider paces
//! itself with a conservative default minimum interval of 2s between
//! requests; tune via [`TinyFishProvider::with_min_interval`] (zero
//! disables pacing).
//!
//! The API key is handed to the constructor by the caller (resolved from the
//! secrets broker or the env by the parent) and never appears in any log
//! line, error, or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default API base URL (no trailing path).
pub const DEFAULT_BASE_URL: &str = "https://api.search.tinyfish.ai";
/// Env/secret name the parent resolves the TinyFish key from.
pub const TINYFISH_API_KEY: &str = "TINYFISH_API_KEY";
/// Conservative default pacing: 2s between requests ≈ 30 req/min, the
/// free-tier rate limit.
pub const DEFAULT_MIN_INTERVAL: Duration = Duration::from_secs(2);

/// TinyFish search provider.
pub struct TinyFishProvider {
    api_key: String,
    base_url: String,
    min_interval: Duration,
    last_call: Mutex<Option<Instant>>,
}

impl TinyFishProvider {
    /// Production constructor: default endpoint, conservative 2s pacing.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the API base URL.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
            min_interval: DEFAULT_MIN_INTERVAL,
            last_call: Mutex::new(None),
        }
    }

    /// Override the minimum interval between requests. `Duration::ZERO`
    /// disables pacing (e.g. in tests).
    pub fn with_min_interval(mut self, interval: Duration) -> Self {
        self.min_interval = interval;
        self
    }

    /// The base URL in use.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The minimum interval between requests.
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Sleep until [`Self::min_interval`] has passed since the previous
    /// call, then record this call. Keeps the provider inside the free
    /// tier's ~30 req/min budget instead of tripping 429s.
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
        // Comma-separated domain filters are native TinyFish parameters.
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
        // No count parameter on the API: trim client-side.
        results.truncate(usize::from(opts.max_results));
        Ok(results)
    }
}

/// Parse a TinyFish search JSON envelope. Pure and unit-testable.
///
/// Hits live at `results` with `title`/`snippet`/`url` (`site_name` and
/// `position` are ignored). `published` comes from `date` when the API
/// provides one, else `None`. A single malformed hit is skipped, never
/// fatal; a hit without a usable URL is skipped. A missing or non-array
/// `results` field is a `BadResponse`.
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
