//! `gateway run` startup: the scheduler loop starts unconditionally while
//! chat surfaces are gated on tokens + allowlist. Plus stop/restart service
//! machinery.
//!
//! Nothing real is ever installed or stopped: platform detection is driven
//! by a fake PATH and temp dirs via `InstallEnv`, and env vars are set and
//! removed inside a single test.

use pantheon_gateway::service::{restart_with, status_with, stop_with, InstallEnv};
use pantheon_gateway::{
    channels_disabled_note, read_channel_env, ChannelPlan, RestartOutcome, ServiceMechanism,
    StopOutcome, CRON_MARKER, UNIT_NAME,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn allow(ids: &[&str]) -> HashSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

fn tempdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon-gwstartup-test-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
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

fn env_with_bins(bins: &Path, systemd_dir: &Path, launchd_dir: &Path) -> InstallEnv {
    InstallEnv {
        path_dirs: vec![bins.to_path_buf()],
        systemd_user_dir: systemd_dir.to_path_buf(),
        launchd_agents_dir: launchd_dir.to_path_buf(),
    }
}

// ── channel plan ────────────────────────────────────────────────────────

#[test]
fn plan_empty_without_tokens() {
    let p = ChannelPlan::from_env(None, None, &allow(&["123"]));
    assert!(p.is_empty());
    assert!(!p.telegram && !p.discord);
}

#[test]
fn plan_telegram_with_token_and_allow() {
    let p = ChannelPlan::from_env(None, Some("tok"), &allow(&["123"]));
    assert!(p.telegram && !p.discord);
    assert!(!p.is_empty());
}

#[test]
fn plan_discord_with_token_and_allow() {
    let p = ChannelPlan::from_env(Some("tok"), None, &allow(&["123"]));
    assert!(p.discord && !p.telegram);
}

#[test]
fn plan_both_surfaces_with_both_tokens() {
    let p = ChannelPlan::from_env(Some("d"), Some("t"), &allow(&["123"]));
    assert!(p.discord && p.telegram);
}

#[test]
fn plan_token_without_allowlist_starts_nothing() {
    // Security posture: a token alone must not expose the host.
    let p = ChannelPlan::from_env(Some("d"), Some("t"), &allow(&[]));
    assert!(p.is_empty());
}

#[test]
fn plan_blank_token_counts_as_missing() {
    let p = ChannelPlan::from_env(Some("   "), Some(""), &allow(&["123"]));
    assert!(p.is_empty());
}

#[test]
fn read_channel_env_roundtrip() {
    // These vars are ours alone; no other test reads them.
    std::env::set_var("PANTHEON_DISCORD_TOKEN", "d123");
    std::env::set_var("PANTHEON_TELEGRAM_BOT_TOKEN", "t456");
    std::env::set_var("PANTHEON_GATEWAY_ALLOW", "111, 222 ,");
    let (d, t, allow) = read_channel_env();
    assert_eq!(d.as_deref(), Some("d123"));
    assert_eq!(t.as_deref(), Some("t456"));
    assert!(allow.contains("111") && allow.contains("222"));
    assert_eq!(allow.len(), 2);
    std::env::remove_var("PANTHEON_DISCORD_TOKEN");
    std::env::remove_var("PANTHEON_TELEGRAM_BOT_TOKEN");
    std::env::remove_var("PANTHEON_GATEWAY_ALLOW");
    let (d, t, allow) = read_channel_env();
    assert!(d.is_none() && t.is_none() && allow.is_empty());
}

#[test]
fn disabled_note_names_the_fix() {
    let note = channels_disabled_note();
    assert!(note.contains("PANTHEON_TELEGRAM_BOT_TOKEN"));
    assert!(note.contains("PANTHEON_GATEWAY_ALLOW"));
    assert!(note.contains("scheduler"));
}

// ── stop / restart ──────────────────────────────────────────────────────

fn systemd_env(name: &str, stub_body: &str) -> (InstallEnv, PathBuf) {
    let root = tempdir(name);
    let bins = root.join("bin");
    let systemd_dir = root.join("systemd");
    let launchd_dir = root.join("launchd");
    for d in [&bins, &systemd_dir, &launchd_dir] {
        std::fs::create_dir_all(d).unwrap();
    }
    write_stub(&bins, "systemctl", stub_body);
    // The unit file existing is what marks the install.
    std::fs::write(systemd_dir.join(format!("{UNIT_NAME}.service")), "[Unit]\n").unwrap();
    (env_with_bins(&bins, &systemd_dir, &launchd_dir), root)
}

#[test]
fn restart_not_installed() {
    let root = tempdir("restart-none");
    let bins = root.join("bin");
    std::fs::create_dir_all(&bins).unwrap();
    write_stub(&bins, "systemctl", "#!/bin/sh\nexit 0\n");
    let env = env_with_bins(&bins, &root.join("systemd"), &root.join("launchd"));
    assert_eq!(status_with(&env).installed, None);
    assert_eq!(restart_with(&env), RestartOutcome::NotInstalled);
    assert_eq!(stop_with(&env), StopOutcome::NotInstalled);
}

#[test]
fn restart_systemd() {
    let (env, _root) = systemd_env("restart-systemd", "#!/bin/sh\nexit 0\n");
    assert_eq!(
        restart_with(&env),
        RestartOutcome::Restarted {
            mechanism: ServiceMechanism::Systemd
        }
    );
}

#[test]
fn restart_systemd_failure_is_honest() {
    let (env, _root) = systemd_env("restart-systemd-fail", "#!/bin/sh\nexit 1\n");
    match restart_with(&env) {
        RestartOutcome::Failed { mechanism, error } => {
            assert_eq!(mechanism, ServiceMechanism::Systemd);
            assert!(!error.is_empty());
        }
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[test]
fn stop_systemd() {
    let (env, _root) = systemd_env(
        "stop-systemd",
        "#!/bin/sh\nif [ \"$2\" = \"is-active\" ]; then echo active; fi\nexit 0\n",
    );
    assert_eq!(
        stop_with(&env),
        StopOutcome::Stopped {
            mechanism: ServiceMechanism::Systemd
        }
    );
}

#[test]
fn stop_systemd_already_stopped_is_still_stopped() {
    // stop fails but the unit is not active: idempotent success, not an error.
    let (env, _root) = systemd_env(
        "stop-systemd-idem",
        "#!/bin/sh\nif [ \"$2\" = \"stop\" ]; then exit 1; fi\nexit 0\n",
    );
    assert!(!status_with(&env).running);
    assert_eq!(
        stop_with(&env),
        StopOutcome::Stopped {
            mechanism: ServiceMechanism::Systemd
        }
    );
}

fn cron_env(name: &str) -> (InstallEnv, PathBuf) {
    let root = tempdir(name);
    let bins = root.join("bin");
    std::fs::create_dir_all(&bins).unwrap();
    // No systemctl in PATH: cron is the detected mechanism. The stub
    // answers `crontab -l` with our marker line.
    write_stub(
        &bins,
        "crontab",
        &format!("#!/bin/sh\necho '@reboot /usr/bin/pantheon gateway run {CRON_MARKER}'\n"),
    );
    (
        env_with_bins(&bins, &root.join("systemd"), &root.join("launchd")),
        root,
    )
}

#[test]
fn restart_cron_is_honest_noop() {
    let (env, _root) = cron_env("restart-cron");
    assert_eq!(status_with(&env).installed, Some(ServiceMechanism::Cron));
    match restart_with(&env) {
        RestartOutcome::Noop { mechanism, note } => {
            assert_eq!(mechanism, ServiceMechanism::Cron);
            assert!(note.contains("no-op") && note.contains("gateway run"));
        }
        other => panic!("expected Noop, got {other:?}"),
    }
}

#[test]
fn stop_cron_is_honest_noop() {
    let (env, _root) = cron_env("stop-cron");
    match stop_with(&env) {
        StopOutcome::Noop { mechanism, note } => {
            assert_eq!(mechanism, ServiceMechanism::Cron);
            assert!(note.contains("no running service"));
        }
        other => panic!("expected Noop, got {other:?}"),
    }
}
