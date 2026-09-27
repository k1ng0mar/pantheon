//! Tests for `pantheon_scheduler::cron::tests` — sibling file so sources stay test-free.
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
fn day_of_week_wildcard_does_not_force_daily_firing() {
    // Regression: the `*` day-of-week field used to lose its wildcard flag
    // when 7-is-Sunday was folded onto 0, so `0 0 1 * *` fell into the
    // dom/dow OR-arm and fired every day. It must fire on the 1st only.
    let first_of_month = CronSchedule::parse("0 0 1 * *").unwrap();
    // 2026-09-01 was a Tuesday; 2026-09-20 a Sunday; 2026-10-01 a Thursday.
    let tue_first = CivilTime {
        year: 2026,
        month: 9,
        day: 1,
        hour: 0,
        minute: 0,
        weekday: 2,
    };
    let wed_second = CivilTime {
        day: 2,
        weekday: 3,
        ..tue_first
    };
    let sun_twentieth = CivilTime {
        day: 20,
        weekday: 0,
        ..tue_first
    };
    let thu_oct_first = CivilTime {
        month: 10,
        day: 1,
        weekday: 4,
        ..tue_first
    };
    assert!(first_of_month.matches(tue_first));
    assert!(first_of_month.matches(thu_oct_first));
    assert!(
        !first_of_month.matches(wed_second),
        "the 2nd is not the 1st"
    );
    assert!(
        !first_of_month.matches(sun_twentieth),
        "a Sunday that is not the 1st must not fire"
    );
}

#[test]
fn day_of_month_wildcard_with_restricted_dow_fires_weekly() {
    // Mirror image: `0 0 * * 1` fires Mondays, not every day.
    let mondays = CronSchedule::parse("0 0 * * 1").unwrap();
    let monday = CivilTime {
        year: 2026,
        month: 9,
        day: 21,
        hour: 0,
        minute: 0,
        weekday: 1,
    };
    let tuesday = CivilTime {
        day: 22,
        weekday: 2,
        ..monday
    };
    assert!(mondays.matches(monday));
    assert!(!mondays.matches(tuesday));
}

#[test]
fn both_day_fields_wildcard_matches_any_day() {
    let daily = CronSchedule::parse("0 0 * * *").unwrap();
    for (day, weekday) in [(1, 2), (2, 3), (20, 0), (30, 3)] {
        assert!(
            daily.matches(CivilTime {
                year: 2026,
                month: 9,
                day,
                hour: 0,
                minute: 0,
                weekday,
            }),
            "day {day} should match"
        );
    }
}

#[test]
fn weekday_range_fires_weekdays_only() {
    // Standard Vixie semantics: restricted dow alone selects those days.
    let weekdays = CronSchedule::parse("0 9 * * 1-5").unwrap();
    let friday_9am = CivilTime {
        year: 2026,
        month: 9,
        day: 25,
        hour: 9,
        minute: 0,
        weekday: 5,
    };
    let saturday_9am = CivilTime {
        day: 26,
        weekday: 6,
        ..friday_9am
    };
    assert!(weekdays.matches(friday_9am));
    assert!(!weekdays.matches(saturday_9am));
}

#[test]
fn sunday_alias_seven_still_wildcard_star() {
    // `*` in dow must stay a wildcard even though the raw parse includes 7.
    let daily = CronSchedule::parse("0 0 * * *").unwrap();
    assert!(daily.day_of_week.is_wildcard());
    let sunday_only = CronSchedule::parse("0 0 * * 7").unwrap();
    assert!(!sunday_only.day_of_week.is_wildcard());
    let sunday = CivilTime {
        year: 2026,
        month: 9,
        day: 20,
        hour: 0,
        minute: 0,
        weekday: 0,
    };
    let monday = CivilTime {
        day: 21,
        weekday: 1,
        ..sunday
    };
    assert!(sunday_only.matches(sunday));
    assert!(!sunday_only.matches(monday));
}

#[test]
fn validate_accepts_good_rejects_bad() {
    assert!(CronSchedule::validate("0 0 1 * *").is_ok());
    assert!(CronSchedule::validate("* * * * *").is_ok());
    let err = CronSchedule::validate("61 * * * *").unwrap_err();
    assert_eq!(err.field, "minute");
    assert!(CronSchedule::validate("not a cron").is_err());
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
