//! `pantheon stats`: usage rows persist to the ledger and aggregate into
//! totals with by-model, by-session, and by-day breakdowns.
//!
//! Behavioral tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_api::events::Event;
use pantheon_storage::Ledger;
use pantheon_tui::stats::{self, DAY_MS};

fn usage(run_id: &str, model: &str, input: u64, output: u64, cost: Option<f64>) -> Event {
    Event::UsageRecorded {
        run_id: run_id.into(),
        model: model.into(),
        provider: "anthropic".into(),
        input_tokens: input,
        output_tokens: output,
        total_tokens: input + output,
        cost_usd: cost,
    }
}

fn seed() -> Ledger {
    let ledger = Ledger::open_in_memory().expect("in-memory ledger");
    ledger
        .append(&Event::RunStarted {
            run_id: "run_a".into(),
        })
        .unwrap();
    ledger
        .append(&Event::SessionTitled {
            run_id: "run_a".into(),
            title: "alpha task".into(),
            model: "m".into(),
            source: "test".into(),
        })
        .unwrap();
    ledger
        .append(&usage(
            "run_a",
            "anthropic:claude-opus",
            100,
            50,
            Some(0.01),
        ))
        .unwrap();
    ledger
        .append(&usage(
            "run_a",
            "anthropic:claude-opus",
            200,
            100,
            Some(0.02),
        ))
        .unwrap();
    ledger
        .append(&usage("run_b", "openai:gpt-5", 1000, 500, None))
        .unwrap();
    ledger
}

#[test]
fn stats_aggregate_totals_and_breakdowns() {
    let ledger = seed();
    let now = stats::now_ms();
    let report = stats::collect(&ledger, now - DAY_MS, now + DAY_MS).expect("collect");

    assert_eq!(report.totals.calls, 3);
    assert_eq!(report.totals.input_tokens, 1300);
    assert_eq!(report.totals.output_tokens, 650);
    assert_eq!(report.totals.total_tokens, 1950);
    assert!((report.totals.cost_usd - 0.03).abs() < 1e-9);
    assert_eq!(report.totals.priced_calls, 2);

    let models: Vec<&str> = report.by_model.iter().map(|m| m.model.as_str()).collect();
    assert!(models.contains(&"anthropic:claude-opus"));
    assert!(models.contains(&"openai:gpt-5"));
    let opus = report
        .by_model
        .iter()
        .find(|m| m.model == "anthropic:claude-opus")
        .unwrap();
    assert_eq!(opus.totals.calls, 2);
    assert_eq!(opus.totals.total_tokens, 450);

    let runs: Vec<&str> = report.by_run.iter().map(|r| r.run_id.as_str()).collect();
    assert!(runs.contains(&"run_a"));
    assert!(runs.contains(&"run_b"));
    let run_a = report.by_run.iter().find(|r| r.run_id == "run_a").unwrap();
    assert_eq!(run_a.title.as_deref(), Some("alpha task"));
    assert_eq!(run_a.totals.calls, 2);
}

#[test]
fn stats_empty_window_reports_zero() {
    let ledger = seed();
    // Long before any seeded row (append uses current time).
    let report = stats::collect(&ledger, 0, 1_000).expect("collect");
    assert_eq!(report.totals.calls, 0);
    assert!(report.by_model.is_empty());
    assert!(report.by_run.is_empty());
}

#[test]
fn stats_day_helpers_are_consistent() {
    // 2026-09-28 00:00:00 UTC = 1790553600000 ms.
    let day = stats::parse_day("2026-09-28").expect("parses");
    assert_eq!(day, 1_790_553_600_000);
    assert_eq!(stats::day_label(day), "2026-09-28");
    assert_eq!(stats::day_label(day + 12 * 3_600_000), "2026-09-28");
    assert_eq!(stats::day_start_ms(day + 3_600_000), day);
    assert!(stats::parse_day("not-a-date").is_none());
    assert!(stats::parse_day("2026-13-01").is_none());
}

#[test]
fn stats_json_is_machine_readable() {
    let ledger = seed();
    let now = stats::now_ms();
    let report = stats::collect(&ledger, now - DAY_MS, now + DAY_MS).expect("collect");
    let json = stats::render_json(&report);
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(v["totals"]["calls"], 3);
    assert_eq!(v["totals"]["total_tokens"], 1950);
    assert_eq!(v["by_model"].as_array().unwrap().len(), 2);
    assert_eq!(v["by_session"].as_array().unwrap().len(), 2);
    assert!(!v["by_day"].as_array().unwrap().is_empty());
}

#[test]
fn stats_human_render_mentions_key_figures() {
    let ledger = seed();
    let now = stats::now_ms();
    let report = stats::collect(&ledger, now - DAY_MS, now + DAY_MS).expect("collect");
    let text = stats::render_human(&report);
    assert!(text.contains("3 calls"));
    assert!(text.contains("anthropic:claude-opus"));
    assert!(text.contains("by session:"));
    assert!(text.contains("by day:"));

    let summary = stats::render_session_summary(&report, "run_a");
    assert!(summary.contains("this session: 2 calls"));
    let missing = stats::render_session_summary(&report, "run_unknown");
    assert!(missing.contains("no usage today"));
}
