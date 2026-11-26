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
//! to plain `unshare` and reporting `sandboxed=true`. A direct host
//! spawn is only available via explicit opt-in -
//! `SandboxProfile::allow_direct_fallback` or the
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

/// Scrub a sandboxed child's environment to an allowlist. The agent's full
/// environment - API keys, tokens, session secrets - must not cross the boundary
/// implicitly. Callers add back exactly what the child needs via `cmd.env` *after*
/// [`build_sandboxed`] returns; `env_clear` here only wipes what was set before.
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
///   the remount is hidden under a fresh empty tmpfs instead, and a
///   mount that refuses both aborts the spawn (exit 126). Before, both
///   failures were swallowed with `|| true`, the root remount and the
///   sandbox-root mount were unchecked, and nothing verified the result,
///   so a mount could stay writable while the run reported isolation
///   (audit SEC-11). The final table is now checked before exec;
///   pseudo filesystems under /proc, /dev and /sys are left alone, and
///   octal-escaped mount points (`\040` for a space) are decoded;
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
fail() { echo "pantheon sandbox: $*; refusing to run" >&2; exit 126; }
mount --make-rprivate / || fail "cannot make mounts private"
while read -r _dev mp _fs opts _rest; do
  mp=$(printf '%b' "$mp")
  case "$mp" in
    /proc|/proc/*|/dev|/dev/*|/sys|/sys/*) continue ;;
  esac
  # Flags locked by the parent namespace must be kept on the remount, or
  # the kernel refuses it (EPERM): nosuid/nodev/noexec and atime flags.
  keep=""
  for f in nosuid nodev noexec noatime nodiratime relatime strictatime; do
    case ",$opts," in *,$f,*) keep="$keep,$f" ;; esac
  done
  mount -o "remount,ro,bind$keep" "$mp" "$mp" 2>/dev/null && continue
  mount -t tmpfs -o ro tmpfs "$mp" 2>/dev/null && continue
  # A mount this namespace cannot even stat (another user's FUSE mount
  # without allow_other) is unreachable for reads and writes alike.
  stat "$mp" >/dev/null 2>&1 || continue
  fail "cannot make $mp read-only"
done < /proc/self/mounts
mount -o remount,ro,bind / / 2>/dev/null || fail "cannot make / read-only"
mount -t tmpfs tmpfs /tmp || fail "cannot mount a private /tmp"
mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true
ROOT="/tmp/$SB"
mkdir -p "$ROOT" || fail "cannot create the sandbox root"
mount -t tmpfs -o size=256m,mode=0700 tmpfs "$ROOT" || fail "cannot mount the sandbox root"
while read -r _dev mp fs opts _rest; do
  mp=$(printf '%b' "$mp")
  case "$mp" in /proc|/proc/*|/dev|/dev/*|/sys|/sys/*|/tmp|/tmp/*) continue ;; esac
  case ",$opts," in *,ro,*) continue ;; esac
  [ "$fs" = tmpfs ] && continue
  stat "$mp" >/dev/null 2>&1 || continue
  fail "$mp is still writable"
done < /proc/self/mounts
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
    // VeryHigh's seccomp program, staged for bwrap's `--seccomp FD`
    // (applied to the payload, not the wrapper). When it is wanted and
    // cannot be staged, the strict boundary builds unwrapped, so the
    // checked builder fails closed with SANDBOX_UNAVAILABLE.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let seccomp_fd = if profile.seccomp
        && matches!(profile.boundary, super::ExecutionBoundary::StrictNamespaces)
    {
        match stage_seccomp_fd() {
            Ok(fd) => Some(fd),
            Err(e) => {
                eprintln!("pantheon: seccomp filter unavailable: {e}");
                None
            }
        }
    } else {
        None
    };
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let seccomp_ready = !profile.seccomp || seccomp_fd.is_some();
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let seccomp_ready = !profile.seccomp;
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
            // offers. --unshare-user is REQUIRED, not best-effort: the
            // old --unshare-user-try silently stayed in the host user
            // namespace when one could not be created, and a root caller
            // then ran the payload with a full capability set (audit
            // SEC-10). A host that cannot create a user namespace fails
            // the probe and the boundary fails closed. --cap-drop ALL
            // empties the set explicitly for privileged callers.
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

                push_hidden_masks(&mut cmd, profile, cwd);

                for writable in &profile.writable_paths {
                    cmd.arg("--bind").arg(writable).arg(writable);
                }

                if profile.drop_capabilities {
                    cmd.arg("--unshare-user");
                    cmd.arg("--cap-drop").arg("ALL");
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
            // bwrap with --unshare-all + --dev-bind for the program: every namespace
            // (user, pid, net, ipc, uts, cgroup, mount) unshared. Not a VM - no
            // hypervisor. This level is only reached after interactive approval.
            //
            // bwrap is mandatory here: a weaker wrapper must not stand in for this
            // boundary. Without bwrap the command builds unwrapped, and
            // `run_sandboxed` fails closed (SANDBOX_UNAVAILABLE) unless the direct
            // fallback is explicitly opted in.
            if let Some(bwrap) = find_binary("bwrap").filter(|_| seccomp_ready) {
                let mut cmd = Command::new(bwrap);
                // --unshare-all only TRIES the user namespace; the
                // explicit --unshare-user makes it mandatory.
                cmd.arg("--die-with-parent")
                    .arg("--unshare-all")
                    .arg("--unshare-user")
                    .arg("--cap-drop")
                    .arg("ALL")
                    .arg("--ro-bind")
                    .arg("/")
                    .arg("/")
                    .arg("--dev")
                    .arg("/dev")
                    .arg("--proc")
                    .arg("/proc")
                    .arg("--tmpfs")
                    .arg("/tmp");
                push_hidden_masks(&mut cmd, profile, cwd);
                for writable in &profile.writable_paths {
                    cmd.arg("--bind").arg(writable).arg(writable);
                }
                // Seccomp goes inside the sandbox: bwrap loads the BPF program from
                // an fd and applies it to the payload after its own setup. Installing
                // the filter in the wrapper's pre_exec denies the mount/pivot_root/
                // unshare calls bwrap itself needs.
                #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                if let Some(fd) = seccomp_fd {
                    attach_seccomp_fd(&mut cmd, fd);
                }
                cmd.arg("--chdir").arg(cwd).arg("--").arg(program);
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

/// The `SANDBOX_UNAVAILABLE` detail: what was unavailable, what to set to
/// opt in, and - when the kernel refused the user namespace - the Ubuntu
/// AppArmor restriction that causes it. Split from
/// [`build_sandboxed_checked`] so the wording is testable.
fn sandbox_unavailable_msg(boundary: &super::ExecutionBoundary, failure: Option<&str>) -> String {
    let mut detail = format!(
        "sandbox wrapper for {boundary:?} boundary unavailable and direct fallback \
         not opted in (set PANTHEON_SANDBOX_FALLBACK=allow or \
         allow_direct_fallback); refusing to run unsandboxed"
    );
    match failure {
        Some(reason) if userns_blocked(reason) => {
            // The single most common cause on a current Ubuntu desktop:
            // AppArmor refuses the unprivileged user namespace, so bwrap
            // cannot build the boundary at all.
            detail.push_str(
                "; the kernel refused the user namespace, which on Ubuntu is \
                 kernel.apparmor_restrict_unprivileged_userns=1 (AppArmor blocks \
                 unprivileged user namespaces). Allow it (sudo sysctl \
                 kernel.apparmor_restrict_unprivileged_userns=0, or an AppArmor \
                 profile), or set PANTHEON_SANDBOX_FALLBACK=allow to accept the \
                 unisolated run",
            );
        }
        Some(reason) if !reason.trim().is_empty() => {
            detail.push_str(&format!(": {}", reason.trim()));
        }
        _ => {}
    }
    detail
}

/// Build a sandboxed command and enforce the fail-closed contract.
///
/// [`build_sandboxed`] is best-effort by construction: when the boundary's
/// wrapper is missing it returns a plain command. This helper is the
/// fail-closed gate that [`run_sandboxed`] applies, exposed so the callers
/// that build and spawn themselves (the MCP client, the Python plugin
/// runner) cannot silently run un-isolated. When the boundary needs a
/// wrapper that cannot initialize and the direct fallback is not opted in,
/// it returns `SANDBOX_UNAVAILABLE` instead of a command.
///
/// Returns the command to spawn and whether it is actually isolated
/// (`false` only when the direct fallback was explicitly opted in, or the
/// boundary is `InProcess` - the caller-gated exception).
pub fn build_sandboxed_checked(
    profile: &SandboxProfile,
    program: &str,
    args: &[&str],
    cwd: &str,
) -> Result<(Command, bool), PantheonError> {
    let builder = build_sandboxed(profile, program, args, cwd);
    let sandboxed = builder.get_program().to_string_lossy() != program;
    let needs_wrapper = !matches!(profile.boundary, super::ExecutionBoundary::InProcess);
    if needs_wrapper && (!sandboxed || !wrapper_initializes(profile, cwd)) {
        if !direct_fallback_allowed(profile) {
            return Err(berr(
                "SANDBOX_UNAVAILABLE",
                sandbox_unavailable_msg(
                    &profile.boundary,
                    wrapper_failure_reason(profile, cwd).as_deref(),
                ),
                false,
            ));
        }
        // Fallback: run directly. The wrapper is either missing or present
        // but unusable on this host (e.g. bwrap refuses the uid map), so
        // `build_sandboxed` may have handed back a wrapped command - rebuild
        // as a plain one. The fallback loses the namespace boundary, never
        // the env boundary: the child still does not inherit host secrets,
        // and the profile's rlimits still apply.
        //
        // One choke point, every caller: the shell tool reports the
        // degraded run in its tool result, but the MCP client and the
        // Python plugin runner discard the `sandboxed` flag - so an
        // unsandboxed spawn is announced on stderr here, never silent.
        let origin = if profile.allow_direct_fallback {
            "SandboxProfile::allow_direct_fallback"
        } else {
            "PANTHEON_SANDBOX_FALLBACK"
        };
        eprintln!("{}", degraded_spawn_msg(program, origin));
        let mut cmd = direct_command(program, args, cwd);
        scrub_child_env(&mut cmd);
        #[cfg(unix)]
        confine_child(&mut cmd, profile);
        return Ok((cmd, false));
    }
    Ok((builder, sandboxed))
}

/// Mask the profile's `hidden_paths` inside a bwrap boundary, after the
/// read-only root bind: an existing directory gets an empty tmpfs, an
/// existing file (or socket, symlink target...) is replaced by
/// `/dev/null`. Absent paths are skipped (bwrap cannot mount over
/// nothing, and there is nothing to read). When the command's `cwd` lies
/// under a masked directory it is bound back read-only so `--chdir`
/// still works; writable binds come after this and re-expose their own
/// paths. Paths are canonicalized so a symlinked `~/.ssh` masks its real
/// target.
fn push_hidden_masks(cmd: &mut Command, profile: &SandboxProfile, cwd: &str) {
    let mut masked_dirs: Vec<std::path::PathBuf> = Vec::new();
    for hidden in &profile.hidden_paths {
        let path = std::path::Path::new(hidden);
        let Ok(real) = std::fs::canonicalize(path) else {
            continue;
        };
        if real == std::path::Path::new("/") {
            continue;
        }
        if real.is_dir() {
            if masked_dirs.iter().any(|d| real.starts_with(d)) {
                continue;
            }
            cmd.arg("--tmpfs").arg(&real);
            masked_dirs.push(real);
        } else {
            cmd.arg("--ro-bind").arg("/dev/null").arg(&real);
        }
    }
    let cwd_real = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.into());
    let writable = profile
        .writable_paths
        .iter()
        .any(|w| cwd_real.starts_with(std::fs::canonicalize(w).unwrap_or_else(|_| w.into())));
    if !writable && masked_dirs.iter().any(|d| cwd_real.starts_with(d)) {
        cmd.arg("--ro-bind").arg(&cwd_real).arg(&cwd_real);
        // Secret files directly in a re-exposed tree stay hidden.
        for hidden in &profile.hidden_paths {
            if let Ok(real) = std::fs::canonicalize(hidden) {
                if real.starts_with(&cwd_real) && real != cwd_real {
                    if real.is_dir() {
                        cmd.arg("--tmpfs").arg(&real);
                    } else {
                        cmd.arg("--ro-bind").arg("/dev/null").arg(&real);
                    }
                }
            }
        }
    }
}

/// Compile the VeryHigh seccomp program and stage it in a sealed-size
/// memfd (offset 0) for bwrap's `--seccomp FD`. The fd is CLOEXEC in the
/// parent; [`attach_seccomp_fd`] clears that only in the child.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn stage_seccomp_fd() -> Result<std::os::fd::OwnedFd, String> {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::FromRawFd;
    let program = super::kernel::compile_seccomp()?;
    let mut bytes = Vec::with_capacity(program.len() * 8);
    for ins in &program {
        bytes.extend_from_slice(&ins.code.to_ne_bytes());
        bytes.push(ins.jt);
        bytes.push(ins.jf);
        bytes.extend_from_slice(&ins.k.to_ne_bytes());
    }
    // SAFETY: memfd_create with a static NUL-terminated name; the
    // returned fd is owned exactly once by the File below.
    let raw = unsafe { libc::memfd_create(c"pantheon-seccomp".as_ptr(), libc::MFD_CLOEXEC) };
    if raw < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    file.write_all(&bytes)
        .and_then(|_| file.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|e| format!("staging seccomp program: {e}"))?;
    Ok(file.into())
}

/// Pass a staged seccomp fd to bwrap. The `pre_exec` closure owns the fd,
/// so the parent's copy closes when the `Command` is dropped; in the
/// forked child it only clears CLOEXEC so bwrap inherits it.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn attach_seccomp_fd(cmd: &mut Command, fd: std::os::fd::OwnedFd) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let raw = fd.as_raw_fd();
    cmd.arg("--seccomp").arg(raw.to_string());
    // SAFETY: fcntl on an fd we own is async-signal-safe.
    unsafe {
        cmd.pre_exec(move || {
            let _keep = &fd;
            if libc::fcntl(raw, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
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

    // When bwrap wraps the payload, the kernel layers must not land on
    // bwrap itself: seccomp denies the mount/pivot_root/unshare calls it
    // needs, and Landlock makes it fail ("Failed to make / slave"), so
    // VeryHigh never ran (audit COR-02). Under bwrap, seccomp is applied
    // to the payload via `--seccomp FD` (see `build_sandboxed`), and
    // Landlock is redundant: the mount namespace already makes the whole
    // tree read-only except the profile's writable binds, which is
    // stricter than the Landlock ruleset (that one also opens the cwd).
    // Both layers still apply to direct spawns (the opted-in fallback),
    // which have no mount namespace.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let wrapped_by_bwrap = std::path::Path::new(cmd.get_program())
        .file_name()
        .is_some_and(|n| n == "bwrap");

    // Compile the kernel-layer programs in the PARENT. `pre_exec` runs
    // after fork, where only async-signal-safe calls are permitted: no
    // allocation, no locks. Compiling here means the child's closure only
    // makes the two syscalls.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let seccomp_program = if profile.seccomp && !wrapped_by_bwrap {
        // A profile that asks for seccomp and cannot get it must not run
        // unfiltered: `None` here is enforced inside the closure, which is
        // the only place that can abort the spawn.
        super::kernel::compile_seccomp().ok()
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let seccomp_wanted = profile.seccomp && !wrapped_by_bwrap;
    #[cfg(target_os = "linux")]
    let mut landlock_ruleset = if profile.landlock && !wrapped_by_bwrap {
        let cwd = cmd
            .get_current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| "/".to_string());
        super::kernel::compile_landlock(&cwd, &profile.writable_paths).ok()
    } else {
        None
    };
    #[cfg(target_os = "linux")]
    let landlock_wanted = profile.landlock && !wrapped_by_bwrap;

    let as_bytes = profile
        .max_memory_mb
        .map(|mb| mb.saturating_mul(1024 * 1024));
    let want_pids = profile.max_pids.map(u64::from);
    // PR_SET_NO_NEW_PRIVS is Linux-only; read the flag under the same
    // cfg so other Unix targets don't carry an unused binding.
    #[cfg(target_os = "linux")]
    let no_new_privs = profile.no_new_privs;
    // NPROC's inherited (soft, hard) pair, read now (parent, safe
    // context): probes must stay within the hard ceiling, the final cap
    // may not exceed either value, and every path that declines to cap
    // restores the soft value the probes overwrote (audit COR-01).
    let nproc_inherited = want_pids.and_then(|_| {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        (unsafe { libc::getrlimit(libc::RLIMIT_NPROC, &mut lim) } == 0)
            .then_some((lim.rlim_cur, lim.rlim_max))
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
            if let (Some(want), Some((soft, hard))) = (want_pids, nproc_inherited) {
                // The probes overwrite the soft limit, so the final value
                // is always written back: the cap when one applies, the
                // inherited pair otherwise.
                let lim = match calibrate_nproc(want, soft, hard) {
                    Some(cap) => libc::rlimit {
                        rlim_cur: cap,
                        rlim_max: cap,
                    },
                    None => libc::rlimit {
                        rlim_cur: soft,
                        rlim_max: hard,
                    },
                };
                if libc::setrlimit(libc::RLIMIT_NPROC, &lim) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Outcome of one NPROC fork probe.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForkProbe {
    /// The fork succeeded under the probed limit.
    Forks,
    /// The fork failed with EAGAIN: the limit is at or below usage.
    Refused,
    /// The probe could not be run (setrlimit failed) or the fork failed
    /// for another reason (ENOMEM...). Carries no NPROC information, so
    /// the search must stop rather than be steered by it.
    Unknown,
}

/// A NPROC cap granting `want` processes above the kernel's true current
/// usage for this UID - or `None` to leave NPROC at its inherited value.
/// The caller always writes the final limit back (cap or inherited),
/// because every probe overwrites the soft limit.
///
/// SAFETY: pre-exec context - only setrlimit/fork/_exit/waitpid, no
/// allocation.
#[cfg(unix)]
unsafe fn calibrate_nproc(
    want: u64,
    soft: libc::rlim_t,
    hard: libc::rlim_t,
) -> Option<libc::rlim_t> {
    /// Probe whether `limit` currently allows a fork. Overwrites NPROC's
    /// soft limit as a side effect.
    unsafe fn forkable(limit: u64, hard: libc::rlim_t) -> ForkProbe {
        let lim = libc::rlimit {
            rlim_cur: limit as libc::rlim_t,
            rlim_max: hard,
        };
        if libc::setrlimit(libc::RLIMIT_NPROC, &lim) != 0 {
            return ForkProbe::Unknown;
        }
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return if std::io::Error::last_os_error().raw_os_error() == Some(libc::EAGAIN) {
                ForkProbe::Refused
            } else {
                ForkProbe::Unknown
            };
        }
        if pid == 0 {
            unsafe { libc::_exit(0) };
        }
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
        {}
        ForkProbe::Forks
    }
    nproc_search(want, soft, hard, |limit| unsafe { forkable(limit, hard) })
}

/// The calibration search, separated from the fork probe so its
/// decisions are unit-testable. Returns `None` whenever no cap should
/// apply. A returned cap never exceeds the inherited soft limit (limits
/// are only ever lowered) or the hard limit.
#[cfg(unix)]
fn nproc_search(
    want: u64,
    soft: libc::rlim_t,
    hard: libc::rlim_t,
    mut probe: impl FnMut(u64) -> ForkProbe,
) -> Option<libc::rlim_t> {
    if hard == 0 {
        return None;
    }
    // Not enforced for this user (root, CAP_SYS_RESOURCE): a cap would
    // be decoration. Unknown also stops: no information to search on.
    if probe(1) != ForkProbe::Refused {
        return None;
    }
    // Search ceiling: covers any realistic usage, and stays far from
    // RLIM_INFINITY (u64::MAX would need 64 halvings to converge).
    let top = hard.min(soft).min(1 << 20);
    let mut lo = 1u64; // known unforkable (usage ≥ 1: we exist)
    let mut hi = top; // assumed forkable (we are running under it)
    if probe(hi) != ForkProbe::Forks {
        // Usage is at the ceiling, or the probe is unreliable.
        return None;
    }
    while hi - lo > 16 {
        let mid = lo + (hi - lo) / 2;
        match probe(mid) {
            ForkProbe::Forks => hi = mid,
            ForkProbe::Refused => lo = mid,
            ForkProbe::Unknown => return None,
        }
    }
    // `hi` ≈ usage + 1; grant the profile's headroom above it, never
    // above what the process already had.
    let cap = hi.saturating_add(want).min(hard).min(soft);
    // Belt and braces: never apply a cap that can't fork - degrade to
    // the inherited limit instead of breaking the command.
    if probe(cap) != ForkProbe::Forks {
        return None;
    }
    Some(cap)
}

#[cfg(all(test, unix))]
mod nproc_search_tests {
    use super::{nproc_search, ForkProbe};

    /// A probe for a host whose true usage is `usage` processes.
    fn enforced(usage: u64) -> impl FnMut(u64) -> ForkProbe {
        move |limit| {
            if limit > usage {
                ForkProbe::Forks
            } else {
                ForkProbe::Refused
            }
        }
    }

    #[test]
    fn not_enforced_declines_to_cap() {
        // Root: even a limit of 1 forks.
        assert_eq!(nproc_search(8, 4096, 4096, |_| ForkProbe::Forks), None);
    }

    #[test]
    fn unknown_probe_declines_to_cap() {
        assert_eq!(nproc_search(8, 4096, 4096, |_| ForkProbe::Unknown), None);
        let mut calls = 0;
        let r = nproc_search(8, 4096, 4096, |limit| {
            calls += 1;
            match calls {
                1 => ForkProbe::Refused,
                2 => ForkProbe::Forks,
                _ if limit > 100 => ForkProbe::Unknown,
                _ => ForkProbe::Refused,
            }
        });
        assert_eq!(r, None, "a non-EAGAIN failure mid-search must not steer it");
    }

    #[test]
    fn enforced_grants_headroom_above_usage() {
        let cap = nproc_search(8, 4096, 4096, enforced(100)).expect("cap");
        assert!(cap > 100 + 8 - 1 && cap <= 100 + 17 + 8, "cap {cap}");
    }

    #[test]
    fn cap_never_exceeds_inherited_soft() {
        // Soft 120 < usage(100) + want(256): the cap is clamped to 120.
        let cap = nproc_search(256, 120, 4096, enforced(100)).expect("cap");
        assert_eq!(cap, 120);
    }

    #[test]
    fn zero_hard_declines() {
        assert_eq!(nproc_search(8, 0, 0, enforced(1)), None);
    }
}

/// A plain direct command (no wrapper); rlimits attached by the caller.
fn direct_command(program: &str, args: &[&str], cwd: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.current_dir(cwd);
    cmd
}

/// The result of one wrapper probe: whether it initialized, and the
/// wrapper's own stderr when it did not. The stderr is what lets a caller
/// tell a kernel-blocked user namespace apart from a missing binary.
#[derive(Clone)]
struct ProbeResult {
    ok: bool,
    stderr: String,
}

/// Has this boundary's wrapper successfully initialized once in this
/// process? Probed once and cached: a wrapper that cannot initialize
/// here (missing privileges, blocked user namespaces) would fail *every*
/// command before it runs, so [`run_sandboxed`] fails closed (or takes
/// the opt-in direct fallback) instead of running each command
/// un-isolated.
fn wrapper_initializes(profile: &SandboxProfile, cwd: &str) -> bool {
    wrapper_probe(profile, cwd).ok
}

/// The cached failure reason for a wrapper shape whose probe did not
/// initialize, so [`build_sandboxed_checked`] can name the cause instead of
/// reporting only that a fallback was not opted in.
fn wrapper_failure_reason(profile: &SandboxProfile, cwd: &str) -> Option<String> {
    let probe = wrapper_probe(profile, cwd);
    if probe.ok {
        return None;
    }
    Some(probe.stderr)
}

fn wrapper_probe(profile: &SandboxProfile, cwd: &str) -> ProbeResult {
    // Cached per wrapper shape, not per boundary type: the probe's result depends on
    // the profile's masks, binds, seccomp/Landlock and the cwd. Keyed per boundary,
    // one caller's failure (a cwd bwrap cannot reach) made every later profile of
    // that type SANDBOX_UNAVAILABLE for the life of the process, and one caller's
    // success vouched for profiles it never tried.
    use std::hash::{Hash, Hasher};
    static PROBES: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<u64, ProbeResult>>,
    > = std::sync::LazyLock::new(Default::default);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (profile.boundary as u8).hash(&mut h);
    profile.network.hash(&mut h);
    profile.seccomp.hash(&mut h);
    profile.landlock.hash(&mut h);
    profile.drop_capabilities.hash(&mut h);
    profile.writable_paths.hash(&mut h);
    profile.hidden_paths.hash(&mut h);
    cwd.hash(&mut h);
    let key = h.finish();
    if let Some(known) = PROBES.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return known.clone();
    }
    let probe = probe_wrapper(profile, cwd);
    let mut cache = PROBES.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= 256 {
        cache.clear();
    }
    cache.insert(key, probe.clone());
    probe
}

/// Whether a failed probe's stderr says the kernel refused to create the
/// user namespace. On Ubuntu that is AppArmor's
/// `kernel.apparmor_restrict_unprivileged_userns=1`, which blocks
/// unprivileged user namespaces for every confined process: bwrap reports
/// it as "setting up uid map: Permission denied", `unshare` as
/// "write failed /proc/self/uid_map: Operation not permitted".
fn userns_blocked(stderr: &str) -> bool {
    let s = stderr.to_ascii_lowercase();
    let refused = [
        "permission denied",
        "operation not permitted",
        "no permissions",
        "eperm",
        "eacces",
    ]
    .iter()
    .any(|m| s.contains(m));
    let userns = [
        "uid map",
        "gid map",
        "uid_map",
        "gid_map",
        "namespace",
        "unshare",
    ]
    .iter()
    .any(|m| s.contains(m));
    refused && userns
}

fn probe_wrapper(profile: &SandboxProfile, cwd: &str) -> ProbeResult {
    let program = "sh";
    let mut probe = build_sandboxed(profile, program, &["-c", "true"], cwd);
    // A plain command means no wrapper was used at all: the binary was not
    // found, or the strict boundary's seccomp filter could not be staged.
    // Its exit status says nothing about the boundary, so it must never be
    // cached as "the wrapper initializes". The lookup reads the process
    // environment (`PANTHEON_SANDBOX_PATH`, then `PATH`), which a parallel
    // test can change mid-process; caching a wrapper-less probe as success
    // let a later spawn use a wrapper that cannot run, and the payload's
    // output never arrived (audit REL-01).
    if probe.get_program() == std::ffi::OsStr::new(program) {
        return ProbeResult {
            ok: false,
            stderr: "probe ran without a sandbox wrapper".to_string(),
        };
    }
    probe
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let Ok(mut child) = probe.spawn() else {
        return ProbeResult {
            ok: false,
            stderr: String::new(),
        };
    };
    // Drain the wrapper's own stderr on a thread so a wedged wrapper cannot
    // fill the pipe and block itself, while the wait below stays bounded.
    let mut pipe = child.stderr.take();
    let drain = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(p) = pipe.as_mut() {
            let _ = std::io::Read::read_to_string(p, &mut buf);
        }
        buf
    });
    // Bounded: a wedged wrapper must not hang the first command.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let ok = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let stderr = drain.join().unwrap_or_default();
    ProbeResult { ok, stderr }
}

/// Whether the direct-spawn fallback is opted in: either the profile's
/// `allow_direct_fallback` or `PANTHEON_SANDBOX_FALLBACK=allow` in the
/// environment (a deployment-level override that needs no profile change).
///
/// The env-var origin is logged once per process the first time it opts
/// in: trading away namespace isolation is a legitimate deployment
/// choice (a host whose wrapper cannot initialize), but it must never be
/// invisible. The profile flag is silent because it is explicit in code.
fn direct_fallback_allowed(profile: &SandboxProfile) -> bool {
    if profile.allow_direct_fallback {
        return true;
    }
    if !profile.allow_env_fallback {
        return false;
    }
    env_fallback_allows(&std::env::var("PANTHEON_SANDBOX_FALLBACK").ok())
}

/// The env-var half of [`direct_fallback_allowed`], split out so the
/// opt-in and its once-per-process announcement are testable without
/// mutating the real environment (the sandbox tests share a process).
fn env_fallback_allows(value: &Option<String>) -> bool {
    match value.as_deref() {
        Some(v) if v.eq_ignore_ascii_case("allow") => {
            if let Some(msg) = env_fallback_warn_msg() {
                eprintln!("pantheon: {msg}");
            }
            true
        }
        _ => false,
    }
}

/// Message announcing the env-var fallback opt-in, returned exactly once
/// per process (`None` on every later consult). Split from the print so
/// the once-semantics are unit-testable without capturing stderr.
static ENV_FALLBACK_WARNED: AtomicBool = AtomicBool::new(false);

fn env_fallback_warn_msg() -> Option<String> {
    if ENV_FALLBACK_WARNED.swap(true, Ordering::SeqCst) {
        return None;
    }
    Some(
        "PANTHEON_SANDBOX_FALLBACK=allow is set: commands whose sandbox wrapper \
         is unavailable will run WITHOUT namespace isolation (env scrub and rlimits \
         still apply). Deployment-level opt-in; remove it to restore fail-closed."
            .to_string(),
    )
}

/// The stderr line for one degraded (direct-fallback) spawn. Every
/// caller funnels through here, so the line names the program and the
/// exact origin that authorized it.
fn degraded_spawn_msg(program: &str, origin: &str) -> String {
    format!(
        "pantheon: {program} spawning WITHOUT namespace isolation (direct sandbox fallback via {origin})"
    )
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
                let mut b = budget.lock().unwrap_or_else(|e| e.into_inner());
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

    let (mut builder, sandboxed) = build_sandboxed_checked(profile, program, args, cwd)?;
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
        // into the error text verbatim. Same treatment as the dangerous-pattern
        // gate: a short stable digest plus the length - enough to correlate the
        // timeout with the request that caused it, without leaking the command.
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
/// Mirrors `cmd_digest` in `sandbox_group`; keep the two in sync so a digest
/// from either identifies the same command.
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

/// Adversarial regression tests for the sandbox boundary. Each test performs a real
/// bypass attempt, so it fails if the boundary regresses.
#[cfg(all(test, target_os = "linux"))]
mod sandbox_escape_tests {
    use super::*;
    use crate::SandboxLevel;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);
    /// Serializes tests that mutate process-global state (the
    /// `PANTHEON_SANDBOX_PATH` lookup hook, sentinel env vars).
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// S1 (logged-only): the `PANTHEON_SANDBOX_FALLBACK=allow` opt-in
    /// must announce itself exactly once per process. This module never
    /// sets the real env var (other tests rely on the fail-closed
    /// default), which is why the env half is a pure function here.
    #[test]
    fn env_var_fallback_opt_in_is_flagged_once() {
        // Shares ENV_FALLBACK_WARNED with the once-primitive test below.
        let _guard = ENV_GUARD.lock().unwrap();
        assert!(!env_fallback_allows(&None));
        assert!(!env_fallback_allows(&Some("deny".to_string())));
        ENV_FALLBACK_WARNED.store(false, Ordering::SeqCst);
        assert!(
            env_fallback_allows(&Some("allow".to_string())),
            "the documented opt-in value must still opt in (logged-only, not removed)"
        );
        assert!(
            ENV_FALLBACK_WARNED.load(Ordering::SeqCst),
            "the env-var opt-in must arm its once-per-process warning"
        );
        // Later consults keep opting in but never re-announce.
        assert!(env_fallback_allows(&Some("ALLOW".to_string())));
        assert!(
            env_fallback_warn_msg().is_none(),
            "the announcement must fire exactly once per process"
        );
        ENV_FALLBACK_WARNED.store(false, Ordering::SeqCst);
    }

    /// The once-message primitive itself: first consult returns the
    /// announcement, every later one is silent.
    #[test]
    fn env_fallback_warning_message_is_once_per_process() {
        let _guard = ENV_GUARD.lock().unwrap();
        ENV_FALLBACK_WARNED.store(false, Ordering::SeqCst);
        assert!(env_fallback_warn_msg().is_some());
        assert!(env_fallback_warn_msg().is_none());
        ENV_FALLBACK_WARNED.store(false, Ordering::SeqCst);
    }

    /// One choke point for every degraded spawn: the MCP client and the
    /// plugin runner discard the `sandboxed` flag, so the stderr line is
    /// their record. It must name both the program and the origin that
    /// authorized running it unsandboxed.
    #[test]
    fn degraded_spawn_message_names_program_and_origin() {
        let msg = degraded_spawn_msg("python3", "PANTHEON_SANDBOX_FALLBACK");
        assert!(msg.contains("python3"), "{msg}");
        assert!(msg.contains("WITHOUT namespace isolation"), "{msg}");
        assert!(msg.contains("PANTHEON_SANDBOX_FALLBACK"), "{msg}");
        let profile_origin =
            degraded_spawn_msg("mcp-server", "SandboxProfile::allow_direct_fallback");
        assert!(
            profile_origin.contains("SandboxProfile::allow_direct_fallback"),
            "{profile_origin}"
        );
    }

    /// The AppArmor refusal is the one cause an operator can act on, so it
    /// must be recognised from both wrappers' wording and not confused with
    /// an unrelated failure.
    #[test]
    fn userns_refusals_are_recognised() {
        for msg in [
            "bwrap: setting up uid map: Permission denied",
            "bwrap: setting up gid map: Permission denied",
            "unshare: write failed /proc/self/uid_map: Operation not permitted",
            "bwrap: Creating new namespace failed: Permission denied",
            "bwrap: No permissions to creating new namespace, likely because \
             the kernel does not allow non-privileged user namespaces.",
        ] {
            assert!(userns_blocked(msg), "not recognised: {msg}");
        }
        for msg in [
            "bwrap: No such file or directory",
            "sh: 1: true: not found",
            "permission denied while binding a read-only path",
            "",
        ] {
            assert!(!userns_blocked(msg), "false positive: {msg}");
        }
    }

    /// The `SANDBOX_UNAVAILABLE` detail must name the AppArmor restriction
    /// alongside the fallback env var when the kernel refused the userns,
    /// and must not blame AppArmor for an unrelated failure.
    #[test]
    fn unavailable_error_names_apparmor_when_the_userns_is_refused() {
        let boundary = crate::sandbox::ExecutionBoundary::IsolatedProcess;
        let refused = sandbox_unavailable_msg(
            &boundary,
            Some("bwrap: setting up uid map: Permission denied"),
        );
        assert!(refused.contains("PANTHEON_SANDBOX_FALLBACK"), "{refused}");
        assert!(
            refused.contains("apparmor_restrict_unprivileged_userns"),
            "{refused}"
        );

        let other = sandbox_unavailable_msg(&boundary, Some("bwrap: bind failed"));
        assert!(other.contains("PANTHEON_SANDBOX_FALLBACK"), "{other}");
        assert!(
            !other.contains("apparmor_restrict_unprivileged_userns"),
            "{other}"
        );
        assert!(other.contains("bind failed"), "{other}");

        let none = sandbox_unavailable_msg(&boundary, None);
        assert!(none.contains("PANTHEON_SANDBOX_FALLBACK"), "{none}");
        assert!(
            !none.contains("apparmor_restrict_unprivileged_userns"),
            "{none}"
        );
    }

    /// A probe that ran without a wrapper must not count as "the wrapper
    /// initializes". The wrapper lookup reads the process environment
    /// (`PANTHEON_SANDBOX_PATH`, then `PATH`), which a parallel test can
    /// change; a cached success then let a later spawn use a wrapper that
    /// cannot run, and the payload's output never arrived (audit REL-01).
    #[test]
    fn wrapper_less_probe_is_not_a_success() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let profile = medium_profile();
        let cwd = scratch_cwd("nowrapper");
        let saved = std::env::var_os("PANTHEON_SANDBOX_PATH");
        std::env::set_var(
            "PANTHEON_SANDBOX_PATH",
            std::path::Path::new("/nonexistent-pantheon-sandbox-path"),
        );
        let probe = probe_wrapper(&profile, &cwd);
        match saved {
            Some(v) => std::env::set_var("PANTHEON_SANDBOX_PATH", v),
            None => std::env::remove_var("PANTHEON_SANDBOX_PATH"),
        }
        assert!(!probe.ok, "a wrapper-less probe reported success");
        assert!(
            !probe.stderr.is_empty(),
            "the reason is recorded: {}",
            probe.stderr
        );
    }

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

    /// Item 1a: `Medium` sets `network: false`, so the child must land in a
    /// DIFFERENT network namespace than the parent; without a net flag the two
    /// namespaces are identical.
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

    /// `VeryHigh` without `bwrap` must fail closed (`SANDBOX_UNAVAILABLE`) rather
    /// than silently downgrading to plain unshare while reporting `sandboxed=true`.
    /// Simulates the bwrap-absent host with a PATH dir containing unshare but no
    /// bwrap.
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
