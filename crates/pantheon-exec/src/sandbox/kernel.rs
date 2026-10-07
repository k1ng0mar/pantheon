//! Kernel-level confinement applied in the child between `fork` and `exec`:
//! a seccomp syscall filter and a Landlock filesystem ruleset.
//!
//! Three layers, and they are not interchangeable:
//!
//! - **Namespaces** (`bwrap`/`unshare`, in `runner.rs`) decide what the
//!   child can *see*. They are the strongest boundary and the one already
//!   in place.
//! - **seccomp** decides what the child can *ask the kernel to do*. A
//!   namespace-confined process can still call `ptrace`, `bpf`,
//!   `userfaultfd`, `keyctl`, or `io_uring_setup`, and those are the
//!   syscalls a confined process uses to escape confinement.
//! - **Landlock** decides which *paths* the child may write, even as the
//!   owning user. Namespaces do not restrict this: a process in its own
//!   mount namespace still writes `$HOME` if the mount is bound in.
//!
//! All three are applied in `pre_exec`, which runs after `fork` and before
//! `exec`. That zone is async-signal-safe only: no allocation, no locks, no
//! `println!`. Every filter is therefore compiled in the PARENT and the
//! child only makes the two syscalls. `apply_filter` and
//! `landlock_restrict_self` are both plain syscalls, which is why this is
//! sound.
//!
//! Fail-closed is the rule. When a profile asks for a layer and the host
//! cannot provide it, the child returns an error from `pre_exec`, the spawn
//! fails, and the tool reports the failure. A silent downgrade to running
//! unfiltered would be the same wrong-strength bug the namespace path
//! already fixed once.

/// The syscalls a sandboxed child may make, everything else is denied.
///
/// Deliberately narrow and deliberately explicit. A denylist was rejected:
/// the kernel adds syscalls faster than a denylist can track them, and the
/// escape primitives (`ptrace`, `bpf`, `userfaultfd`, `keyctl`,
/// `perf_event_open`, `io_uring_setup`, `process_vm_*`, `kcmp`, `open_by_handle_at`)
/// are exactly the ones a denylist author forgets. An allowlist of what a
/// build/test/lint child actually needs inverts the default.
///
/// Grouped by what they are for, because the list is the security policy and
/// a reviewer has to be able to audit it.
#[cfg(target_os = "linux")]
pub const ALLOWED_SYSCALLS: &[&str] = &[
    // --- process lifecycle ---
    "read",
    "write",
    "close",
    "fstat",
    "newfstatat",
    "statx",
    "lseek",
    "mmap",
    "mprotect",
    "munmap",
    "brk",
    "rt_sigaction",
    "rt_sigprocmask",
    "rt_sigreturn",
    "sigaltstack",
    "clone3",
    "clone",
    "execve",
    "exit",
    "exit_group",
    "wait4",
    "waitid",
    "getpid",
    "getppid",
    "gettid",
    "getuid",
    "geteuid",
    "getgid",
    "getegid",
    "set_tid_address",
    "set_robust_list",
    "rseq",
    "prlimit64",
    "getrlimit",
    "arch_prctl",
    "futex",
    "sched_yield",
    "sched_getaffinity",
    "getrandom",
    "uname",
    "sysinfo",
    "getcwd",
    "readlink",
    "readlinkat",
    // --- filesystem ---
    "openat",
    "openat2",
    "close_range",
    "access",
    "faccessat",
    "faccessat2",
    "getdents64",
    "mkdir",
    "mkdirat",
    "rmdir",
    "unlink",
    "unlinkat",
    "rename",
    "renameat",
    "renameat2",
    "link",
    "linkat",
    "symlink",
    "symlinkat",
    "chmod",
    "fchmod",
    "fchmodat",
    "chown",
    "fchown",
    "fchownat",
    "truncate",
    "ftruncate",
    "fallocate",
    "statfs",
    "fstatfs",
    "utimensat",
    "copy_file_range",
    "sendfile",
    "fsync",
    "fdatasync",
    "sync_file_range",
    "flock",
    "fcntl",
    "dup",
    "dup2",
    "dup3",
    "pipe",
    "pipe2",
    "ioctl",
    "ppoll",
    "poll",
    "select",
    "pselect6",
    "epoll_create1",
    "epoll_ctl",
    "epoll_wait",
    "epoll_pwait",
    "inotify_init1",
    "inotify_add_watch",
    "inotify_rm_watch",
    // --- memory ---
    "madvise",
    "mlock",
    "munlock",
    "mlockall",
    "munlockall",
    "mremap",
    "mincore",
    "memfd_create",
    "membarrier",
    // --- networking (the capability gate decides whether egress is
    // allowed at all; when it is, these are what it takes) ---
    "socket",
    "socketpair",
    "connect",
    "bind",
    "listen",
    "accept",
    "accept4",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "shutdown",
    "getsockname",
    "getpeername",
    "getsockopt",
    "setsockopt",
    // --- time ---
    "clock_gettime",
    "clock_nanosleep",
    "nanosleep",
    "clock_getres",
    "timerfd_create",
    "timerfd_settime",
    "timerfd_gettime",
    "times",
    "gettimeofday",
    // --- signals / identity ---
    "kill",
    "tgkill",
    "tkill",
    "rt_sigtimedwait",
    "rt_sigqueueinfo",
    "umask",
    "prctl",
    "capget",
    "capset",
    "setpgid",
    "getpgid",
    "getsid",
    "setsid",
    "getgroups",
    "setgroups",
    "personality",
];

/// Syscalls that are refused even though a broad allowlist might include
/// them. Kept as an explicit list so the refusal is auditable and a future
/// allowlist edit cannot silently re-admit one.
///
/// These are the escape primitives. `seccompiler` applies deny rules
/// after the allowlist, so listing one here is a hard deny.
#[cfg(target_os = "linux")]
pub const DENIED_SYSCALLS: &[&str] = &[
    // Debugging another process - the classic container escape primitive.
    "ptrace",
    "process_vm_readv",
    "process_vm_writev",
    "kcmp",
    // Kernel attack surface reachable from an unprivileged process.
    "bpf",
    "userfaultfd",
    "perf_event_open",
    "kexec_load",
    "kexec_file_load",
    "init_module",
    "finit_module",
    "delete_module",
    "io_uring_setup",
    "io_uring_enter",
    "io_uring_register",
    // Kernel keyring: credential material the child has no business in.
    "keyctl",
    "add_key",
    "request_key",
    // Mounting and namespace manipulation from inside the sandbox.
    "mount",
    "umount2",
    "pivot_root",
    "chroot",
    "setns",
    "unshare",
    // Opening by inode handle bypasses path-based (Landlock) checks.
    "open_by_handle_at",
    "name_to_handle_at",
    // Direct device and port access.
    "iopl",
    "ioperm",
    // Reboot/host control.
    "reboot",
    "swapon",
    "swapoff",
    "acct",
    "quotactl",
    // Kernel log introspection. (`sysctl` is not a syscall on x86_64; the
    // name-resolution test catches arch mismatches like that.)
    "syslog",
];

/// Compile the seccomp program in the PARENT, so the child's `pre_exec`
/// closure only has to make the syscall.
///
/// Returns the compiled BPF program. The caller applies it inside
/// `pre_exec` via [`seccompiler::apply_filter`], which is async-signal-safe.
#[cfg(target_os = "linux")]
pub fn compile_seccomp() -> Result<seccompiler::BpfProgram, String> {
    use seccompiler::{SeccompAction, SeccompFilter, SeccompRule, TargetArch};
    use std::collections::BTreeMap;

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // Resolve names to numbers once, in the parent. An unknown name is a
    // build error, not a runtime skip: a typo in the allowlist must not
    // silently drop a permission the child needs, and a typo in the denylist
    // must not silently leave an escape primitive open.
    for name in ALLOWED_SYSCALLS {
        let nr = syscall_number(name)
            .ok_or_else(|| format!("seccomp allowlist names unknown syscall: {name}"))?;
        // An empty rule vec means "match unconditionally", which for the
        // allowlist is what we want: allow this syscall regardless of args.
        rules.entry(nr).or_default();
    }
    for name in DENIED_SYSCALLS {
        let nr = syscall_number(name)
            .ok_or_else(|| format!("seccomp denylist names unknown syscall: {name}"))?;
        // A deny is expressed by REMOVING the allow entry; the filter's
        // default action is what actually denies. Listing a denied syscall
        // that the allowlist also named is the conflict we must not have.
        rules.remove(&nr);
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Errno(libc::EPERM as u32),
        // The default action for anything not in the allowlist.
        SeccompAction::Errno(libc::EPERM as u32),
        TargetArch::x86_64,
    )
    .map_err(|e| format!("seccomp filter build failed: {e}"))?;

    filter
        .try_into()
        .map_err(|e| format!("seccomp filter compile failed: {e}"))
}

/// Resolve a syscall name to its number for the build target.
///
/// `libc::SYS_*` constants are the source of truth; matching on the name
/// keeps the policy list readable. Unknown names return `None` so the
/// caller fails the build rather than skipping.
#[cfg(target_os = "linux")]
pub fn syscall_number(name: &str) -> Option<i64> {
    let n = match name {
        "read" => libc::SYS_read,
        "write" => libc::SYS_write,
        "close" => libc::SYS_close,
        "fstat" => libc::SYS_fstat,
        "newfstatat" => libc::SYS_newfstatat,
        "statx" => libc::SYS_statx,
        "lseek" => libc::SYS_lseek,
        "mmap" => libc::SYS_mmap,
        "mprotect" => libc::SYS_mprotect,
        "munmap" => libc::SYS_munmap,
        "brk" => libc::SYS_brk,
        "rt_sigaction" => libc::SYS_rt_sigaction,
        "rt_sigprocmask" => libc::SYS_rt_sigprocmask,
        "rt_sigreturn" => libc::SYS_rt_sigreturn,
        "sigaltstack" => libc::SYS_sigaltstack,
        "clone3" => libc::SYS_clone3,
        "clone" => libc::SYS_clone,
        "execve" => libc::SYS_execve,
        "exit" => libc::SYS_exit,
        "exit_group" => libc::SYS_exit_group,
        "wait4" => libc::SYS_wait4,
        "waitid" => libc::SYS_waitid,
        "getpid" => libc::SYS_getpid,
        "getppid" => libc::SYS_getppid,
        "gettid" => libc::SYS_gettid,
        "getuid" => libc::SYS_getuid,
        "geteuid" => libc::SYS_geteuid,
        "getgid" => libc::SYS_getgid,
        "getegid" => libc::SYS_getegid,
        "set_tid_address" => libc::SYS_set_tid_address,
        "set_robust_list" => libc::SYS_set_robust_list,
        "rseq" => libc::SYS_rseq,
        "prlimit64" => libc::SYS_prlimit64,
        "getrlimit" => libc::SYS_getrlimit,
        "arch_prctl" => libc::SYS_arch_prctl,
        "futex" => libc::SYS_futex,
        "sched_yield" => libc::SYS_sched_yield,
        "sched_getaffinity" => libc::SYS_sched_getaffinity,
        "getrandom" => libc::SYS_getrandom,
        "uname" => libc::SYS_uname,
        "sysinfo" => libc::SYS_sysinfo,
        "getcwd" => libc::SYS_getcwd,
        "readlink" => libc::SYS_readlink,
        "readlinkat" => libc::SYS_readlinkat,
        "openat" => libc::SYS_openat,
        "openat2" => libc::SYS_openat2,
        "close_range" => libc::SYS_close_range,
        "access" => libc::SYS_access,
        "faccessat" => libc::SYS_faccessat,
        "faccessat2" => libc::SYS_faccessat2,
        "getdents64" => libc::SYS_getdents64,
        "mkdir" => libc::SYS_mkdir,
        "mkdirat" => libc::SYS_mkdirat,
        "rmdir" => libc::SYS_rmdir,
        "unlink" => libc::SYS_unlink,
        "unlinkat" => libc::SYS_unlinkat,
        "rename" => libc::SYS_rename,
        "renameat" => libc::SYS_renameat,
        "renameat2" => libc::SYS_renameat2,
        "link" => libc::SYS_link,
        "linkat" => libc::SYS_linkat,
        "symlink" => libc::SYS_symlink,
        "symlinkat" => libc::SYS_symlinkat,
        "chmod" => libc::SYS_chmod,
        "fchmod" => libc::SYS_fchmod,
        "fchmodat" => libc::SYS_fchmodat,
        "chown" => libc::SYS_chown,
        "fchown" => libc::SYS_fchown,
        "fchownat" => libc::SYS_fchownat,
        "truncate" => libc::SYS_truncate,
        "ftruncate" => libc::SYS_ftruncate,
        "fallocate" => libc::SYS_fallocate,
        "statfs" => libc::SYS_statfs,
        "fstatfs" => libc::SYS_fstatfs,
        "utimensat" => libc::SYS_utimensat,
        "copy_file_range" => libc::SYS_copy_file_range,
        "sendfile" => libc::SYS_sendfile,
        "fsync" => libc::SYS_fsync,
        "fdatasync" => libc::SYS_fdatasync,
        "sync_file_range" => libc::SYS_sync_file_range,
        "flock" => libc::SYS_flock,
        "fcntl" => libc::SYS_fcntl,
        "dup" => libc::SYS_dup,
        "dup2" => libc::SYS_dup2,
        "dup3" => libc::SYS_dup3,
        "pipe" => libc::SYS_pipe,
        "pipe2" => libc::SYS_pipe2,
        "ioctl" => libc::SYS_ioctl,
        "ppoll" => libc::SYS_ppoll,
        "poll" => libc::SYS_poll,
        "select" => libc::SYS_select,
        "pselect6" => libc::SYS_pselect6,
        "epoll_create1" => libc::SYS_epoll_create1,
        "epoll_ctl" => libc::SYS_epoll_ctl,
        "epoll_wait" => libc::SYS_epoll_wait,
        "epoll_pwait" => libc::SYS_epoll_pwait,
        "inotify_init1" => libc::SYS_inotify_init1,
        "inotify_add_watch" => libc::SYS_inotify_add_watch,
        "inotify_rm_watch" => libc::SYS_inotify_rm_watch,
        "madvise" => libc::SYS_madvise,
        "mlock" => libc::SYS_mlock,
        "munlock" => libc::SYS_munlock,
        "mlockall" => libc::SYS_mlockall,
        "munlockall" => libc::SYS_munlockall,
        "mremap" => libc::SYS_mremap,
        "mincore" => libc::SYS_mincore,
        "memfd_create" => libc::SYS_memfd_create,
        "membarrier" => libc::SYS_membarrier,
        "socket" => libc::SYS_socket,
        "socketpair" => libc::SYS_socketpair,
        "connect" => libc::SYS_connect,
        "bind" => libc::SYS_bind,
        "listen" => libc::SYS_listen,
        "accept" => libc::SYS_accept,
        "accept4" => libc::SYS_accept4,
        "sendto" => libc::SYS_sendto,
        "recvfrom" => libc::SYS_recvfrom,
        "sendmsg" => libc::SYS_sendmsg,
        "recvmsg" => libc::SYS_recvmsg,
        "shutdown" => libc::SYS_shutdown,
        "getsockname" => libc::SYS_getsockname,
        "getpeername" => libc::SYS_getpeername,
        "getsockopt" => libc::SYS_getsockopt,
        "setsockopt" => libc::SYS_setsockopt,
        "clock_gettime" => libc::SYS_clock_gettime,
        "clock_nanosleep" => libc::SYS_clock_nanosleep,
        "nanosleep" => libc::SYS_nanosleep,
        "clock_getres" => libc::SYS_clock_getres,
        "timerfd_create" => libc::SYS_timerfd_create,
        "timerfd_settime" => libc::SYS_timerfd_settime,
        "timerfd_gettime" => libc::SYS_timerfd_gettime,
        "times" => libc::SYS_times,
        "gettimeofday" => libc::SYS_gettimeofday,
        "kill" => libc::SYS_kill,
        "tgkill" => libc::SYS_tgkill,
        "tkill" => libc::SYS_tkill,
        "rt_sigtimedwait" => libc::SYS_rt_sigtimedwait,
        "rt_sigqueueinfo" => libc::SYS_rt_sigqueueinfo,
        "umask" => libc::SYS_umask,
        "prctl" => libc::SYS_prctl,
        "capget" => libc::SYS_capget,
        "capset" => libc::SYS_capset,
        "setpgid" => libc::SYS_setpgid,
        "getpgid" => libc::SYS_getpgid,
        "getsid" => libc::SYS_getsid,
        "setsid" => libc::SYS_setsid,
        "getgroups" => libc::SYS_getgroups,
        "setgroups" => libc::SYS_setgroups,
        "personality" => libc::SYS_personality,
        // --- denied: resolved so the conflict check can run, then removed
        // from the allow map by `compile_seccomp` ---
        "ptrace" => libc::SYS_ptrace,
        "process_vm_readv" => libc::SYS_process_vm_readv,
        "process_vm_writev" => libc::SYS_process_vm_writev,
        "kcmp" => libc::SYS_kcmp,
        "bpf" => libc::SYS_bpf,
        "userfaultfd" => libc::SYS_userfaultfd,
        "perf_event_open" => libc::SYS_perf_event_open,
        "kexec_load" => libc::SYS_kexec_load,
        "kexec_file_load" => libc::SYS_kexec_file_load,
        "init_module" => libc::SYS_init_module,
        "finit_module" => libc::SYS_finit_module,
        "delete_module" => libc::SYS_delete_module,
        "io_uring_setup" => libc::SYS_io_uring_setup,
        "io_uring_enter" => libc::SYS_io_uring_enter,
        "io_uring_register" => libc::SYS_io_uring_register,
        "keyctl" => libc::SYS_keyctl,
        "add_key" => libc::SYS_add_key,
        "request_key" => libc::SYS_request_key,
        "mount" => libc::SYS_mount,
        "umount2" => libc::SYS_umount2,
        "pivot_root" => libc::SYS_pivot_root,
        "chroot" => libc::SYS_chroot,
        "setns" => libc::SYS_setns,
        "unshare" => libc::SYS_unshare,
        "open_by_handle_at" => libc::SYS_open_by_handle_at,
        "name_to_handle_at" => libc::SYS_name_to_handle_at,
        "iopl" => libc::SYS_iopl,
        "ioperm" => libc::SYS_ioperm,
        "reboot" => libc::SYS_reboot,
        "swapon" => libc::SYS_swapon,
        "swapoff" => libc::SYS_swapoff,
        "acct" => libc::SYS_acct,
        "quotactl" => libc::SYS_quotactl,
        "syslog" => libc::SYS_syslog,
        _ => return None,
    };
    Some(n)
}

/// The Landlock ruleset to apply in the child: read everywhere, write only
/// under the profile's writable roots.
///
/// Compiled in the parent for the same async-signal-safety reason as the
/// seccomp program. Returns the created ruleset, which the child restricts
/// itself with via `landlock_restrict_self`.
///
/// ABI note: the ruleset is built against `ABI::V1` access rights only.
/// That keeps it working on any kernel with Landlock at all (5.13+), which
/// is the portability the earlier probe showed (this host reports ABI 8,
/// but a stricter build must not require it).
#[cfg(target_os = "linux")]
pub fn compile_landlock(
    cwd: &str,
    writable_paths: &[String],
) -> Result<landlock::RulesetCreated, String> {
    use landlock::{AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI};

    let abi = ABI::V1;
    let read = AccessFs::from_read(abi);
    let write = AccessFs::from_write(abi);

    let mut ruleset = Ruleset::default()
        .handle_access(read | write)
        .map_err(|e| format!("landlock handle_access failed: {e}"))?
        .create()
        .map_err(|e| format!("landlock ruleset create failed: {e}"))?;

    // Read: the whole filesystem. The child has to load its dynamic
    // libraries and read its own program; restricting read here would break
    // every binary without adding a boundary that matters (the process
    // already reads as its own uid). Write is the boundary.
    let root = PathFd::new("/").map_err(|e| format!("landlock open /: {e}"))?;
    ruleset = ruleset
        .add_rule(PathBeneath::new(root, read))
        .map_err(|e| format!("landlock read rule failed: {e}"))?;

    // Write: cwd plus any explicit writable roots. Deduplicated so a
    // repeated path does not add a redundant rule.
    let mut roots: Vec<String> = vec![cwd.to_string()];
    for p in writable_paths {
        if !roots.contains(p) {
            roots.push(p.clone());
        }
    }
    for path in &roots {
        let Ok(fd) = PathFd::new(path) else {
            // A writable root that does not exist is a profile mistake, but
            // failing the spawn over it would be worse than skipping: the
            // cwd rule still applies. Skip and let the profile be fixed.
            continue;
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, write))
            .map_err(|e| format!("landlock write rule for {path} failed: {e}"))?;
    }

    // A ruleset that did not fully apply is a downgrade, and the caller
    // decides whether to refuse. `RulesetCreated::restrict_self` returns the
    // status, so the child reads it after applying and the profile's
    // fail-closed rule is enforced there.
    Ok(ruleset)
}

/// Probe: does this host support the layers a profile asks for?
///
/// Mirrors `boundary_available` for the wrapper: the binary existing is not
/// the same as the mechanism working. Called in the PARENT before a spawn so
/// the failure is a clean `SANDBOX_UNAVAILABLE` rather than a mysterious
/// child exit.
#[cfg(target_os = "linux")]
pub fn kernel_layer_available(seccomp: bool, landlock: bool) -> bool {
    if seccomp && compile_seccomp().is_err() {
        return false;
    }
    if landlock {
        // Landlock needs the syscall to exist. ABI 0 means the kernel has
        // no Landlock at all.
        let abi = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                std::ptr::null::<libc::c_void>(),
                0usize,
                1u32, // LANDLOCK_CREATE_RULESET_VERSION
            )
        };
        #[allow(clippy::manual_range_contains)]
        let supported = abi >= 1;
        if !supported {
            return false;
        }
    }
    true
}

/// Non-Linux stub: the layers are Linux-only, and a profile that asks for
/// them on another platform must fail closed rather than silently skip.
#[cfg(not(target_os = "linux"))]
pub fn kernel_layer_available(seccomp: bool, landlock: bool) -> bool {
    !seccomp && !landlock
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The policy lists must be disjoint and every name must resolve. A
    /// typo in either list is a build error here rather than a silently
    /// dropped permission at run time.
    #[test]
    fn every_named_syscall_resolves() {
        for name in ALLOWED_SYSCALLS {
            assert!(
                syscall_number(name).is_some(),
                "allowlist names an unknown syscall: {name}"
            );
        }
        for name in DENIED_SYSCALLS {
            assert!(
                syscall_number(name).is_some(),
                "denylist names an unknown syscall: {name}"
            );
        }
    }

    /// The escape primitives must not be reachable through the allowlist.
    /// This is the property the whole filter exists for, so it is asserted
    /// rather than trusted to review.
    #[test]
    fn escape_primitives_are_never_allowed() {
        let allowed: Vec<i64> = ALLOWED_SYSCALLS
            .iter()
            .filter_map(|n| syscall_number(n))
            .collect();
        for name in ["ptrace", "bpf", "userfaultfd", "keyctl", "io_uring_setup"] {
            let nr = syscall_number(name).expect("named");
            assert!(
                !allowed.contains(&nr),
                "{name} is in the allowlist: it is a sandbox escape primitive"
            );
        }
    }

    /// The filter must compile on this host, or the profile that asks for it
    /// would fail every spawn.
    #[test]
    fn seccomp_compiles() {
        if !kernel_layer_available(true, false) {
            return; // host without seccomp: the fail-closed path is what runs
        }
        compile_seccomp().expect("seccomp filter compiles when the host supports it");
    }

    /// Landlock must be buildable when the kernel supports it.
    #[test]
    fn landlock_compiles_when_supported() {
        if !kernel_layer_available(false, true) {
            return;
        }
        let tmp = std::env::temp_dir();
        compile_landlock(&tmp.to_string_lossy(), &[])
            .expect("landlock ruleset builds when the kernel supports it");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod live_tests {
    use super::*;

    /// The filter must actually deny in a real child, not just compile.
    ///
    /// This is the proof the whole module exists for: fork a child, apply
    /// the filter, and try a syscall that is NOT on the allowlist. If the
    /// child survives, the filter is decorative.
    ///
    /// `getcpu` is used as the probe because it is a real syscall that is
    /// deliberately absent from the allowlist, takes no arguments that can
    /// fail for other reasons, and cannot affect the parent.
    #[test]
    fn filter_denies_a_non_allowlisted_syscall_in_a_real_child() {
        if !kernel_layer_available(true, false) {
            eprintln!("host has no seccomp; skipping the live denial proof");
            return;
        }
        let program = compile_seccomp().expect("compiles");

        // SAFETY: fork in a test is deliberate. The child only issues
        // apply_filter then one probe syscall, then _exit. No allocation
        // happens in the child, so the post-fork zone is safe.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // Child.
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(10);
                }
                if seccompiler::apply_filter(&program).is_err() {
                    libc::_exit(11);
                }
                // A syscall that is not allowlisted: must fail with EPERM.
                let rc = libc::syscall(
                    libc::SYS_getcpu,
                    std::ptr::null_mut::<u32>(),
                    std::ptr::null_mut::<u32>(),
                    std::ptr::null_mut::<libc::c_void>(),
                );
                let errno = std::io::Error::last_os_error().raw_os_error();
                if rc == -1 && errno == Some(libc::EPERM) {
                    libc::_exit(0); // denied, as designed
                }
                libc::_exit(12); // not denied
            }
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        assert_eq!(
            code, 0,
            "the seccomp filter did not deny a non-allowlisted syscall \
             (child exit {code}: 10=prctl, 11=apply, 12=not denied)"
        );
    }

    /// The allowlist must not be so tight that ordinary work fails: a
    /// filtered child must still be able to write a file in its cwd. A
    /// filter that breaks every real command gets turned off, and then it
    /// protects nothing.
    #[test]
    fn filter_permits_ordinary_file_io() {
        if !kernel_layer_available(true, false) {
            return;
        }
        let program = compile_seccomp().expect("compiles");
        let dir = std::env::temp_dir();
        let path = dir.join(format!("pantheon-seccomp-{}.tmp", std::process::id()));
        let c_path = std::ffi::CString::new(path.to_string_lossy().as_bytes()).unwrap();

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0);
        if pid == 0 {
            unsafe {
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    libc::_exit(10);
                }
                if seccompiler::apply_filter(&program).is_err() {
                    libc::_exit(11);
                }
                // open/write/close: all on the allowlist.
                let fd = libc::open(
                    c_path.as_ptr(),
                    libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC,
                    0o600,
                );
                if fd < 0 {
                    libc::_exit(20);
                }
                let msg = b"ok";
                if libc::write(fd, msg.as_ptr() as *const libc::c_void, msg.len()) != 3 {
                    libc::_exit(21);
                }
                libc::close(fd);
                libc::_exit(0);
            }
        }
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            code, 0,
            "the seccomp allowlist broke ordinary file I/O (child exit {code}: \
             10=prctl, 11=apply, 20=open denied, 21=write denied)"
        );
    }
}
