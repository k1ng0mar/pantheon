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
