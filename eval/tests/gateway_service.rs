//! Gateway always-on service: install matrix, status, scheduler queue
//! summary, and cross-platform desktop notifications.
//!
//! No real service is ever installed: platform detection is driven by a
//! fake PATH and temp dirs via `InstallEnv`, and notifier argv is asserted
//! without spawning anything.

use pantheon_gateway::service::{install_with, status_with};
use pantheon_gateway::{
    command_for, cron_line, desktop_notify, detect_notifier, detect_service_mechanism,
    escape_applescript, escape_powershell, manual_cron_line, merge_crontab, queue_summary,
    render_launchd_plist, render_systemd_unit, render_task_xml, InstallEnv, InstallOutcome,
    Notifier, SchedulableJob, ServiceMechanism, CRON_MARKER, LAUNCHD_LABEL, UNIT_NAME,
};
use pantheon_scheduler::{cron::civil_from_ms, Job, ScheduleKind};
use std::path::{Path, PathBuf};

fn sched_job(id: &str, kind: ScheduleKind, last_run: Option<i64>) -> SchedulableJob {
    let mut job = Job::new(id, kind, "agent");
    job.id = id.to_string();
    SchedulableJob {
        job,
        task: "do the thing".to_string(),
        last_run,
    }
}

fn write_stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
    }
    p
}

fn tempdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-gwsvc-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn env_with_bins(bins: &Path, systemd_dir: &Path, launchd_dir: &Path) -> InstallEnv {
    InstallEnv {
        path_dirs: vec![bins.to_path_buf()],
        systemd_user_dir: systemd_dir.to_path_buf(),
        launchd_agents_dir: launchd_dir.to_path_buf(),
    }
}

// ── mechanism detection ───────────────────────────────────────────────

#[test]
fn detect_prefers_systemd_then_cron_then_none() {
    let root = tempdir("detect");
    let bins = root.join("bins");
    let sysd = root.join("systemd");
    let lag = root.join("launchd");
    std::fs::create_dir_all(&bins).unwrap();

    // Nothing on PATH: no mechanism.
    let env = env_with_bins(&bins, &sysd, &lag);
    assert_eq!(detect_service_mechanism(&env), ServiceMechanism::None);

    // crontab only: cron fallback.
    write_stub(&bins, "crontab", "#!/bin/sh\nexit 1\n");
    let env = env_with_bins(&bins, &sysd, &lag);
    assert_eq!(detect_service_mechanism(&env), ServiceMechanism::Cron);

    // systemctl present: systemd wins.
    write_stub(&bins, "systemctl", "#!/bin/sh\nexit 0\n");
    let env = env_with_bins(&bins, &sysd, &lag);
    assert_eq!(detect_service_mechanism(&env), ServiceMechanism::Systemd);
}

// ── install idempotency ───────────────────────────────────────────────

#[test]
fn systemd_install_is_idempotent() {
    let root = tempdir("systemd");
    let bins = root.join("bins");
    let sysd = root.join("systemd");
    let lag = root.join("launchd");
    std::fs::create_dir_all(&bins).unwrap();
    // systemctl stub: every invocation succeeds.
    write_stub(&bins, "systemctl", "#!/bin/sh\nexit 0\n");
    let env = env_with_bins(&bins, &sysd, &lag);
    let data_dir = root.join("data");
    let exe = PathBuf::from("/usr/local/bin/pantheon");

    let first = install_with(&env, &data_dir, &exe);
    assert_eq!(
        first,
        InstallOutcome::Installed {
            mechanism: ServiceMechanism::Systemd,
            changed: true
        }
    );
    let unit = sysd.join(format!("{UNIT_NAME}.service"));
    let body = std::fs::read_to_string(&unit).unwrap();
    assert_eq!(body, render_systemd_unit(&exe, &data_dir));
    assert!(body.contains("pantheon gateway run"));

    // Second run: byte-identical unit, nothing changed, still ensured.
    let second = install_with(&env, &data_dir, &exe);
    assert_eq!(
        second,
        InstallOutcome::Installed {
            mechanism: ServiceMechanism::Systemd,
            changed: false
        }
    );
    assert_eq!(std::fs::read_to_string(&unit).unwrap(), body);
}

#[test]
fn cron_install_merges_idempotently() {
    let root = tempdir("cron");
    let bins = root.join("bins");
    let sysd = root.join("systemd");
    let lag = root.join("launchd");
    std::fs::create_dir_all(&bins).unwrap();
    // Fake crontab with a state file: `-l` prints it (exit 1 when empty),
    // `-` replaces it from stdin.
    let state = root.join("crontab-state");
    write_stub(
        &bins,
        "crontab",
        &format!(
            "#!/bin/sh\nSTATE={}\nif [ \"$1\" = \"-l\" ]; then\n  cat \"$STATE\" 2>/dev/null || exit 1\nelif [ \"$1\" = \"-\" ]; then\n  cat > \"$STATE\"\nfi\n",
            state.display()
        ),
    );
    // Seed a user entry that must survive the merge.
    std::fs::write(&state, "0 5 * * * /usr/bin/backup\n").unwrap();
    let env = env_with_bins(&bins, &sysd, &lag);
    let data_dir = root.join("data");
    let exe = PathBuf::from("/usr/local/bin/pantheon");

    let first = install_with(&env, &data_dir, &exe);
    assert_eq!(
        first,
        InstallOutcome::Installed {
            mechanism: ServiceMechanism::Cron,
            changed: true
        }
    );
    let after_first = std::fs::read_to_string(&state).unwrap();
    assert!(after_first.contains("/usr/bin/backup"));
    assert!(after_first.contains(CRON_MARKER));

    let second = install_with(&env, &data_dir, &exe);
    assert_eq!(
        second,
        InstallOutcome::Installed {
            mechanism: ServiceMechanism::Cron,
            changed: false
        }
    );
    let after_second = std::fs::read_to_string(&state).unwrap();
    assert_eq!(after_first, after_second);
    assert_eq!(
        after_second
            .lines()
            .filter(|l| l.contains(CRON_MARKER))
            .count(),
        1,
        "exactly one owned cron line after two installs"
    );
}

#[test]
fn unavailable_manager_fails_open_with_manual_line() {
    let root = tempdir("none");
    let bins = root.join("bins");
    let sysd = root.join("systemd");
    let lag = root.join("launchd");
    std::fs::create_dir_all(&bins).unwrap();
    let env = env_with_bins(&bins, &sysd, &lag);
    let exe = PathBuf::from("/usr/local/bin/pantheon");
    match install_with(&env, &root.join("data"), &exe) {
        InstallOutcome::Unavailable { note } => {
            assert!(note.contains("schedule tick"), "note: {note}");
            assert!(note.contains(&exe.display().to_string()));
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    assert!(manual_cron_line(&exe).contains("schedule tick"));
    assert!(manual_cron_line(&exe).contains(CRON_MARKER));
}

// ── pure renderers ────────────────────────────────────────────────────

#[test]
fn renderers_carry_identity_and_paths() {
    let exe = PathBuf::from("/usr/local/bin/pantheon");
    let data = PathBuf::from("/home/u/.local/share/pantheon");
    let unit = render_systemd_unit(&exe, &data);
    assert!(unit.contains(&exe.display().to_string()));
    assert!(unit.contains("gateway run"));
    let plist = render_launchd_plist(&exe, &data);
    assert!(plist.contains(LAUNCHD_LABEL));
    assert!(plist.contains(&exe.display().to_string()));
    let xml = render_task_xml(&exe, &data);
    // The task name itself travels via schtasks /TN at install time, not in
    // the XML; the XML carries the trigger, principal, and command.
    assert!(xml.contains("LogonTrigger"));
    assert!(xml.contains("LeastPrivilege"));
    assert!(xml.contains("gateway run"));
    let cron = cron_line(&exe);
    assert!(cron.starts_with("@reboot"));
    assert!(cron.contains(CRON_MARKER));
}

#[test]
fn merge_crontab_replaces_owned_line_only() {
    let line = "@reboot /bin/pantheon gateway run # pantheon-gateway";
    let existing = "0 5 * * * backup\n@reboot /bin/pantheon gateway run # pantheon-gateway\n";
    let merged = merge_crontab(existing, line);
    assert_eq!(
        merged.lines().filter(|l| l.contains(CRON_MARKER)).count(),
        1
    );
    assert!(merged.contains("backup"));
    // Installing twice converges byte-for-byte.
    assert_eq!(merge_crontab(&merged, line), merged);
}

// ── status shape ──────────────────────────────────────────────────────

#[test]
fn status_without_install_reports_absent() {
    let root = tempdir("status");
    let bins = root.join("bins");
    let sysd = root.join("systemd");
    let lag = root.join("launchd");
    std::fs::create_dir_all(&bins).unwrap();
    let env = env_with_bins(&bins, &sysd, &lag);
    let st = status_with(&env);
    assert_eq!(st.detected, ServiceMechanism::None);
    assert_eq!(st.installed, None);
    assert!(!st.running);
}

#[test]
fn status_sees_installed_unit_file() {
    let root = tempdir("status2");
    let bins = root.join("bins");
    let sysd = root.join("systemd");
    let lag = root.join("launchd");
    std::fs::create_dir_all(&bins).unwrap();
    std::fs::create_dir_all(&sysd).unwrap();
    // Unit file exists; systemctl stub reports inactive.
    std::fs::write(sysd.join(format!("{UNIT_NAME}.service")), "unit").unwrap();
    write_stub(&bins, "systemctl", "#!/bin/sh\necho inactive\n");
    let env = env_with_bins(&bins, &sysd, &lag);
    let st = status_with(&env);
    assert_eq!(st.installed, Some(ServiceMechanism::Systemd));
    assert!(!st.running);
}

// ── scheduler queue summary ───────────────────────────────────────────

#[test]
fn queue_summary_counts_and_next_fire() {
    // Fixed clock: 2026-09-28 00:00:00 UTC.
    let now: i64 = 1_790_467_200_000;
    let jobs = vec![
        // Due now: interval, last fire 2h ago on a 1h cadence.
        sched_job(
            "hourly",
            ScheduleKind::Interval {
                every_ms: 3_600_000,
            },
            Some(now - 7_200_000),
        ),
        // Future: daily 09:00 UTC cron.
        sched_job(
            "daily",
            ScheduleKind::Cron {
                expr: "0 9 * * *".into(),
            },
            None,
        ),
        // Paused jobs are invisible to the summary.
        {
            let mut j = sched_job("paused", ScheduleKind::Interval { every_ms: 60_000 }, None);
            j.job.paused = true;
            j
        },
        // Manual jobs never fire.
        sched_job("manual", ScheduleKind::Manual, None),
    ];
    let s = queue_summary(&jobs, now);
    assert!(s.contains("3 active"), "summary: {s}");
    assert!(s.contains("1 due now"), "summary: {s}");
    assert!(s.contains("next: daily in 9h 0m"), "summary: {s}");
}

#[test]
fn queue_summary_empty_queue() {
    let s = queue_summary(&[], 1_790_467_200_000);
    assert_eq!(s, "0 active, 0 due now");
}

#[test]
fn next_fire_ms_is_consistent_with_due() {
    let now: i64 = 1_790_467_200_000;
    // Cron: next fire must be a minute the expression actually selects.
    let job = Job::new(
        "c",
        ScheduleKind::Cron {
            expr: "30 14 * * *".into(),
        },
        "a",
    );
    let t = job.next_fire_ms(now, None).expect("must have a next fire");
    assert!(t > now);
    let civil = civil_from_ms(t);
    assert_eq!((civil.hour, civil.minute), (14, 30));
    assert!(job.due(t, None), "next fire must be due at t");

    // One-shot in the past with no fire recorded: no next fire.
    let past = Job::new("o", ScheduleKind::OneShot { at_ms: now - 1_000 }, "a");
    assert_eq!(past.next_fire_ms(now, None), None);
    // One-shot in the future: fires then.
    let future = Job::new(
        "o2",
        ScheduleKind::OneShot {
            at_ms: now + 60_000,
        },
        "a",
    );
    assert_eq!(future.next_fire_ms(now, None), Some(now + 60_000));
    // Already fired: never again.
    assert_eq!(future.next_fire_ms(now, Some(now + 60_000)), None);

    // Broken cron expression: no next fire (registration rejects it, but
    // the function must not scan forever on a bad stored row).
    let broken = Job::new(
        "b",
        ScheduleKind::Cron {
            expr: "not a cron".into(),
        },
        "a",
    );
    assert_eq!(broken.next_fire_ms(now, None), None);

    // Paused: nothing.
    let mut paused = Job::new("p", ScheduleKind::Interval { every_ms: 1_000 }, "a");
    paused.paused = true;
    assert_eq!(paused.next_fire_ms(now, None), None);
}

// ── cross-platform notifier ───────────────────────────────────────────

#[test]
fn notifier_detection_from_explicit_path() {
    let root = tempdir("notify");
    let bins = root.join("bins");
    std::fs::create_dir_all(&bins).unwrap();

    // Empty dir: none (on this Linux host).
    assert_eq!(detect_notifier(std::slice::from_ref(&bins)), Notifier::None);

    // notify-send present and executable: selected with its path.
    let stub = write_stub(&bins, "notify-send", "#!/bin/sh\nexit 0\n");
    assert_eq!(
        detect_notifier(std::slice::from_ref(&bins)),
        Notifier::NotifySend(stub)
    );
}

#[test]
fn osascript_argv_escapes_correctly() {
    let cmd = command_for(&Notifier::Osascript, "say \"hi\" \\ now", "body").unwrap();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(cmd.get_program().to_string_lossy(), "osascript");
    assert_eq!(args[0], "-e");
    assert_eq!(
        args[1],
        "display notification \"body\" with title \"say \\\"hi\\\" \\\\ now\""
    );
}

#[test]
fn powershell_argv_escapes_single_quotes() {
    let cmd = command_for(&Notifier::PowerShell, "it's", "don't").unwrap();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(cmd.get_program().to_string_lossy(), "powershell");
    assert!(args.contains(&"-NoProfile".to_string()));
    let script = args.last().unwrap();
    assert!(script.contains("it''s"), "script: {script}");
    assert!(script.contains("don''t"), "script: {script}");
    assert!(script.contains("ToastNotificationManager"));
}

#[test]
fn notify_send_argv_shape() {
    let bin = PathBuf::from("/usr/bin/notify-send");
    let cmd = command_for(&Notifier::NotifySend(bin), "t", "b").unwrap();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        vec!["--app-name", "Pantheon", "--expire-time", "8000", "t", "b"]
    );
}

#[test]
fn escaping_helpers() {
    assert_eq!(escape_applescript("a\"b\\c"), "a\\\"b\\\\c");
    assert_eq!(escape_powershell("it's"), "it''s");
    assert!(command_for(&Notifier::None, "t", "b").is_none());
}

#[test]
fn desktop_notify_without_notifier_is_err_not_panic() {
    // With an empty PATH there is no notifier on any platform; the call
    // must return Err, never panic. (PATH is process-global, so scope the
    // mutation to this test and restore afterwards.)
    let saved = std::env::var_os("PATH");
    std::env::set_var("PATH", tempdir("emptypath"));
    let r = desktop_notify("t", "b");
    if let Some(s) = saved {
        std::env::set_var("PATH", s);
    } else {
        std::env::remove_var("PATH");
    }
    assert!(r.is_err(), "expected Err, got {r:?}");
}
