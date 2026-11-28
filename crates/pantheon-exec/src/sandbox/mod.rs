//! Sandbox hierarchy (spec section 10). policy picks the boundary:
//! LOW = in-process/restricted, MEDIUM = isolated process + limits,
//! HIGH = container, VERY HIGH = strict namespaces.
//!
//! isolation policy + execution engine live in one crate: the hierarchy
//! decides the boundary, `pantheon-exec` enforces it around what it spawns.
//!
//! [`level`] turns a level into the concrete limits the executor must apply;
//! [`enforce`] maps the core capability policy (Allow/Deny/Approval) onto
//! those boundaries. levels are ordered weakest -> strongest.
pub mod enforce;
pub mod kernel;
pub mod level;
pub mod runner;

pub use enforce::{capability_label, enforce, Enforcement};
pub use kernel::kernel_layer_available;
// the seccomp policy lists are x86_64-only, like the filter they feed
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub use kernel::{ALLOWED_SYSCALLS, DENIED_SYSCALLS};
pub use level::{profile_for, ExecutionBoundary, SandboxProfile};
pub use runner::{
    build_sandboxed, build_sandboxed_checked, run_sandboxed, run_sandboxed_with_env,
    run_sandboxed_with_spawn_hook_and_env, SandboxResult,
};

use pantheon_api::capability::Capability;
use serde::{Deserialize, Serialize};

/// credential locations a model-run command must never read: the Pantheon
/// data dir as a whole (ledgers, sessions, secret stores), its secret files
/// individually (so they stay hidden even when cwd re-exposes part of the
/// data dir), SSH/GPG keys, cloud/registry creds, and shell history. shell
/// and terminal sandboxes mask these (audit SEC-01); the file-tool deny list
/// refuses [`data_dir_secret_paths`] plus the home entries (audit SEC-02).
/// paths are returned whether or not they exist.
pub fn sensitive_paths(
    data_dir: &std::path::Path,
    home: Option<&std::path::Path>,
) -> Vec<std::path::PathBuf> {
    let mut out = vec![data_dir.to_path_buf()];
    out.extend(data_dir_secret_paths(data_dir));
    out.extend(home_secret_paths(home));
    out
}

/// secret stores Pantheon writes under its data dir
pub fn data_dir_secret_paths(data_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    DATA_DIR_SECRET_FILES
        .iter()
        .map(|f| data_dir.join(f))
        .collect()
}

/// home on every platform: HOME on Unix, USERPROFILE on Windows (same
/// fallback `pantheon_api::data_dir::resolve` uses)
pub fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
}

/// home-relative credential locations (absolute), empty without a home
pub fn home_secret_paths(home: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    home.map(|h| SENSITIVE_HOME_PATHS.iter().map(|rel| h.join(rel)).collect())
        .unwrap_or_default()
}

/// dotenv (provider keys, gateway tokens), vault key + vault, OAuth logins,
/// gateway env, secure-forms metadata - all under the data dir
pub const DATA_DIR_SECRET_FILES: &[&str] = &[
    ".env",
    ".secrets.key",
    "secrets",
    "secrets.json",
    "logins.env",
    "logins.json",
    "gateway.env",
    "secure-forms.json",
];

/// home-relative credential locations covered by [`sensitive_paths`]
pub const SENSITIVE_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".config/gh",
    ".config/gcloud",
    ".config/hub",
    ".config/op",
    // secrets managers + IaC creds: a plaintext token file each
    ".vault-token",
    ".config/sops",
    ".config/age",
    ".terraform.d",
    ".password-store",
    // browser profiles. cookie/login DBs are the highest-value files on most
    // desktops and Firefox doesn't encrypt them at rest on Linux, so a shell
    // that can read the home tree must not see these (audit F12).
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".config/microsoft-edge",
    ".config/BraveSoftware",
    ".local/share/keyrings",
    ".config/kwalletd",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".bash_history",
    ".zsh_history",
    ".python_history",
];

/// can THIS host actually build the given boundary?
///
/// binary existing != boundary working. both `unshare --map-root-user` and
/// `bwrap --unshare-user` need to write a uid map, and hardened / container
/// hosts refuse that with EPERM even w/ the binary installed. when the probe
/// fails the runner fails closed w/ `SANDBOX_UNAVAILABLE`, which is correct;
/// callers that want "host can't do this" vs "code is broken" ask here first.
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
                            "--unshare-user",
                            "--cap-drop",
                            "ALL",
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
    /// minimum level for a capability use
    pub fn for_capability(cap: &Capability) -> SandboxLevel {
        match cap {
            Capability::FilesystemRead | Capability::MemoryRead => SandboxLevel::Low,
            Capability::FilesystemWrite
            | Capability::MemoryWrite
            | Capability::GitRead
            | Capability::MessageSend(_) => SandboxLevel::Medium,
            Capability::ShellExecute
            | Capability::GitWrite
            | Capability::NetworkOutbound
            // Generation leaves the machine and writes a file, but drives no
            // local process: the same rung as any other network call that
            // produces an artifact.
            | Capability::MediaGenerate => SandboxLevel::High,
            Capability::GitPush
            | Capability::Browser
            | Capability::BrowserAct
            | Capability::BrowserFillLogin
            | Capability::ComputerUse
            | Capability::BinaryTrace
            | Capability::SecretsUse
            | Capability::AgentSpawn
            | Capability::AgentMessage
            | Capability::MemoryConfirm
            | Capability::PluginEnable
            | Capability::McpEnable
            | Capability::McpUnclassified
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
