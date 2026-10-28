//! Shared number formatting for the token lines.
//!
//! The module used to carry a "live status bar" surface
//! (`StatusBarData` + `render`), but no caller ever displayed it: the
//! footer and sidebar render their own token lines from
//! [`fmt_count`] alone. The dead surface was deleted rather than left
//! documented as a live feature; if a status bar is wanted later it
//! should be built from the footer token line, not revived from here.

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
