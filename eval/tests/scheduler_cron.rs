//! Cron scheduling invariants for pantheon-scheduler.
//!
//! Rewritten against the crate's public cron API only: expression parsing,
//! minute matching, and fire-once semantics. Tests that needed the private
//! field set (e.g. `Field::is_wildcard`) were rewritten in terms of
//! `CronSchedule::matches` or cut; the `sunday_alias` wildcard assertions
//! were subsumed by the matching-behavior tests. Runs under
//! `cargo test -p pantheon-eval`, not beside the code.
use pantheon_scheduler::cron::{civil_from_ms, CivilTime, CronSchedule};
use pantheon_scheduler::{Job, ScheduleKind};

/// 2026-09-20T14:30:00Z - a Sunday.
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

    // Weekday advances by one per day across the epoch.
    assert_eq!(civil_from_ms(4 * 86_400_000).weekday, 1); // 1970-01-05 Monday
    assert_eq!(civil_from_ms(10 * 86_400_000).weekday, 0); // 1970-01-11 Sunday
}

#[test]
fn minute_and_hour_select_the_matching_minute_only() {
    let every_minute = CronSchedule::parse("* * * * *").unwrap();
    assert!(every_minute.matches_ms(sunday_1430()));
    assert!(every_minute.matches_ms(0));

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
fn sunday_alias_zero_and_seven() {
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
fn validate_accepts_good_and_rejects_bad_with_field_names() {
    assert!(CronSchedule::validate("0 0 1 * *").is_ok());
    assert!(CronSchedule::validate("* * * * *").is_ok());
    assert!(CronSchedule::validate("0 0 1 1 0").is_ok());
    assert_eq!(
        CronSchedule::parse("* * * *").unwrap_err().field,
        "expression"
    );
    assert_eq!(
        CronSchedule::parse("61 * * * *").unwrap_err().field,
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
}

#[test]
fn cron_job_fires_once_per_matching_minute_with_catch_up() {
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

    // A neighbouring minute does not match, so without catch-up it never
    // fires. With catch-up (the default) it fires once for the missed
    // 14:30 occurrence instead.
    let mut no_catchup = job.clone();
    no_catchup.catch_up = false;
    assert!(!no_catchup.due(now + 60_000, None));
    assert!(job.due(now + 60_000, None));

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
