//! Nightly-gated proactive ideas (the Ideas page backend).
//!
//! Two flavors, both deterministic and rule-based:
//!
//! - **General ideas** - repair opportunities and observations from the
//!   pass: repair targets the repair phase contained but could not fix,
//!   and tools that failed repeatedly in the scan window.
//! - **Suggested scheduled tasks** - mined from repeating session
//!   patterns: a tool sequence run on several separate days becomes a
//!   "you do X most mornings - want a daily task?" proposal, with the
//!   cron set to the median hour (UTC) the sequence ran.
//!
//! Daily refresh: at most one batch per calendar day (a second pass the
//! same day mints nothing), and unanswered `pending` ideas older than
//! [`PENDING_KEEP_DAYS`] roll off. Feedback tuning: a topic the user
//! dismissed twice with no accept is never proposed again.
//!
//! This phase runs only inside an enabled pass - `run_pass` is never
//! invoked when the nightly master switch is off, so no second flag is
//! invented here.

use crate::repair_targets::{RepairOutcome, RepairReport};
use crate::signals::Signal;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_storage::{IdeaKind, IdeaStore, NewIdea, ScheduleSpec, TopicSignals};

/// Max ideas minted per calendar day.
pub const MAX_IDEAS_PER_DAY: usize = 5;
/// Unanswered pending ideas older than this many days roll off.
pub const PENDING_KEEP_DAYS: i64 = 3;
/// A topic dismissed this many times (with zero accepts) is downranked:
/// generation skips it.
const DISMISS_DOWNRANK: i64 = 2;
/// Repeated-failure signals below this count are noise, not an idea.
const MIN_FAILURE_COUNT: usize = 3;
/// A sequence must span this many distinct days to count as a habit
/// worth scheduling (one burst is not a routine).
const MIN_HABIT_DAYS: usize = 2;

fn err(cause: String) -> PantheonError {
    PantheonError::new(
        "NLY_IDEA",
        Layer::Runtime,
        false,
        cause,
        "check the data dir and ideas.db health (`pantheon doctor`)",
        String::new(),
    )
}

/// One generated idea, before the store assigns its id.
#[derive(Debug, Clone)]
pub struct GeneratedIdea {
    pub id: String,
    pub title: String,
    pub description: String,
    pub includes: Vec<String>,
    pub kind: IdeaKind,
    pub topic: String,
    pub schedule: Option<ScheduleSpec>,
}

// ---------------------------------------------------------------------------
// Calendar helpers (pure, UTC, no chrono dependency)
// ---------------------------------------------------------------------------

/// `YYYY-MM-DD` (UTC) for an epoch-millis timestamp.
pub fn today_utc(at_ms: i64) -> String {
    let (y, m, d) = civil_from_days(at_ms.div_euclid(86_400_000));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Add (or subtract) days from a `YYYY-MM-DD` date. `None` on bad input.
pub fn add_days(day: &str, delta: i64) -> Option<String> {
    let (y, m, d) = parse_day(day)?;
    let (y, m, d) = civil_from_days(days_from_civil(y, m, d)? + delta);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

fn parse_day(day: &str) -> Option<(i64, u32, u32)> {
    let mut parts = day.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

// Howard Hinnant's civil <-> days algorithms.
fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Median hour of day (UTC, 0-23) across timestamps. Lower median on
/// even counts; deterministic.
pub fn median_hour_utc(timestamps_ms: &[i64]) -> u32 {
    let mut hours: Vec<u32> = timestamps_ms
        .iter()
        .map(|ts| (ts.div_euclid(3_600_000) % 24) as u32)
        .collect();
    hours.sort_unstable();
    hours.get(hours.len() / 2).copied().unwrap_or(0)
}

/// Distinct UTC calendar days spanned by timestamps.
pub fn distinct_days(timestamps_ms: &[i64]) -> usize {
    let mut days: Vec<i64> = timestamps_ms
        .iter()
        .map(|ts| ts.div_euclid(86_400_000))
        .collect();
    days.sort_unstable();
    days.dedup();
    days.len()
}

/// Feedback tuning: the user keeps dismissing this topic and never
/// accepted it - stop proposing it.
pub fn downranked(signals: &TopicSignals) -> bool {
    signals.dismissed >= DISMISS_DOWNRANK && signals.accepted == 0
}

// ---------------------------------------------------------------------------
// Candidate builders (pure)
// ---------------------------------------------------------------------------

fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// A repair target the pass contained but could not fix: suggest the
/// user finish the job by hand.
pub fn general_from_repair(report: &RepairReport) -> Option<GeneratedIdea> {
    match report.outcome {
        RepairOutcome::Contained => {}
        // Repaired/healthy need no idea; dry-run detections are audited
        // elsewhere and must not mint user-facing ideas.
        _ => return None,
    }
    let topic = format!("repair:{}", slug(&report.target));
    Some(GeneratedIdea {
        id: String::new(),
        title: format!("Repair couldn't finish: {}", report.target),
        description: format!(
            "The nightly repair pass tried to fix {} but could only contain it. {}",
            report.target,
            truncate(&report.detail, 300)
        ),
        includes: vec![
            "Read the nightly repair report for the failed step".into(),
            "Fix the root cause by hand (config, credentials, path)".into(),
            "Re-enable or unpause the target once it works".into(),
        ],
        kind: IdeaKind::General,
        topic,
        schedule: None,
    })
}

/// A tool that failed repeatedly in the scan window.
pub fn general_from_failure(tool: &str, count: usize) -> Option<GeneratedIdea> {
    if count < MIN_FAILURE_COUNT {
        return None;
    }
    Some(GeneratedIdea {
        id: String::new(),
        title: format!("The \"{tool}\" tool keeps failing"),
        description: format!(
            "{count} failed \"{tool}\" invocations in the last scan window. \
             Something about its setup may be broken."
        ),
        includes: vec![
            "Look at the recent error output for the tool".into(),
            "Fix the underlying cause (config, credentials, path)".into(),
            "Dismiss this idea if the failures are expected".into(),
        ],
        kind: IdeaKind::General,
        topic: format!("fail:{}", slug(tool)),
        schedule: None,
    })
}

/// A tool sequence run like a habit: propose it as a scheduled task at
/// the median hour (UTC) it ran. `None` when it is a one-off burst.
pub fn scheduled_from_sequence(
    tools: &[String],
    hits: &[crate::signals::TurnRef],
) -> Option<GeneratedIdea> {
    if hits.len() < MIN_FAILURE_COUNT {
        return None;
    }
    let ts: Vec<i64> = hits.iter().map(|h| h.ts_ms).collect();
    if distinct_days(&ts) < MIN_HABIT_DAYS {
        return None;
    }
    let hour = median_hour_utc(&ts);
    let days = distinct_days(&ts);
    let seq = tools.join(" → ");
    let prompt = format!(
        "Run the recurring \"{seq}\" workflow you usually do around {hour:02}:00 UTC and report the result."
    );
    Some(GeneratedIdea {
        id: String::new(),
        title: format!("Recurring task: {seq}"),
        description: format!(
            "You've run this sequence on {days} separate days, mostly around {hour:02}:00 UTC. \
             A scheduled task could run it for you."
        ),
        includes: vec![
            "Review the proposed daily schedule".into(),
            "Accept to create the scheduled task".into(),
            "Dismiss if this was a one-off burst".into(),
        ],
        kind: IdeaKind::ScheduledTask,
        topic: format!(
            "seq:{}",
            tools.iter().map(|t| slug(t)).collect::<Vec<_>>().join("+")
        ),
        schedule: Some(ScheduleSpec {
            cron: format!("0 {hour} * * *"),
            deliver: "home".into(),
            prompt,
        }),
    })
}

// ---------------------------------------------------------------------------
// The phase
// ---------------------------------------------------------------------------

/// Mint today's ideas: expire stale pending ideas, then generate one
/// batch per calendar day. Returns the number minted. A no-op in dry-run
/// mode (the pass mutates nothing then).
pub fn run_ideas_phase(
    store: &IdeaStore,
    signals: &[Signal],
    repairs: &[RepairReport],
    dry_run: bool,
    today: &str,
    _now_ms: i64,
) -> Result<usize, PantheonError> {
    if dry_run {
        return Ok(0);
    }
    // Daily refresh, first half: unanswered pending ideas older than
    // PENDING_KEEP_DAYS roll off instead of piling up forever.
    let cutoff = add_days(today, -PENDING_KEEP_DAYS)
        .ok_or_else(|| err(format!("bad day string '{today}'")))?;
    store.expire_pending_before(&cutoff)?;
    // Daily refresh, second half: one batch per day; a second pass the
    // same day mints nothing.
    if store.has_day(today)? {
        return Ok(0);
    }

    let mut candidates: Vec<GeneratedIdea> = Vec::new();
    for report in repairs {
        if let Some(g) = general_from_repair(report) {
            candidates.push(g);
        }
    }
    for signal in signals {
        match signal {
            Signal::RepeatedFailure { tool, count, .. } => {
                if let Some(g) = general_from_failure(tool, *count) {
                    candidates.push(g);
                }
            }
            Signal::RepeatedSequence { tools, hits } => {
                if let Some(g) = scheduled_from_sequence(tools, hits) {
                    candidates.push(g);
                }
            }
            // User corrections, denied approvals, and preferences already
            // feed the memory/persona pipelines; ideas must not duplicate
            // them.
            _ => {}
        }
    }

    let day_tag = today.replace('-', "");
    let mut minted = 0usize;
    for (i, mut g) in candidates.into_iter().enumerate() {
        if minted >= MAX_IDEAS_PER_DAY {
            break;
        }
        // Feedback tuning: skip topics the user keeps dismissing, and
        // never pile up a second pending idea for an unanswered topic.
        if downranked(&store.topic_signals(&g.topic)?) {
            continue;
        }
        if store.pending_topic_exists(&g.topic)? {
            continue;
        }
        g.id = format!("idea_{day_tag}_{i}");
        let new = NewIdea {
            id: g.id.clone(),
            title: g.title,
            description: g.description,
            includes: g.includes,
            kind: g.kind,
            topic: g.topic,
            created_day: today.to_string(),
            schedule: g.schedule,
        };
        if store.mint(&new)? {
            minted += 1;
        }
    }
    Ok(minted)
}
