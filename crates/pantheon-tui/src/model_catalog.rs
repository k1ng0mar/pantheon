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

use pantheon_providers::catalog::ApiMode;

/// Anthropic `anthropic-version` header value. Kept in step with the
/// transport in `model.rs`; an Anthropic-wire `/models` rejects the
/// request without it.
const ANTHROPIC_VERSION: &str = "2023-06-01";

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
    /// 401/403: the endpoint wants a key. Not fatal - the wizard falls
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

/// Fetch the live model list **with** an API key, for the wizard's
/// post-key model step. Same response shape as [`fetch_live_models`]
/// but it sends the auth header for the wire mode, so a keyed endpoint
/// (the local router, most hosted providers) returns its real list
/// instead of a 401 that would drop the wizard back to curated-only.
///
/// An empty key degrades to the keyless call. `mode` picks the header:
/// OpenAI sends `Authorization: Bearer`, Anthropic sends `x-api-key`
/// plus `anthropic-version`, because sending the wrong one fails auth
/// and reads as "this endpoint has no models".
pub fn fetch_live_models_keyed(
    base_url: &str,
    key: &str,
    mode: ApiMode,
) -> Result<Vec<LiveModel>, FetchError> {
    let url = models_url(base_url);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let mut req = agent.get(&url);
    let key = key.trim();
    if !key.is_empty() {
        match mode {
            ApiMode::Anthropic => {
                req = req
                    .set("x-api-key", key)
                    .set("anthropic-version", ANTHROPIC_VERSION);
            }
            ApiMode::OpenAi => {
                req = req.set("Authorization", &format!("Bearer {key}"));
            }
        }
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(code, _) => classify_status(code),
        _ => FetchError::Unreachable(e.to_string()),
    })?;
    let body = resp
        .into_string()
        .map_err(|e| FetchError::Unreachable(e.to_string()))?;
    parse_models_body(&body)
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
/// skipped - they cannot be displayed or selected.
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

/// Context window as a compact string: `131k`. `None` when unknown.
pub fn context_str(row: &ModelRow) -> Option<String> {
    row.context_limit.map(|l| format!("{}k", l / 1000))
}

/// Compact price for the second metadata line: `$0.15 in / $0.60 out`,
/// `free`, or a single-sided `$0.15 in`. `None` when no pricing is
/// known, never invented.
pub fn price_compact(input: Option<f64>, output: Option<f64>) -> Option<String> {
    match (input, output) {
        (Some(i), Some(o)) if i == 0.0 && o == 0.0 => Some("free".to_string()),
        (Some(i), Some(o)) => Some(format!("{} in / {} out", fmt_price(i), fmt_price(o))),
        (Some(i), None) => Some(format!("{} in", fmt_price(i))),
        (None, Some(o)) => Some(format!("{} out", fmt_price(o))),
        (None, None) => None,
    }
}

/// The second dim line under a model row: context and price joined
/// (`131k · $0.15 in / $0.60 out`). This lives on its own line, not the
/// right-aligned tag, because the combined string is long and clips
/// against the label on a narrow terminal when it shares the row.
/// `None` when neither context nor price is known.
pub fn row_meta(row: &ModelRow) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(c) = context_str(row) {
        parts.push(c);
    }
    if let Some(p) = price_compact(row.input_per_mtok_usd, row.output_per_mtok_usd) {
        parts.push(p);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ctx: Option<u64>, inp: Option<f64>, out: Option<f64>) -> ModelRow {
        ModelRow {
            id: "m".into(),
            context_limit: ctx,
            input_per_mtok_usd: inp,
            output_per_mtok_usd: out,
            curated: true,
        }
    }

    #[test]
    fn price_compact_joins_input_and_output() {
        assert_eq!(
            price_compact(Some(0.15), Some(0.60)).as_deref(),
            Some("$0.15 in / $0.60 out")
        );
    }

    #[test]
    fn price_compact_marks_zero_as_free() {
        assert_eq!(price_compact(Some(0.0), Some(0.0)).as_deref(), Some("free"));
    }

    #[test]
    fn price_compact_single_sided() {
        assert_eq!(price_compact(Some(2.50), None).as_deref(), Some("$2.50 in"));
        assert_eq!(
            price_compact(None, Some(0.30)).as_deref(),
            Some("$0.30 out")
        );
    }

    #[test]
    fn price_compact_none_when_unknown() {
        assert_eq!(price_compact(None, None), None);
    }

    #[test]
    fn row_meta_puts_context_and_price_on_one_line() {
        // The whole point of `meta`: context plus price together, ready
        // for its own dim line so it never clips against the label.
        assert_eq!(
            row_meta(&row(Some(131_000), Some(0.15), Some(0.60))).as_deref(),
            Some("131k · $0.15 in / $0.60 out")
        );
    }

    #[test]
    fn row_meta_context_only() {
        assert_eq!(
            row_meta(&row(Some(128_000), None, None)).as_deref(),
            Some("128k")
        );
    }

    #[test]
    fn row_meta_none_when_nothing_known() {
        assert_eq!(row_meta(&row(None, None, None)), None);
    }
}
