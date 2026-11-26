//! execution boundaries. level is the policy, boundary is the mechanism that
//! actually enforces it here, profile is the limit set handed to the executor.
//!
//! containers with dropped caps, no-new-privs, rlimits. no docker wrapper.

use super::SandboxLevel;
use serde::{Deserialize, Serialize};

/// separate from SandboxLevel so a level can be compared and stored as policy
/// while the mechanism stays an implementation detail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionBoundary {
    /// in-process
    InProcess,
    /// forked child under rlimits
    IsolatedProcess,
    /// container with dropped caps and no-new-privs
    Container,
    /// bwrap with every namespace unshared. the strongest thing we've got - no
    /// vm, no guest kernel. a real vm backend was considered and rejected.
    StrictNamespaces,
}

impl SandboxLevel {
    /// which mechanism enforces this level
    pub fn boundary(self) -> ExecutionBoundary {
        match self {
            SandboxLevel::Low => ExecutionBoundary::InProcess,
            SandboxLevel::Medium => ExecutionBoundary::IsolatedProcess,
            SandboxLevel::High => ExecutionBoundary::Container,
            SandboxLevel::VeryHigh => ExecutionBoundary::StrictNamespaces,
        }
    }

    /// limits for this level
    pub fn profile(self) -> SandboxProfile {
        profile_for(self)
    }
}

/// `None` means this level doesn't impose that limit - not "unlimited". the
/// runtime can tighten a field, never loosen one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxProfile {
    pub level: SandboxLevel,
    pub boundary: ExecutionBoundary,
    pub drop_capabilities: bool,
    /// PR_SET_NO_NEW_PRIVS, so a setuid/setcap binary can't climb out
    pub no_new_privs: bool,
    /// may this boundary mediate egress at all. still gated by
    /// `network.outbound`, this is just the outer limit
    pub network: bool,
    pub max_memory_mb: Option<u64>,
    pub max_pids: Option<u32>,
    /// wall clock budget, ms
    pub wall_clock_ms: u64,
    /// when bwrap/unshare can't init, run the command on the host instead of
    /// failing. default false, runner fails closed. PANTHEON_SANDBOX_FALLBACK=allow
    /// does the same at runtime without touching profiles. tool plugins force this
    /// false so only their manifest can opt in.
    #[serde(default)]
    pub allow_direct_fallback: bool,
    /// may the global PANTHEON_SANDBOX_FALLBACK env var opt in for this one.
    /// tool plugins set false, so fallback is manifest-only.
    #[serde(default = "default_allow_env_fallback")]
    pub allow_env_fallback: bool,
    /// seccomp filter in the child. namespaces decide what it can see, seccomp
    /// decides what it can ask the kernel to do - a namespace-confined process
    /// can still call ptrace/bpf/userfaultfd/keyctl, and that's the way out.
    /// deny by default with an allowlist, after no_new_privs (seccomp needs that
    /// bit for an unprivileged filter).
    ///
    /// linux only. if the kernel can't do it we refuse instead of running
    /// unfiltered - a silent downgrade is the wrong-strength bug the namespace
    /// path already had once.
    #[serde(default)]
    pub seccomp: bool,
    /// landlock ruleset in the child. the third layer: namespaces hide, seccomp
    /// restricts syscalls, landlock restricts paths - even for the owning uid.
    /// needs kernel ABI >= 1.
    #[serde(default)]
    pub landlock: bool,
    /// paths landlock grants read+write. empty = cwd only. read stays open
    /// everywhere so libraries load; write is the part that matters.
    #[serde(default)]
    pub writable_paths: Vec<String>,
    /// masked inside bwrap (Container, StrictNamespaces): dirs get an empty
    /// tmpfs, files get /dev/null. shell/terminal fill this from [`sensitive_paths`]
    /// so a model-run command can't read the data dir's secrets, ssh keys, or cloud
    /// creds through the read-only root bind. empty for plugin and mcp profiles.
    #[serde(default)]
    pub hidden_paths: Vec<String>,
}

/// on for built-in profiles: PANTHEON_SANDBOX_FALLBACK=allow is an operator opt-in
/// existing setups rely on. never on unless the var is set, the .env loader flags
/// it as a security key, and every degraded spawn says so on stderr + in the tool
/// result.
fn default_allow_env_fallback() -> bool {
    true
}

/// limits for a level
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
            hidden_paths: Vec::new(),
            allow_direct_fallback: false,
            allow_env_fallback: true,
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
            hidden_paths: Vec::new(),
            allow_direct_fallback: false,
            allow_env_fallback: true,
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
            hidden_paths: Vec::new(),
            allow_direct_fallback: false,
            allow_env_fallback: true,
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
            // strongest boundary, all three layers. only reachable after
            // interactive approval, so it costs nothing on the common path
            seccomp: true,
            landlock: true,
            writable_paths: Vec::new(),
            hidden_paths: Vec::new(),
            allow_direct_fallback: false,
            allow_env_fallback: true,
        },
    }
}

impl From<SandboxLevel> for SandboxProfile {
    fn from(level: SandboxLevel) -> Self {
        profile_for(level)
    }
}
