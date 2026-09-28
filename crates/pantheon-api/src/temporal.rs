//! Tacit temporal awareness: time, but only when it matters.
//!
//! The model is never timestamped per message. Before a turn's first model
//! call, the pipeline measures how long the conversation has been idle —
//! from the durable ledger, so the reading is restart-safe by construction —
//! and when the gap is meaningful it appends one coarse, human-friendly
//! hint to the outgoing user message. The hint is ephemeral: it rides the
//! API call only, is never written to the ledger or the transcript, rides
//! the user turn (never the system prompt, so prompt caching is unaffected),
//! and gets coarser as the gap grows.
//!
//! All decision logic here is pure ([`temporal_hint`]): hand it the last
//! assistant turn's ledger timestamp, now, a timezone, and the config, and
//! it returns the hint or `None`. The pipeline hook in pantheon-runtime
//! only wires ledger reads around it and fails open.

use chrono::TimeZone;
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_min_gap_secs() -> u64 {
    7200
}

/// `[temporal]` behavior knobs. Zero tokens by construction — the hint is
/// pure string injection — so the feature defaults to on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalConfig {
    /// Master switch. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Idle seconds before an elapsed-gap hint fires. Default 7200 (2h).
    /// `0` disables the elapsed-gap trigger; the date-rollover trigger
    /// still works.
    #[serde(default = "default_min_gap_secs")]
    pub min_gap_secs: u64,
    /// Fire a hint when the local calendar date rolled over since the
    /// last turn, even on a short gap. Default true.
    #[serde(default = "default_true")]
    pub notify_date_change: bool,
    /// IANA timezone name, e.g. `"Africa/Lagos"`. Absent = the system
    /// local timezone; unparseable = system local, then UTC. Never fails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl Default for TemporalConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_gap_secs: 7200,
            notify_date_change: true,
            timezone: None,
        }
    }
}

/// Resolve the effective timezone: the explicit config name wins, then
/// the OS local zone, then UTC. Total — never fails, never panics.
pub fn resolve_tz(cfg: &TemporalConfig) -> Tz {
    if let Some(name) = cfg.timezone.as_deref() {
        if let Ok(tz) = name.parse::<Tz>() {
            return tz;
        }
    }
    if let Ok(name) = iana_time_zone::get_timezone() {
        if let Ok(tz) = name.parse::<Tz>() {
            return tz;
        }
    }
    Tz::UTC
}

/// Coarse inner sentence for an elapsed gap, e.g.
/// `"about 5 hours have passed"`. Deliberately imprecise — never exact
/// wall-clock time, and coarser as the gap grows.
fn gap_sentence(gap_secs: i64) -> String {
    const MIN: i64 = 60;
    const HOUR: i64 = 3600;
    const DAY: i64 = 86400;
    if gap_secs < 90 * MIN {
        // Only reachable when the operator lowers `min_gap_secs`; with
        // the default the elapsed trigger already implies hours or more.
        let m = (gap_secs as f64 / MIN as f64).round().max(1.0) as i64;
        format!("about {m} minutes have passed")
    } else if gap_secs < DAY {
        let h = (gap_secs as f64 / HOUR as f64).round().max(1.0) as i64;
        format!("about {h} hours have passed")
    } else if gap_secs < 2 * DAY {
        "about a day has passed".to_string()
    } else if gap_secs < 7 * DAY {
        let d = (gap_secs as f64 / DAY as f64).round().max(2.0) as i64;
        format!("about {d} days have passed")
    } else if gap_secs < 14 * DAY {
        "about a week has passed".to_string()
    } else if gap_secs < 30 * DAY {
        let w = (gap_secs as f64 / (7 * DAY) as f64).round().max(2.0) as i64;
        format!("about {w} weeks have passed")
    } else if gap_secs < 60 * DAY {
        "about a month has passed".to_string()
    } else {
        let mo = (gap_secs as f64 / (30 * DAY) as f64).round().max(2.0) as i64;
        format!("about {mo} months have passed")
    }
}

/// Decide whether the idle gap since the last assistant turn deserves a
/// hint. Pure: no I/O, no clock reads, trivially testable.
///
/// `last_assistant_ts_ms` is `None` for a new session (or when the caller
/// could not find a prior assistant turn) — always silent. A non-positive
/// gap (clock skew, same-millisecond turns) is silent too.
///
/// When both triggers fire, the elapsed-gap wording wins: a multi-day gap
/// already implies the date changed, so the wordings never stack.
pub fn temporal_hint(
    last_assistant_ts_ms: Option<i64>,
    now_ms: i64,
    tz: &Tz,
    cfg: &TemporalConfig,
) -> Option<String> {
    if !cfg.enabled {
        return None;
    }
    let last_ms = last_assistant_ts_ms?;
    let gap_ms = now_ms.saturating_sub(last_ms);
    if gap_ms <= 0 {
        return None;
    }
    let gap_secs = gap_ms / 1000;

    // Elapsed-gap trigger.
    if cfg.min_gap_secs > 0 && gap_secs >= cfg.min_gap_secs as i64 {
        return Some(format!(
            "[temporal: {} since the previous exchange]",
            gap_sentence(gap_secs)
        ));
    }
    // Date-rollover trigger: the local calendar date changed even though
    // the gap is short. Only reachable when the elapsed trigger stayed
    // silent, so a sub-`min_gap_secs` gap implies at most one midnight and
    // "yesterday" is literally correct.
    if cfg.notify_date_change {
        let last_day = tz
            .timestamp_millis_opt(last_ms)
            .single()
            .map(|dt| dt.date_naive());
        let now_day = tz
            .timestamp_millis_opt(now_ms)
            .single()
            .map(|dt| dt.date_naive());
        if let (Some(a), Some(b)) = (last_day, now_day) {
            if a != b {
                return Some("[temporal: the previous exchange was yesterday]".to_string());
            }
        }
    }
    None
}
