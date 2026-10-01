//! Live model lists for the wizard's Screen 3 (model picker).
//!
//! For OpenAI-compatible providers the picker merges the catalog's
//! curated entries with the provider's live `{base}/models` list, so the
//! user sees what actually exists rather than only what the catalog
//! shipped. The fetch is keyless-first: many OpenAI-style endpoints
//! (OpenRouter included) serve `/models` without auth. A 401/403 falls
//! back to the curated entries with a "full list after API key" note;
//! other failures fall back with a note naming the failure kind. The
//! wizard never invents models: with no rows at all, free-text entry is
//! the only path.
//!
//! Everything network-adjacent is split so the pure parts are
//! unit-testable without a socket: [`parse_models_body`] for the JSON
//! shape, [`classify_status`] for the auth-vs-rest distinction, and
//! [`merge_model_rows`] for the union.

use std::time::Duration;

/// One live model entry from a `/models` response.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveModel {
    pub id: String,
    pub input_per_mtok_usd: Option<f64>,
    pub output_per_mtok_usd: Option<f64>,
}

/// Why a live fetch failed.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchError {
    /// 401/403: the endpoint wants a key. Not fatal — the wizard falls
    /// back to curated entries.
    Auth,
    /// Transport failure, timeout, or a non-auth HTTP status.
    Unreachable(String),
    /// The body was not the expected `{"data":[...]}` shape.
    BadShape(String),
}

impl FetchError {
    /// Short kind name for the one-line picker note.
    pub fn kind(&self) -> &'static str {
        match self {
            FetchError::Auth => "auth",
            FetchError::Unreachable(_) => "unreachable",
            FetchError::BadShape(_) => "bad response",
        }
    }
}

/// Fetch the live model list from `{base_url}/models`, keyless. One
/// HTTPS GET with a ~10s timeout.
pub fn fetch_live_models(base_url: &str) -> Result<Vec<LiveModel>, FetchError> {
    let url = models_url(base_url);
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(10))
        .call()
        .map_err(|e| match e {
            ureq::Error::Status(code, _) => classify_status(code),
            _ => FetchError::Unreachable(e.to_string()),
        })?;
    let body = resp
        .into_string()
        .map_err(|e| FetchError::Unreachable(e.to_string()))?;
    parse_models_body(&body)
}

/// `{base}/models`, tolerant of a trailing slash on the base.
fn models_url(base_url: &str) -> String {
    format!("{}/models", base_url.trim_end_matches('/'))
}

/// Map an HTTP status to the fetch error. Only 401/403 is auth;
/// everything else reachable-but-failing is grouped with unreachable,
/// because the wizard's response is the same: curated + a note.
fn classify_status(code: u16) -> FetchError {
    match code {
        401 | 403 => FetchError::Auth,
        _ => FetchError::Unreachable(format!("http {code}")),
    }
}

/// Parse the OpenAI `{"data":[{"id", ...}]}` shape. OpenRouter-style
/// `pricing: {prompt, completion}` string fields are per-token prices,
/// converted to per-million-token USD. Entries without a string id are
/// skipped — they cannot be displayed or selected.
fn parse_models_body(body: &str) -> Result<Vec<LiveModel>, FetchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| FetchError::BadShape(format!("invalid json: {e}")))?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| FetchError::BadShape("missing data array".to_string()))?;
    let mut out = Vec::with_capacity(data.len());
    for entry in data {
        let Some(id) = entry.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        let pricing = entry.get("pricing");
        out.push(LiveModel {
            id: id.to_string(),
            input_per_mtok_usd: per_mtok(pricing.and_then(|p| p.get("prompt"))),
            output_per_mtok_usd: per_mtok(pricing.and_then(|p| p.get("completion"))),
        });
    }
    Ok(out)
}

/// OpenRouter-style per-token price (string or number) → USD per
/// million tokens. Non-finite, negative, or unparsable values are
/// treated as unknown, never as zero.
fn per_mtok(v: Option<&serde_json::Value>) -> Option<f64> {
    let per_token = match v? {
        serde_json::Value::String(s) => s.parse::<f64>().ok()?,
        serde_json::Value::Number(n) => n.as_f64()?,
        _ => return None,
    };
    if !per_token.is_finite() || per_token < 0.0 {
        return None;
    }
    Some(per_token * 1_000_000.0)
}

// ---------------------------------------------------------------------------
// row merging and tags
// ---------------------------------------------------------------------------

/// A curated catalog entry, normalized for the merge.
#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub id: String,
    pub context_limit: Option<u64>,
    pub input_per_mtok_usd: Option<f64>,
    pub output_per_mtok_usd: Option<f64>,
}

/// One picker row: the union of curated and live.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    pub id: String,
    pub context_limit: Option<u64>,
    pub input_per_mtok_usd: Option<f64>,
    pub output_per_mtok_usd: Option<f64>,
    /// True when the id came from the catalog (curated metadata wins).
    pub curated: bool,
}

/// Union by model id. Curated entries win on metadata (context limit,
/// catalog cost); a live id with no curated entry gets a row with live
/// pricing; a curated id absent from live still shows. Order is
/// curated first (catalog order), then live-only ids in live order.
pub fn merge_model_rows(curated: &[CatalogModel], live: &[LiveModel]) -> Vec<ModelRow> {
    let mut rows: Vec<ModelRow> = curated
        .iter()
        .map(|c| ModelRow {
            id: c.id.clone(),
            context_limit: c.context_limit,
            input_per_mtok_usd: c.input_per_mtok_usd,
            output_per_mtok_usd: c.output_per_mtok_usd,
            curated: true,
        })
        .collect();
    for l in live {
        if rows.iter().any(|r| r.id == l.id) {
            continue;
        }
        rows.push(ModelRow {
            id: l.id.clone(),
            context_limit: None,
            input_per_mtok_usd: l.input_per_mtok_usd,
            output_per_mtok_usd: l.output_per_mtok_usd,
            curated: false,
        });
    }
    rows
}

/// Compact price: two decimals for normal values (`$0.20`), more
/// precision for tiny-but-nonzero ones so they never round to `$0.00`.
fn fmt_price(v: f64) -> String {
    if v >= 0.01 {
        format!("${v:.2}")
    } else if v > 0.0 {
        let s = format!("{v:.6}");
        format!("${}", s.trim_end_matches('0').trim_end_matches('.'))
    } else {
        "$0.00".to_string()
    }
}

/// Price tag for a row: `in $0.20/M · out $0.60/M`; `free` when both
/// prices are zero (OpenRouter marks free models with zero pricing);
/// `None` when there is no pricing at all — never invented.
pub fn price_tag(input: Option<f64>, output: Option<f64>) -> Option<String> {
    match (input, output) {
        (Some(i), Some(o)) if i == 0.0 && o == 0.0 => Some("free".to_string()),
        (Some(i), Some(o)) => Some(format!("in {}/M · out {}/M", fmt_price(i), fmt_price(o))),
        (Some(i), None) => Some(format!("in {}/M", fmt_price(i))),
        (None, Some(o)) => Some(format!("out {}/M", fmt_price(o))),
        (None, None) => None,
    }
}

/// Full picker tag for a row: context first, then price
/// (`70k · in $0.20/M · out $0.60/M`, or `70k · free`). `None` when
/// neither is known.
pub fn row_tag(row: &ModelRow) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(limit) = row.context_limit {
        parts.push(format!("{}k", limit / 1000));
    }
    if let Some(p) = price_tag(row.input_per_mtok_usd, row.output_per_mtok_usd) {
        parts.push(p);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}
