//! Tests for `pantheon_scheduler::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn interval_and_oneshot() {
    let j = Job::new("a", ScheduleKind::Interval { every_ms: 1000 }, "nyx");
    assert!(j.due(5000, None));
    assert!(!j.due(5500, Some(5000)));
    assert!(j.due(6000, Some(5000)));
    let o = Job::new("b", ScheduleKind::OneShot { at_ms: 100 }, "nyx");
    assert!(o.due(200, None));
    assert!(!o.due(200, Some(150)));
}

#[test]
fn blank_pins_are_rejected_not_silently_inherited() {
    let mut j = Job::new("j1", ScheduleKind::Manual, "nyx");
    assert!(j.pin_model("  ", None).is_err());
    assert!(j.pin_model("m", Some("  ")).is_err());
    assert!(j.model.is_none(), "a rejected pin must not stick");
    j.pin_model("cheap-model", Some("nous")).unwrap();
    assert_eq!(j.model.as_deref(), Some("cheap-model"));
    assert_eq!(j.provider.as_deref(), Some("nous"));
}

#[test]
fn effective_model_falls_back_to_the_runtime_default() {
    let mut j = Job::new("j1", ScheduleKind::Manual, "nyx");
    assert_eq!(j.effective_model("big"), "big");
    j.pin_model("small", None).unwrap();
    assert_eq!(j.effective_model("big"), "small");
}

#[test]
fn old_rows_without_pins_still_deserialize() {
    // schedule.json rows written before pinning have no model/provider keys.
    let v: Job = serde_json::from_str(
        r#"{"id":"j","kind":"Manual","idempotency_key":"job:j:","missed":"RunOnce","paused":false,"target_agent":"nyx"}"#,
    )
    .unwrap();
    assert!(v.model.is_none() && v.provider.is_none());
}

#[test]
fn timeout_defaults_to_ten_minutes_and_zero_is_unset() {
    let j = Job::new("a", ScheduleKind::Interval { every_ms: 1000 }, "nyx");
    assert_eq!(j.effective_timeout_secs(), DEFAULT_JOB_TIMEOUT_SECS);
    assert_eq!(DEFAULT_JOB_TIMEOUT_SECS, 600);
    let mut j = j;
    j.timeout_secs = Some(0);
    assert_eq!(
        j.effective_timeout_secs(),
        DEFAULT_JOB_TIMEOUT_SECS,
        "0 must not mean 'abandon immediately'"
    );
    j.timeout_secs = Some(30);
    assert_eq!(j.effective_timeout_secs(), 30);
}

#[test]
fn overlap_defaults_to_skip_and_parses() {
    let j = Job::new("a", ScheduleKind::Manual, "nyx");
    assert_eq!(j.overlap, OverlapPolicy::Skip);
    assert_eq!("skip".parse(), Ok(OverlapPolicy::Skip));
    assert_eq!("REPLACE".parse(), Ok(OverlapPolicy::Replace));
    assert_eq!("queue".parse(), Ok(OverlapPolicy::Queue));
    assert!("bogus".parse::<OverlapPolicy>().is_err());
}

#[test]
fn occurrence_stamp_collapses_racing_ticks() {
    // Two ticks racing the same due fire compute the same stamp, so their
    // claims collapse onto one key.
    let cron = Job::new(
        "c",
        ScheduleKind::Cron {
            expr: "* * * * *".into(),
        },
        "nyx",
    );
    assert_eq!(
        cron.occurrence_stamp(1_789_914_600_000, None),
        cron.occurrence_stamp(1_789_914_612_345, None),
        "same minute, same stamp"
    );
    assert_ne!(
        cron.occurrence_stamp(1_789_914_600_000, None),
        cron.occurrence_stamp(1_789_914_600_000 + 60_000, None),
        "next minute, next stamp"
    );

    let interval = Job::new("i", ScheduleKind::Interval { every_ms: 60_000 }, "nyx");
    assert_eq!(
        interval.occurrence_stamp(5_000, Some(0)),
        interval.occurrence_stamp(9_999, Some(0)),
        "same scheduled fire, same stamp"
    );
    // First fire: racing ticks a few ms apart quantize to one stamp.
    assert_eq!(
        interval.occurrence_stamp(61_000, None),
        interval.occurrence_stamp(61_500, None)
    );

    let once = Job::new("o", ScheduleKind::OneShot { at_ms: 42 }, "nyx");
    assert_eq!(once.occurrence_stamp(1_000_000, None), Some(42));

    let manual = Job::new("m", ScheduleKind::Manual, "nyx");
    assert_eq!(manual.occurrence_stamp(1_000_000, None), None);
}

#[test]
fn old_rows_without_timeout_or_overlap_still_deserialize() {
    let v: Job = serde_json::from_str(
        r#"{"id":"j","kind":"Manual","idempotency_key":"job:j:","missed":"RunOnce","paused":false,"target_agent":"nyx"}"#,
    )
    .unwrap();
    assert!(v.timeout_secs.is_none());
    assert_eq!(v.overlap, OverlapPolicy::Skip);
    assert_eq!(v.effective_timeout_secs(), DEFAULT_JOB_TIMEOUT_SECS);
}
