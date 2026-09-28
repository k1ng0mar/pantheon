//! Sandbox executor: runs tool subprocesses behind a real boundary.
//!
//! Maps `ExecutionBoundary` onto concrete host mechanisms:
//! - `InProcess`: direct `Command` (caller already gated)
//! - `IsolatedProcess`: `unshare` (PID + mount namespace) + rlimits
//! - `Container`: `bwrap` (bubblewrap) with user/mount/PID namespaces + rlimits
//! - `Vm`: falls back to `Container` (bwrap with --unshare-all) — VM is Phase B
//!
//! When the sandbox binary (bwrap/unshare) is unavailable or fails to
//! initialize, the runner FAILS CLOSED: it returns a `SANDBOX_UNAVAILABLE`
//! error rather than running the command without isolation. The old
//! silent-degrade to a direct host spawn is only available via explicit
//! opt-in — `SandboxProfile::allow_direct_fallback` or the
//! `PANTHEON_SANDBOX_FALLBACK=allow` environment variable. The capability
//! gate is always the first check, so the fallback (when opted in) only
//! loses the OS-level isolation, never the policy enforcement.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::SandboxProfile;
use pantheon_api::error::{Layer, PantheonError};

/// Result of a sandboxed command: stdout/stderr merged, exit code, timeout flag.
#[derive(Debug)]
pub struct SandboxResult {
    pub output: String,
    pub exit_code: i32,
    pub timed_out: bool,
    /// Whether the sandbox binary was actually available. If false, the
    /// command ran un-isolated (capability gate is still enforced upstream).
    pub sandboxed: bool,
}

/// Check whether a binary is available on PATH.
fn has(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdout(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Build a sandboxed command without spawning.
///
/// `program` is the binary to run. `args` are its argv. `cwd` is the
/// working directory (must be an absolute path).
/// `profile` selects the boundary and limit set.
///
/// The timeout is always derived from `SandboxProfile::wall_clock_ms`,
/// so every tool call is inherently bounded — there is no unbounded wait.
pub fn build_sandboxed(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
) -> Command {
    #[allow(unused_mut)] // non-unix has no rlimits to attach
    let mut cmd = match profile.boundary {
        crate::ExecutionBoundary::InProcess => {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.current_dir(cwd);
            cmd
        }

        crate::ExecutionBoundary::IsolatedProcess => {
            // unshare PID + mount namespace. No network: the profile for
            // Medium already has network: false.
            if has("unshare") {
                let mut cmd = Command::new("unshare");
                cmd.arg("--pid")
                    .arg("--mount")
                    .arg("--fork")
                    .arg("--mount-proc");
                cmd.current_dir(cwd);
                cmd.arg(program);
                cmd.args(args);
                cmd
            } else {
                let mut cmd = Command::new(program);
                cmd.args(args);
                cmd.current_dir(cwd);
                cmd
            }
        }

        crate::ExecutionBoundary::Container => {
            // bwrap: the strongest general-purpose sandbox without a VM.
            // --unshare-user-try attempts a user namespace (so no caps survive),
            // falling back gracefully if user namespaces are not available.
            // --unshare-pid + --unshare-cgroup limit process creation.
            if has("bwrap") {
                let mut cmd = Command::new("bwrap");
                cmd.arg("--die-with-parent")
                    .arg("--ro-bind")
                    .arg("/")
                    .arg("/");

                cmd.arg("--dev").arg("/dev");
                cmd.arg("--proc").arg("/proc");

                cmd.arg("--tmpfs").arg("/tmp");
                cmd.arg("--tmpfs").arg("/var/tmp");

                if profile.drop_capabilities {
                    cmd.arg("--unshare-user-try");
                    cmd.arg("--unshare-pid");
                    cmd.arg("--unshare-cgroup");
                    cmd.arg("--unshare-uts");
                    cmd.arg("--unshare-ipc");
                }

                // Network: only if the profile allows it.
                if !profile.network {
                    cmd.arg("--unshare-net");
                }

                cmd.arg("--chdir").arg(cwd);
                cmd.arg("--");
                cmd.arg(program);
                cmd.args(args);
                cmd
            } else {
                let mut cmd = Command::new(program);
                cmd.args(args);
                cmd.current_dir(cwd);
                cmd
            }
        }

        crate::ExecutionBoundary::Vm => {
            // Phase A: VM-level isolation (Firecracker/krun) is out of scope.
            // We use bwrap with --unshare-all + --dev-bind for the program,
            // which gives strong isolation. A real VM backend is Phase B.
            // This level is only reached after interactive approval.
            if has("bwrap") {
                let mut cmd = Command::new("bwrap");
                cmd.arg("--die-with-parent")
                    .arg("--unshare-all")
                    .arg("--ro-bind")
                    .arg("/")
                    .arg("/")
                    .arg("--dev")
                    .arg("/dev")
                    .arg("--proc")
                    .arg("/proc")
                    .arg("--tmpfs")
                    .arg("/tmp")
                    .arg("--chdir")
                    .arg(cwd)
                    .arg("--")
                    .arg(program);
                cmd.args(args);
                cmd
            } else if has("unshare") {
                let mut cmd = Command::new("unshare");
                cmd.arg("--pid")
                    .arg("--mount")
                    .arg("--fork")
                    .arg("--mount-proc");
                cmd.current_dir(cwd);
                cmd.arg(program);
                cmd.args(args);
                cmd
            } else {
                let mut cmd = Command::new(program);
                cmd.args(args);
                cmd.current_dir(cwd);
                cmd
            }
        }
    };

    // The profile's limits are rlimits on the child itself, set just
    // before exec — so they hold with or without a namespace wrapper and
    // survive the (opt-in) fallback to a direct spawn.
    #[cfg(unix)]
    confine_child(&mut cmd, profile);
    cmd
}

/// Child-side setup, issued in the forked child just before exec (via
/// `pre_exec`):
///
/// 1. `setsid()`: the child becomes a session leader, so its pid is its
///    process-group id. On timeout [`run_sandboxed`] kills the whole group
///    (`killpg`), which also reaps grandchildren the direct child spawned.
///    A forked child is never a process-group leader, so `setsid` cannot
///    fail with EPERM here; any other failure aborts the spawn.
/// 2. The profile's limits as rlimits:
///
/// - `max_memory_mb` → `RLIMIT_AS`: the address space the child may map.
/// - `max_pids` → `RLIMIT_NPROC`: extra processes it may spawn.
///
/// NPROC needs care: the kernel's accounting is UID-wide, not
/// per-sandbox, and in containers sharing the host user namespace it
/// counts processes this container's `/proc` cannot even show. An
/// absolute cap — or one derived from a `/proc` scan — therefore refuses
/// every fork the wrapper itself needs, turning the pids cap into a
/// denial of service. Instead we calibrate against the kernel directly:
/// binary-search the smallest NPROC limit at which a fork still
/// succeeds; that boundary is true usage, and the profile's `max_pids`
/// is granted above it. The probing uses only setrlimit/fork/_exit/
/// waitpid — async-signal-safe in the pre-exec zone. Where NPROC isn't
/// enforced for this user (some containers), or a calibrated cap can no
/// longer fork, the pids cap is skipped and every other limit still
/// applies.
///
/// Limits are only ever lowered, and only in the child — the parent is
/// untouched. The wall-clock budget is enforced separately by
/// [`run_sandboxed`], and the capability gate runs before any of this.
#[cfg(unix)]
fn confine_child(cmd: &mut Command, profile: &SandboxProfile) {
    use std::os::unix::process::CommandExt;

    let as_bytes = profile
        .max_memory_mb
        .map(|mb| mb.saturating_mul(1024 * 1024));
    let want_pids = profile.max_pids.map(u64::from);
    // NPROC's hard ceiling, read now (parent, safe context): probes must
    // stay within it and the final cap may not exceed it.
    let nproc_hard = want_pids.and_then(|_| {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        (unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) } == 0).then_some(lim.rlim_max)
    });

    // SAFETY: between fork and exec the closure only issues setsid and
    // setrlimit (plus fork/waitpid/_exit inside calibration) with plain
    // integers — no allocation, no locks. All measuring happens above,
    // in the parent.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(bytes) = as_bytes {
                let lim = libc::rlimit {
                    rlim_cur: bytes as libc::rlim_t,
                    rlim_max: bytes as libc::rlim_t,
                };
                if libc::setrlimit(libc::RLIMIT_AS, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let (Some(want), Some(hard)) = (want_pids, nproc_hard) {
                if let Some(cap) = calibrate_nproc(want, hard) {
                    let lim = libc::rlimit {
                        rlim_cur: cap,
                        rlim_max: cap,
                    };
                    if libc::setrlimit(libc::RLIMIT_NPROC, &lim) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
            }
            Ok(())
        });
    }
}

/// A NPROC cap granting `want` processes above the kernel's true current
/// usage for this UID — or `None` to leave NPROC untouched.
///
/// SAFETY: pre-exec context — only setrlimit/fork/_exit/waitpid, no
/// allocation.
#[cfg(unix)]
unsafe fn calibrate_nproc(want: u64, hard: libc::rlim_t) -> Option<libc::rlim_t> {
    /// Would `limit` currently allow a fork? Resets NPROC's soft limit
    /// as a side effect (the caller re-sets the final value afterwards).
    unsafe fn forkable(limit: u64, hard: libc::rlim_t) -> bool {
        let lim = libc::rlimit {
            rlim_cur: limit as libc::rlim_t,
            rlim_max: hard,
        };
        if libc::setrlimit(libc::RLIMIT_NPROC, &lim) != 0 {
            return false;
        }
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            // Only EAGAIN carries information about NPROC; anything else
            // (ENOMEM…) must not steer the search.
            return std::io::Error::last_os_error().raw_os_error() != Some(libc::EAGAIN);
        }
        if pid == 0 {
            unsafe { libc::_exit(0) };
        }
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        true
    }

    if hard == 0 {
        return None;
    }
    // Not enforced for this user: a cap would be decoration.
    if unsafe { forkable(1, hard) } {
        return None;
    }
    // Search ceiling: covers any realistic usage, and stays far from
    // RLIM_INFINITY (u64::MAX would need 64 halvings to converge).
    let top = hard.min(1 << 20);
    let mut lo = 1u64; // known unforkable (usage ≥ 1: we exist)
    let mut hi = top; // assumed forkable (we are running under it)
    if !unsafe { forkable(hi, hard) } {
        // Usage is at the ceiling — nothing safe to grant.
        return None;
    }
    while hi - lo > 16 {
        let mid = lo + (hi - lo) / 2;
        if unsafe { forkable(mid, hard) } {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    // `hi` ≈ usage + 1; grant the profile's headroom above it.
    let cap = hi.saturating_add(want).min(hard);
    // Belt and braces: never apply a cap that can't fork — degrade to
    // no pids limit instead of breaking the command.
    if !unsafe { forkable(cap, hard) } {
        return None;
    }
    Some(cap)
}

/// A plain direct command (no wrapper); rlimits attached by the caller.
fn direct_command(program: &str, args: &[&str], cwd: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd
}

/// Has this boundary's wrapper successfully initialized once in this
/// process? Probed once and cached: a wrapper that cannot initialize
/// here (missing privileges, blocked user namespaces) would fail *every*
/// command before it runs, so [`run_sandboxed`] fails closed (or takes
/// the opt-in direct fallback) instead of running each command
/// un-isolated.
fn wrapper_initializes(profile: &SandboxProfile, cwd: &str) -> bool {
    static PROBE: [std::sync::OnceLock<bool>; 4] = [
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
        std::sync::OnceLock::new(),
    ];
    *PROBE[profile.boundary as usize].get_or_init(|| {
        let mut probe = build_sandboxed(profile, "sh", &["-c", "true"], cwd);
        probe
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let Ok(mut child) = probe.spawn() else {
            return false;
        };
        // Bounded: a wedged wrapper must not hang the first command.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.success(),
                Ok(None) if std::time::Instant::now() > deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => return false,
            }
        }
    })
}

/// Whether the direct-spawn fallback is opted in: either the profile's
/// `allow_direct_fallback` or `PANTHEON_SANDBOX_FALLBACK=allow` in the
/// environment (a deployment-level override that needs no profile change).
fn direct_fallback_allowed(profile: &SandboxProfile) -> bool {
    if profile.allow_direct_fallback {
        return true;
    }
    std::env::var("PANTHEON_SANDBOX_FALLBACK")
        .map(|v| v.eq_ignore_ascii_case("allow"))
        .unwrap_or(false)
}

/// Cap on captured child output (stdout + stderr combined), in bytes.
const OUTPUT_CAP_BYTES: usize = 2 * 1024 * 1024;

/// One drained pipe: the bytes kept (up to the shared cap) and the number
/// of bytes dropped past it.
struct DrainedPipe {
    bytes: Vec<u8>,
    dropped: usize,
}

/// Spawn a thread draining `pipe` into a bounded buffer. `budget` is the
/// output cap shared with the sibling stream; each thread takes what it
/// needs from the budget and counts the rest as dropped.
fn spawn_drainer<R>(
    pipe: R,
    budget: std::sync::Arc<std::sync::Mutex<usize>>,
) -> std::thread::JoinHandle<DrainedPipe>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut pipe = pipe;
        let mut kept = Vec::new();
        let mut dropped = 0usize;
        let mut chunk = [0u8; 8192];
        loop {
            let n = match std::io::Read::read(&mut pipe, &mut chunk) {
                Ok(0) => break, // EOF
                Ok(n) => n,
                Err(_) => break,
            };
            let take = {
                let mut b = budget.lock().unwrap();
                let take = (*b).min(n);
                *b -= take;
                take
            };
            kept.extend_from_slice(&chunk[..take]);
            dropped += n - take;
        }
        DrainedPipe {
            bytes: kept,
            dropped,
        }
    })
}

/// Kill the child and its whole process tree, then reap it. The child was
/// placed in its own process group via `setsid()` in `pre_exec`, so a
/// negative pid reaches grandchildren the direct child spawned too.
#[cfg(unix)]
fn kill_child_tree(child: &mut std::process::Child) {
    let pgid = child.id() as libc::pid_t;
    // Defensive: never signal init's group or our own group.
    if pgid > 1 && pgid != std::process::id() as libc::pid_t {
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_child_tree(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Run a sandboxed command with the profile's wall-clock timeout.
///
/// Returns `SandboxResult` with merged stdout/stderr. If the deadline
/// expires, the whole process group is killed and a `SANDBOX_TIMEOUT`
/// error is returned. If the boundary needs a wrapper that cannot
/// initialize and the direct fallback is not opted in, a
/// `SANDBOX_UNAVAILABLE` error is returned (fail closed).
pub fn run_sandboxed(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
) -> Result<SandboxResult, PantheonError> {
    let timeout_ms = profile.wall_clock_ms;
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);

    let mut builder = build_sandboxed(profile, program, args, cwd);
    let mut sandboxed = builder.get_program().to_string_lossy() != program;
    // Fail closed: a boundary that needs a wrapper but didn't get one
    // (wrapper binary missing, or the probe showed it cannot initialize in
    // this environment) refuses to run rather than silently dropping the
    // isolation. The direct-spawn fallback needs explicit opt-in.
    let needs_wrapper = !matches!(profile.boundary, crate::ExecutionBoundary::InProcess);
    if needs_wrapper && (!sandboxed || !wrapper_initializes(profile, cwd)) {
        if !direct_fallback_allowed(profile) {
            return Err(berr(
                "SANDBOX_UNAVAILABLE",
                format!(
                    "sandbox wrapper for {:?} boundary unavailable and direct fallback \
                     not opted in (set PANTHEON_SANDBOX_FALLBACK=allow or \
                     allow_direct_fallback); refusing to run unsandboxed",
                    profile.boundary
                ),
                false,
            ));
        }
        builder = direct_command(program, args, cwd);
        #[cfg(unix)]
        confine_child(&mut builder, profile);
        sandboxed = false;
    }

    let mut child = builder
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| berr("SANDBOX_SPAWN", format!("spawn {}: {}", program, e), false))?;

    // Drain both pipes from spawn: a child filling the 64KiB pipe buffer
    // while we only try_wait() would wedge until the wall-clock timeout.
    // The drainers share one 2MiB budget across both streams.
    let budget = std::sync::Arc::new(std::sync::Mutex::new(OUTPUT_CAP_BYTES));
    let stdout_drainer = child
        .stdout
        .take()
        .map(|p| spawn_drainer(p, std::sync::Arc::clone(&budget)));
    let stderr_drainer = child
        .stderr
        .take()
        .map(|p| spawn_drainer(p, std::sync::Arc::clone(&budget)));

    let mut timed_out = false;
    let mut wait_err: Option<std::io::Error> = None;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    timed_out = true;
                    kill_child_tree(&mut child);
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                wait_err = Some(e);
                kill_child_tree(&mut child);
                break;
            }
        }
    }

    // Reap the drainers (the pipes hit EOF once the tree is dead) and
    // merge stdout then stderr, appending a truncation marker if the cap
    // dropped anything. Joining before returning also keeps a timed-out
    // run from leaking blocked reader threads.
    let mut output = String::new();
    let mut dropped_total = 0usize;
    for handle in [stdout_drainer, stderr_drainer].into_iter().flatten() {
        if let Ok(d) = handle.join() {
            output.push_str(&String::from_utf8_lossy(&d.bytes));
            dropped_total += d.dropped;
        }
    }
    if dropped_total > 0 {
        output.push_str(&format!("\n[...truncated {dropped_total} bytes...]"));
    }

    if timed_out {
        // The args may carry secrets (API keys, tokens), so they never go
        // into the error text verbatim. Same treatment as the
        // dangerous-pattern gate in pantheon-exec/src/danger.rs: a short
        // stable digest plus the length — enough to correlate the timeout
        // with the request that caused it, without leaking the command.
        let digest = cmd_digest(&args.join(" "));
        return Err(berr(
            "SANDBOX_TIMEOUT",
            format!(
                "command exceeded {}s: {} ({digest})",
                timeout_ms / 1000,
                program
            ),
            true,
        ));
    }
    if let Some(e) = wait_err {
        return Err(berr("SANDBOX_WAIT", format!("wait: {}", e), false));
    }
    let status = status.expect("wait loop only breaks with a status, a timeout, or a wait error");
    Ok(SandboxResult {
        output,
        exit_code: status.code().unwrap_or(-1),
        timed_out: false,
        sandboxed,
    })
}

fn berr(code: &str, cause: String, recoverable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        recoverable,
        cause,
        "check sandbox profile and system dependencies (bwrap/unshare)",
        "",
    )
}

/// Stable, non-secret identifier for a command's argument string:
/// `cmd:0123abcd len:42` (first 8 hex of an FNV-1a 64 hash, upper bits,
/// plus the char length). Deterministic across runs so repeated timeouts
/// of the same command correlate, but irreversible, so an error carrying
/// it cannot leak the arguments — which may themselves contain secrets.
///
/// Mirrors `cmd_digest` in pantheon-exec/src/danger.rs; keep the two in
/// sync so a digest from either crate identifies the same command.
/// FNV-1a rather than a crypto hash because this is an identifier, not
/// authentication; it needs to be std-only and fast on the hot path.
fn cmd_digest(args: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_4842_2235;
    for b in args.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("cmd:{:08x} len:{}", (h >> 32) as u32, args.len())
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
