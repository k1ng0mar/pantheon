//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_sandbox::level::ExecutionBoundary;
use pantheon_sandbox::runner::run_sandboxed;
use pantheon_sandbox::{SandboxLevel, SandboxProfile};

#[test]
fn echo_runs_and_produces_output() {
    let _env_lock = ENV_LOCK.lock().unwrap();
    let profile = SandboxLevel::Low.profile();
    let result = run_sandboxed(&profile, "echo", &["test-output"], "/tmp");
    assert!(result.is_ok(), "run failed: {:?}", result.err());
    let r = result.unwrap();
    assert!(r.output.contains("test-output"), "output was: {}", r.output);
}

#[test]
fn timeout_kills_long_running_command() {
    let _env_lock = ENV_LOCK.lock().unwrap();
    let profile = SandboxProfile {
        level: SandboxLevel::Low,
        boundary: ExecutionBoundary::InProcess,
        drop_capabilities: false,
        no_new_privs: false,
        network: false,
        max_memory_mb: None,
        max_pids: None,
        wall_clock_ms: 100, // 100ms timeout
        allow_direct_fallback: false,
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
    // Serialized with the env-mutating tests: they blank PATH, which
    // would break this helper's `sh` lookup mid-run.
    let _env_lock = ENV_LOCK.lock().unwrap();
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
    // through the (opted-in) direct fallback when it doesn't.
    let mut profile = SandboxLevel::High.profile();
    profile.allow_direct_fallback = true;
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
    // keeps untouched. The direct fallback is opted in so this also
    // passes where the wrapper cannot initialize.
    let mut profile = SandboxLevel::High.profile();
    profile.allow_direct_fallback = true;
    let want = u64::from(profile.max_pids.expect("High carries a pids cap"));
    let soft = proc_limit(&profile, "Max processes").expect("NPROC must be finite");
    assert!(
        soft > want,
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
        level: SandboxLevel::Low,
        boundary: ExecutionBoundary::InProcess,
        drop_capabilities: false,
        no_new_privs: false,
        network: false,
        max_memory_mb: Some(512),
        max_pids: Some(8),
        wall_clock_ms: 30_000,
        allow_direct_fallback: false,
    };
    assert_eq!(
        proc_limit(&profile, "Max address space"),
        Some(512 * 1024 * 1024)
    );
    let soft = proc_limit(&profile, "Max processes").expect("NPROC must be finite");
    assert!(soft > 8, "nproc limit {soft} must cover the 8-pid cap");
    if let Some(hard) = host_limit("Max processes") {
        assert!(soft <= hard, "got {soft}, host hard {hard}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn low_profile_imposes_no_limits() {
    // Low sets neither field: the child must keep the system's own
    // defaults exactly — proven against this process's limits.
    let profile = SandboxLevel::Low.profile();
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

/// Env mutation is process-global and tests run on threads: serialize
/// every test that touches PATH / PANTHEON_SANDBOX_FALLBACK.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Restores PATH and PANTHEON_SANDBOX_FALLBACK on drop, so a panicking
/// test cannot poison the rest of the suite.
struct EnvGuard {
    path: Option<std::ffi::OsString>,
    fallback: Option<std::ffi::OsString>,
}

impl EnvGuard {
    /// Empty PATH: `which` cannot resolve anything, so `has("bwrap")` /
    /// `has("unshare")` are false and wrapper binaries are "missing".
    /// Absolute program paths still spawn (execvp skips PATH lookup when
    /// the name contains a slash).
    fn break_wrapper_lookup() -> Self {
        let guard = Self {
            path: std::env::var_os("PATH"),
            fallback: std::env::var_os("PANTHEON_SANDBOX_FALLBACK"),
        };
        std::env::set_var("PATH", "");
        std::env::remove_var("PANTHEON_SANDBOX_FALLBACK");
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.path.take() {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        match self.fallback.take() {
            Some(v) => std::env::set_var("PANTHEON_SANDBOX_FALLBACK", v),
            None => std::env::remove_var("PANTHEON_SANDBOX_FALLBACK"),
        }
    }
}

#[test]
fn wrapper_init_failure_fails_closed_without_opt_in() {
    // No wrapper binary visible and no fallback opt-in: the runner must
    // refuse to run, not silently drop the isolation.
    let _lock = ENV_LOCK.lock().unwrap();
    let _env = EnvGuard::break_wrapper_lookup();
    let profile = SandboxLevel::High.profile();
    assert!(!profile.allow_direct_fallback);
    let r = run_sandboxed(&profile, "/bin/sh", &["-c", "echo unreachable"], "/tmp");
    let e = r.expect_err("must fail closed when the wrapper cannot initialize");
    assert_eq!(e.code, "SANDBOX_UNAVAILABLE", "unexpected error: {e:?}");
}

#[test]
fn direct_fallback_allowed_via_env_opt_in() {
    // Same broken wrapper lookup, but PANTHEON_SANDBOX_FALLBACK=allow:
    // the command runs directly, flagged as unsandboxed.
    let _lock = ENV_LOCK.lock().unwrap();
    let _env = EnvGuard::break_wrapper_lookup();
    std::env::set_var("PANTHEON_SANDBOX_FALLBACK", "allow");
    let profile = SandboxLevel::High.profile();
    let r = run_sandboxed(&profile, "/bin/sh", &["-c", "echo fallback-ok"], "/tmp")
        .expect("opted-in fallback must run");
    assert_eq!(r.exit_code, 0, "command failed: {}", r.output);
    assert!(r.output.contains("fallback-ok"), "got: {}", r.output);
    assert!(!r.sandboxed, "fallback run must be flagged unsandboxed");
}

#[test]
fn direct_fallback_allowed_via_profile_opt_in() {
    // Same, but opted in on the profile instead of the environment.
    let _lock = ENV_LOCK.lock().unwrap();
    let _env = EnvGuard::break_wrapper_lookup();
    let mut profile = SandboxLevel::High.profile();
    profile.allow_direct_fallback = true;
    let r = run_sandboxed(
        &profile,
        "/bin/sh",
        &["-c", "echo profile-fallback-ok"],
        "/tmp",
    )
    .expect("opted-in fallback must run");
    assert_eq!(r.exit_code, 0, "command failed: {}", r.output);
    assert!(
        r.output.contains("profile-fallback-ok"),
        "got: {}",
        r.output
    );
    assert!(!r.sandboxed, "fallback run must be flagged unsandboxed");
}

#[test]
fn large_stdout_does_not_deadlock() {
    let _env_lock = ENV_LOCK.lock().unwrap();
    // 200KiB of stdout — over the 64KiB pipe buffer. The old code only
    // read the pipes after try_wait reported exit, so this wedged until
    // the wall-clock timeout; the drainer threads must let it complete.
    let profile = SandboxLevel::Low.profile();
    let r = run_sandboxed(
        &profile,
        "sh",
        &["-c", "head -c 200000 /dev/zero | tr '\\0' 'x'"],
        "/tmp",
    )
    .expect("large-output child must complete");
    assert_eq!(
        r.exit_code,
        0,
        "command failed: {}",
        &r.output[..r.output.len().min(200)]
    );
    assert_eq!(r.output.len(), 200_000, "expected all 200KiB captured");
}

#[test]
fn output_over_cap_is_truncated_with_marker() {
    let _env_lock = ENV_LOCK.lock().unwrap();
    // 3MiB of stdout against the 2MiB cap: output must be cut at the cap
    // with a marker naming the dropped byte count.
    let profile = SandboxLevel::Low.profile();
    let r = run_sandboxed(
        &profile,
        "sh",
        &["-c", "head -c 3000000 /dev/zero | tr '\\0' 'x'"],
        "/tmp",
    )
    .expect("over-cap child must complete");
    assert_eq!(r.exit_code, 0, "command failed");
    let dropped = 3_000_000 - 2 * 1024 * 1024;
    let marker = format!("\n[...truncated {dropped} bytes...]");
    assert!(
        r.output.ends_with(&marker),
        "missing truncation marker; tail was: {:?}",
        &r.output[r.output.len().saturating_sub(80)..]
    );
    assert!(
        r.output.len() < 3_000_000,
        "output was not capped: {} bytes",
        r.output.len()
    );
}

#[test]
fn timeout_error_redacts_secret_bearing_args() {
    let _env_lock = ENV_LOCK.lock().unwrap();
    let profile = SandboxProfile {
        level: SandboxLevel::Low,
        boundary: ExecutionBoundary::InProcess,
        drop_capabilities: false,
        no_new_privs: false,
        network: false,
        max_memory_mb: None,
        max_pids: None,
        wall_clock_ms: 100, // 100ms timeout
        allow_direct_fallback: false,
    };
    // The canary rides in the args: a timeout error that echoes them
    // verbatim leaks secrets into the ledger and the model transcript.
    let result = run_sandboxed(
        &profile,
        "sh",
        &["-c", "sleep 5 # CANARY_do_not_leak_9f8e7d"],
        "/tmp",
    );
    let Err(e) = result else {
        panic!("expected a SANDBOX_TIMEOUT, the command finished instead");
    };
    assert_eq!(e.code, "SANDBOX_TIMEOUT");
    assert!(
        !e.cause.contains("CANARY_do_not_leak_9f8e7d"),
        "timeout error leaked raw arg text: {}",
        e.cause
    );
    assert!(
        !e.cause.contains("sleep 5"),
        "timeout error leaked raw arg text: {}",
        e.cause
    );
    // ...but it still carries the digest+length treatment, so the timeout
    // can be correlated with the request that caused it.
    assert!(
        e.cause.contains("cmd:") && e.cause.contains("len:"),
        "timeout error lost its digest: {}",
        e.cause
    );
}
