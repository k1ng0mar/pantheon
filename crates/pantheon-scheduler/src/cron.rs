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
