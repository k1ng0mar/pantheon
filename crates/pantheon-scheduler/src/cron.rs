//! Cron expressions for durable jobs (§21).
//!
//! Dependency-free by design: a job's fire decision must be a pure function
//! of the wall clock, so it can be tested without a clock, a timezone
//! database, or a scheduler running. Times are UTC; a job that cares about
//! local wall-clock belongs to a context policy, not to the expression.
//!
//! Supports the standard five fields — minute, hour, day-of-month, month,
//! day-of-week — with `*`, `a`, `a-b`, `*/n`, `a-b/n`, and comma lists.
//! Day-of-week accepts 0 or 7 for Sunday, and follows Vixie cron's rule:
//! when both day fields are restricted, either one matching is enough.

use std::fmt;

/// A parsed five-field expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronSchedule {
    minute: Field,
    hour: Field,
    day_of_month: Field,
    month: Field,
    day_of_week: Field,
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
        Ok(Self {
            minute: Field::parse(parts[0], "minute", 0, 59)?,
            hour: Field::parse(parts[1], "hour", 0, 23)?,
            day_of_month: Field::parse(parts[2], "day-of-month", 1, 31)?,
            month: Field::parse(parts[3], "month", 1, 12)?,
            day_of_week: Field::parse(parts[4], "day-of-week", 0, 7)?.sunday_normalized(),
        })
    }

    /// Does this expression select the given minute?
    pub fn matches(&self, at: CivilTime) -> bool {
        if !self.minute.matches(at.minute)
            || !self.hour.matches(at.hour)
            || !self.month.matches(at.month)
        {
            return false;
        }
        let dom = self.day_of_month.matches(at.day);
        let dow = self.day_of_week.matches(at.weekday);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Job, ScheduleKind};

    /// 2026-09-20T14:30:00Z — a Sunday.
    fn sunday_1430() -> i64 {
        1_789_914_600_000
    }

    #[test]
    fn civil_conversion_matches_known_instants() {
        let t = civil_from_ms(0);
        assert_eq!(
            (t.year, t.month, t.day, t.hour, t.minute),
            (1970, 1, 1, 0, 0)
        );
        assert_eq!(t.weekday, 4, "1970-01-01 was a Thursday");

        let t = civil_from_ms(sunday_1430());
        assert_eq!((t.year, t.month, t.day), (2026, 9, 20));
        assert_eq!((t.hour, t.minute), (14, 30));
        assert_eq!(t.weekday, 0);

        // Leap day.
        let t = civil_from_ms(1_709_164_800_000); // 2024-02-29T00:00:00Z
        assert_eq!((t.year, t.month, t.day), (2024, 2, 29));
    }

    #[test]
    fn weekday_is_correct_across_weeks() {
        // 1970-01-01 (Thu) + 4 days = 1970-01-05, a Monday.
        assert_eq!(civil_from_ms(4 * 86_400_000).weekday, 1);
        // 1970-01-01 + 10 days = 1970-01-11, a Sunday.
        assert_eq!(civil_from_ms(10 * 86_400_000).weekday, 0);
    }

    #[test]
    fn every_minute_matches_any_minute() {
        let c = CronSchedule::parse("* * * * *").unwrap();
        assert!(c.matches_ms(sunday_1430()));
        assert!(c.matches_ms(0));
    }

    #[test]
    fn specific_minute_and_hour() {
        let c = CronSchedule::parse("30 14 * * *").unwrap();
        assert!(c.matches_ms(sunday_1430()));
        assert!(!c.matches_ms(sunday_1430() + 60_000));
    }

    #[test]
    fn steps_and_ranges() {
        let c = CronSchedule::parse("*/15 9-17 * * *").unwrap();
        assert!(c.matches(civil_from_ms(sunday_1430())), "14:30 is in range");
        assert!(c.matches(CivilTime {
            hour: 9,
            minute: 45,
            ..civil_from_ms(sunday_1430())
        }));
        assert!(!c.matches(CivilTime {
            hour: 8,
            minute: 0,
            ..civil_from_ms(sunday_1430())
        }));
        assert!(!c.matches(CivilTime {
            hour: 14,
            minute: 31,
            ..civil_from_ms(sunday_1430())
        }));
    }

    #[test]
    fn day_of_week_restriction_and_sunday_alias() {
        let sunday = CronSchedule::parse("0 3 * * 0").unwrap();
        let sunday_seven = CronSchedule::parse("0 3 * * 7").unwrap();
        let monday = CronSchedule::parse("0 3 * * 1").unwrap();
        let at = CivilTime {
            hour: 3,
            minute: 0,
            ..civil_from_ms(sunday_1430())
        };
        assert!(sunday.matches(at));
        assert!(sunday_seven.matches(at), "7 is an alias for Sunday");
        assert!(!monday.matches(at));
    }

    #[test]
    fn day_fields_are_or_ed_when_both_are_restricted() {
        // Vixie cron: the 1st OR any Sunday.
        let c = CronSchedule::parse("0 0 1 * 0").unwrap();
        let sunday_not_first = CivilTime {
            year: 2026,
            month: 9,
            day: 20,
            hour: 0,
            minute: 0,
            weekday: 0,
        };
        let tuesday_first = CivilTime {
            year: 2026,
            month: 9,
            day: 1,
            hour: 0,
            minute: 0,
            weekday: 2,
        };
        let tuesday_second = CivilTime {
            year: 2026,
            month: 9,
            day: 2,
            hour: 0,
            minute: 0,
            weekday: 3,
        };
        assert!(c.matches(sunday_not_first));
        assert!(c.matches(tuesday_first));
        assert!(!c.matches(tuesday_second));
    }

    #[test]
    fn bad_expressions_are_rejected_with_a_field_name() {
        assert_eq!(
            CronSchedule::parse("* * * *").unwrap_err().field,
            "expression"
        );
        assert_eq!(
            CronSchedule::parse("60 * * * *").unwrap_err().field,
            "minute"
        );
        assert_eq!(CronSchedule::parse("* 24 * * *").unwrap_err().field, "hour");
        assert_eq!(
            CronSchedule::parse("* * * 13 *").unwrap_err().field,
            "month"
        );
        assert_eq!(
            CronSchedule::parse("*/0 * * * *").unwrap_err().field,
            "minute"
        );
        assert_eq!(
            CronSchedule::parse("5-2 * * * *").unwrap_err().field,
            "minute"
        );
        assert!(CronSchedule::parse("0 0 1 1 0").is_ok());
    }

    #[test]
    fn cron_job_fires_once_per_matching_minute() {
        // The gap this closes: a Cron arm that never fired at all.
        let job = Job::new(
            "nightly",
            ScheduleKind::Cron {
                expr: "30 14 * * *".into(),
            },
            "nyx",
        );
        let now = sunday_1430();

        assert!(job.due(now, None), "must fire on the matching minute");
        assert!(
            !job.due(now + 5_000, Some(now)),
            "must not refire inside the same minute"
        );

        // A neighbouring minute does not match, so it never fires.
        assert!(!job.due(now + 60_000, None));

        // The same minute on the next day does, even though last_fire is old.
        let next_day = now + 1_440 * 60_000;
        assert!(job.due(next_day, Some(now)));
    }

    #[test]
    fn paused_cron_job_never_fires() {
        let mut job = Job::new(
            "nightly",
            ScheduleKind::Cron {
                expr: "* * * * *".into(),
            },
            "nyx",
        );
        job.paused = true;
        assert!(!job.due(sunday_1430(), None));
    }

    #[test]
    fn invalid_expression_never_fires_instead_of_panicking() {
        let job = Job::new(
            "broken",
            ScheduleKind::Cron {
                expr: "not a cron".into(),
            },
            "nyx",
        );
        assert!(!job.due(sunday_1430(), None));
        assert!(job.validate().is_err());
    }
}
