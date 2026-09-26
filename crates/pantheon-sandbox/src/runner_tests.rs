//! Tests for `pantheon_sandbox::runner::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn in_process_boundary_uses_direct_command() {
    let profile = SandboxProfile {
        level: crate::SandboxLevel::Low,
        boundary: crate::ExecutionBoundary::InProcess,
        drop_capabilities: false,
        no_new_privs: false,
        network: false,
        max_memory_mb: None,
        max_pids: None,
        wall_clock_ms: 30_000,
    };
    let cmd = build_sandboxed(&profile, "echo", &["hello"], "/tmp");
    assert_eq!(cmd.get_program(), "echo");
}

#[test]
fn container_boundary_uses_bwrap_if_available() {
    let profile = crate::SandboxLevel::High.profile();
    let cmd = build_sandboxed(&profile, "echo", &["hello"], "/tmp");
    let prog = cmd.get_program().to_string_lossy().to_string();
    // Should be bwrap if available, or echo as fallback
    assert!(prog == "bwrap" || prog == "echo", "got: {}", prog);
}

#[test]
fn high_level_attempts_userns() {
    let profile = crate::SandboxLevel::High.profile();
    let cmd = build_sandboxed(&profile, "ls", &[], "/tmp");
    let prog = cmd.get_program().to_string_lossy().to_string();
    if prog == "bwrap" {
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // High should attempt user namespace isolation
        assert!(
            args.iter().any(|a| a == "--unshare-user-try"),
            "high should attempt user namespace: {:?}",
            args
        );
    }
    // If bwrap is not available, the command falls back to direct exec
}

#[test]
fn echo_runs_and_produces_output() {
    let profile = crate::SandboxLevel::Low.profile();
    let result = run_sandboxed(&profile, "echo", &["test-output"], "/tmp");
    assert!(result.is_ok(), "run failed: {:?}", result.err());
    let r = result.unwrap();
    assert!(r.output.contains("test-output"), "output was: {}", r.output);
}

#[test]
fn timeout_kills_long_running_command() {
    let profile = SandboxProfile {
        level: crate::SandboxLevel::Low,
        boundary: crate::ExecutionBoundary::InProcess,
        drop_capabilities: false,
        no_new_privs: false,
        network: false,
        max_memory_mb: None,
        max_pids: None,
        wall_clock_ms: 100, // 100ms timeout
    };
    let result = run_sandboxed(&profile, "sleep", &["5"], "/tmp");
    assert!(result.is_err(), "should have timed out");
    if let Err(e) = result {
        assert_eq!(e.code, "SANDBOX_TIMEOUT");
    }
}

/// Parse one limit line ("name soft hard units") from
/// `/proc/self/limits` text. `None` = unlimited.
#[cfg(target_os = "linux")]
fn parse_limit(text: &str, what: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with(what))
        .unwrap_or_else(|| panic!("no '{what}' line in {text}"));
    // Trailing shape: "<soft> <hard> <units>" — name words vary
    // ("Max processes" vs "Max address space"), so parse from the end.
    let tokens: Vec<&str> = line.split_whitespace().collect();
    assert!(tokens.len() >= 3, "malformed line: {line}");
    tokens[tokens.len() - 3].parse().ok()
}

/// This test process's own limit — the baseline a child must keep
/// when the profile imposes nothing.
#[cfg(target_os = "linux")]
fn host_limit(what: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/limits").unwrap();
    parse_limit(&text, what)
}

/// Run a command under `profile` and read back one of its limits.
#[cfg(target_os = "linux")]
fn proc_limit(profile: &SandboxProfile, what: &str) -> Option<u64> {
    let r = run_sandboxed(
        profile,
        "sh",
        &["-c", &format!("grep '{what}' /proc/self/limits")],
        "/tmp",
    )
    .expect("run");
    assert_eq!(r.exit_code, 0, "command failed: {}", r.output);
    parse_limit(&r.output, what)
}

#[test]
#[cfg(target_os = "linux")]
fn memory_cap_is_applied_as_rlimit_as() {
    // Whatever the level carries as max_memory_mb must reach the
    // child as RLIMIT_AS — through the wrapper when it works here,
    // through the direct fallback when it doesn't.
    let profile = crate::SandboxLevel::High.profile();
    let want = profile.max_memory_mb.expect("High carries a memory cap") * 1024 * 1024;
    assert_eq!(
        proc_limit(&profile, "Max address space"),
        Some(want),
        "profile's max_memory_mb must be the child's address-space cap"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn pids_cap_is_applied_as_rlimit_nproc() {
    // NPROC is calibrated against the kernel's true usage and grants
    // max_pids above it: strictly more than the raw cap (usage ≥ 1),
    // never more than the user's system hard limit — which the parent
    // keeps untouched.
    let profile = crate::SandboxLevel::High.profile();
    let want = u64::from(profile.max_pids.expect("High carries a pids cap"));
    let soft = proc_limit(&profile, "Max processes").expect("NPROC must be finite");
    assert!(
        soft >= want + 1,
        "nproc limit {soft} must grant max_pids ({want}) above at least our own process"
    );
    if let Some(hard) = host_limit("Max processes") {
        assert!(
            soft <= hard,
            "child cap {soft} must not exceed the host hard limit {hard}"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn in_process_boundary_also_gets_the_limits() {
    // Limits live on the child, not the wrapper: even a direct spawn
    // (no bwrap/unshare) enforces the profile.
    let profile = SandboxProfile {
        level: crate::SandboxLevel::Low,
        boundary: crate::ExecutionBoundary::InProcess,
        drop_capabilities: false,
        no_new_privs: false,
        network: false,
        max_memory_mb: Some(512),
        max_pids: Some(8),
        wall_clock_ms: 30_000,
    };
    assert_eq!(
        proc_limit(&profile, "Max address space"),
        Some(512 * 1024 * 1024)
    );
    let soft = proc_limit(&profile, "Max processes").expect("NPROC must be finite");
    assert!(soft >= 8 + 1, "nproc limit {soft} must cover the 8-pid cap");
    if let Some(hard) = host_limit("Max processes") {
        assert!(soft <= hard, "got {soft}, host hard {hard}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn low_profile_imposes_no_limits() {
    // Low sets neither field: the child must keep the system's own
    // defaults exactly — proven against this process's limits.
    let profile = crate::SandboxLevel::Low.profile();
    assert_eq!(
        proc_limit(&profile, "Max address space"),
        host_limit("Max address space"),
        "Low must not impose a memory cap"
    );
    assert_eq!(
        proc_limit(&profile, "Max processes"),
        host_limit("Max processes"),
        "Low must not impose a pids cap"
    );
}

#[test]
fn a_command_still_runs_when_the_wrapper_cannot_initialize() {
    // The runner's contract: wrapper-init failure degrades to direct
    // execution instead of failing every command. Environments where
    // bwrap works take the wrapper path instead — either way the
    // command's output must come back.
    let profile = crate::SandboxLevel::High.profile();
    let r = run_sandboxed(&profile, "sh", &["-c", "echo fallback-sentinel"], "/tmp").expect("run");
    assert_eq!(r.exit_code, 0, "command failed: {}", r.output);
    assert!(r.output.contains("fallback-sentinel"), "got: {}", r.output);
}
