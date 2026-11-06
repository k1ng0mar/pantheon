//! Scheduler merged into one file. External paths keep working through the
//! crate-root shims in lib.rs (same pattern as the providers merge).

pub mod cron {
//! Cron expressions for durable jobs (§21).
//!
//! Dependency-free by design: a job's fire decision must be a pure function
//! of the wall clock, so it can be tested without a clock, a timezone
//! database, or a scheduler running. Times are UTC; a job that cares about
//! local wall-clock belongs to a context policy, not to the expression.
//!
//! Supports the standard five fields - minute, hour, day-of-month, month,
//! day-of-week - with `*`, `a`, `a-b`, `*/n`, `a-b/n`, and comma lists.
//! Day-of-week accepts 0 or 7 for Sunday, and follows Vixie cron's rule:
//! when both day fields are restricted, either one matching is enough.

use std::fmt;

/// Maximum number of minutes between two consecutive occurrences of any
/// valid five-field cron expression.
///
/// The largest gap is February 29th across a non-leap century (e.g. 2096 ->
/// 2104, since 2100 is not a leap year): 2922 days = 4,207,680 minutes.
/// A scan bound smaller than this misses a leap-day occurrence when the
/// clock has moved past it (catch-up returns nothing; status reports no
/// next fire), so both the forward and backward scans use this bound.
pub const MAX_CRON_GAP_MINUTES: i64 = 4_207_680;

/// Same bound expressed in days (2922 = 8 years), plus a small margin so a
/// scan starting mid-gap still reaches the far occurrence. Day-level
/// scanning is what keeps this bound cheap: at most ~2932 iterations
/// instead of 4.2M minute-by-minute steps.
const MAX_CRON_GAP_DAYS: i64 = 2932;

/// A parsed five-field expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Field,
    hour: Field,
    day_of_month: Field,
    month: Field,
    day_of_week: Field,
    /// Sorted allowed minute-of-day values (hour * 60 + minute). The hour
    /// and minute fields are independent, so every value here is a match
    /// on the time part; the calendar fields are checked per day. Built at
    /// parse time so the fire scans step a day at a time and then binary/
    /// linear-search at most 1440 values instead of scanning every minute.
    minute_of_day: Vec<u32>,
}

/// Why an expression was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronError {
    pub field: &'static str,
    pub cause: String,
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid cron {} field: {}", self.field, self.cause)
    }
}

impl std::error::Error for CronError {}

/// Wall-clock fields for one instant, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    /// 0 = Sunday .. 6 = Saturday.
    pub weekday: u32,
}

impl CronSchedule {
    /// Parse `minute hour day-of-month month day-of-week`.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(CronError {
                field: "expression",
                cause: format!("expected 5 fields, found {}", parts.len()),
            });
        }
        let minute = Field::parse(parts[0], "minute", 0, 59)?;
        let hour = Field::parse(parts[1], "hour", 0, 23)?;
        let day_of_month = Field::parse(parts[2], "day-of-month", 1, 31)?;
        let month = Field::parse(parts[3], "month", 1, 12)?;
        let day_of_week = Field::parse(parts[4], "day-of-week", 0, 7)?.sunday_normalized();
        // The hour and minute fields are independent, so the allowed
        // minute-of-day set is their product; iterating hour outer and
        // minute inner keeps it sorted.
        let minute_of_day: Vec<u32> = hour
            .allowed
            .iter()
            .flat_map(|&h| minute.allowed.iter().map(move |&m| h * 60 + m))
            .collect();
        Ok(Self {
            minute,
            hour,
            day_of_month,
            month,
            day_of_week,
            minute_of_day,
        })
    }

    /// Validate an expression without keeping the parsed schedule.
    ///
    /// This is the registration-time gate: a broken expression must be
    /// rejected when the job is created, never stored as a job that
    /// silently never fires.
    pub fn validate(expr: &str) -> Result<(), CronError> {
        Self::parse(expr).map(|_| ())
    }

    /// Does this expression select the given minute?
    pub fn matches(&self, at: CivilTime) -> bool {
        self.minute.matches(at.minute)
            && self.hour.matches(at.hour)
            && self.day_matches(at.month, at.day, at.weekday)
    }

    /// Calendar half of [`matches`]: month plus the Vixie day-of-month /
    /// day-of-week rule. No hour or minute involved.
    fn day_matches(&self, month: u32, day: u32, weekday: u32) -> bool {
        if !self.month.matches(month) {
            return false;
        }
        let dom = self.day_of_month.matches(day);
        let dow = self.day_of_week.matches(weekday);
        match (
            self.day_of_month.is_wildcard(),
            self.day_of_week.is_wildcard(),
        ) {
            (true, true) => true,
            (false, true) => dom,
            (true, false) => dow,
            // Vixie cron: both restricted means "either matches".
            (false, false) => dom || dow,
        }
    }

    /// Does this expression select the minute containing `now_ms`?
    pub fn matches_ms(&self, now_ms: i64) -> bool {
        self.matches(civil_from_ms(now_ms))
    }

    /// Latest minute boundary ≤ now_ms whose wall-clock fields match.
    ///
    /// Scans one day at a time (calendar fields) and then picks the largest
    /// allowed minute-of-day within the day, so the worst case is
    /// [`MAX_CRON_GAP_DAYS`] day steps instead of 4.2M minute steps. Used
    /// for missed-occurrence catch-up.
    pub fn prev_fire_ms(&self, now_ms: i64) -> Option<i64> {
        let now_min = now_ms.div_euclid(60_000);
        let first_day = now_min.div_euclid(1440);
        let first_minute_of_day = now_min.rem_euclid(1440) as u32;
        ((first_day - MAX_CRON_GAP_DAYS)..=first_day)
            .rev()
            .enumerate()
            .find_map(|(step, day)| {
                let (_, m, d) = civil_from_days(day);
                let wd = (day + 4).rem_euclid(7) as u32;
                if !self.day_matches(m, d, wd) {
                    return None;
                }
                // On the first day only minutes at or before `now` count;
                // every later day allows the whole day.
                let limit = if step == 0 { first_minute_of_day } else { 1439 };
                self.minute_of_day
                    .iter()
                    .rev()
                    .find(|&&v| v <= limit)
                    .map(|&mom| (day * 1440 + mom as i64) * 60_000)
            })
    }

    /// Earliest minute boundary > now_ms whose wall-clock fields match.
    /// Same day-level scan as [`prev_fire_ms`], walking forward. Used for
    /// status displays.
    pub fn next_fire_ms(&self, now_ms: i64) -> Option<i64> {
        let now_min = now_ms.div_euclid(60_000);
        let first_day = now_min.div_euclid(1440);
        let first_minute_of_day = now_min.rem_euclid(1440) as u32;
        (first_day..=first_day + MAX_CRON_GAP_DAYS)
            .enumerate()
            .find_map(|(step, day)| {
                let (_, m, d) = civil_from_days(day);
                let wd = (day + 4).rem_euclid(7) as u32;
                if !self.day_matches(m, d, wd) {
                    return None;
                }
                // Strictly after `now`: skip the current minute on day one.
                let start = if step == 0 {
                    first_minute_of_day + 1
                } else {
                    0
                };
                self.minute_of_day
                    .iter()
                    .find(|&&v| v >= start)
                    .map(|&mom| (day * 1440 + mom as i64) * 60_000)
            })
    }
}

/// Convert epoch milliseconds to UTC wall-clock fields.
pub fn civil_from_ms(ms: i64) -> CivilTime {
    let minute_of_day = ms.div_euclid(60_000);
    let days = minute_of_day.div_euclid(1440);
    let minute = minute_of_day.rem_euclid(1440);
    let (year, month, day) = civil_from_days(days);
    CivilTime {
        year,
        month,
        day,
        hour: (minute / 60) as u32,
        minute: (minute % 60) as u32,
        // 1970-01-01 was a Thursday (4).
        weekday: (days + 4).rem_euclid(7) as u32,
    }
}

/// Days since 1970-01-01 -> civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// One cron field: a set of allowed values in `[min, max]`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Field {
    allowed: Vec<u32>,
    range: (u32, u32),
    wildcard: bool,
}

impl Field {
    fn parse(raw: &str, name: &'static str, min: u32, max: u32) -> Result<Self, CronError> {
        let mut allowed: Vec<u32> = Vec::new();
        let mut wildcard = false;
        for item in raw.split(',') {
            let item = item.trim();
            if item.is_empty() {
                return Err(CronError {
                    field: name,
                    cause: "empty list item".into(),
                });
            }
            let (range_part, step) = match item.split_once('/') {
                Some((r, s)) => {
                    let step: u32 = s.parse().map_err(|_| CronError {
                        field: name,
                        cause: format!("bad step '{s}'"),
                    })?;
                    if step == 0 {
                        return Err(CronError {
                            field: name,
                            cause: "step must be > 0".into(),
                        });
                    }
                    (r, step)
                }
                None => (item, 1),
            };
            let (lo, hi) = if range_part == "*" {
                if step == 1 {
                    wildcard = true;
                }
                (min, max)
            } else if let Some((a, b)) = range_part.split_once('-') {
                let a = parse_val(a, name, min, max)?;
                let b = parse_val(b, name, min, max)?;
                if a > b {
                    return Err(CronError {
                        field: name,
                        cause: format!("reversed range '{a}-{b}'"),
                    });
                }
                (a, b)
            } else {
                let v = parse_val(range_part, name, min, max)?;
                (v, v)
            };
            allowed.extend((lo..=hi).step_by(step as usize));
        }
        allowed.sort_unstable();
        allowed.dedup();
        Ok(Self {
            allowed,
            range: (min, max),
            wildcard,
        })
    }

    fn matches(&self, value: u32) -> bool {
        self.allowed.binary_search(&value).is_ok()
    }

    fn is_wildcard(&self) -> bool {
        self.wildcard && self.allowed.len() == (self.range.1 - self.range.0 + 1) as usize
    }

    /// Fold cron's 7-is-Sunday alias onto 0.
    fn sunday_normalized(mut self) -> Self {
        if self.allowed.contains(&7) {
            self.allowed.retain(|v| *v != 7);
            if !self.allowed.contains(&0) {
                self.allowed.push(0);
            }
            self.allowed.sort_unstable();
            // Folding 7 onto 0 shrinks the set by one, so keep `range` in
            // sync: after the fold the canonical day-of-week range is 0-6.
            // Without this, `is_wildcard` stopped recognizing a literal `*`
            // in the day-of-week field (7 values vs a range of 8), the
            // day-of-week arm read as "restricted", and `0 0 1 * *` fell
            // into the dom/dow OR-arm and fired every day instead of on the
            // 1st of the month.
            self.range = (0, 6);
        }
        self
    }
}

fn parse_val(raw: &str, name: &'static str, min: u32, max: u32) -> Result<u32, CronError> {
    let v: u32 = raw.trim().parse().map_err(|_| CronError {
        field: name,
        cause: format!("'{raw}' is not a number"),
    })?;
    if v < min || v > max {
        return Err(CronError {
            field: name,
            cause: format!("{v} outside {min}-{max}"),
        });
    }
    Ok(v)
}

/// Deterministic repair for a cron expression that fails
/// [`CronSchedule::validate`]: normalize common nonstandard shapes into
/// the five-field form this parser accepts, or `None` when no safe
/// normalization exists (the caller must not guess).
///
/// Repairs applied, in order:
/// - `@daily` / `@hourly` / `@weekly` / `@monthly` / `@yearly` /
///   `@annually` → the equivalent five-field expression;
/// - Quartz `?` in any field → `*` (means "no specific value", same as
///   `*` under Vixie semantics used here);
/// - six fields with a leading `0` seconds field → drop the seconds
///   field (a nonzero seconds field is *not* droppable: silently
///   changing the fire minute would be a guess).
///
/// The result is returned unvalidated: the caller re-validates with
/// [`CronSchedule::validate`] and only applies it on success.
pub fn normalize_cron_expr(expr: &str) -> Option<String> {
    let e = expr.trim();
    let lower = e.to_ascii_lowercase();
    let from_macro = match lower.as_str() {
        "@yearly" | "@annually" => Some("0 0 1 1 *"),
        "@monthly" => Some("0 0 1 * *"),
        "@weekly" => Some("0 0 * * 0"),
        "@daily" => Some("0 0 * * *"),
        "@hourly" => Some("0 * * * *"),
        _ => None,
    };
    if let Some(m) = from_macro {
        return Some(m.to_string());
    }
    let mut parts: Vec<&str> = e.split_whitespace().collect();
    if parts.len() == 6 && parts[0] == "0" {
        parts.remove(0);
    }
    if parts.len() != 5 {
        return None;
    }
    let fixed: Vec<String> = parts
        .iter()
        .map(|p| {
            if *p == "?" {
                "*".to_string()
            } else {
                p.to_string()
            }
        })
        .collect();
    let out = fixed.join(" ");
    (out != e).then_some(out)
}
}

pub mod durable {
//! Crash-safe claim ledger (§21).
//!
//! Occurrence claims must survive restarts, or a crash mid-run lets a
//! recovered process fire the same occurrence twice. [`DurableClaimLedger`]
//! makes the claim decision from `pantheon-storage`'s `ClaimStore`, which is
//! the single source of truth: every claim is written with an atomic
//! first-wins INSERT before it is honoured, so a crash mid-run cannot cause
//! a fired job to fire again, and a recovered process needs no separate
//! rebuild step - it just asks the store whether the occurrence was already
//! claimed.

use pantheon_api::error::PantheonError;
use pantheon_storage::ClaimStore;

/// A claim ledger whose claims survive restarts.
///
/// No in-memory copy: the store is the truth, so there is nothing to desync
/// and nothing to hydrate after a restart.
#[derive(Debug)]
pub struct DurableClaimLedger {
    store: ClaimStore,
}

impl DurableClaimLedger {
    /// Open the durable ledger on a fresh `ClaimStore`.
    pub fn new(store: ClaimStore) -> Self {
        Self { store }
    }

    /// Open (creating if missing) on a storage path.
    pub fn open(path: &std::path::Path) -> Result<Self, PantheonError> {
        Ok(Self::new(ClaimStore::open(path)?))
    }

    /// Claim an occurrence. `Ok(true)` means this call won and the run
    /// should start; `Ok(false)` is a replay; `Err` means the claim could
    /// not be persisted and the run must not start.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        self.store.claim(key)
    }

    /// Has this occurrence already been claimed?
    pub fn is_claimed(&self, key: &str) -> Result<bool, PantheonError> {
        self.store.is_claimed(key)
    }

    /// Explicitly reset a claim (admin/migration only).
    ///
    /// The tick loop never releases a claim when a run ends: a finished run
    /// keeps its key so a replayed delivery cannot fire again. Use this only
    /// to deliberately un-block a stuck key; growth is bounded by
    /// [`Self::prune_before`], not by releasing per run.
    pub fn release(&self, key: &str) -> Result<bool, PantheonError> {
        self.store.release(key)
    }

    pub fn prune_before(&self, cutoff_ms: i64) -> Result<usize, PantheonError> {
        self.store.prune_before(cutoff_ms)
    }

    /// Durably record that `job_id` owes one queued fire (the durable
    /// twin of the driver's in-memory queue insert).
    pub fn enqueue_drain(&self, job_id: &str) -> Result<(), PantheonError> {
        self.store.enqueue_drain(job_id)
    }

    /// Drop `job_id`'s owed fire, if any. Called when the drain is taken,
    /// on abandon, and on claim failure - the same points where the
    /// in-memory queue entry is cleared.
    pub fn dequeue_drain(&self, job_id: &str) -> Result<bool, PantheonError> {
        self.store.dequeue_drain(job_id)
    }

    /// Atomically take `job_id`'s owed fire left by a crashed process.
    /// See [`ClaimStore::take_pending_drain`] for the take-before-complete
    /// trade-off.
    pub fn take_pending_drain(&self, job_id: &str) -> Result<bool, PantheonError> {
        self.store.take_pending_drain(job_id)
    }

    /// Number of occurrences currently claimed.
    pub fn len(&self) -> Result<usize, PantheonError> {
        self.store.len()
    }

    pub fn is_empty(&self) -> Result<bool, PantheonError> {
        self.store.is_empty()
    }

    /// All currently claimed keys, sorted.
    pub fn names(&self) -> Result<Vec<String>, PantheonError> {
        self.store.names()
    }
}
}

pub mod repair_budget {
//! Per-task daily repair budget (self-heal spend cap).
//!
//! The self-heal investigator is bounded per incident (12 turns, 24 tool
//! calls, 5 minutes), but nothing bounded how many incidents one flapping
//! task could trigger per day - a task failing every minute would burn API
//! budget on investigator sessions without bound. [`RepairBudget`] caps
//! repair investigations per task per UTC day (default 3, via
//! `[repair].max_repairs_per_task_per_day`); counts persist in
//! `<data_dir>/repair-budget.json` so restarts don't reset them.
//!
//! Wiring (tui `SelfHealer::heal`, other leaf): before spawning the
//! investigator, open the budget on the data dir and call
//! [`RepairBudget::try_consume`] with the task id and the cap from
//! [`RepairSection::repair_cap_per_day`](pantheon_api::config::RepairSection::repair_cap_per_day).
//! Skip the investigation (and surface "daily repair budget exhausted"
//! in the outcome) when it returns false.

use pantheon_api::logging::now_ms;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File name under the data dir.
const FILE_NAME: &str = "repair-budget.json";
/// UTC days of counters to keep: today plus a small tail, so a repair
/// just after midnight UTC doesn't resurrect a pruned counter.
const KEEP_DAYS: i64 = 3;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct BudgetFile {
    /// UTC day ("YYYY-MM-DD") -> task id -> repairs consumed that day.
    days: HashMap<String, HashMap<String, u32>>,
}

/// Per-task daily repair budget, persisted to
/// `<data_dir>/repair-budget.json`.
pub struct RepairBudget {
    path: PathBuf,
    file: BudgetFile,
}

/// UTC `YYYY-MM-DD` for `now_ms`. Howard Hinnant's civil-date algorithm:
/// days since the Unix epoch -> calendar date, no chrono needed for one
/// formatting call.
fn day_string(now_ms: i64) -> String {
    let days = now_ms.div_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

impl RepairBudget {
    /// Open the budget file under `data_dir` (creating parent dirs on
    /// first persist). A missing or corrupt file starts empty - the
    /// fail-open here is deliberate and narrow: [`Self::try_consume`]
    /// fails closed on write errors, so a repair never runs uncounted.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let path = data_dir.join(FILE_NAME);
        let file: BudgetFile = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => BudgetFile::default(),
            Err(e) => {
                return Err(format!(
                    "repair-budget: cannot read {}: {e}",
                    path.display()
                ))
            }
        };
        let mut budget = Self { path, file };
        budget.prune_old_days();
        Ok(budget)
    }

    /// Try to consume one repair investigation for `job_id` against
    /// `max_per_day`. Returns true when the investigation may proceed
    /// the consumption is recorded and persisted. Returns false when the
    /// task already spent its daily budget, or the cap is 0 (repair
    /// investigations disabled). A persistence failure fails closed
    /// (false): a repair must never run uncounted.
    pub fn try_consume(&mut self, job_id: &str, max_per_day: u32, now_ms: i64) -> bool {
        if max_per_day == 0 {
            return false;
        }
        let day = day_string(now_ms);
        let used = self
            .file
            .days
            .get(&day)
            .and_then(|m| m.get(job_id))
            .copied()
            .unwrap_or(0);
        if used >= max_per_day {
            return false;
        }
        self.file
            .days
            .entry(day.clone())
            .or_default()
            .insert(job_id.to_string(), used + 1);
        if self.persist().is_err() {
            // Roll back the in-memory bump: the count must never exceed
            // what survived to disk.
            if used == 0 {
                if let Some(m) = self.file.days.get_mut(&day) {
                    m.remove(job_id);
                    if m.is_empty() {
                        self.file.days.remove(&day);
                    }
                }
            } else if let Some(m) = self.file.days.get_mut(&day) {
                m.insert(job_id.to_string(), used);
            }
            return false;
        }
        true
    }

    /// Repairs consumed by `job_id` today (UTC). Observability for the
    /// "why didn't it heal?" question.
    pub fn used_today(&self, job_id: &str, now_ms: i64) -> u32 {
        let day = day_string(now_ms);
        self.file
            .days
            .get(&day)
            .and_then(|m| m.get(job_id))
            .copied()
            .unwrap_or(0)
    }

    fn persist(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&self.file)
            .map_err(|e| format!("repair-budget: serialize: {e}"))?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("repair-budget: mkdir: {e}"))?;
        }
        // Write-then-rename: a crash mid-write never leaves a torn file.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("repair-budget: write: {e}"))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("repair-budget: rename: {e}"))?;
        Ok(())
    }

    fn prune_old_days(&mut self) {
        // YYYY-MM-DD is zero-padded, so lexicographic order is
        // chronological order.
        let cutoff = day_string(now_ms() - (KEEP_DAYS - 1) * 86_400_000);
        self.file
            .days
            .retain(|day, _| day.as_str() >= cutoff.as_str());
    }
}

#[cfg(test)]
mod repair_budget_tests {
    use super::*;

    // Instants inside the prune window relative to *today*, not fixed
    // constants. `open` prunes days older than `now - KEEP_DAYS`, so a
    // hardcoded date silently rots: these assertions passed when written
    // and started failing days later, with nothing in the diff to explain
    // it. Anchoring to the real clock keeps the window permanent.
    fn day_ms(days_from_today: i64) -> i64 {
        let today = now_ms().div_euclid(86_400_000) * 86_400_000;
        today + days_from_today * 86_400_000
    }

    /// Today, in UTC ms: the newest day the pruner always keeps.
    fn today_ms() -> i64 {
        day_ms(0)
    }

    /// Two days back: still inside `KEEP_DAYS`.
    fn two_days_ago_ms() -> i64 {
        day_ms(-2)
    }

    fn open_temp() -> (RepairBudget, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let budget = RepairBudget::open(dir.path()).unwrap();
        (budget, dir)
    }

    #[test]
    fn consumes_up_to_cap_then_denies() {
        let (mut b, _dir) = open_temp();
        assert!(b.try_consume("task-a", 3, today_ms()));
        assert!(b.try_consume("task-a", 3, today_ms()));
        assert!(b.try_consume("task-a", 3, today_ms()));
        assert!(!b.try_consume("task-a", 3, today_ms()));
        assert_eq!(b.used_today("task-a", today_ms()), 3);
    }

    #[test]
    fn budget_is_per_task() {
        let (mut b, _dir) = open_temp();
        assert!(b.try_consume("task-a", 1, today_ms()));
        assert!(!b.try_consume("task-a", 1, today_ms()));
        assert!(b.try_consume("task-b", 1, today_ms()));
    }

    #[test]
    fn budget_resets_next_utc_day() {
        let (mut b, _dir) = open_temp();
        assert!(b.try_consume("task-a", 1, today_ms()));
        assert!(!b.try_consume("task-a", 1, today_ms()));
        assert!(b.try_consume("task-a", 1, two_days_ago_ms()));
        assert_eq!(b.used_today("task-a", today_ms()), 1);
        assert_eq!(b.used_today("task-a", two_days_ago_ms()), 1);
    }

    #[test]
    fn zero_cap_disables() {
        let (mut b, _dir) = open_temp();
        assert!(!b.try_consume("task-a", 0, today_ms()));
        assert_eq!(b.used_today("task-a", today_ms()), 0);
    }

    #[test]
    fn counts_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let mut b = RepairBudget::open(dir.path()).unwrap();
            assert!(b.try_consume("task-a", 3, today_ms()));
            assert!(b.try_consume("task-a", 3, today_ms()));
        }
        // A restart must not reset the budget.
        let mut b2 = RepairBudget::open(dir.path()).unwrap();
        assert_eq!(b2.used_today("task-a", today_ms()), 2);
        assert!(b2.try_consume("task-a", 3, today_ms()));
        assert!(!b2.try_consume("task-a", 3, today_ms()));
    }

    #[test]
    fn corrupt_file_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), b"not json{{{").unwrap();
        let mut b = RepairBudget::open(dir.path()).unwrap();
        assert!(b.try_consume("task-a", 3, today_ms()));
    }

    #[test]
    fn day_string_matches_known_dates() {
        // Fixed instants here on purpose: this test pins the civil-date
        // algorithm against known calendar days, so the expectations must
        // not move with the wall clock.
        assert_eq!(day_string(0), "1970-01-01");
        assert_eq!(day_string(1_790_812_800_000), "2026-10-01");
        assert_eq!(day_string(1_790_899_200_000), "2026-10-02");
        // A leap day, to catch a wrong days-in-month handling.
        assert_eq!(day_string(1_709_164_800_000), "2024-02-29");
    }
}
}

pub mod retention {
//! Event retention for the scheduler daemon.
//!
//! This is the single implementation of the retention pass: the gateway
//! daemon's tick loop calls [`maybe_run_retention`], and the TUI's
//! `schedule tick --watch` loop and `schedule prune` route through this
//! module too (see `pantheon-tui/src/schedule.rs`). There used to be a
//! second copy in the TUI; it was retired in favour of this one so a
//! change to the prune rule can no longer drift between the two tick
//! loops. The 24h gate is tracked in `<data_dir>/retention.json`, shared
//! by both callers, and active runs are never pruned.
//!
//! Wire-up: the gateway daemon should call [`maybe_run_retention`]
//! from its tick loop (e.g. `pantheon-gateway/src/daemon.rs`).

use crate::durable::DurableClaimLedger;
use pantheon_api::config::{Config, DEFAULT_RETENTION_DAYS};
use pantheon_api::logging::now_ms;
use std::path::{Path, PathBuf};

/// At most one retention pass per interval; the last-pass timestamp
/// lives in `<data_dir>/retention.json`.
const RETENTION_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

/// What one retention pass pruned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RetentionReport {
    pub keep_days: u32,
    pub events_pruned: usize,
    pub search_chunks_pruned: usize,
    pub claims_pruned: usize,
}

/// Run the retention pass now: prune ledger events, the FTS search
/// sidecar, and idempotency claims older than `keep_days`. Runs whose
/// status is not terminal are never pruned, however old their events
/// the active run's transcript is what a resume rebuilds from.
pub fn run_retention(data_dir: &Path, keep_days: u32) -> Result<RetentionReport, String> {
    let cutoff = now_ms() - keep_days as i64 * 86_400_000;
    let mut report = RetentionReport {
        keep_days,
        ..Default::default()
    };

    let ledger_path = data_dir.join("ledger.db");
    if ledger_path.exists() {
        let ledger = pantheon_storage::ledger::Ledger::open(&ledger_path)
            .map_err(|e| format!("retention: cannot open ledger: {e}"))?;
        report.events_pruned = ledger
            .prune_events_before_active_safe(cutoff)
            .map_err(|e| format!("retention: prune events: {e}"))?;
        // The FTS sidecar lives in the same database file.
        match pantheon_storage::search::SessionSearch::open(&ledger_path) {
            Ok(search) => {
                report.search_chunks_pruned = search
                    .prune_before(cutoff)
                    .map_err(|e| format!("retention: prune search index: {e}"))?;
            }
            Err(e) => eprintln!("retention: search index unavailable, skipping: {e}"),
        }
    }
    let claims_path = data_dir.join("claims.db");
    if claims_path.exists() {
        let claims = DurableClaimLedger::open(&claims_path)
            .map_err(|e| format!("retention: cannot open claim ledger: {e}"))?;
        report.claims_pruned = claims
            .prune_before(cutoff)
            .map_err(|e| format!("retention: prune claims: {e}"))?;
    }
    Ok(report)
}

fn retention_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("retention.json")
}

/// True when a retention pass is due: never ran, or the last pass is
/// older than `RETENTION_INTERVAL_MS`.
fn retention_due(data_dir: &Path) -> bool {
    let last: i64 = std::fs::read_to_string(retention_state_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("last_prune_ms")?.as_i64())
        .unwrap_or(0);
    now_ms() - last >= RETENTION_INTERVAL_MS
}

/// Record that a retention pass just ran (the 24h gate). Public so the
/// TUI's manual `schedule prune` can stamp the same shared gate after
/// routing its prune through [`run_retention`].
pub fn record_retention_pass(data_dir: &Path) {
    let state = serde_json::json!({ "last_prune_ms": now_ms() });
    let _ = std::fs::write(retention_state_path(data_dir), state.to_string());
}

/// The automatic pass, for the gateway daemon's tick loop. No-op when
/// retention is disabled (`keep_days = 0`) or the last pass is fresh.
/// Logs what was pruned.
pub fn maybe_run_retention(data_dir: &Path) {
    let keep_days = Config::load_or_report(data_dir)
        .map(|c| c.retention_days())
        .unwrap_or(DEFAULT_RETENTION_DAYS);
    if keep_days == 0 || !retention_due(data_dir) {
        return;
    }
    match run_retention(data_dir, keep_days) {
        Ok(r) => {
            record_retention_pass(data_dir);
            println!(
                "retention: pruned {} events, {} search chunks, {} claims older than {}d (active runs kept)",
                r.events_pruned, r.search_chunks_pruned, r.claims_pruned, r.keep_days
            );
        }
        Err(e) => eprintln!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_api::events::Event;
    use pantheon_storage::ledger::Ledger;

    fn data_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// Empty data dir: nothing to open, nothing pruned, no error.
    #[test]
    fn run_retention_empty_dir_is_noop() {
        let dir = data_dir();
        let r = run_retention(dir.path(), 90).expect("retention on empty dir");
        assert_eq!(r.keep_days, 90);
        assert_eq!(r.events_pruned, 0);
        assert_eq!(r.search_chunks_pruned, 0);
        assert_eq!(r.claims_pruned, 0);
    }

    /// Fresh data is kept: a completed run's recent events and a fresh
    /// claim are all newer than the cutoff, so the pass prunes nothing
    /// but still runs cleanly end to end.
    #[test]
    fn run_retention_keeps_fresh_data() {
        let dir = data_dir();
        let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("ledger");
        ledger
            .append(&Event::RunStarted {
                run_id: "r1".to_string(),
            })
            .expect("RunStarted");
        ledger
            .append(&Event::RunCompleted {
                run_id: "r1".to_string(),
            })
            .expect("RunCompleted");
        drop(ledger);
        let claims = DurableClaimLedger::open(&dir.path().join("claims.db")).expect("claims");
        assert!(claims.claim("occ-1").expect("claim"));
        drop(claims);

        let r = run_retention(dir.path(), 90).expect("retention");
        assert_eq!(r.events_pruned, 0, "fresh events kept");
        assert_eq!(r.claims_pruned, 0, "fresh claims kept");

        let claims = DurableClaimLedger::open(&dir.path().join("claims.db")).expect("claims");
        assert!(claims.is_claimed("occ-1").expect("is_claimed"));
    }

    /// The 24h gate: the first pass records `retention.json`; a second
    /// call while the pass is fresh is a no-op (state file untouched).
    #[test]
    fn maybe_run_retention_gates_on_24h() {
        let dir = data_dir();
        maybe_run_retention(dir.path());
        let state_path = dir.path().join("retention.json");
        let first = std::fs::read_to_string(&state_path).expect("retention.json written");
        maybe_run_retention(dir.path());
        let second = std::fs::read_to_string(&state_path).expect("retention.json still there");
        assert_eq!(first, second, "fresh pass must not re-run");
    }

    /// `keep_days = 0` disables retention: no pass, no state file.
    #[test]
    fn maybe_run_retention_disabled_when_keep_days_zero() {
        let dir = data_dir();
        std::fs::write(
            dir.path().join("config.toml"),
            "[retention]\nkeep_days = 0\n",
        )
        .expect("config");
        maybe_run_retention(dir.path());
        assert!(
            !dir.path().join("retention.json").exists(),
            "disabled retention must not record a pass"
        );
    }
}
}

pub mod run_history {
//! Per-job run history: consecutive-failure counters for the nightly
//! repair loop.
//!
//! The tick driver reports each fired run's [`RunOutcome`] through a
//! channel; nothing durable recorded it (the gateway loop dropped the
//! receiver). [`RunHistory`] is the durable sink: one JSON file,
//! `<data_dir>/schedule-run-history.json`, mapping job id → counters.
//! `Completed` resets the consecutive-failure count; `TimedOut`,
//! `Panicked`, and `Failed` (app-level task failure) increment it;
//! `Replaced` (a superseded generation) is neutral - a replace is not a
//! failure.
//!
//! The nightly repair loop reads these counters through its
//! `ScheduleRepairTarget` adapter: a job with
//! `consecutive_failures >= schedule_max_failures` is broken and gets
//! paused + escalated.

use crate::tick::RunOutcome;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Path of the run-history file: `<data_dir>/schedule-run-history.json`.
pub fn run_history_path(data_dir: &Path) -> PathBuf {
    data_dir.join("schedule-run-history.json")
}

/// Counters for one job's recent runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobRunStats {
    /// Consecutive `TimedOut`/`Panicked` outcomes. Reset by `Completed`.
    pub consecutive_failures: u32,
    /// "completed" | "timed_out" | "panicked" | "replaced".
    pub last_outcome: String,
    pub last_error: Option<String>,
    pub last_run_ms: i64,
    pub total_runs: u64,
    pub total_failures: u64,
    pub last_delivery: Option<String>,
    pub last_delivery_ok: Option<bool>,
}

/// Durable per-job run history. The map is the whole file: `open`,
/// mutate via [`RunHistory::record`], and every mutation persists.
pub struct RunHistory {
    path: PathBuf,
    stats: HashMap<String, JobRunStats>,
}

impl RunHistory {
    /// Open (creating if missing) on `<data_dir>`. A corrupt file is an
    /// error naming the file - a silently empty history would hide
    /// failing jobs from the repair loop.
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let path = run_history_path(data_dir);
        let stats = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
            Ok(text) => serde_json::from_str(&text)
                .map_err(|e| format!("cannot parse {}: {e}", path.display()))?,
        };
        Ok(Self { path, stats })
    }

    /// Record one fired run's outcome and persist. `error` carries the
    /// executor's failure detail for `TimedOut`/`Panicked` runs.
    pub fn record_delivery(&mut self, job_id: &str, target: &str, ok: bool) -> Result<(), String> {
        let st = self.stats.entry(job_id.to_string()).or_default();
        st.last_delivery = Some(target.to_string());
        st.last_delivery_ok = Some(ok);
        self.save()
    }

    pub fn record(
        &mut self,
        job_id: &str,
        outcome: RunOutcome,
        error: Option<String>,
        at_ms: i64,
    ) -> Result<(), String> {
        let st = self.stats.entry(job_id.to_string()).or_default();
        st.total_runs += 1;
        st.last_run_ms = at_ms;
        match outcome {
            RunOutcome::Completed => {
                st.consecutive_failures = 0;
                st.last_outcome = "completed".to_string();
                st.last_error = None;
            }
            RunOutcome::TimedOut | RunOutcome::Panicked | RunOutcome::Failed => {
                st.consecutive_failures = st.consecutive_failures.saturating_add(1);
                st.total_failures += 1;
                st.last_outcome = match outcome {
                    RunOutcome::TimedOut => "timed_out",
                    RunOutcome::Failed => "failed",
                    _ => "panicked",
                }
                .to_string();
                st.last_error = error;
            }
            RunOutcome::Replaced => {
                // A superseded generation is not a failure: leave the
                // counters alone, record the outcome.
                st.last_outcome = "replaced".to_string();
            }
        }
        self.save()
    }

    /// This job's counters, if it has any recorded runs.
    pub fn stats(&self, job_id: &str) -> Option<&JobRunStats> {
        self.stats.get(job_id)
    }

    /// Drop one job's history (job deleted or repaired). Persists.
    pub fn reset(&mut self, job_id: &str) -> Result<(), String> {
        self.stats.remove(job_id);
        self.save()
    }

    /// Drop history for jobs no longer in the store, so deleted jobs
    /// don't accumulate rows forever. Persists.
    pub fn retain<'a>(&mut self, active: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
        let keep: std::collections::HashSet<&str> = active.into_iter().collect();
        self.stats.retain(|id, _| keep.contains(id.as_str()));
        self.save()
    }

    fn save(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&self.stats)
            .map_err(|e| format!("encode history: {e}"))?;
        std::fs::write(&self.path, text).map_err(|e| format!("write {}: {e}", self.path.display()))
    }
}
}

pub mod templates {
//! Schedule templates: built-in job blueprints plus a user-managed store.
//!
//! A template is a named, documented starting point for creating a job: a
//! default schedule, a prompt with `{{variable}}` placeholders, and the
//! questions needed to fill them. Built-ins are embedded in
//! [`builtin_templates`]; users add or override templates through the
//! [`TemplateStore`] API, which persists user templates to
//! `<data_dir>/templates.json`:
//!
//! ```json
//! { "templates": [
//!     { "name": "my-watch",
//!       "description": "Watch something I care about.",
//!       "schedule": { "every": "1h" },
//!       "prompt": "Check {{thing}} and report back.",
//!       "vars": [
//!         { "name": "thing", "question": "What should I watch?",
//!           "default": "the build" }
//!       ] }
//! ] }
//! ```
//!
//! A user template with a built-in's name overrides it for [`TemplateStore::get`]
//! and [`TemplateStore::list`]; built-ins themselves cannot be deleted.
//! Templates created through [`TemplateStore::save`] are validated loudly
//! a bad schedule is an error at save time, never a silent no-show at fire
//! time. A job names a template in [`Job::template`](crate::Job::template);
//! [`Job::resolve_task`](crate::Job::resolve_task) re-renders it at fire time.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// One fill-in variable in a template prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateVar {
    pub name: String,
    pub question: String,
    pub default: Option<String>,
}

/// A template's default schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TemplateSchedule {
    /// An interval, e.g. `"30m"`.
    Every(String),
    /// A cron expression, e.g. `"0 7 * * *"`.
    Cron(String),
}

/// A schedule blueprint: what to run, when, and what it needs to know.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleTemplate {
    pub name: String,
    pub description: String,
    pub schedule: TemplateSchedule,
    pub prompt: String,
    pub vars: Vec<TemplateVar>,
}

fn var(name: &str, question: &str, default: Option<&str>) -> TemplateVar {
    TemplateVar {
        name: name.to_string(),
        question: question.to_string(),
        default: default.map(str::to_string),
    }
}

/// The embedded built-ins. Deliberately few and sharp: each one is
/// something a working developer would genuinely schedule. No wellness
/// nudges, no habit trackers.
pub fn builtin_templates() -> Vec<ScheduleTemplate> {
    vec![
        ScheduleTemplate {
            name: "morning-briefing".into(),
            description: "Overnight digest: GitHub notifications, AI news on a topic, today's calendar."
                .into(),
            schedule: TemplateSchedule::Cron("0 7 * * *".into()),
            prompt: "Write my morning briefing, three sections, tight:\n\n\
                1. GitHub - notifications and activity from the last ~12h on my repos: new issues, \
                PRs needing review, failing CI. Use `gh` (gh api notifications, gh run list).\n\
                2. {{topic}} news - what happened in the last 24h: releases, papers, notable launches \
                and discussions. Search the web; link sources.\n\
                3. Today - what's on the calendar: meetings, deadlines, focus blocks. Use the \
                connected calendar tools.\n\n\
                End with a 3-bullet \"today's priorities\" distilled from all three. \
                Under 400 words total. No preamble.".into(),
            vars: vec![var(
                "topic",
                "Which topic should the AI-news section track?",
                Some("AI agents"),
            )],
        },
        ScheduleTemplate {
            name: "inbox-triage".into(),
            description: "Unread messages across connected channels, summarized with suggested actions."
                .into(),
            schedule: TemplateSchedule::Every("2h".into()),
            prompt: "Triage unread messages across {{channels}} from the last few hours. For each \
                thread with unread activity: one line on what it is, who it's from, and a suggested \
                action - draft the reply, defer, or ignore. Skip newsletters, CI bots, and automated \
                noise entirely. End with a prioritized action list, most urgent first. If there's \
                genuinely nothing new, say so in one line and stop.".into(),
            vars: vec![var(
                "channels",
                "Which channels should I check (comma-separated)?",
                Some("telegram"),
            )],
        },
        ScheduleTemplate {
            name: "repo-watch".into(),
            description: "{{repo}}: failing CI, open PRs/issues, stale branches.".into(),
            schedule: TemplateSchedule::Every("6h".into()),
            prompt: "Watch {{repo}} for problems. Check: CI status on the default branch and open \
                PRs (failing checks first), open PRs and issues sorted by staleness, and branches \
                untouched for 30+ days that look merged or abandoned. Use `gh` for GitHub remotes, \
                git locally. Report as: RED - needs action / YELLOW - watch / GREEN - healthy. \
                One line per item, no fluff.".into(),
            vars: vec![var(
                "repo",
                "Which repo? (owner/name for GitHub, or a local path)",
                None,
            )],
        },
        ScheduleTemplate {
            name: "dep-audit".into(),
            description: "Weekly vulnerable/outdated dependency scan.".into(),
            schedule: TemplateSchedule::Cron("0 9 * * 1".into()),
            prompt: "Audit dependencies in {{path}}. Detect the project type from lockfiles and run \
                the right audit: `cargo audit` (Rust), `npm audit` (Node), `pip-audit` (Python), or \
                the ecosystem equivalent. Report: critical/high vulnerabilities with CVE id and fixed \
                version, notably outdated direct dependencies worth upgrading, and a suggested upgrade \
                order. Skip dev-only low-severity noise unless the fix is trivial. If no audit tool \
                is available for the project type, say which one to install and stop.".into(),
            vars: vec![var(
                "path",
                "Which project directory should I scan?",
                Some("."),
            )],
        },
        ScheduleTemplate {
            name: "weekly-review".into(),
            description: "Pantheon-native: the week's runs, costs, and what shipped, read from the ledger."
                .into(),
            schedule: TemplateSchedule::Cron("0 8 * * 1".into()),
            prompt: "Write my weekly review from the Pantheon ledger. Gather data with \
                `pantheon stats --week --json` and the run history: total runs, success vs failure \
                rate, token and cost totals by model, the most expensive runs, and what shipped \
                (completed runs with real output). Then: biggest win, biggest time sink, one concrete \
                thing to change next week. Honest and specific - cite run ids and numbers, no filler."
                .into(),
            vars: vec![],
        },
        ScheduleTemplate {
            name: "cost-report".into(),
            description: "Spend by model/project over a range, with anomaly flags.".into(),
            schedule: TemplateSchedule::Cron("0 9 * * 1".into()),
            prompt: "Report Pantheon spend over the last {{range}}. Run `pantheon stats --json` for \
                the range (use --week for 7d, --month for 30d): total cost in USD, breakdown by model \
                and by project, per-run cost outliers, and trend vs the previous equal-length period \
                if the data exists. Flag anything anomalous - a single run at 10x the median cost, a \
                new expensive model appearing, a project suddenly burning tokens. Concrete numbers, \
                no padding.".into(),
            vars: vec![var("range", "Over what range?", Some("7d"))],
        },
        ScheduleTemplate {
            name: "gmail-monitor".into(),
            description: "Watch Gmail for a search query, summarize matching new mail."
                .into(),
            schedule: TemplateSchedule::Every("30m".into()),
            prompt: "Watch Gmail for new mail matching: {{query}}. Use the Gmail skill: search for \
                unread messages matching the query from the last check window, and summarize each \
                hit - sender, subject, one-line gist, and why it matched. End with suggested actions \
                for anything needing a response. If nothing new matches, say so in one line and stop. \
                This job is usually created with a --deliver target so hits reach me.".into(),
            vars: vec![var(
                "query",
                "What Gmail search query? (e.g. from:bank subject:alert)",
                None,
            )],
        },
        ScheduleTemplate {
            name: "cost-watch".into(),
            description: "Watch an item's price; alert on moves ≥ threshold or a target cross."
                .into(),
            schedule: TemplateSchedule::Every("6h".into()),
            prompt: "Check the current price of {{item}} at {{url_or_source}}. Use web search/fetch \
                tools; if the page needs JS rendering, say so and stop rather than guessing. Keep a \
                state file at $PANTHEON_DATA_DIR/price-watch.json mapping item names to last-seen \
                prices (create it if absent). Alert - with old price, new price, and % change - when \
                the price moved ≥ {{threshold_pct}}% since the last check, or when it crosses \
                {{target_price}} in either direction (ignore the target condition when blank). If \
                neither fired, reply in one line with the current price and stop - no noise.".into(),
            vars: vec![
                var("item", "What item should I watch the price of?", None),
                var(
                    "url_or_source",
                    "URL or source to check the price at?",
                    None,
                ),
                var(
                    "threshold_pct",
                    "Alert when the price moves by what percent?",
                    Some("5"),
                ),
                var(
                    "target_price",
                    "Alert when the price crosses (blank to skip)?",
                    Some(""),
                ),
            ],
        },
    ]
}

/// Fill `{{name}}` placeholders from `vars`.
///
/// Errors when the prompt references a variable with no value, or when a
/// provided var is not declared by the template (usually a typo). A var
/// with an empty-string value is provided, not missing.
pub fn render_prompt(
    template: &ScheduleTemplate,
    vars: &HashMap<String, String>,
) -> Result<String, String> {
    let known: HashSet<&str> = template.vars.iter().map(|v| v.name.as_str()).collect();
    let mut unknown: Vec<&str> = vars
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k) && !is_reserved_var(k))
        .collect();
    unknown.sort_unstable();
    if !unknown.is_empty() {
        return Err(format!(
            "template '{}' has no variable(s): {}",
            template.name,
            unknown.join(", ")
        ));
    }
    let mut out = String::with_capacity(template.prompt.len());
    let mut rest = template.prompt.as_str();
    let mut missing: Vec<String> = Vec::new();
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => {
                        if !missing.iter().any(|m| m == name) {
                            missing.push(name.to_string());
                        }
                        out.push_str(&format!("{{{{{name}}}}}"));
                    }
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    if !missing.is_empty() {
        return Err(format!(
            "template '{}' missing variable(s): {}",
            template.name,
            missing.join(", ")
        ));
    }
    Ok(out)
}

/// Vars the CLI intercepts instead of substituting into the prompt:
/// `model` (and `provider`) become the job's model pin, not prompt text.
pub fn is_reserved_var(name: &str) -> bool {
    matches!(name, "model" | "provider")
}

/// Insert template defaults for vars the caller did not provide.
pub fn apply_defaults(template: &ScheduleTemplate, vars: &mut HashMap<String, String>) {
    for v in &template.vars {
        if !vars.contains_key(&v.name) {
            if let Some(d) = &v.default {
                vars.insert(v.name.clone(), d.clone());
            }
        }
    }
}

/// Expand a template into a job's creation fields: the shared half of the
/// CLI's `schedule create --template` and the dashboard's
/// `POST /api/schedule/jobs`.
///
/// Reserved vars (`model`, `provider`) become the model pin, not prompt
/// text - an explicit pin wins, a blank var is dropped. Defaults fill the
/// remaining gaps; every var left without a value or a default is resolved
/// through `missing_var(name, question)` - the CLI prompts interactively,
/// the dashboard fails loudly.
///
/// A `missing_var` error does not abort the scan: the remaining vars are
/// still offered to the callback (so a caller can collect *every* missing
/// name for a single loud error), and the first error is returned
/// afterwards - without rendering, since a render with unresolved vars
/// cannot succeed. [`render_prompt`] failures propagate unchanged.
///
/// On success `vars` holds the final var map (reserved vars removed),
/// `task` holds the rendered snapshot when the caller left it empty, and
/// the template's own schedule fills `every`/`cron` when the caller
/// specified none (`has_schedule`).
/// The mutable render targets `expand_template` fills: the var map plus
/// the fields a template may set. Bundled so the function signature stays
/// readable as the template surface grows.
pub struct TemplateParams<'a> {
    pub vars: &'a mut HashMap<String, String>,
    pub model: &'a mut Option<String>,
    pub provider: &'a mut Option<String>,
    pub task: &'a mut String,
    pub every: &'a mut Option<String>,
    pub cron: &'a mut Option<String>,
}

pub fn expand_template(
    template: &ScheduleTemplate,
    params: TemplateParams<'_>,
    has_schedule: bool,
    missing_var: &mut dyn FnMut(&str, &str) -> Result<String, String>,
) -> Result<(), String> {
    let TemplateParams {
        vars,
        model,
        provider,
        task,
        every,
        cron,
    } = params;
    // Reserved vars become the job's model pin, not prompt text. An
    // explicit pin wins over the var; a blank var is dropped silently.
    for reserved in ["model", "provider"] {
        if let Some(v) = vars.remove(reserved) {
            if v.trim().is_empty() {
                continue;
            }
            if reserved == "model" && model.is_none() {
                *model = Some(v);
            } else if reserved == "provider" && provider.is_none() {
                *provider = Some(v);
            }
        }
    }
    apply_defaults(template, vars);
    let mut first_err: Option<String> = None;
    for tv in &template.vars {
        let current = vars.get(&tv.name).map(String::as_str).unwrap_or("");
        // A default (even "") satisfies the var without resolving it.
        if !current.is_empty() || tv.default.is_some() {
            continue;
        }
        match missing_var(&tv.name, &tv.question) {
            Ok(answer) => {
                vars.insert(tv.name.clone(), answer);
            }
            // Keep scanning: the caller may be collecting every missing
            // name for one loud error instead of failing on the first.
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    }
    if let Some(e) = first_err {
        return Err(e);
    }
    let rendered = render_prompt(template, vars)?;
    if task.is_empty() {
        *task = rendered;
    }
    if !has_schedule {
        match &template.schedule {
            TemplateSchedule::Every(d) => *every = Some(d.clone()),
            TemplateSchedule::Cron(e) => *cron = Some(e.clone()),
        }
    }
    Ok(())
}

/// The on-disk shape of `<data_dir>/templates.json`: user templates only.
/// Built-ins are embedded in the binary and never written.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TemplateFile {
    #[serde(default)]
    templates: Vec<ScheduleTemplate>,
}

/// The template store: embedded built-ins overlaid with the user's
/// `<data_dir>/templates.json`. A user template with a built-in's name
/// overrides it; anything else is added.
///
/// The store is managed through [`TemplateStore::save`] and
/// [`TemplateStore::delete`], which validate loudly and write through to
/// `templates.json`. Hand-edited files are read leniently: entries that
/// fail validation are skipped with a warning.
pub struct TemplateStore {
    templates: Vec<ScheduleTemplate>,
    user: Vec<ScheduleTemplate>,
    data_dir: PathBuf,
}

impl TemplateStore {
    /// Open the store on `data_dir`. A missing or corrupt `templates.json`
    /// means built-ins only; a corrupt file warns on stderr instead of
    /// crashing.
    pub fn open(data_dir: &Path) -> Self {
        let path = data_dir.join("templates.json");
        let user = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                eprintln!(
                    "scheduler: cannot read {}: {e}; using built-in templates only",
                    path.display()
                );
                Vec::new()
            }
            Ok(text) => match serde_json::from_str::<TemplateFile>(&text) {
                Ok(file) => file
                    .templates
                    .into_iter()
                    .filter(|t| {
                        if let Err(e) = validate_template(t) {
                            eprintln!(
                                "scheduler: ignoring invalid template in {}: {e}",
                                path.display()
                            );
                            false
                        } else {
                            true
                        }
                    })
                    .collect(),
                Err(e) => {
                    eprintln!(
                        "scheduler: ignoring corrupt {}: {e}; using built-in templates only",
                        path.display()
                    );
                    Vec::new()
                }
            },
        };
        Self {
            templates: builtin_templates(),
            user,
            data_dir: data_dir.to_path_buf(),
        }
    }

    /// Look up a template by name. A user template wins over a built-in
    /// with the same name.
    pub fn get(&self, name: &str) -> Option<&ScheduleTemplate> {
        self.user
            .iter()
            .find(|t| t.name == name)
            .or_else(|| self.templates.iter().find(|t| t.name == name))
    }

    /// All templates: built-ins first (a user template with a built-in's
    /// name is shown in that built-in's slot), then user-only templates in
    /// save order. Consistent with [`TemplateStore::get`]: every name
    /// resolves to the template shown here.
    pub fn list(&self) -> Vec<&ScheduleTemplate> {
        let mut out: Vec<&ScheduleTemplate> = self
            .templates
            .iter()
            .map(|b| self.user.iter().find(|u| u.name == b.name).unwrap_or(b))
            .collect();
        out.extend(self.user.iter().filter(|u| !self.is_builtin(&u.name)));
        out
    }

    /// Save (add or replace) a user template, validating loudly and
    /// writing through to `templates.json`. A template with a built-in's
    /// name overrides that built-in.
    pub fn save(&mut self, t: ScheduleTemplate) -> Result<(), String> {
        validate_template(&t)?;
        if let Some(slot) = self.user.iter_mut().find(|u| u.name == t.name) {
            *slot = t;
        } else {
            self.user.push(t);
        }
        self.write_through()
    }

    /// Delete a user template. Deleting a user template that overrides a
    /// built-in removes the override and reveals the same-named built-in
    /// again; the built-in itself is never deleted. Deleting a built-in
    /// name with no user override is an error, as is an unknown name.
    pub fn delete(&mut self, name: &str) -> Result<(), String> {
        let before = self.user.len();
        self.user.retain(|u| u.name != name);
        if self.user.len() == before {
            if self.is_builtin(name) {
                return Err(format!(
                    "cannot delete built-in template '{name}'; save a template with the same name to override it"
                ));
            }
            return Err(format!("unknown template '{name}'"));
        }
        self.write_through()
    }

    /// Is this the name of an embedded built-in template?
    pub fn is_builtin(&self, name: &str) -> bool {
        self.templates.iter().any(|t| t.name == name)
    }

    fn write_through(&self) -> Result<(), String> {
        std::fs::create_dir_all(&self.data_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.data_dir.display()))?;
        let path = self.data_dir.join("templates.json");
        let text = serde_json::to_string_pretty(&TemplateFile {
            templates: self.user.clone(),
        })
        .map_err(|e| format!("cannot serialize templates: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// Loud validation, shared by [`TemplateStore::save`] and the lenient
/// [`TemplateStore::open`] filter.
fn validate_template(t: &ScheduleTemplate) -> Result<(), String> {
    if t.name.trim().is_empty() {
        return Err("template name cannot be empty".to_string());
    }
    if t.prompt.trim().is_empty() {
        return Err(format!("template '{}' has an empty prompt", t.name));
    }
    if t.vars.iter().any(|v| v.name.trim().is_empty()) {
        return Err(format!(
            "template '{}' has a variable with an empty name",
            t.name
        ));
    }
    match &t.schedule {
        TemplateSchedule::Cron(expr) => {
            crate::CronSchedule::parse(expr)
                .map(|_| ())
                .map_err(|e| format!("template '{}' has an invalid cron schedule: {e}", t.name))?;
        }
        // Every used to be saved unvalidated and failed only at job
        // creation, leaving a template that could never run.
        TemplateSchedule::Every(dur) => {
            crate::parse_duration(dur).map(|_| ()).map_err(|e| {
                format!(
                    "template '{}' has an invalid interval schedule: {e}",
                    t.name
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(schedule: TemplateSchedule) -> ScheduleTemplate {
        ScheduleTemplate {
            name: "t".to_string(),
            description: String::new(),
            schedule,
            prompt: "do it".to_string(),
            vars: vec![],
        }
    }

    #[test]
    fn every_schedule_is_validated_at_save_time() {
        assert!(validate_template(&template(TemplateSchedule::Every("30m".to_string()))).is_ok());
        let err = validate_template(&template(TemplateSchedule::Every("banana".to_string())))
            .expect_err("invalid interval must fail validation, not persist");
        assert!(err.contains("invalid interval schedule"), "got: {err}");
    }
}
}

pub mod tick {
//! Durable tick driver (§21).
//!
//! One code path owns the fire decision. For each due job it:
//!
//! 1. applies the job's [`OverlapPolicy`](crate::OverlapPolicy) against the
//!    in-flight set (same-process concurrency);
//! 2. wins a durable claim for the occurrence from the
//!    [`DurableClaimLedger`] - an atomic first-wins INSERT, so two ticks
//!    racing the same due job (two threads, two processes, or a restart
//!    replaying a minute) agree on exactly one winner;
//! 3. runs the job with its timeout: a run that outlives
//!    [`Job::effective_timeout_secs`](crate::Job::effective_timeout_secs)
//!    is abandoned - the tick stops waiting. Rust cannot kill a thread,
//!    so the abandoned run keeps going detached until it finishes on its
//!    own.
//!
//! A claim that cannot be persisted fails closed: the run must not start.
//! Execution itself is injected (`Arc<dyn Fn()>`), so the whole driver is
//! testable without a runtime.

use crate::durable::DurableClaimLedger;
use crate::{Job, OverlapPolicy};
use pantheon_api::logging::now_ms;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Process-wide drain-occurrence sequence: recovery drain claim keys
/// must be unique per take, so two crashes in the same millisecond (or
/// two racing recoveries) cannot share a key.
static DRAIN_SEQ: AtomicU64 = AtomicU64::new(0);

/// Key for one occurrence of a job.
///
/// `occurrence` must identify the fire instant (a millis stamp for cron and
/// interval jobs) so that two fires inside the same occurrence collapse to
/// one key.
pub fn occurrence_key(job: &Job, occurrence: &str) -> String {
    format!("{}{}", job.idempotency_key, occurrence)
}

/// Which occurrences have already been claimed.
///
/// In-memory on purpose: the caller persists keys in the ledger. A
/// recovered process rebuilds this from the ledger before the first tick.
#[derive(Debug, Default)]
pub struct ClaimLedger {
    claimed: HashSet<String>,
}

impl ClaimLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim an occurrence. `true` means this is the first claim and the run
    /// should start; `false` means it is a replay and must not run twice.
    pub fn claim(&mut self, key: &str) -> bool {
        self.claimed.insert(key.to_string())
    }

    /// Has this occurrence already had a run started?
    pub fn is_claimed(&self, key: &str) -> bool {
        self.claimed.contains(key)
    }

    /// Release a claim after a run ends, so the key is not held forever by
    /// long-lived processes.
    pub fn release(&mut self, key: &str) -> bool {
        self.claimed.remove(key)
    }

    pub fn len(&self) -> usize {
        self.claimed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claimed.is_empty()
    }
}

/// How one fired run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The executor returned.
    Completed,
    /// The run outlived the job's timeout and was abandoned: the tick
    /// stopped waiting. The thread keeps running detached until it
    /// finishes on its own.
    TimedOut,
    /// The executor panicked.
    Panicked,
    /// The executor returned normally but the task itself failed
    /// (app-level failure, e.g. the agent run errored). Never produced
    /// by the tick driver - the app records it when its executor
    /// reports a task error. Counts as a failure for run history.
    Failed,
    /// The run was superseded by an [`OverlapPolicy::Replace`] fire while
    /// it was still in flight.
    Replaced,
}

/// What one tick decided for one job.
#[derive(Debug)]
pub enum TickDecision {
    /// The claim was won and the run started. The receiver yields one
    /// [`RunOutcome`] per run this fire produces (a queue drain can
    /// produce a second one).
    Fired {
        completion: mpsc::Receiver<RunOutcome>,
    },
    /// The fire was deferred: the job was still running and its overlap
    /// policy is [`OverlapPolicy::Queue`]. It runs once more after the
    /// in-flight run finishes.
    Queued,
    /// Not due (or paused).
    NotDue,
    /// Another tick won the durable claim for this occurrence: a replay,
    /// and it must not run twice.
    SkippedClaimLost,
    /// Still running from an earlier fire and the overlap policy is
    /// [`OverlapPolicy::Skip`]. The caller logs this.
    SkippedOverlap,
    /// The claim could not be persisted. Fail closed: the run must not
    /// start.
    ClaimFailed(String),
}

/// Fresh paused-state predicate for queue drains: `Fn(job_id) -> bool`.
/// Installed by the app crate, which owns the job store - the driver
/// itself never touches the store, it just asks before draining.
pub type PausedCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Owns the fire decision for every tick.
///
/// Share with `Arc`: [`TickDriver::tick_job`] takes `&Arc<Self>` so worker
/// threads can report back to the same driver.
pub struct TickDriver {
    ledger: DurableClaimLedger,
    /// Job ids with a run in flight in this process. The generation
    /// counter lets an [`OverlapPolicy::Replace`] fire disown the run it
    /// supersedes without racing its cleanup.
    in_flight: Mutex<HashMap<String, u64>>,
    /// Job ids that became due while running and asked to queue, mapped
    /// to the occurrence stamp of the fire they stand for ("" when the
    /// occurrence is unknown, e.g. a crash-recovered drain row, which
    /// carries no stamp). Durable twin: the [`DurableClaimLedger`]
    /// drain queue - the in-memory map is the fast path, the store is
    /// what survives a crash. The stamp is what makes a queue drain
    /// deduplicate: a drain claims its occurrence key, so a drain for
    /// an occurrence that already ran is skipped instead of firing a
    /// second run for the same event (see [`TickDriver::run_worker`]).
    queued: Mutex<HashMap<String, String>>,
    paused_check: std::sync::RwLock<Option<PausedCheck>>,
}

impl TickDriver {
    pub fn new(ledger: DurableClaimLedger) -> Self {
        Self {
            ledger,
            in_flight: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashMap::new()),
            paused_check: std::sync::RwLock::new(None),
        }
    }

    /// Install the paused-state predicate consulted before draining a
    /// queued fire. Without one, drains assume "not paused" (tests, and
    /// the tick's own paused filter).
    pub fn set_paused_check(&self, check: PausedCheck) {
        if let Ok(mut g) = self.paused_check.write() {
            *g = Some(check);
        }
    }

    /// Lock the in-flight map. A poisoned mutex means a worker thread
    /// panicked while holding it, so the guarded state is untrustworthy:
    /// callers fail closed instead of panicking the tick path.
    fn lock_in_flight(&self) -> Option<std::sync::MutexGuard<'_, HashMap<String, u64>>> {
        self.in_flight.lock().ok()
    }

    /// Lock the queued map; see [`TickDriver::lock_in_flight`].
    fn lock_queued(&self) -> Option<std::sync::MutexGuard<'_, HashMap<String, String>>> {
        self.queued.lock().ok()
    }

    /// Fresh paused state for `id`, for queue drains. Fails closed: a
    /// poisoned check lock, or a check that reports paused, drops the
    /// drain instead of firing blind.
    fn is_paused(&self, id: &str) -> bool {
        match self.paused_check.read() {
            Ok(g) => g.as_ref().is_some_and(|check| check(id)),
            Err(_) => true,
        }
    }

    /// Attempt one fire of `job` at `now_ms`.
    ///
    /// `last_fire_ms` is the job's persisted last fire (drives
    /// [`Job::due`](crate::Job::due) and the occurrence stamp).
    /// `execute` runs the job; it is called at most once per won claim,
    /// on a worker thread - this method never blocks on the run itself.
    pub fn tick_job(
        self: &Arc<Self>,
        job: &Job,
        now_ms: i64,
        last_fire_ms: Option<i64>,
        execute: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> TickDecision {
        // The due decision and its occurrence stamp come first: the
        // recovery path below seeds the queue with this stamp, and the
        // worker drain claims it, so a drain for an occurrence that
        // already ran is skipped instead of firing twice (catch-up
        // multi-fire: one fire per event, ever, enforced at the
        // source - the claim - not by suppressing duplicates
        // downstream).
        let is_due = job.due(now_ms, last_fire_ms);
        let due_stamp = if is_due {
            job.occurrence_stamp(now_ms, last_fire_ms)
        } else {
            None
        };
        // Crash recovery for the durable drain queue: a previous process
        // may have died owing this job a queued fire. Atomic take - this
        // process now owns the owed fire.
        let recovered = match self.ledger.take_pending_drain(&job.id) {
            Ok(taken) => taken,
            Err(e) => {
                eprintln!("tick: drain-queue take failed for {}: {e}", job.id);
                false
            }
        };
        if recovered {
            if self.is_paused(&job.id) {
                eprintln!(
                    "tick: job {} owed a queued fire at shutdown; paused now, dropping it",
                    job.id
                );
            } else if !is_due && self.generation_of(&job.id).is_none() {
                // Not due and nothing running: nothing would drain a
                // seeded queue entry, so fire the owed run directly as a
                // drain-claimed fire, exactly like a worker drain does.
                return self.fire_drain(job, now_ms, execute);
            } else if let Some(mut queued) = self.lock_queued() {
                // Also due (or busy): seed the in-memory queue with the
                // due occurrence's stamp when known, so the worker drain
                // claims that occurrence: if the due fire below already
                // claimed it, the drain loses the claim and is skipped
                // (one fire for the event); if the due fire is skipped
                // (overlap=skip) or queued behind the in-flight run, the
                // drain wins the claim and fires it once. A busy-but-
                // not-due recovery has no due stamp: seed with "" (the
                // drain fires on a unique key, as before).
                let stamp = due_stamp.map(|s| s.to_string()).unwrap_or_default();
                queued.insert(job.id.clone(), stamp);
            }
        }
        if !is_due {
            return TickDecision::NotDue;
        }
        let Some(stamp) = due_stamp else {
            return TickDecision::NotDue;
        };

        // Overlap gate first: cheaper than a claim write, and a skipped
        // fire must not consume an occurrence claim.
        //
        // Replace deliberately bumps the generation only AFTER winning the
        // claim below: the claim serializes racing replacers (same
        // occurrence, same key - exactly one wins), and a replacer that
        // loses the claim leaves the in-flight run untouched.
        let (generation, replacing) = {
            let mut in_flight = match self.lock_in_flight() {
                Some(g) => g,
                // Poisoned: the in-flight set is untrustworthy, so the
                // overlap decision can't be made - fail closed, don't fire.
                None => return TickDecision::ClaimFailed("tick state lock poisoned".to_string()),
            };
            match in_flight.get(&job.id).copied() {
                None => {
                    in_flight.insert(job.id.clone(), 0);
                    (0, false)
                }
                Some(gen) => match job.overlap {
                    OverlapPolicy::Skip => return TickDecision::SkippedOverlap,
                    OverlapPolicy::Replace => (gen, true),
                    OverlapPolicy::Queue => {
                        match self.lock_queued() {
                            Some(mut queued) => {
                                queued.insert(job.id.clone(), stamp.to_string());
                            }
                            None => {
                                return TickDecision::ClaimFailed(
                                    "tick state lock poisoned".to_string(),
                                )
                            }
                        }
                        // Durable twin of the in-memory insert: a crash
                        // must not lose the owed fire. Best-effort - if
                        // the write fails the entry still queues for this
                        // process's lifetime; the failure is loud.
                        if let Err(e) = self.ledger.enqueue_drain(&job.id) {
                            eprintln!(
                                "tick: durable drain-queue write failed for {}: {e}; queued in-process only",
                                job.id
                            );
                        }
                        return TickDecision::Queued;
                    }
                },
            }
        };

        let key = occurrence_key(job, &stamp.to_string());
        let generation = match self.ledger.claim(&key) {
            Ok(true) => {
                if replacing {
                    let next = generation + 1;
                    match self.lock_in_flight() {
                        Some(mut in_flight) => {
                            in_flight.insert(job.id.clone(), next);
                        }
                        // Won the claim but can't record the generation:
                        // fail closed - don't start a run we can't track.
                        None => {
                            self.release_if_current(&job.id, generation);
                            return TickDecision::ClaimFailed(
                                "tick state lock poisoned".to_string(),
                            );
                        }
                    }
                    next
                } else {
                    generation
                }
            }
            Ok(false) => {
                // A replay: another tick won this occurrence. Only release
                // the slot we installed ourselves; a replacer that lost the
                // claim must not disturb the in-flight run.
                if !replacing {
                    self.release_if_current(&job.id, generation);
                    self.clear_queued(&job.id);
                    let _ = self.ledger.dequeue_drain(&job.id);
                }
                return TickDecision::SkippedClaimLost;
            }
            Err(e) => {
                if !replacing {
                    self.release_if_current(&job.id, generation);
                    self.clear_queued(&job.id);
                    let _ = self.ledger.dequeue_drain(&job.id);
                }
                return TickDecision::ClaimFailed(e.to_string());
            }
        };

        let (tx, rx) = mpsc::channel();
        let driver = Arc::clone(self);
        let job = job.clone();
        std::thread::spawn(move || driver.run_worker(job, generation, execute, tx));
        TickDecision::Fired { completion: rx }
    }

    /// Fire a drain-claimed run immediately: the crash-recovery path for
    /// a durable drain-queue leftover when the job is not otherwise due.
    /// Claims a unique drain occurrence (idempotent across restarts),
    /// installs the in-flight slot, and spawns the worker - the same
    /// shape as a natural fire, minus the occurrence claim.
    fn fire_drain(
        self: &Arc<Self>,
        job: &Job,
        now_ms: i64,
        execute: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> TickDecision {
        let seq = DRAIN_SEQ.fetch_add(1, Ordering::SeqCst);
        let key = occurrence_key(
            job,
            &format!("recovery:{now_ms}:{}:q{seq}", std::process::id()),
        );
        match self.ledger.claim(&key) {
            Ok(true) => {}
            Ok(false) => return TickDecision::SkippedClaimLost,
            Err(e) => return TickDecision::ClaimFailed(e.to_string()),
        }
        {
            let mut in_flight = match self.lock_in_flight() {
                Some(g) => g,
                // Won the claim but can't record the generation:
                // fail closed - don't start a run we can't track. (The
                // claim stays won; like the natural path, claims are
                // never released.)
                None => {
                    return TickDecision::ClaimFailed("tick state lock poisoned".to_string());
                }
            };
            if in_flight.contains_key(&job.id) {
                // Raced with a natural fire between the recovery check
                // and this lock: seed the queue and let the in-flight
                // run's worker drain the owed fire. The owed occurrence
                // is unknown here (a recovery event, not a due tick),
                // so the drain fires on a unique key, as before.
                drop(in_flight);
                if let Some(mut queued) = self.lock_queued() {
                    queued.insert(job.id.clone(), String::new());
                }
                let _ = self.ledger.enqueue_drain(&job.id);
                return TickDecision::Queued;
            }
            in_flight.insert(job.id.clone(), 0);
        }
        let (tx, rx) = mpsc::channel();
        let driver = Arc::clone(self);
        let job = job.clone();
        std::thread::spawn(move || driver.run_worker(job, 0, execute, tx));
        TickDecision::Fired { completion: rx }
    }

    /// The worker loop for one fired generation: run, enforce the timeout,
    /// report the outcome, drain one queued fire, then release the slot.
    fn run_worker(
        self: &Arc<Self>,
        job: Job,
        generation: u64,
        execute: Arc<dyn Fn() + Send + Sync + 'static>,
        tx: mpsc::Sender<RunOutcome>,
    ) {
        let timeout = Duration::from_secs(job.effective_timeout_secs());
        // Drain claim keys: a queued entry with a known occurrence
        // stamp claims that occurrence's key (dedup, see below); only
        // unknown-stamp entries (crash-recovery events with no stamp)
        // use a unique wall-clock suffix, so the owed fire still runs
        // once. The owed fire itself is durable (the ledger's drain
        // queue) and only deleted once taken - a crash before the take
        // is recovered at the next `tick_job` entry.
        let mut drain_seq: u64 = 0;
        loop {
            let (done_tx, done_rx) = mpsc::channel();
            let ex = Arc::clone(&execute);
            std::thread::spawn(move || {
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ex())).is_ok();
                let _ = done_tx.send(ok);
            });
            let outcome = match done_rx.recv_timeout(timeout) {
                Ok(true) => RunOutcome::Completed,
                Ok(false) => RunOutcome::Panicked,
                Err(mpsc::RecvTimeoutError::Timeout) => RunOutcome::TimedOut,
                // Unreachable in practice: panics are caught above, so the
                // sender only drops if spawning the child failed.
                Err(mpsc::RecvTimeoutError::Disconnected) => RunOutcome::Panicked,
            };
            if self.generation_of(&job.id) != Some(generation) {
                // A Replace fire disowned us while we ran: report the
                // supersession instead of the run's own outcome, and touch
                // nothing - the new generation owns the in-flight slot and
                // the queue now. Exactly one outcome per run.
                let _ = tx.send(RunOutcome::Replaced);
                return;
            }
            if outcome == RunOutcome::TimedOut {
                // Abandon: stop waiting. The thread cannot be killed; it
                // stays detached until it finishes on its own.
                eprintln!(
                    "tick: job {} ran past its {}s timeout; abandoning the run",
                    job.id,
                    job.effective_timeout_secs()
                );
                // Release the slot BEFORE reporting: an observer that
                // sees TimedOut must already be able to fire again.
                // (Reporting first let a fast re-tick observe the stale
                // slot and skip a fire that should have run.)
                self.abandon(&job.id, generation);
                let _ = tx.send(RunOutcome::TimedOut);
                return;
            }
            let _ = tx.send(outcome);
            if let Some(queued_stamp) = self.take_queued(&job.id) {
                // The durable row is deleted when the drain is TAKEN,
                // before the drain run completes (take-before-complete):
                // a crash in between loses the owed run, but deleting
                // only after completion would re-fire a drain whose
                // pre-crash run may already have executed the job.
                if let Err(e) = self.ledger.dequeue_drain(&job.id) {
                    eprintln!("tick: drain-queue delete failed for {}: {e}", job.id);
                }
                // The job may have been paused while the run was in
                // flight: re-check paused state fresh from the store
                // instead of trusting the snapshot taken at fire time.
                // A paused job's queued fire is dropped, not executed.
                if self.is_paused(&job.id) {
                    eprintln!(
                        "tick: job {} paused while queued; dropping the queued fire",
                        job.id
                    );
                    break;
                }
                // Deduplicate the drain at the source: when the queued
                // entry carries its occurrence stamp, the drain claims
                // that occurrence's key. A lost claim means the
                // occurrence already ran (the due fire claimed it, or
                // the in-flight run was itself that occurrence), so the
                // drain is consumed WITHOUT firing: one fire per event,
                // ever. An unknown stamp ("") keeps the old behaviour -
                // a unique key, so a genuinely owed recovery fire still
                // runs once.
                drain_seq += 1;
                let key = if queued_stamp.is_empty() {
                    occurrence_key(&job, &format!("{}:q{drain_seq}", now_ms()))
                } else {
                    occurrence_key(&job, &queued_stamp)
                };
                match self.ledger.claim(&key) {
                    Ok(true) => continue,
                    Ok(false) => {
                        eprintln!(
                            "tick: queue-drain for {} already claimed (occurrence {queued_stamp}); skipping duplicate fire",
                            job.id
                        );
                        break;
                    }
                    Err(e) => {
                        eprintln!(
                            "tick: queue-drain claim failed for {}: {e}; dropping the queued fire",
                            job.id
                        );
                        break;
                    }
                }
            }
            break;
        }
        self.release_if_current(&job.id, generation);
    }

    fn generation_of(&self, id: &str) -> Option<u64> {
        self.lock_in_flight()?.get(id).copied()
    }

    /// Remove the in-flight entry, but only if it is still ours: a Replace
    /// fire may have installed a newer generation we must not clear.
    fn release_if_current(&self, id: &str, generation: u64) {
        let Some(mut in_flight) = self.lock_in_flight() else {
            // Poisoned: leave the entry rather than touch untrustworthy
            // state; the tick fails closed (future fires skip/queue).
            eprintln!("tick: in-flight state lock poisoned; leaving entry for {id}");
            return;
        };
        if in_flight.get(id).copied() == Some(generation) {
            in_flight.remove(id);
        }
    }

    /// Timeout abandon: give up the slot, and drop any queued duplicate
    /// the run it would have followed never finished, so the next natural
    /// due fire is the honest retry. Only when we still own the slot; a
    /// Replace fire that disowned us owns the queue now.
    fn abandon(&self, id: &str, generation: u64) {
        if self.generation_of(id) == Some(generation) {
            self.clear_queued(id);
            let _ = self.ledger.dequeue_drain(id);
            self.release_if_current(id, generation);
        }
    }

    fn take_queued(&self, id: &str) -> Option<String> {
        // Poisoned: don't drain - fail closed.
        self.lock_queued().and_then(|mut q| q.remove(id))
    }

    fn clear_queued(&self, id: &str) {
        if let Some(mut q) = self.lock_queued() {
            q.remove(id);
        }
    }
}

#[cfg(test)]
mod tick_drain_queue_tests {
    use super::*;
    use crate::ScheduleKind;
    use std::sync::atomic::AtomicUsize;

    fn reopen_driver(dir: &tempfile::TempDir) -> Arc<TickDriver> {
        Arc::new(TickDriver::new(
            DurableClaimLedger::open(&dir.path().join("claims.db")).unwrap(),
        ))
    }

    fn counter_execute(ran: &Arc<AtomicUsize>) -> Arc<dyn Fn() + Send + Sync + 'static> {
        let ran_w = Arc::clone(ran);
        Arc::new(move || {
            ran_w.fetch_add(1, Ordering::SeqCst);
        })
    }

    /// A crashed process queued a fire and died before draining it: the
    /// recovered process fires the owed run directly when the job is
    /// idle and not otherwise due.
    #[test]
    fn recovery_fires_owed_run_when_idle_and_not_due() {
        let dir = tempfile::tempdir().unwrap();
        DurableClaimLedger::open(&dir.path().join("claims.db"))
            .unwrap()
            .enqueue_drain("rj1")
            .unwrap();

        let driver = reopen_driver(&dir);
        let mut job = Job::new(
            "rj1",
            ScheduleKind::OneShot {
                at_ms: now_ms() + 3_600_000,
            },
            "agent",
        );
        job.overlap = OverlapPolicy::Queue;
        let ran = Arc::new(AtomicUsize::new(0));

        match driver.tick_job(&job, now_ms(), None, counter_execute(&ran)) {
            TickDecision::Fired { completion } => {
                assert_eq!(
                    completion.recv_timeout(Duration::from_secs(10)),
                    Ok(RunOutcome::Completed)
                );
            }
            d => panic!("expected Fired, got {d:?}"),
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        // The leftover was consumed: a second tick sees nothing to recover.
        match driver.tick_job(&job, now_ms(), None, Arc::new(|| {})) {
            TickDecision::NotDue => {}
            d => panic!("expected NotDue, got {d:?}"),
        }
    }

    /// When the job is also due, the recovered fire does NOT produce a
    /// second run: the due fire below claims the one-shot's occurrence,
    /// and the worker drain claims the same occurrence key, loses, and
    /// is consumed without firing. One fire per event, ever.
    ///
    /// NOTE: this test previously expected TWO executions (natural fire
    /// + recovered drain, back to back). For a one-shot that is a
    /// duplicate: there is only one event (its `at_ms`), and firing it
    /// twice is the catch-up multi-fire this fix removes.
    #[test]
    fn recovery_plus_due_fires_once_not_twice() {
        let dir = tempfile::tempdir().unwrap();
        DurableClaimLedger::open(&dir.path().join("claims.db"))
            .unwrap()
            .enqueue_drain("rj2")
            .unwrap();

        let driver = reopen_driver(&dir);
        let mut job = Job::new(
            "rj2",
            ScheduleKind::OneShot {
                at_ms: now_ms() - 1_000,
            },
            "agent",
        );
        job.overlap = OverlapPolicy::Queue;
        let ran = Arc::new(AtomicUsize::new(0));

        match driver.tick_job(&job, now_ms(), None, counter_execute(&ran)) {
            TickDecision::Fired { completion } => {
                assert_eq!(
                    completion.recv_timeout(Duration::from_secs(10)),
                    Ok(RunOutcome::Completed)
                );
                // No second run: the drain for the same occurrence is
                // skipped, so the sender drops and the channel closes.
                assert_eq!(
                    completion.recv_timeout(Duration::from_secs(10)),
                    Err(mpsc::RecvTimeoutError::Disconnected)
                );
            }
            d => panic!("expected Fired, got {d:?}"),
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// A tick that queues behind an in-flight run for the SAME
    /// occurrence must not fire again when the run drains: the drain
    /// claims the occurrence key the in-flight run already won and is
    /// skipped. A genuinely newer occurrence still drains normally
    /// (its claim wins).
    #[test]
    fn queued_same_occurrence_drain_is_skipped() {
        use std::sync::Mutex as StdMutex;
        let dir = tempfile::tempdir().unwrap();
        let driver = reopen_driver(&dir);
        let mut job = Job::new(
            "rj3",
            ScheduleKind::OneShot {
                at_ms: now_ms() - 1_000,
            },
            "agent",
        );
        job.overlap = OverlapPolicy::Queue;
        let ran = Arc::new(AtomicUsize::new(0));
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(StdMutex::new(release_rx));
        let ran_w = Arc::clone(&ran);
        let release_w = Arc::clone(&release_rx);
        let blocking: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            ran_w.fetch_add(1, Ordering::SeqCst);
            // Block until the test releases this run.
            let _ = release_w.lock().unwrap().recv();
        });

        let completion = match driver.tick_job(&job, now_ms(), None, blocking) {
            TickDecision::Fired { completion } => completion,
            d => panic!("expected Fired, got {d:?}"),
        };
        // Same occurrence, still running: the second tick queues.
        match driver.tick_job(&job, now_ms(), None, counter_execute(&ran)) {
            TickDecision::Queued => {}
            d => panic!("expected Queued, got {d:?}"),
        }
        release_tx.send(()).unwrap();
        assert_eq!(
            completion.recv_timeout(Duration::from_secs(10)),
            Ok(RunOutcome::Completed)
        );
        // The drain for the same occurrence is skipped: no second run,
        // and the channel closes when the worker returns.
        assert_eq!(
            completion.recv_timeout(Duration::from_secs(10)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        );
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    /// The Queue overlap path writes the durable row: a fresh handle on
    /// the same DB (a stand-in for the post-crash process) sees the owed
    /// fire even though the in-memory set died with the old process.
    /// The row's presence is the contract, not a second run: the queued
    /// tick observed the same one-shot occurrence the natural fire
    /// already claimed, so the drain is consumed as a duplicate (one
    /// fire per event, ever) and never runs.
    #[test]
    fn queue_path_persists_drain_durably() {
        let dir = tempfile::tempdir().unwrap();
        let driver = reopen_driver(&dir);
        let mut job = Job::new(
            "qj",
            ScheduleKind::OneShot {
                at_ms: now_ms() - 1_000,
            },
            "agent",
        );
        job.overlap = OverlapPolicy::Queue;

        // The first fire blocks until the test releases it, so the
        // second tick observes the overlap.
        let (block_tx, block_rx) = mpsc::channel::<()>();
        let block_rx = Arc::new(Mutex::new(block_rx));
        let execute: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            block_rx.lock().unwrap().recv().unwrap();
        });

        let first = driver.tick_job(&job, now_ms(), None, Arc::clone(&execute));
        assert!(matches!(first, TickDecision::Fired { .. }));
        let second = driver.tick_job(&job, now_ms(), None, Arc::clone(&execute));
        assert!(matches!(second, TickDecision::Queued));
        // The queue is durable: a fresh handle sees the owed fire.
        let fresh = DurableClaimLedger::open(&dir.path().join("claims.db")).unwrap();
        assert!(fresh.take_pending_drain("qj").unwrap());

        // Release the natural fire only. The drain is NOT a second
        // run: it observed the same occurrence the natural fire
        // already claimed, so it is consumed as a duplicate and its
        // completion never arrives - the channel closes instead.
        block_tx.send(()).unwrap();
        if let TickDecision::Fired { completion } = first {
            assert_eq!(
                completion.recv_timeout(Duration::from_secs(10)),
                Ok(RunOutcome::Completed)
            );
            assert_eq!(
                completion.recv_timeout(Duration::from_secs(10)),
                Err(mpsc::RecvTimeoutError::Disconnected)
            );
        }
    }

    /// Two tickers (gateway daemon + `schedule tick --watch`) sharing one
    /// claim DB, straddling an interval quantum boundary, must fire a
    /// never-fired job exactly once: the first-fire stamp no longer
    /// depends on each ticker's own clock, so both claims collapse onto
    /// one key and exactly one ticker wins.
    #[test]
    fn two_tickers_straddling_quantum_fire_first_interval_job_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("claims.db");
        let gateway = Arc::new(TickDriver::new(DurableClaimLedger::open(&db).unwrap()));
        let tui = Arc::new(TickDriver::new(DurableClaimLedger::open(&db).unwrap()));
        let mut job = Job::new(
            "first1",
            ScheduleKind::Interval { every_ms: 10_000 },
            "agent",
        );
        job.overlap = OverlapPolicy::Skip;
        let ran = Arc::new(AtomicUsize::new(0));

        // Gateway ticks at 9_999 (quantum 0), TUI at 10_000 (quantum 1).
        let first = gateway.tick_job(&job, 9_999, None, counter_execute(&ran));
        let second = tui.tick_job(&job, 10_000, None, counter_execute(&ran));
        let mut fired = 0;
        for decision in [first, second] {
            match decision {
                TickDecision::Fired { completion } => {
                    fired += 1;
                    assert_eq!(
                        completion.recv_timeout(Duration::from_secs(10)),
                        Ok(RunOutcome::Completed)
                    );
                }
                TickDecision::SkippedClaimLost => {}
                d => panic!("expected Fired or SkippedClaimLost, got {d:?}"),
            }
        }
        assert_eq!(fired, 1, "exactly one ticker may fire the first occurrence");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
}
