//! Tests for `pantheon_tui::schedule::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_scheduler::OverlapPolicy;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn invalid_cron_is_rejected_at_registration() {
    // The gap this closes: a broken expression used to be stored without a
    // murmur and then silently never fire.
    let err = build_scheduled_job(&args(&["backup", "--cron", "not a cron"])).unwrap_err();
    assert!(
        err.contains("invalid --cron"),
        "error must name the problem: {err}"
    );
    let err = build_scheduled_job(&args(&["backup", "--cron", "61 * * * *"])).unwrap_err();
    assert!(err.contains("minute"), "error must name the field: {err}");
}

#[test]
fn valid_cron_and_interval_register() {
    let job = build_scheduled_job(&args(&["backup", "--cron", "0 9 * * *"])).unwrap();
    assert!(matches!(job.kind, ScheduleKind::Cron { .. }));
    // A monthly-on-the-1st expression — the old wildcard bug's victim —
    // registers fine and converts to a scheduler Job that validates.
    let job = build_scheduled_job(&args(&["report", "--cron", "0 0 1 * *"])).unwrap();
    assert!(Job::from(job).validate().is_ok());

    let job = build_scheduled_job(&args(&["ping", "--every", "30m"])).unwrap();
    assert!(matches!(
        job.kind,
        ScheduleKind::Interval { every_ms: 1_800_000 }
    ));
}

#[test]
fn registration_rejects_bad_durations_and_missing_schedule() {
    let err = build_scheduled_job(&args(&["x", "--every", "10x"])).unwrap_err();
    assert!(err.contains("bad duration"), "{err}");
    let err = build_scheduled_job(&args(&["x"])).unwrap_err();
    assert!(err.contains("--every"), "{err}");
}

#[test]
fn timeout_flag_parses_and_rejects_zero() {
    let job = build_scheduled_job(&args(&["x", "--every", "1h", "--timeout", "5m"])).unwrap();
    assert_eq!(job.timeout_secs, Some(300));
    let job = build_scheduled_job(&args(&["x", "--every", "1h"])).unwrap();
    assert_eq!(job.timeout_secs, None, "unset means the scheduler default");
    let err = build_scheduled_job(&args(&["x", "--every", "1h", "--timeout", "0s"])).unwrap_err();
    assert!(err.contains("at least 1s"), "{err}");
}

#[test]
fn overlap_flag_parses_and_defaults_to_skip() {
    let job = build_scheduled_job(&args(&["x", "--every", "1h", "--overlap", "replace"])).unwrap();
    assert_eq!(job.overlap, OverlapPolicy::Replace);
    let job = build_scheduled_job(&args(&["x", "--every", "1h", "--overlap", "queue"])).unwrap();
    assert_eq!(job.overlap, OverlapPolicy::Queue);
    let job = build_scheduled_job(&args(&["x", "--every", "1h"])).unwrap();
    assert_eq!(job.overlap, OverlapPolicy::Skip);
    let err = build_scheduled_job(&args(&["x", "--every", "1h", "--overlap", "bogus"])).unwrap_err();
    assert!(err.contains("bad --overlap"), "{err}");
}

#[test]
fn stored_job_converts_with_timeout_and_overlap() {
    let stored = build_scheduled_job(&args(&[
        "x", "--every", "1h", "--timeout", "90s", "--overlap", "queue",
    ]))
    .unwrap();
    let job = Job::from(stored);
    assert_eq!(job.timeout_secs, Some(90));
    assert_eq!(job.overlap, OverlapPolicy::Queue);
    assert_eq!(job.effective_timeout_secs(), 90);
}

#[test]
fn old_schedule_rows_without_new_fields_still_load() {
    // schedule.json rows written before timeout/overlap existed must load
    // with scheduler defaults.
    let dir = std::env::temp_dir().join(format!("pantheon-sched-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("schedule.json"),
        r#"[{"id":"job_1","task":"t","kind":{"Interval":{"every_ms":60000}},"agent":null,"missed":"RunOnce","paused":false,"last_run":null,"model":null,"provider":null}]"#,
    )
    .unwrap();
    let jobs = load_jobs(&dir).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].timeout_secs, None);
    assert_eq!(jobs[0].overlap, OverlapPolicy::Skip);
    let _ = std::fs::remove_dir_all(&dir);
}
