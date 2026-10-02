//! Extensions: the hook spec, the `plugin.yaml` loader, the Python
//! subprocess runner (fail-open per hook, fail-closed on gates), the manager,
//! the skill doctor, and third-party plugin approval.
//!
//! Security posture: plugins are third-party code. Secrets are scrubbed from
//! the child environment, but a plugin otherwise runs with the operator's full
//! user privileges - it is NOT sandboxed. Enabling a third-party plugin
//! requires explicit operator approval (see [`pantheon_api::approval`]);
//! approval is informed consent to those privileges.
//!
//! The approval store itself lives in `pantheon-api` (shared with
//! `pantheon-exec`'s tool plugins). The wrappers below only supply this
//! crate's `plugin.yaml` manifest loader.
pub mod bundled;
pub mod doctor;
pub mod event_bridge;
pub mod hooks;
pub mod manager;
pub mod manifest;
pub mod python_runner;

use pantheon_api::approval;
use pantheon_api::error::PantheonError;
use std::path::Path;

/// Load a `plugin.yaml`'s name and version for one plugin directory; used
/// as the manifest loader for the shared approval store.
fn load_name_version(dir: &Path) -> Option<(String, String)> {
    manifest::PluginManifest::load(&dir.join("plugin.yaml"))
        .ok()
        .map(|m| (m.name, m.version))
}

/// Third-party hook plugins installed but not approved (or whose approval
/// lapsed after a code change). They are not loaded and never fire.
pub fn pending_approvals(ext_dir: &Path) -> Vec<PendingPlugin> {
    approval::pending_approvals(ext_dir, &load_name_version)
}

/// Record operator approval for the hook plugin named `name`. Errors when
/// the plugin is not in the pending set. The caller must have shown
/// [`warning_text`] and obtained explicit consent first.
pub fn record_approval(ext_dir: &Path, name: &str) -> Result<ApprovalRecord, PantheonError> {
    approval::record_approval(ext_dir, name, &load_name_version)
}

/// Revoke a recorded approval. The plugin stays installed but will no
/// longer load until re-approved.
pub fn revoke_approval(ext_dir: &Path, name: &str) -> Result<bool, PantheonError> {
    approval::revoke_approval_for(ext_dir, name)
}

pub use approval::{
    is_approved, warning_text, ApprovalRecord, ApprovalStore, PendingPlugin, PRIVILEGE_WARNING,
};
pub use bundled::{
    bundled_plugins, disable, enable, find, is_enabled, seed, set_enabled, BundledPlugin,
    PluginKind,
};
pub use doctor::{doctor, DoctorReport};
pub use event_bridge::{dispatch, dispatch_stream_edges, HookDispatcher, HookFire};
pub use hooks::{Hook, HookClass};
pub use manager::{ExtensionManager, GateDecision};
pub use manifest::PluginManifest;
pub use python_runner::{
    fire_hook, fire_hook_full, HookDirective, HookInput, HookOutput, PythonPlugin, RunnerConfig,
};

/// Wipe a plugin child process's environment down to the curated minimum.
///
/// Plugin code is third-party: it must never inherit the host's ambient
/// environment (API keys, session tokens, `PANTHEON_SECRET_*`). After the
/// clear, only PATH is restored so the interpreter resolves. Anything a
/// plugin legitimately needs beyond PATH must arrive through an explicit
/// operator allowlist, never inheritance.
pub(crate) fn minimal_child_env(cmd: &mut std::process::Command) {
    cmd.env_clear();
    // env_clear() wipes everything set before it, so PATH must come after.
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
}
