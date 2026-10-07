//! Execution boundaries and the limits attached to each level (§10).
//!
//! The level is a policy value; the boundary is the mechanism that enforces
//! it on this host, and the profile is the concrete limit set handed to the
//! executor. Pattern we follow: capability-dropped containers, no-new-privs,
//! per-boundary rlimits - sandboxing as a runtime primitive, not a Docker
//! wrapper.

use super::SandboxLevel;
use serde::{Deserialize, Serialize};

/// The mechanism that actually enforces a level.
///
/// Kept separate from [`SandboxLevel`] so a level can be compared and
/// persisted as policy while the boundary stays an implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionBoundary {
    /// Direct in-process call. LOW.
    InProcess,
    /// Forked child process under rlimits. MEDIUM.
    IsolatedProcess,
    /// Container with dropped capabilities and no-new-privs. HIGH.
    Container,
    /// Strict namespace isolation (bwrap with every namespace unshared).
    /// VERY HIGH. Honest naming: this is the strongest OS-level boundary
    /// the runner offers - no hypervisor, no guest kernel, no VM semantics
    /// of any kind. (A real VM backend was considered and rejected.)
    StrictNamespaces,
}

impl SandboxLevel {
    /// Boundary that enforces this level.
    pub fn boundary(self) -> ExecutionBoundary {
        match self {
            SandboxLevel::Low => ExecutionBoundary::InProcess,
            SandboxLevel::Medium => ExecutionBoundary::IsolatedProcess,
            SandboxLevel::High => ExecutionBoundary::Container,
            SandboxLevel::VeryHigh => ExecutionBoundary::StrictNamespaces,
        }
    }

    /// Execution limits for this level.
    pub fn profile(self) -> SandboxProfile {
        profile_for(self)
    }
}

/// Concrete limits handed to the executor for one execution.
///
/// `None` means "this level does not impose that limit" - never "unlimited
/// by design". The runtime may tighten any field, never loosen it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProfile {
    pub level: SandboxLevel,
    pub boundary: ExecutionBoundary,
    /// Drop all capabilities from the bounding set before exec.
    pub drop_capabilities: bool,
    /// Set PR_SET_NO_NEW_PRIVS so setuid/setcap binaries cannot escalate.
    pub no_new_privs: bool,
    /// Whether this boundary may mediate network egress at all. Egress is
    /// still capability-gated (`network.outbound`); this is the outer limit.
    pub network: bool,
    pub max_memory_mb: Option<u64>,
    pub max_pids: Option<u32>,
    /// Wall-clock budget for the execution, in milliseconds.
    pub wall_clock_ms: u64,
    /// Opt-in to the direct-spawn fallback: when the boundary's wrapper
    /// (bwrap/unshare) cannot initialize, run the command directly on the
    /// host instead of failing. Default is `false` - the runner fails
    /// closed. The `PANTHEON_SANDBOX_FALLBACK=allow` environment variable
    /// enables the same fallback at run time without touching profiles.
    #[serde(default)]
    pub allow_direct_fallback: bool,
    /// Apply a seccomp syscall filter in the child before exec.
    ///
    /// Namespaces (bwrap) decide what the child can *see*; seccomp decides
    /// what it can *ask the kernel to do*. The two are complementary: a
    /// namespace-confined process can still call `ptrace`, `bpf`,
    /// `userfaultfd`, or `keyctl`, and those are how a confined process
    /// escapes. Deny-by-default with a curated allowlist, applied after
    /// `no_new_privs` (seccomp requires that bit for unprivileged filters).
    ///
    /// Linux-only. On a kernel or build without seccomp support the runner
    /// refuses rather than running unfiltered when this is set, because a
    /// silent downgrade is exactly the wrong-strength bug the namespace
    /// path already fixed.
    #[serde(default)]
    pub seccomp: bool,
    /// Apply a Landlock filesystem ruleset in the child before exec.
    ///
    /// The third layer: namespaces hide, seccomp restricts syscalls,
    /// Landlock restricts *file paths* the process may touch even as the
    /// owning user. Where the profile's `cwd` is the only writable root,
    /// a confined process cannot write to `$HOME` or `/tmp` even though
    /// its uid allows it. Requires kernel Landlock support (ABI >= 1).
    #[serde(default)]
    pub landlock: bool,
    /// Paths the Landlock ruleset grants read+write. Empty = the `cwd`
    /// alone. Read access is granted to the whole filesystem so the
    /// program can load its libraries; only write is restricted, which is
    /// the boundary that matters for an agent running untrusted commands.
    #[serde(default)]
    pub writable_paths: Vec<String>,
}

/// Execution limits for a level.
pub fn profile_for(level: SandboxLevel) -> SandboxProfile {
    match level {
        SandboxLevel::Low => SandboxProfile {
            level,
            boundary: ExecutionBoundary::InProcess,
            drop_capabilities: false,
            no_new_privs: false,
            network: false,
            max_memory_mb: None,
            max_pids: None,
            wall_clock_ms: 30_000,
            seccomp: false,
            landlock: false,
            writable_paths: Vec::new(),
            allow_direct_fallback: false,
        },
        SandboxLevel::Medium => SandboxProfile {
            level,
            boundary: ExecutionBoundary::IsolatedProcess,
            drop_capabilities: false,
            no_new_privs: true,
            network: false,
            max_memory_mb: Some(2048),
            max_pids: Some(64),
            wall_clock_ms: 120_000,
            seccomp: false,
            landlock: false,
            writable_paths: Vec::new(),
            allow_direct_fallback: false,
        },
        SandboxLevel::High => SandboxProfile {
            level,
            boundary: ExecutionBoundary::Container,
            drop_capabilities: true,
            no_new_privs: true,
            network: true,
            max_memory_mb: Some(4096),
            max_pids: Some(256),
            wall_clock_ms: 600_000,
            seccomp: false,
            landlock: false,
            writable_paths: Vec::new(),
            allow_direct_fallback: false,
        },
        SandboxLevel::VeryHigh => SandboxProfile {
            level,
            boundary: ExecutionBoundary::StrictNamespaces,
            drop_capabilities: true,
            no_new_privs: true,
            network: true,
            max_memory_mb: Some(2048),
            max_pids: Some(64),
            wall_clock_ms: 300_000,
            // The strongest boundary gets all three layers. Namespaces
            // decide what the child sees, seccomp what it may ask the
            // kernel to do, Landlock which paths it may write. This is the
            // level only reached after interactive approval, so the extra
            // layers cost nothing in the common path.
            seccomp: true,
            landlock: true,
            writable_paths: Vec::new(),
            allow_direct_fallback: false,
        },
    }
}

impl From<SandboxLevel> for SandboxProfile {
    fn from(level: SandboxLevel) -> Self {
        profile_for(level)
    }
}
