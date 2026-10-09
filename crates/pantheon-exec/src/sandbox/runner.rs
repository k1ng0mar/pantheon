//! Sandbox executor: runs tool subprocesses behind a real boundary.
//!
//! Maps `ExecutionBoundary` onto concrete host mechanisms:
//! - `InProcess`: direct `Command` (caller already gated)
//! - `IsolatedProcess`: `unshare` (user + PID + mount + optionally net
//!   namespaces) + a mount-setup script that remounts the host
//!   filesystem read-only (or hides un-remountable mounts under a fresh
//!   tmpfs), gives the child a private tmpfs sandbox root as its cwd,
//!   and a private `/tmp` + `/dev/shm` - plus rlimits
//! - `Container`: `bwrap` (bubblewrap) with user/mount/PID namespaces + rlimits
//! - `StrictNamespaces`: bwrap with `--unshare-all` (every namespace
//!   unshared) - the strongest boundary this runner offers. There is no
//!   VM backend and none is planned; the name describes the mechanism.
//!
//! When the sandbox binary (bwrap/unshare) is unavailable or fails to
//! initialize, the runner FAILS CLOSED: it returns a `SANDBOX_UNAVAILABLE`
//! error rather than running the command without isolation. A weaker
//! wrapper is never substituted for a stronger boundary: in particular
//! `StrictNamespaces` without `bwrap` refuses rather than falling back
//! to plain `unshare` and reporting `sandboxed=true`. The old
//! silent-degrade to a direct host spawn is only available via explicit
//! opt-in - `SandboxProfile::allow_direct_fallback` or the
//! `PANTHEON_SANDBOX_FALLBACK=allow` environment variable. The capability
//! gate is always the first check, so the fallback (when opted in) only
//! loses the OS-level isolation, never the policy enforcement.
//!
//! Children never inherit the agent's environment: every wrapped spawn
//! (and the opt-in direct fallback) scrubs to an allowlist (`PATH` plus
//! locale/temp basics). Callers add back exactly what the child needs
//! with `cmd.env` after building.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::SandboxProfile;
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
///
/// `PANTHEON_SANDBOX_PATH`, when set, replaces `PATH` for the lookup.
/// It is a test hook: tests simulate a missing wrapper (e.g. "bwrap
/// absent") by pointing it at a directory without that binary, without
/// touching the host.
pub(crate) fn find_binary(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PANTHEON_SANDBOX_PATH").or_else(|| std::env::var_os("PATH"))?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(bin);
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(p: &std::path::Path) -> bool {
    p.is_file()
}

/// Scrub a sandboxed child's environment to an allowlist. The agent's
/// full environment - API keys, tokens, session secrets - must never
/// cross the boundary implicitly (contrast the old behavior, where a
/// tool subprocess inherited everything). Callers add back exactly what
/// the child needs via `cmd.env` *after* [`build_sandboxed`] returns;
/// `env_clear` here only wipes what was set before.
fn scrub_child_env(cmd: &mut Command) {
    cmd.env_clear();
    // env_clear() wipes everything set before it, so PATH comes first.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    } else {
        cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
    }
    // Locale, timezone, and temp-dir basics: harmless, and programs
    // misbehave without them. Everything else stays out.
    for key in [
        "HOME", "TMPDIR", "TEMP", "TMP", "LANG", "LC_ALL", "LC_CTYPE", "LANGUAGE", "TZ", "TERM",
    ] {
        if let Ok(v) = std::env::var(key) {
            cmd.env(key, v);
        }
    }
}

/// Mount-setup script for the `IsolatedProcess` (unshare) boundary.
/// Runs with positional args: `$1` = sandbox name, `$2` = requested
/// working directory (host path), `$3` = program, `$4..` = argv.
/// Everything the script references is a positional parameter or a
/// fixed literal - no string interpolation, so a hostile program path
/// or argument cannot inject shell.
///
/// What it builds, inside a fresh user+mount (+optional net) namespace:
/// - every inherited mount remounted read-only; a mount that refuses
///   the remount is hidden under a fresh empty tmpfs instead (fail
///   safe: read-only or invisible, never left writable);
/// - a private tmpfs sandbox root (`/tmp/<name>`, 256MiB, mode 0700)
///   the only host-invisible writable spot besides the private `/tmp`
///   and `/dev/shm` (bwrap-equivalent scratch semantics);
/// - the child starts in the requested working directory (visible
///   read-only like the rest of the host tree, so relative reads keep
///   working), falling back to the sandbox root when that directory is
///   unavailable (e.g. hidden by the step above);
/// - `TMPDIR`/`TEMP`/`TMP` pointed at the sandbox root so temp files
///   land inside it;
/// - a fresh `/proc` for the PID namespace where the kernel allows it
///   (best effort: some containers forbid proc mounts in a user
///   namespace, in which case the host's /proc stays visible
///   read-only process info, not a write path).
///
/// Requires a user namespace (`unshare --map-root-user`): unprivileged
/// mounts need `CAP_SYS_ADMIN` in the namespace. Where user namespaces
/// are unavailable the wrapper probe fails and the runner fails closed.
#[cfg(unix)]
const UNSHARE_SETUP_SCRIPT: &str = r#"SB="$1"; HOSTCWD="$2"; PROG="$3"; shift 3
mount --make-rprivate /
while read -r _dev mp _rest; do
  case "$mp" in
    /proc|/dev|/sys) ;;
    *) mount -o remount,ro,bind "$mp" "$mp" 2>/dev/null || mount -t tmpfs tmpfs "$mp" 2>/dev/null || true ;;
  esac
done < /proc/self/mounts
mount -o remount,ro,bind / /
mount -t tmpfs tmpfs /tmp 2>/dev/null || true
mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true
ROOT="/tmp/$SB"
mkdir -p "$ROOT"
mount -t tmpfs -o size=256m,mode=0700 tmpfs "$ROOT"
export TMPDIR="$ROOT" TEMP="$ROOT" TMP="$ROOT"
mount -t proc proc /proc 2>/dev/null || true
cd "$HOSTCWD" 2>/dev/null || cd "$ROOT"
exec "$PROG" "$@""#;

#[cfg(not(unix))]
const UNSHARE_SETUP_SCRIPT: &str = "";

static SANDBOX_SEQ: AtomicU64 = AtomicU64::new(0);

/// Build a sandboxed command without spawning.
///
/// `program` is the binary to run. `args` are its argv. `cwd` is the
/// working directory (must be an absolute path): `InProcess` spawns
/// there directly; the namespace boundaries start the child there
/// read-only (`IsolatedProcess` falls back to its private sandbox root
/// when the directory is unavailable inside the namespace).
/// `profile` selects the boundary and limit set.
///
/// The child's environment is scrubbed to an allowlist (see
/// [`scrub_child_env`]) on every boundary except `InProcess` - add back
/// what the child needs with `cmd.env` after this returns.
///
/// The timeout is always derived from `SandboxProfile::wall_clock_ms`,
/// so every tool call is inherently bounded - there is no unbounded wait.
pub fn build_sandboxed(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
) -> Command {
    #[allow(unused_mut)] // non-unix has no rlimits to attach
    let mut cmd = match profile.boundary {
        super::ExecutionBoundary::InProcess => {
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd.current_dir(cwd);
            cmd
        }

        super::ExecutionBoundary::IsolatedProcess => {
            // unshare with a user namespace (so mounts are permitted
            // unprivileged) + PID + mount namespaces, and the network
            // namespace when the profile forbids network access. The
            // setup script remounts the host filesystem read-only (or
            // hides it) and confines writes to a private tmpfs root.
            // `--map-root-user` maps our uid to root inside the
            // namespace; outside, the child is still us.
            if let Some(unshare) = find_binary("unshare") {
                let sb_name = format!(
                    "pantheon-sb-{}-{}",
                    std::process::id(),
                    SANDBOX_SEQ.fetch_add(1, Ordering::SeqCst)
                );
                let mut cmd = Command::new(unshare);
                cmd.arg("--map-root-user")
                    .arg("--pid")
                    .arg("--mount")
                    .arg("--fork");
                // The profile's network flag is honored on EVERY
                // backend, not just bwrap: network:false unshares the
                // net namespace (the child gets an isolated loopback).
                if !profile.network {
                    cmd.arg("--net");
                }
                cmd.arg("sh")
                    .arg("-c")
                    .arg(UNSHARE_SETUP_SCRIPT)
                    .arg("sh")
                    .arg(sb_name)
                    .arg(cwd)
                    .arg(program);
                cmd.args(args);
                cmd
            } else {
                let mut cmd = Command::new(program);
                cmd.args(args);
                cmd.current_dir(cwd);
                cmd
            }
        }

        super::ExecutionBoundary::Container => {
            // bwrap: the strongest general-purpose sandbox this runner
            // offers. --unshare-user-try attempts a user namespace (so no caps survive),
            // falling back gracefully if user namespaces are not available.
            // --unshare-pid + --unshare-cgroup limit process creation.
            if let Some(bwrap) = find_binary("bwrap") {
                let mut cmd = Command::new(bwrap);
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

        super::ExecutionBoundary::StrictNamespaces => {
            // bwrap with --unshare-all + --dev-bind for the program:
            // every namespace (user, pid, net, ipc, uts, cgroup, mount)
            // unshared. Deliberately not a VM - no hypervisor involved.
            // This level is only reached after interactive approval.
            //
            // bwrap is MANDATORY here: a weaker wrapper must never stand
            // in for this boundary. The old code fell back to plain
            // `unshare --pid --mount` when bwrap was absent and still
            // reported `sandboxed=true` - a wrong-strength downgrade.
            // Without bwrap the command builds unwrapped, and
            // `run_sandboxed` fails closed (SANDBOX_UNAVAILABLE) unless
            // the direct fallback is explicitly opted in.
            if let Some(bwrap) = find_binary("bwrap") {
                let mut cmd = Command::new(bwrap);
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
            } else {
                let mut cmd = Command::new(program);
                cmd.args(args);
                cmd.current_dir(cwd);
                cmd
            }
        }
    };

    // The sandbox boundary scrubs the child's environment (API keys and
    // other host secrets never cross implicitly); InProcess is the
    // caller-gated exception. Callers add back what the child needs via
    // `cmd.env` after this returns.
    if !matches!(profile.boundary, super::ExecutionBoundary::InProcess) {
        scrub_child_env(&mut cmd);
    }

    // The profile's limits are rlimits on the child itself, set just
    // before exec - so they hold with or without a namespace wrapper and
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
/// 3. `PR_SET_NO_NEW_PRIVS` when the profile asks for it: once set,
///    neither the wrapper nor the program it execs can gain privileges
///    via setuid/setcap binaries. Linux-only - the profile flag documents
///    the intent everywhere, the kernel enforces it where supported.
///
/// NPROC needs care: the kernel's accounting is UID-wide, not
/// per-sandbox, and in containers sharing the host user namespace it
/// counts processes this container's `/proc` cannot even show. An
/// absolute cap - or one derived from a `/proc` scan - therefore refuses
/// every fork the wrapper itself needs, turning the pids cap into a
/// denial of service. Instead we calibrate against the kernel directly:
/// binary-search the smallest NPROC limit at which a fork still
/// succeeds; that boundary is true usage, and the profile's `max_pids`
/// is granted above it. The probing uses only setrlimit/fork/_exit/
/// waitpid - async-signal-safe in the pre-exec zone. Where NPROC isn't
/// enforced for this user (some containers), or a calibrated cap can no
/// longer fork, the pids cap is skipped and every other limit still
/// applies.
///
/// Limits are only ever lowered, and only in the child - the parent is
/// untouched. The wall-clock budget is enforced separately by
/// [`run_sandboxed`], and the capability gate runs before any of this.
#[cfg(unix)]
fn confine_child(cmd: &mut Command, profile: &SandboxProfile) {
    use std::os::unix::process::CommandExt;

    // Compile the kernel-layer programs in the PARENT. `pre_exec` runs
    // after fork, where only async-signal-safe calls are permitted: no
    // allocation, no locks. Compiling here means the child's closure only
    // makes the two syscalls.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let seccomp_program = if profile.seccomp {
        // A profile that asks for seccomp and cannot get it must not run
        // unfiltered: `None` here is enforced inside the closure, which is
        // the only place that can abort the spawn.
        super::kernel::compile_seccomp().ok()
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let seccomp_wanted = profile.seccomp;
    #[cfg(target_os = "linux")]
    let mut landlock_ruleset = if profile.landlock {
        let cwd = cmd
            .get_current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        super::kernel::compile_landlock(&cwd, &profile.writable_paths).ok()
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let landlock_wanted = profile.landlock;

    let as_bytes = profile
        .max_memory_mb
        .map(|mb| mb.saturating_mul(1024 * 1024));
    let want_pids = profile.max_pids.map(u64::from);
    // PR_SET_NO_NEW_PRIVS is Linux-only; read the flag under the same
    // cfg so other Unix targets don't carry an unused binding.
    #[cfg(target_os = "linux")]
    let no_new_privs = profile.no_new_privs;
    // NPROC's hard ceiling, read now (parent, safe context): probes must
    // stay within it and the final cap may not exceed it.
    let nproc_hard = want_pids.and_then(|_| {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        (unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) } == 0).then_some(lim.rlim_max)
    });

    // SAFETY: between fork and exec the closure only issues setsid,
    // prctl, and setrlimit (plus fork/waitpid/_exit inside calibration)
    // with plain integers - no allocation, no locks. All measuring happens
    // above, in the parent.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // No-new-privs before anything else runs: setuid/setcap
            // binaries the child (or its wrapper) reaches cannot escalate.
            // Failing closed - a child that cannot take the bit must not
            // run with the profile's promise unkept.
            #[cfg(target_os = "linux")]
            if no_new_privs && libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Kernel confinement layers, after no_new_privs (seccomp
            // requires that bit before an unprivileged filter can be
            // installed). Both are fail-closed: a profile that asked for a
            // layer and did not get it aborts the spawn here rather than
            // exec'ing unfiltered. Only syscalls in this zone.
            #[cfg(target_os = "linux")]
            {
                if landlock_wanted {
                    // `restrict_self` consumes the ruleset, so take it out
                    // of the captured Option. The closure is `FnMut` and
                    // runs at most once (one fork, one exec).
                    let Some(ruleset) = landlock_ruleset.take() else {
                        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                    };
                    if ruleset.restrict_self().is_err() {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // The filter is x86_64-only: `SeccompFilter::new` is built
                // with `TargetArch::x86_64`, so on any other arch it would
                // deny the wrong syscall numbers. A profile that asks for
                // seccomp on a non-x86_64 arch must not run unfiltered.
                #[cfg(target_arch = "x86_64")]
                if seccomp_wanted {
                    let Some(program) = seccomp_program.as_ref() else {
                        return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                    };
                    if seccompiler::apply_filter(program).is_err() {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                #[cfg(not(target_arch = "x86_64"))]
                if seccomp_wanted {
                    return Err(std::io::Error::from_raw_os_error(libc::EPERM));
                }
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
/// usage for this UID - or `None` to leave NPROC untouched.
///
/// SAFETY: pre-exec context - only setrlimit/fork/_exit/waitpid, no
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
            // (ENOMEM...) must not steer the search.
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
        // Usage is at the ceiling - nothing safe to grant.
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
    // Belt and braces: never apply a cap that can't fork - degrade to
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

/// Run a sandboxed command with the profile's wall-clock timeout, plus a
/// spawn hook: `on_spawn` (when `Some`) is called with the child's pid
/// right after a successful spawn, on the calling thread, before the
/// wait loop starts. The pid is also the child's process-group id
/// [`confine_child`] runs `setsid()` in `pre_exec` - so a cancel path
/// can register it for `killpg` and unregister it when the call ends. A
/// panicking hook cannot wedge the wait loop: the panic is caught and
/// the child still runs to its normal completion.
///
/// Returns `SandboxResult` with merged stdout/stderr. If the deadline
/// expires, the whole process group is killed and a `SANDBOX_TIMEOUT`
/// error is returned. If the boundary needs a wrapper that cannot
/// initialize and the direct fallback is not opted in, a
/// `SANDBOX_UNAVAILABLE` error is returned (fail closed).
pub fn run_sandboxed_with_spawn_hook(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
    on_spawn: Option<&dyn Fn(u32)>,
) -> Result<SandboxResult, PantheonError> {
    run_sandboxed_with_spawn_hook_and_env(profile, program, args, cwd, on_spawn, &[])
}

pub fn run_sandboxed_with_spawn_hook_and_env(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
    on_spawn: Option<&dyn Fn(u32)>,
    extra_env: &[(String, String)],
) -> Result<SandboxResult, PantheonError> {
    let timeout_ms = profile.wall_clock_ms;
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);

    let mut builder = build_sandboxed(profile, program, args, cwd);
    let mut sandboxed = builder.get_program().to_string_lossy() != program;
    // Fail closed: a boundary that needs a wrapper but didn't get one
    // (wrapper binary missing, or the probe showed it cannot initialize in
    // this environment) refuses to run rather than silently dropping the
    // isolation. The direct-spawn fallback needs explicit opt-in.
    let needs_wrapper = !matches!(profile.boundary, super::ExecutionBoundary::InProcess);
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
        // The fallback loses the namespace boundary, never the env
        // boundary: the child still does not inherit host secrets.
        scrub_child_env(&mut builder);
        #[cfg(unix)]
        confine_child(&mut builder, profile);
        sandboxed = false;
    }
    // Named extras go in after every scrub path, so both the wrapped and
    // the direct-fallback command get exactly the same variables.
    for (k, v) in extra_env {
        builder.env(k, v);
    }

    let mut child = builder
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| berr("SANDBOX_SPAWN", format!("spawn {}: {}", program, e), false))?;

    // The pid is the process-group id too (setsid in pre-exec): report
    // it now so a cancel path can register the group before the child
    // does any real work. Panic-caught - a misbehaving hook must not
    // wedge the wait loop or leak the child unreaped.
    if let Some(hook) = on_spawn {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(child.id())));
    }

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
        // stable digest plus the length - enough to correlate the timeout
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

/// Run a sandboxed command with the profile's wall-clock timeout. Same
/// as [`run_sandboxed_with_spawn_hook`] with no spawn hook.
pub fn run_sandboxed(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
) -> Result<SandboxResult, PantheonError> {
    run_sandboxed_with_spawn_hook(profile, program, args, cwd, None)
}

/// [`run_sandboxed`] plus explicitly named extra env vars for the child.
///
/// The env scrub (see `scrub_child_env`) wipes inheritance so host secrets
/// never cross implicitly. Some tools need exactly one secret the operator
/// resolved ahead of time (the Cloudflare CLI needs `CLOUDFLARE_API_TOKEN`);
/// this is the sanctioned way to hand that in. The caller resolves values
/// through the secrets broker; the map here is the single gate between the
/// broker and the child, and every entry must trace to a config decision,
/// not ambient inheritance.
pub fn run_sandboxed_with_env(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
    extra_env: &[(String, String)],
) -> Result<SandboxResult, PantheonError> {
    run_sandboxed_with_spawn_hook_and_env(profile, program, args, cwd, None, extra_env)
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
/// it cannot leak the arguments - which may themselves contain secrets.
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

/// Adversarial regression tests for the sandbox boundary (item 1).
/// Each test performs a REAL bypass attempt against the old behavior:
/// they fail on the pre-fix code (verified) and pass after.
#[cfg(all(test, target_os = "linux"))]
mod sandbox_escape_tests {
    use super::*;
    use crate::SandboxLevel;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    /// Serializes tests that mutate process-global state (the
    /// `PANTHEON_SANDBOX_PATH` lookup hook, sentinel env vars).
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Skip guard for the tests that need a real `IsolatedProcess` run.
    /// Delegates to the shared probe so this crate and its callers agree
    /// on what "the host can do this" means.
    macro_rules! require_isolated_process {
        () => {
            if !crate::sandbox::boundary_available(
                crate::sandbox::ExecutionBoundary::IsolatedProcess,
            ) {
                eprintln!(
                    "skipping: this host refuses unshare --map-root-user \
                     (SANDBOX_UNAVAILABLE is the intended fail-closed result)"
                );
                return;
            }
        };
    }

    fn medium_profile() -> SandboxProfile {
        SandboxProfile::from(SandboxLevel::Medium)
    }

    fn veryhigh_profile() -> SandboxProfile {
        SandboxProfile::from(SandboxLevel::VeryHigh)
    }

    fn scratch_cwd(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-sb-test-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let s = dir.to_string_lossy().into_owned();
        // Best-effort cleanup; the dir may be on a hidden mount.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        s
    }

    /// Item 1a: `Medium` sets `network: false` - the child must land in
    /// a DIFFERENT network namespace than the parent. The old unshare
    /// branch never passed a net flag, so the namespaces were identical
    /// (verified live before the fix).
    #[test]
    fn unshare_honors_network_false_with_own_netns() {
        // Serialized with the PANTHEON_SANDBOX_PATH mutator below: the
        // wrapper lookup must see the real PATH here.
        require_isolated_process!();
        let _g = ENV_GUARD.lock().unwrap();
        let cwd = scratch_cwd("netns");
        let r = run_sandboxed(&medium_profile(), "readlink", &["/proc/self/ns/net"], &cwd)
            .expect("sandboxed run works");
        assert!(r.sandboxed, "expected the unshare wrapper to be used");
        let child_ns = r.output.trim().to_string();
        let parent_ns = std::fs::read_link("/proc/self/ns/net")
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        assert!(
            !child_ns.is_empty() && child_ns != parent_ns,
            "child netns ({child_ns}) must differ from parent ({parent_ns}) when network:false"
        );
    }

    /// Item 1b: the unshare path must confine writes. The child tries to
    /// write to host paths (`/etc`, and a host-visible probe) - those
    /// must fail - and to its sandbox root (`$TMPDIR`, exported by the
    /// setup script) - which must succeed AND stay invisible on the
    /// host (private tmpfs). Before the fix the child wrote to `/etc`
    /// and the host `/tmp` freely.
    #[test]
    fn unshare_confines_writes_to_sandbox_root() {
        require_isolated_process!();
        let _g = ENV_GUARD.lock().unwrap();
        let cwd = scratch_cwd("fswrite");
        let probe = format!(
            "sb-probe-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let script = format!(
            "touch /etc/{probe} 2>/dev/null && echo ETC_WRITABLE || echo ETC_DENIED; \
             touch \"$TMPDIR/{probe}.ok\" 2>/dev/null && echo ROOT_WRITABLE || echo ROOT_DENIED; \
             echo \"TMPDIR=$TMPDIR\""
        );
        let r = run_sandboxed(&medium_profile(), "sh", &["-c", &script], &cwd)
            .expect("sandboxed run works");
        assert!(r.sandboxed, "expected the unshare wrapper to be used");
        assert!(
            r.output.contains("ETC_DENIED"),
            "child must not write to /etc; output was:\n{}",
            r.output
        );
        assert!(
            r.output.contains("ROOT_WRITABLE"),
            "child must write to its sandbox root; output was:\n{}",
            r.output
        );
        let tmpdir = r
            .output
            .lines()
            .find_map(|l| l.strip_prefix("TMPDIR="))
            .unwrap_or("")
            .trim()
            .to_string();
        assert!(!tmpdir.is_empty(), "setup script must export TMPDIR");
        assert!(
            !std::path::Path::new(&tmpdir).exists(),
            "child's {tmpdir} must be a private mount, invisible on the host"
        );
        assert!(
            !std::path::Path::new(&format!("/etc/{probe}")).exists(),
            "probe file must not exist on the host /etc"
        );
    }

    /// Item 1c: `VeryHigh` without `bwrap` must FAIL CLOSED
    /// (`SANDBOX_UNAVAILABLE`), never silently downgrade to plain
    /// unshare while reporting `sandboxed=true`. Simulates the
    /// bwrap-absent host with a PATH dir containing unshare but no
    /// bwrap - the old code took the unshare fallback and reported
    /// success (verified before the fix).
    #[test]
    fn veryhigh_without_bwrap_fails_closed() {
        let _g = ENV_GUARD.lock().unwrap();
        let dir = std::env::temp_dir().join(format!(
            "pantheon-sb-test-{}-{}-nopath",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/usr/bin/unshare", dir.join("unshare")).unwrap();
        std::env::set_var("PANTHEON_SANDBOX_PATH", &dir);
        let cwd = scratch_cwd("veryhigh");
        let r = run_sandboxed(&veryhigh_profile(), "sh", &["-c", "true"], &cwd);
        std::env::remove_var("PANTHEON_SANDBOX_PATH");
        let _ = std::fs::remove_dir_all(&dir);
        match r {
            Err(e) => assert_eq!(
                e.code, "SANDBOX_UNAVAILABLE",
                "expected SANDBOX_UNAVAILABLE, got {}",
                e.code
            ),
            Ok(res) => panic!(
                "VeryHigh without bwrap must refuse; old code ran with sandboxed={}",
                res.sandboxed
            ),
        }
    }

    /// Item 1d: the sandboxed child must not inherit the agent's
    /// environment. A sentinel secret in the parent env must be absent
    /// from the child's env; the allowlisted basics (PATH) survive.
    /// Before the fix the child inherited everything, sentinel included.
    #[test]
    fn sandboxed_child_env_is_scrubbed() {
        require_isolated_process!();
        let _g = ENV_GUARD.lock().unwrap();
        let sentinel = format!(
            "PANTHEON_SANDBOX_TEST_SENTINEL_{}",
            SEQ.fetch_add(1, Ordering::SeqCst)
        );
        std::env::set_var(&sentinel, "super-secret-value");
        let cwd = scratch_cwd("envscrub");
        let r = run_sandboxed(&medium_profile(), "sh", &["-c", "env"], &cwd)
            .expect("sandboxed run works");
        std::env::remove_var(&sentinel);
        assert!(
            !r.output.contains("super-secret-value"),
            "sentinel secret leaked into the sandboxed child env"
        );
        assert!(
            !r.output
                .lines()
                .any(|l| l.starts_with(&format!("{sentinel}="))),
            "sentinel var present in child env"
        );
        assert!(
            r.output.lines().any(|l| l.starts_with("PATH=")),
            "PATH must survive the scrub (the child needs it to exec)"
        );
    }
}
