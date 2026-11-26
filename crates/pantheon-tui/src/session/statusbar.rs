//! number formatting for the token lines. footer and sidebar render their own
//! lines from [`fmt_count`]; a status bar surface belongs in the footer token
//! line, not here

/// 1234 → "1.2k", 999 → "999", 2_500_000 → "2.5M"
pub fn fmt_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{n}")
    }
}
