//! Usage stats: the same fold the TUI's `pantheon stats` performs.
//!
//! The aggregation is a direct fold over
//! [`Ledger::usage_between`](pantheon_storage::Ledger::usage_between)
//! (`UsageRecorded` rows): totals, by model, by run, by UTC day. The
//! dashboard cannot depend on the TUI crate, so the fold is mirrored here
//! against the stable ledger API — the same rows, the same sums.

use crate::server::{Request, Response};
use crate::{err_json, json_ok, query_usize, App};
use pantheon_api::events::Event;
use pantheon_storage::Ledger;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

const DAY_MS: i64 = 86_400_000;

#[derive(Default)]
struct Totals {
    calls: u64,
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
    cost_usd: f64,
    priced_calls: u64,
}

impl Totals {
    fn add(&mut self, input: u64, output: u64, total: u64, cost: Option<f64>) {
        self.calls += 1;
        self.input_tokens += input;
        self.output_tokens += output;
        self.total_tokens += total;
        if let Some(c) = cost {
            self.cost_usd += c;
            self.priced_calls += 1;
        }
    }
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "calls": self.calls,
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "total_tokens": self.total_tokens,
            "cost_usd": (self.cost_usd * 10000.0).round() / 10000.0,
            "priced_calls": self.priced_calls,
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `YYYY-MM-DD` (UTC). Howard Hinnant's civil-from-days, same as the TUI.
fn day_label(ts_ms: i64) -> String {
    let z = ts_ms.div_euclid(DAY_MS) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// `GET /api/stats?days=30`: usage report for the trailing window.
pub fn stats(app: &App, req: &Request) -> Response {
    let days = query_usize(&req.query, "days", 30).min(365).max(1) as i64;
    let to_ms = now_ms();
    let from_ms = to_ms - days * DAY_MS;
    let ledger = match Ledger::open(&app.data_dir.join("ledger.db")) {
        Ok(l) => l,
        Err(e) => return err_json(500, "LEDGER", &format!("open ledger: {e}")),
    };
    let rows = match ledger.usage_between(from_ms, to_ms) {
        Ok(r) => r,
        Err(e) => return err_json(500, "LEDGER", &format!("usage_between: {e}")),
    };
    let mut titles: BTreeMap<String, String> = BTreeMap::new();
    let mut titles_loaded = false;
    let mut totals = Totals::default();
    let mut by_model: BTreeMap<String, Totals> = BTreeMap::new();
    let mut by_run: BTreeMap<String, Totals> = BTreeMap::new();
    let mut by_day: BTreeMap<String, Totals> = BTreeMap::new();
    for entry in &rows {
        let Event::UsageRecorded {
            run_id,
            model,
            input_tokens,
            output_tokens,
            total_tokens,
            cost_usd,
            ..
        } = &entry.event
        else {
            continue;
        };
        if !titles_loaded {
            for r in ledger.list_runs(usize::MAX).unwrap_or_default() {
                if let Some(t) = r.3 {
                    titles.insert(r.0, t);
                }
            }
            titles_loaded = true;
        }
        totals.add(*input_tokens, *output_tokens, *total_tokens, *cost_usd);
        by_model.entry(model.clone()).or_default().add(
            *input_tokens,
            *output_tokens,
            *total_tokens,
            *cost_usd,
        );
        by_run.entry(run_id.clone()).or_default().add(
            *input_tokens,
            *output_tokens,
            *total_tokens,
            *cost_usd,
        );
        by_day.entry(day_label(entry.ts_ms)).or_default().add(
            *input_tokens,
            *output_tokens,
            *total_tokens,
            *cost_usd,
        );
    }
    // Highest cost first (then tokens), like the TUI's sort.
    let mut by_model: Vec<(String, Totals)> = by_model.into_iter().collect();
    by_model.sort_by(|a, b| {
        b.1.cost_usd
            .partial_cmp(&a.1.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.total_tokens.cmp(&a.1.total_tokens))
    });
    let mut by_run: Vec<(String, Totals)> = by_run.into_iter().collect();
    by_run.sort_by(|a, b| {
        b.1.cost_usd
            .partial_cmp(&a.1.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.1.total_tokens.cmp(&a.1.total_tokens))
    });
    json_ok(serde_json::json!({
        "from_ms": from_ms,
        "to_ms": to_ms,
        "days": days,
        "totals": totals.json(),
        "by_model": by_model.iter().map(|(m, t)| serde_json::json!({"model": m, "totals": t.json()})).collect::<Vec<_>>(),
        "by_run": by_run.iter().map(|(r, t)| serde_json::json!({"run_id": r, "title": titles.get(r), "totals": t.json()})).collect::<Vec<_>>(),
        "by_day": by_day.iter().map(|(d, t)| serde_json::json!({"day": d, "totals": t.json()})).collect::<Vec<_>>(),
    }))
}
