//! Third-party tool-plugin approval: thin adapter over [`pantheon_api::approval`].
//!
//! The approval store, the content-hash binding, and the privilege warning
//! are the shared implementation in `pantheon-api` (one store for tool
//! plugins here and hook plugins in `pantheon-extensions`). This module
//! only maps the tool-plugin [`DiscoveredPlugin`] shape onto that store.
use crate::plugins::DiscoveredPlugin;
use pantheon_api::approval::{self, ApprovalRecord};
use pantheon_api::error::PantheonError;
use std::path::{Path, PathBuf};

pub use approval::{warning_text, PRIVILEGE_WARNING};

/// The scope directory that owns this plugin's approvals. For a plugin at
/// `<scope>/plugins/<name>` this is `<scope>/plugins`; for a bundled one at
/// `<scope>/plugins/bundled/<name>` it is still `<scope>/plugins` (bundled
/// plugins skip approval, so the store is never consulted for them).
fn scope_dir_of(plugin: &DiscoveredPlugin) -> PathBuf {
    let plugins_dir = plugin
        .root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    if plugins_dir.file_name().is_some_and(|n| n == "bundled") {
        plugins_dir
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or(plugins_dir)
    } else {
        plugins_dir
    }
}

/// True for plugins shipped with Pantheon; they skip the approval gate.
pub fn is_bundled(plugin: &DiscoveredPlugin) -> bool {
    approval::is_bundled(&scope_dir_of(plugin), &plugin.root)
}

/// True when the plugin is bundled, or a third-party plugin whose recorded
/// approval is still bound to its current content hash.
pub fn is_approved(plugin: &DiscoveredPlugin) -> bool {
    if is_bundled(plugin) {
        return true;
    }
    let scope = scope_dir_of(plugin);
    match approval::dir_hash(&plugin.root) {
        Ok(h) => approval::is_approved(&scope, &plugin.manifest.name, &plugin.manifest.version, &h),
        Err(_) => false,
    }
}

/// Approval check bound to bytes already in hand.
///
/// The loader opens the runner first (pinning the inode), reads its bytes
/// through that fd, and passes them here with the fd's fstat metadata.
/// The directory hash is recomputed with the runner's bytes+identity taken
/// from the pinned read instead of a fresh path open, then compared
/// against the store. A `true` result proves the exact bytes the caller
/// holds were operator-approved: an in-place rewrite of the runner file
/// (same inode, new bytes) after the read cannot pass, because the check
/// runs on the bytes already read, not on whatever the path resolves to
/// now. The caller must exec a sealed copy of `runner_bytes` — never
/// re-open the path — for the guarantee to hold.
///
/// `canon_root` / `canon_runner` must be the canonicalized plugin root and
/// runner path (the same pair `spawn_verified` containment-checked).
/// Bundled plugins skip the check, like [`is_approved`].
pub fn is_approved_with_pinned_runner(
    plugin: &DiscoveredPlugin,
    canon_root: &Path,
    canon_runner: &Path,
    runner_bytes: &[u8],
    runner_md: &std::fs::Metadata,
) -> bool {
    if is_bundled(plugin) {
        return true;
    }
    let scope = scope_dir_of(plugin);
    let rel = match canon_runner.strip_prefix(canon_root) {
        Ok(r) => r,
        Err(_) => return false,
    };
    match approval::dir_hash_with_override(canon_root, rel, runner_bytes, runner_md) {
        Ok(h) => approval::is_approved(&scope, &plugin.manifest.name, &plugin.manifest.version, &h),
        Err(_) => false,
    }
}

/// Record operator approval for a discovered plugin. The caller must have
/// shown [`warning_text`] and obtained explicit consent first.
pub fn record_approval(plugin: &DiscoveredPlugin) -> Result<ApprovalRecord, PantheonError> {
    approval::record_approval_for(
        &scope_dir_of(plugin),
        &plugin.manifest.name,
        &plugin.manifest.version,
        &plugin.root,
    )
}

/// Revoke a recorded approval. The plugin stays installed but will no
/// longer verify until re-approved.
pub fn revoke_approval(plugin: &DiscoveredPlugin) -> Result<bool, PantheonError> {
    approval::revoke_approval_for(&scope_dir_of(plugin), &plugin.manifest.name)
}

/// Third-party plugins that are installed but not approved (or whose
/// approval lapsed after a code change). They will not verify until
/// approved.
pub fn pending_approvals(plugins: &[DiscoveredPlugin]) -> Vec<&DiscoveredPlugin> {
    plugins
        .iter()
        .filter(|p| !is_bundled(p) && !is_approved(p))
        .collect()
}
