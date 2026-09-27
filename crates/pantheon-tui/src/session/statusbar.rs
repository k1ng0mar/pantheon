//! Live status bar: one line of real session telemetry.
//!
//! Every value here is either measured from events the runtime already
//! emits (`ModelEvent::Usage`, turn timing) or explicitly unknown. A
//! missing value renders as `—`, never as a fabricated zero: showing
//! `0 tok/s` for a provider that never reported usage would be a lie the
//! user might budget against.

/// Everything the status bar can show. `None` = the runtime did not
/// expose this; the bar renders `—` for it.
#[derive(Debug, Clone)]
pub struct StatusBarData {
    /// e.g. "ready", "working", "esc to interrupt".
    pub status_word: String,
    /// Status glyph + color source, e.g. "✓" / "●" / "!".
    pub icon: String,
    /// e.g. "openai/gpt-4o".
    pub model: String,
    /// Fraction of the context window consumed (0.0..1.0+).
    pub context_frac: Option<f64>,
    /// Human token counts, e.g. "12.4k/128k".
    pub context_label: Option<String>,
    /// This turn's input / output tokens, from the last Usage event.
    pub turn_in: Option<u64>,
    pub turn_out: Option<u64>,
    /// This turn's throughput, tokens/sec.
    pub tokens_per_sec: Option<f64>,
    /// Prompt-cache hit rate. The runtime's `ModelUsage` does not carry
    /// cache counters, so this is currently always `None` → `—`.
    pub cache_hit_rate: Option<f64>,
    /// Completed turns + 1 while a turn runs; `None` before the first turn.
    pub turn_no: Option<u32>,
    /// Short session id prefix.
    pub session_prefix: String,
    /// Session cost in dollars; `None` when the catalog knows no price.
    pub cost_usd: Option<f64>,
}

/// Missing value glyph. One rule, used everywhere: unknown is `—`.
pub const UNKNOWN: &str = "—";

/// 1234 → "1.2k", 999 → "999", 2_500_000 → "2.5M".
pub fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{n}")
    }
}

/// 0.1234 → "12%", 1.5 → "150%".
pub fn fmt_pct(frac: f64) -> String {
    format!("{}%", (frac * 100.0).round() as u64)
}

/// 83.6 → "84 tok/s".
pub fn fmt_rate(tps: f64) -> String {
    format!("{} tok/s", tps.round() as u64)
}

/// 0.042 → "$0.04".
pub fn fmt_cost(usd: f64) -> String {
    format!("${usd:.2}")
}

fn seg(label: &str, value: Option<String>) -> String {
    format!("{label} {}", value.unwrap_or_else(|| UNKNOWN.to_string()))
}

/// Render the bar as plain text, segments joined with `│`, truncated to
/// `width` cells with an ellipsis. Pure: no terminal, no time.
pub fn render(data: &StatusBarData, width: usize) -> String {
    let ctx = match (data.context_frac, data.context_label.as_deref()) {
        (Some(f), Some(l)) => Some(format!("{} · {l}", fmt_pct(f))),
        (Some(f), None) => Some(fmt_pct(f)),
        (None, Some(l)) => Some(l.to_string()),
        (None, None) => None,
    };
    let turn = match (data.turn_in, data.turn_out) {
        (Some(i), Some(o)) => Some(format!("in {} out {}", fmt_count(i), fmt_count(o))),
        _ => None,
    };
    let parts = vec![
        format!("{} {}", data.icon, data.status_word),
        seg("ctx", ctx),
        seg("turn", turn),
        seg("rate", data.tokens_per_sec.map(fmt_rate)),
        seg("cache", data.cache_hit_rate.map(fmt_pct)),
        data.model.clone(),
        seg("t", data.turn_no.map(|n| format!("{n}"))),
        seg("cost", data.cost_usd.map(fmt_cost)),
        format!("sess {}", data.session_prefix),
    ];
    let mut line = parts.join("  │  ");
    if width > 0 {
        let w = line.chars().count();
        if w > width {
            let keep = width.saturating_sub(1);
            line = line.chars().take(keep).collect::<String>() + "…";
        }
    }
    line
}
