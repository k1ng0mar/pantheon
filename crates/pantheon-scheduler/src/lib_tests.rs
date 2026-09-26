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
