//! Sandbox hierarchy (spec section 10). Policy chooses the boundary:
//! LOW = in-process/restricted, MEDIUM = isolated process + limits,
//! HIGH = container, VERY HIGH = strict namespaces.
//!
//! Isolation policy + execution engine live in one crate: the sandbox
//! hierarchy decides the boundary, `pantheon-exec` enforces it around
//! the processes it spawns.
//!
//! [`level`] turns a level into the concrete limits the executor must
//! apply; [`enforce`] maps the core capability policy (Allow/Deny/Approval)
//! onto those boundaries. Levels are ordered weakest -> strongest.
pub mod enforce;
pub mod kernel;
pub mod level;
pub mod runner;

pub use enforce::{capability_label, enforce, Enforcement};
pub use kernel::{kernel_layer_available, ALLOWED_SYSCALLS, DENIED_SYSCALLS};
pub use level::{profile_for, ExecutionBoundary, SandboxProfile};
pub use runner::{
    build_sandboxed, run_sandboxed, run_sandboxed_with_env, run_sandboxed_with_spawn_hook_and_env,
    SandboxResult,
};

use pantheon_api::capability::Capability;
use serde::{Deserialize, Serialize};

/// Can this host actually build the given boundary?
///
/// The binary existing is not the same as the boundary working. Both
/// `unshare --map-root-user` and `bwrap --unshare-user` need to write a
/// uid map, and hardened or container hosts refuse that with EPERM even
/// with the binary installed. When the probe fails the runner fails
/// closed with `SANDBOX_UNAVAILABLE`, which is correct behavior, so
/// callers that want to distinguish "the host cannot do this" from "the
/// code is broken" ask here first.
pub fn boundary_available(boundary: ExecutionBoundary) -> bool {
    use std::process::{Command, Stdio};
    use std::sync::OnceLock;

    static UNSHARE: OnceLock<bool> = OnceLock::new();
    static BWRAP: OnceLock<bool> = OnceLock::new();

    let probe = |args: &[&str]| -> bool {
        let Ok(status) = Command::new(args[0])
            .args(&args[1..])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        else {
            return false;
        };
        status.success()
    };

    match boundary {
        ExecutionBoundary::InProcess => true,
        ExecutionBoundary::IsolatedProcess => *UNSHARE.get_or_init(|| {
            runner::find_binary("unshare")
                .map(|u| {
                    probe(&[
                        u.to_string_lossy().as_ref(),
                        "--map-root-user",
                        "--pid",
                        "--mount",
                        "--fork",
                        "true",
                    ])
                })
                .unwrap_or(false)
        }),
        ExecutionBoundary::Container | ExecutionBoundary::StrictNamespaces => {
            *BWRAP.get_or_init(|| {
                runner::find_binary("bwrap")
                    .map(|b| {
                        probe(&[
                            b.to_string_lossy().as_ref(),
                            "--unshare-user-try",
                            "--ro-bind",
                            "/",
                            "/",
                            "true",
                        ])
                    })
                    .unwrap_or(false)
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SandboxLevel {
    Low,
    Medium,
    High,
    VeryHigh,
}

impl SandboxLevel {
    /// Pick the minimum level for a capability use.
    pub fn for_capability(cap: &Capability) -> SandboxLevel {
        match cap {
            Capability::FilesystemRead | Capability::MemoryRead => SandboxLevel::Low,
            Capability::FilesystemWrite
            | Capability::MemoryWrite
            | Capability::GitRead
            | Capability::MessageSend(_) => SandboxLevel::Medium,
            Capability::ShellExecute | Capability::GitWrite | Capability::NetworkOutbound => {
                SandboxLevel::High
            }
            Capability::GitPush
            | Capability::Browser
            | Capability::BrowserAct
            | Capability::BrowserFillLogin
            | Capability::ComputerUse
            | Capability::SecretsUse
            | Capability::AgentSpawn
            | Capability::MemoryConfirm
            | Capability::PluginEnable
            | Capability::McpEnable
            | Capability::Other(_) => SandboxLevel::VeryHigh,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            SandboxLevel::Low => "low",
            SandboxLevel::Medium => "medium",
            SandboxLevel::High => "high",
            SandboxLevel::VeryHigh => "very-high",
        }
    }
}
