//! `pantheon stats`: token/spend aggregation from the event ledger.
//!
//! Usage rows are persisted as `Event::UsageRecorded` (the provider-plane
//! `Usage` event used to be dropped before reaching the ledger). This
//! command folds them over a time window - default today - and breaks
//! the totals down by model, by session (run), and by day.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use pantheon_api::error::PantheonError;
use pantheon_api::events::Event;
use pantheon_storage::Ledger;

/// One aggregated usage row.
#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    pub calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Sum of known costs; calls without a catalog price are excluded.
    pub cost_usd: f64,
    /// Calls that carried a cost.
    pub priced_calls: u64,
}

impl UsageTotals {
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
}

#[derive(Debug, Clone)]
pub struct ModelStat {
    pub model: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone)]
pub struct RunStat {
    pub run_id: String,
    pub title: Option<String>,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone)]
pub struct DayStat {
    /// UTC day, `YYYY-MM-DD`.
    pub day: String,
    pub totals: UsageTotals,
}

/// Aggregated report for a time window.
#[derive(Debug, Clone)]
pub struct StatsReport {
    pub from_ms: i64,
    pub to_ms: i64,
    pub totals: UsageTotals,
    pub by_model: Vec<ModelStat>,
    pub by_run: Vec<RunStat>,
    pub by_day: Vec<DayStat>,
}

/// Fold `UsageRecorded` rows from the ledger into a report.
/// Pure over the ledger: testable without a CLI.
pub fn collect(ledger: &Ledger, from_ms: i64, to_ms: i64) -> Result<StatsReport, PantheonError> {
    let rows = ledger.usage_between(from_ms, to_ms)?;
    let mut totals = UsageTotals::default();
    let mut by_model: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut by_run: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut by_day: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut titles: BTreeMap<String, String> = BTreeMap::new();

    // Session titles for the run breakdown: latest SessionTitled wins.
    // Only fetched when there is usage to attribute, so `stats` on an
    // empty window stays a single cheap query.
    let mut titles_loaded = false;

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

    let by_model = by_model
        .into_iter()
        .map(|(model, totals)| ModelStat { model, totals })
        .collect();
    let by_run = by_run
        .into_iter()
        .map(|(run_id, totals)| {
            let title = titles.remove(&run_id);
            RunStat {
                run_id,
                title,
                totals,
            }
        })
        .collect();
    let by_day = by_day
        .into_iter()
        .map(|(day, totals)| DayStat { day, totals })
        .collect();

    Ok(StatsReport {
        from_ms,
        to_ms,
        totals,
        by_model,
        by_run,
        by_day,
    })
}

/// Sort helper: highest cost first (falls back to tokens).
fn sort_stats<T>(v: &mut [T], totals: impl Fn(&T) -> &UsageTotals) {
    v.sort_by(|a, b| {
        let (ta, tb) = (totals(a), totals(b));
        tb.cost_usd
            .partial_cmp(&ta.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| tb.total_tokens.cmp(&ta.total_tokens))
    });
}

fn fmt_cost(cost: f64, priced: u64, calls: u64) -> String {
    if priced == 0 && calls > 0 {
        "n/a".to_string()
    } else {
        format!("${:.4}", cost)
    }
}

fn fmt_tokens(t: &UsageTotals) -> String {
    format!(
        "{} tok ({} in / {} out)",
        t.total_tokens, t.input_tokens, t.output_tokens
    )
}

/// Human-readable report.
pub fn render_human(r: &StatsReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "stats {} → {}\n",
        day_label(r.from_ms),
        day_label(r.to_ms.saturating_sub(1))
    ));
    out.push_str(&format!(
        "total: {} calls, {}, cost {}\n",
        r.totals.calls,
        fmt_tokens(&r.totals),
        fmt_cost(r.totals.cost_usd, r.totals.priced_calls, r.totals.calls)
    ));
    if r.totals.calls == 0 {
        out.push_str("no usage recorded in this window\n");
        return out;
    }

    let mut by_model = r.by_model.clone();
    sort_stats(&mut by_model, |s| &s.totals);
    out.push_str("\nby model:\n");
    for s in &by_model {
        out.push_str(&format!(
            "  {:<42} {:>6} calls  {:>14}  {}\n",
            s.model,
            s.totals.calls,
            fmt_tokens(&s.totals),
            fmt_cost(s.totals.cost_usd, s.totals.priced_calls, s.totals.calls)
        ));
    }

    let mut by_run = r.by_run.clone();
    sort_stats(&mut by_run, |s| &s.totals);
    out.push_str("\nby session:\n");
    for s in &by_run {
        let label = s
            .title
            .as_deref()
            .map(|t| format!("{} - {}", short_id(&s.run_id), t))
            .unwrap_or_else(|| short_id(&s.run_id));
        out.push_str(&format!(
            "  {:<42} {:>6} calls  {:>14}  {}\n",
            truncate(&label, 42),
            s.totals.calls,
            fmt_tokens(&s.totals),
            fmt_cost(s.totals.cost_usd, s.totals.priced_calls, s.totals.calls)
        ));
    }

    out.push_str("\nby day:\n");
    for d in &r.by_day {
        out.push_str(&format!(
            "  {}  {:>6} calls  {:>14}  {}\n",
            d.day,
            d.totals.calls,
            fmt_tokens(&d.totals),
            fmt_cost(d.totals.cost_usd, d.totals.priced_calls, d.totals.calls)
        ));
    }
    out
}

fn short_id(id: &str) -> String {
    if id.len() > 24 {
        format!("{}...", &id[..23])
    } else {
        id.to_string()
    }
}

/// Compact summary for the TUI `/stats` command: today's totals plus this
/// session's share. Single status block, no tables.
pub fn render_session_summary(r: &StatsReport, run_id: &str) -> String {
    let mine = r.by_run.iter().find(|s| s.run_id == run_id);
    let mut out = format!(
        "stats today: {} calls, {}, cost {}",
        r.totals.calls,
        fmt_tokens(&r.totals),
        fmt_cost(r.totals.cost_usd, r.totals.priced_calls, r.totals.calls)
    );
    match mine {
        Some(s) => {
            out.push_str(&format!(
                "\nthis session: {} calls, {}, cost {}",
                s.totals.calls,
                fmt_tokens(&s.totals),
                fmt_cost(s.totals.cost_usd, s.totals.priced_calls, s.totals.calls)
            ));
        }
        None => out.push_str("\nthis session: no usage today"),
    }
    if let Some(top) = r
        .by_model
        .iter()
        .max_by(|a, b| a.totals.total_tokens.cmp(&b.totals.total_tokens))
    {
        out.push_str(&format!(
            "\ntop model: {} ({} tok)",
            top.model, top.totals.total_tokens
        ));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Machine-readable report.
pub fn render_json(r: &StatsReport) -> String {
    let mut s = String::new();
    s.push_str("{\n");
    s.push_str(&format!(
        "  \"from\": \"{}\",\n  \"to\": \"{}\",\n",
        day_label(r.from_ms),
        day_label(r.to_ms.saturating_sub(1))
    ));
    s.push_str(&format!("  \"totals\": {},\n", totals_json(&r.totals)));
    s.push_str("  \"by_model\": [\n");
    for (i, m) in r.by_model.iter().enumerate() {
        s.push_str(&format!(
            "    {{\"model\": {}, \"totals\": {}}}{}\n",
            json_str(&m.model),
            totals_json(&m.totals),
            if i + 1 < r.by_model.len() { "," } else { "" }
        ));
    }
    s.push_str("  ],\n  \"by_session\": [\n");
    for (i, m) in r.by_run.iter().enumerate() {
        let title = m.title.as_deref().map(json_str).unwrap_or("null".into());
        s.push_str(&format!(
            "    {{\"run_id\": {}, \"title\": {}, \"totals\": {}}}{}\n",
            json_str(&m.run_id),
            title,
            totals_json(&m.totals),
            if i + 1 < r.by_run.len() { "," } else { "" }
        ));
    }
    s.push_str("  ],\n  \"by_day\": [\n");
    for (i, d) in r.by_day.iter().enumerate() {
        s.push_str(&format!(
            "    {{\"day\": {}, \"totals\": {}}}{}\n",
            json_str(&d.day),
            totals_json(&d.totals),
            if i + 1 < r.by_day.len() { "," } else { "" }
        ));
    }
    s.push_str("  ]\n}\n");
    s
}

fn totals_json(t: &UsageTotals) -> String {
    format!(
        "{{\"calls\": {}, \"input_tokens\": {}, \"output_tokens\": {}, \"total_tokens\": {}, \"cost_usd\": {:.6}}}",
        t.calls, t.input_tokens, t.output_tokens, t.total_tokens, t.cost_usd
    )
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// --- time helpers (UTC; no date crate) ---

pub const DAY_MS: i64 = 86_400_000;

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Start of the UTC day containing `ts_ms`.
pub fn day_start_ms(ts_ms: i64) -> i64 {
    ts_ms - ((ts_ms % DAY_MS + DAY_MS) % DAY_MS)
}

/// `YYYY-MM-DD` (UTC) for a timestamp. Howard Hinnant's civil-from-days.
pub fn day_label(ts_ms: i64) -> String {
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
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Parse `YYYY-MM-DD` into the UTC day start (ms). `None` on bad input.
pub fn parse_day(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: i64 = parts[0].parse().ok()?;
    let m: i64 = parts[1].parse().ok()?;
    let d: i64 = parts[2].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1970 {
        return None;
    }
    // days_from_civil
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * DAY_MS)
}

/// CLI entry: `pantheon stats [--week|--month|--from D|--to D] [--json]`.
pub fn cmd_stats(args: &[String], data_dir: &Path) {
    let mut from: Option<i64> = None;
    let mut to: Option<i64> = None;
    let mut json = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--week" => {
                from = Some(day_start_ms(now_ms() - 6 * DAY_MS));
            }
            "--month" => {
                from = Some(day_start_ms(now_ms() - 29 * DAY_MS));
            }
            "--from" => {
                i += 1;
                match args.get(i).and_then(|s| parse_day(s)) {
                    Some(ms) => from = Some(ms),
                    None => {
                        eprintln!("stats: --from needs YYYY-MM-DD");
                        std::process::exit(2);
                    }
                }
            }
            "--to" => {
                i += 1;
                match args.get(i).and_then(|s| parse_day(s)) {
                    Some(ms) => to = Some(ms + DAY_MS),
                    None => {
                        eprintln!("stats: --to needs YYYY-MM-DD");
                        std::process::exit(2);
                    }
                }
            }
            "--help" | "-h" => {
                print_stats_help();
                return;
            }
            other => {
                eprintln!("stats: unknown flag {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let now = now_ms();
    let mut from_ms = from.unwrap_or_else(|| day_start_ms(now));
    let mut to_ms = to.unwrap_or_else(|| day_start_ms(now) + DAY_MS);
    if to_ms < from_ms {
        std::mem::swap(&mut from_ms, &mut to_ms);
    }
    if to_ms > now + DAY_MS {
        to_ms = now + DAY_MS;
    }

    let ledger = Ledger::open(&data_dir.join("ledger.db")).unwrap_or_else(|e| {
        eprintln!("stats: open ledger: {e}");
        std::process::exit(1);
    });
    match collect(&ledger, from_ms, to_ms) {
        Ok(report) => {
            if json {
                println!("{}", render_json(&report));
            } else {
                print!("{}", render_human(&report));
            }
        }
        Err(e) => {
            eprintln!("stats: {e}");
            std::process::exit(1);
        }
    }
}

fn print_stats_help() {
    println!("pantheon stats [--week|--month|--from YYYY-MM-DD|--to YYYY-MM-DD] [--json]");
    println!();
    println!("Token and spend totals from the event ledger. Default: today.");
    println!("  --week        last 7 days");
    println!("  --month       last 30 days");
    println!("  --from DATE   window start (inclusive)");
    println!("  --to DATE     window end (inclusive)");
    println!("  --json        machine-readable output");
}
