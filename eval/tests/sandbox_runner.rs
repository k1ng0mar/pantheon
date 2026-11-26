//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_exec::sandbox::level::ExecutionBoundary;
use pantheon_exec::sandbox::runner::{build_sandboxed_checked, run_sandboxed};
use pantheon_exec::sandbox::{SandboxLevel, SandboxProfile};

#[test]
fn echo_runs_and_produces_output() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let profile = SandboxLevel::Low.profile();
    let result = run_sandboxed(&profile, "echo", &["test-output"], "/tmp");
    assert!(result.is_ok(), "run failed: {:?}", result.err());
    let r = result.unwrap();
    assert!(r.output.contains("test-output"), "output was: {}", r.output);
}

#[test]
fn timeout_kills_long_running_command() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        allow_env_fallback: true,
        seccomp: false,
        landlock: false,
        writable_paths: Vec::new(),
        hidden_paths: Vec::new(),
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
    // Trailing shape: "<soft> <hard> <units>" - name words vary
    // ("Max processes" vs "Max address space"), so parse from the end.
    let tokens: Vec<&str> = line.split_whitespace().collect();
    assert!(tokens.len() >= 3, "malformed line: {line}");
    tokens[tokens.len() - 3].parse().ok()
}

/// This test process's own limit - the baseline a child must keep
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
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    // child as RLIMIT_AS - through the wrapper when it works here,
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
    // never more than the user's system hard limit - which the parent
    // keeps untouched. The direct fallback is opted in so this also
    // passes where the wrapper cannot initialize.
    let mut profile = SandboxLevel::High.profile();
    profile.allow_direct_fallback = true;
    let want = u64::from(profile.max_pids.expect("High carries a pids cap"));
    if host_is_root() {
        // Root: the kernel does not enforce NPROC, so calibration
        // declines to cap. The child must keep the inherited limit, not
        // the probe's leftover soft limit of 1 (audit COR-01).
        assert_nproc_inherited(&profile);
        return;
    }
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

/// Whether this test process runs as uid 0, where the kernel skips
/// RLIMIT_NPROC enforcement and the calibrator must leave NPROC alone.
#[cfg(target_os = "linux")]
fn host_is_root() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .and_then(|l| l.split_whitespace().nth(2))
        .is_some_and(|euid| euid == "0")
}

/// The child's NPROC soft limit equals this process's (the inherited
/// value): the uncapped outcome.
#[cfg(target_os = "linux")]
fn assert_nproc_inherited(profile: &SandboxProfile) {
    let soft = proc_limit(profile, "Max processes");
    let host = host_limit("Max processes");
    assert_eq!(
        soft, host,
        "uncapped NPROC must stay at the inherited soft limit {host:?}, got {soft:?}"
    );
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
        allow_env_fallback: true,
        seccomp: false,
        landlock: false,
        writable_paths: Vec::new(),
        hidden_paths: Vec::new(),
    };
    assert_eq!(
        proc_limit(&profile, "Max address space"),
        Some(512 * 1024 * 1024)
    );
    if host_is_root() {
        assert_nproc_inherited(&profile);
        return;
    }
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
    // defaults exactly - proven against this process's limits.
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

struct EnvPathGuard(Option<std::ffi::OsString>);
impl EnvPathGuard {
    fn prepend(dir: &std::path::Path) -> Self {
        let previous = std::env::var_os("PATH");
        let joined = match &previous {
            Some(p) => format!("{}:{}", dir.display(), p.to_string_lossy()),
            None => dir.display().to_string(),
        };
        std::env::set_var("PATH", joined);
        Self(previous)
    }
}
impl Drop for EnvPathGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}

#[test]
fn wrapper_init_failure_fails_closed_without_opt_in() {
    // No wrapper binary visible and no fallback opt-in: the runner must
    // refuse to run, not silently drop the isolation.
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

/// The fail-closed gate is shared by every caller: the MCP client and the
/// Python plugin runner build and spawn themselves (they cannot use
/// `run_sandboxed`), so they go through `build_sandboxed_checked`. When the
/// boundary needs a wrapper that cannot initialize and no fallback is opted
/// in, that helper must refuse rather than hand back an un-isolated command
/// - the silent degrade the README's fail-closed promise forbids.
#[test]
fn build_sandboxed_checked_fails_closed_without_opt_in() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::break_wrapper_lookup();
    let profile = SandboxLevel::VeryHigh.profile();
    assert!(!profile.allow_direct_fallback);
    let e = build_sandboxed_checked(&profile, "/bin/sh", &["-c", "echo unreachable"], "/tmp")
        .expect_err("must fail closed when the wrapper cannot initialize");
    assert_eq!(e.code, "SANDBOX_UNAVAILABLE", "unexpected error: {e:?}");
}

/// Same helper, with the fallback explicitly opted in: it must build a
/// command and honestly report that it is not isolated.
#[test]
fn build_sandboxed_checked_honors_opt_in_fallback() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::break_wrapper_lookup();
    std::env::set_var("PANTHEON_SANDBOX_FALLBACK", "allow");
    let profile = SandboxLevel::VeryHigh.profile();
    let (_cmd, sandboxed) = build_sandboxed_checked(&profile, "/bin/sh", &["-c", "true"], "/tmp")
        .expect("opted-in fallback must build");
    assert!(!sandboxed, "fallback build must be flagged unsandboxed");
}

#[test]
fn large_stdout_does_not_deadlock() {
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 200KiB of stdout - over the 64KiB pipe buffer. The old code only
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
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        allow_env_fallback: true,
        seccomp: false,
        landlock: false,
        writable_paths: Vec::new(),
        hidden_paths: Vec::new(),
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

/// The argv the High and VeryHigh boundaries hand to bwrap: the user
/// namespace is REQUIRED (`--unshare-user`, never the best-effort
/// `--unshare-user-try`), and capabilities are dropped explicitly so a
/// privileged caller cannot carry them into the sandbox (audit SEC-10).
#[test]
#[cfg(target_os = "linux")]
fn container_boundaries_require_user_namespace_and_drop_caps() {
    use pantheon_exec::sandbox::build_sandboxed;
    for level in [SandboxLevel::High, SandboxLevel::VeryHigh] {
        let profile = level.profile();
        let cmd = build_sandboxed(&profile, "true", &[], "/tmp");
        if !cmd.get_program().to_string_lossy().ends_with("bwrap") {
            eprintln!("skip {level:?}: bwrap not installed");
            continue;
        }
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.iter().any(|a| a == "--unshare-user"),
            "{level:?} must require a user namespace: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a == "--unshare-user-try"),
            "{level:?} must not fall back to the host user namespace: {args:?}"
        );
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--cap-drop" && w[1] == "ALL"),
            "{level:?} must drop every capability: {args:?}"
        );
    }
}

/// Run as root (`sudo` on the test binary): the sandboxed child of the
/// High profile has no effective capabilities, in any namespace. Before
/// the fix, `--unshare-user-try` gave a root caller a fresh user
/// namespace with a full capability set.
#[test]
#[cfg(target_os = "linux")]
fn high_profile_child_has_no_capabilities_even_for_root() {
    if !host_is_root() {
        eprintln!("skip: needs root (the unprivileged case has no caps to drop)");
        return;
    }
    let profile = SandboxLevel::High.profile();
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let r = match run_sandboxed(
        &profile,
        "sh",
        &["-c", "grep CapEff /proc/self/status"],
        "/tmp",
    ) {
        Ok(r) => r,
        Err(e) if e.code == "SANDBOX_UNAVAILABLE" => {
            eprintln!("skip: no user namespaces on this host ({})", e.cause);
            return;
        }
        Err(e) => panic!("run failed: {} {}", e.code, e.cause),
    };
    assert!(r.sandboxed, "must run inside bwrap");
    let eff = r
        .output
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("no CapEff in {}", r.output));
    assert_eq!(
        u64::from_str_radix(eff, 16).unwrap(),
        0,
        "child CapEff must be empty, got {eff}"
    );
}

/// VeryHigh (bwrap --unshare-all + Landlock + seccomp) must actually run
/// a command where the host supports the layers: a tier that always
/// fails is not a sandbox (audit COR-02).
#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn very_high_profile_runs_a_command() {
    use pantheon_exec::sandbox::kernel_layer_available;
    let profile = SandboxLevel::VeryHigh.profile();
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !kernel_layer_available(profile.seccomp, profile.landlock) {
        eprintln!("skip: host lacks seccomp or Landlock");
        return;
    }
    // The runner's own checked builder decides whether this host can stage
    // the real wrapper shape. A simpler bwrap probe can succeed while the
    // profile's seccomp fd cannot be staged, which made this test panic on
    // a host where the product correctly fails closed.
    if build_sandboxed_checked(&profile, "/bin/true", &[], "/tmp").is_err() {
        eprintln!("skip: VeryHigh wrapper cannot initialize on this host");
        return;
    }
    let r = run_sandboxed(&profile, "/bin/true", &[], "/tmp")
        .unwrap_or_else(|e| panic!("VeryHigh spawn failed: {} {}", e.code, e.cause));
    assert_eq!(r.exit_code, 0, "VeryHigh child failed: {}", r.output);
    assert!(r.sandboxed);
    // The filter is live inside the sandbox: an escape primitive on the
    // deny list (unshare) is refused, while ordinary work still runs.
    let r = run_sandboxed(
        &profile,
        "sh",
        &["-c", "unshare -U true && echo ESCAPED; echo done"],
        "/tmp",
    )
    .expect("spawn");
    assert!(
        !r.output.contains("ESCAPED"),
        "unshare must be denied: {}",
        r.output
    );
    assert!(
        r.output.contains("done"),
        "shell must keep running: {}",
        r.output
    );
}

/// The unshare (Medium) boundary leaves the host read-only and refuses
/// to run when a mount cannot be made read-only (audit SEC-11). Root
/// only on hosts that restrict unprivileged user namespaces.
#[test]
#[cfg(target_os = "linux")]
fn medium_boundary_is_read_only_and_fails_closed_on_mount_errors() {
    use pantheon_exec::sandbox::boundary_available;
    let profile = SandboxLevel::Medium.profile();
    if !boundary_available(profile.boundary) {
        eprintln!("skip: unshare cannot create a user namespace here");
        return;
    }
    let _env_lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let probe = "/usr/pantheon-sec11-probe";
    let r = run_sandboxed(
        &profile,
        "sh",
        &[
            "-c",
            &format!("touch {probe} 2>/dev/null && echo WROTE; echo done"),
        ],
        "/tmp",
    )
    .expect("spawn");
    assert!(r.output.contains("done"), "{}", r.output);
    assert!(
        !r.output.contains("WROTE"),
        "host must be read-only: {}",
        r.output
    );
    assert!(!std::path::Path::new(probe).exists());

    // A `mount` that always fails: the script must refuse, not run.
    // Outside any 0750 home: root inside the user namespace must be
    // able to traverse to the shim, or `sh` silently falls back to the
    // real `mount` further down PATH.
    let shim = std::path::PathBuf::from(if host_is_root() {
        "/srv"
    } else {
        env!("CARGO_TARGET_TMPDIR")
    })
    .join(format!("sec11-shim-{}", std::process::id()));
    std::fs::create_dir_all(&shim).unwrap();
    let fake = shim.join("mount");
    std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let _path = EnvPathGuard::prepend(&shim);
    let r = run_sandboxed(&profile, "sh", &["-c", "echo RAN"], "/tmp");
    let _ = std::fs::remove_dir_all(&shim);
    // The wrapper probe runs the same script, so the refusal surfaces
    // either as the probe failing closed or as the script's exit 126.
    match r {
        Err(e) => assert_eq!(e.code, "SANDBOX_UNAVAILABLE", "{e:?}"),
        Ok(r) => {
            assert!(
                !r.output.contains("RAN"),
                "must not run with failed mounts: {}",
                r.output
            );
            assert_eq!(r.exit_code, 126, "{}", r.output);
        }
    }
}
