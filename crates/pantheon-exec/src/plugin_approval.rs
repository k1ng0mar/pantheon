//! Third-party tool-plugin approval: thin adapter over [`pantheon_api::approval`].
//!
//! The approval store, content-hash binding, and privilege warning are shared
//! in `pantheon-api` (one store for tool plugins here and hook plugins in
//! `pantheon-extensions`). This module just maps the tool-plugin
//! [`DiscoveredPlugin`] shape onto that store.
use crate::plugins::DiscoveredPlugin;
use pantheon_api::approval::{self, ApprovalRecord};
use pantheon_api::error::PantheonError;
use std::path::{Path, PathBuf};

pub use approval::{warning_text, PRIVILEGE_WARNING};

/// scope dir that owns this plugin's approvals: for `<scope>/plugins/<name>`
/// that's `<scope>/plugins`; for a bundled one at `<scope>/plugins/bundled/<name>`
/// it's still `<scope>/plugins` (bundled plugins skip approval, so the store is
/// never asked about them)
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

/// true for plugins shipped with Pantheon; they skip the approval gate
pub fn is_bundled(plugin: &DiscoveredPlugin) -> bool {
    approval::is_bundled(&scope_dir_of(plugin), &plugin.root)
}

/// true only when the plugin is a bundled-path dir whose contents match the
/// embedded registry byte-for-byte. a hand-dropped dir under `bundled/`
/// (or a tampered shipped dir) is path-bundled but NOT canonical: it must go
/// through third-party approval like anything else unvetted. this is the
/// check approval gates must use; path-only [`is_bundled`] is for listing/layout.
pub fn is_canonical_bundled(plugin: &DiscoveredPlugin) -> bool {
    is_bundled(plugin) && crate::plugins::verify_bundled_dir(&plugin.root)
}

/// true when bundled, or a third-party plugin whose recorded approval is
/// still bound to its current content hash
pub fn is_approved(plugin: &DiscoveredPlugin) -> bool {
    if is_canonical_bundled(plugin) {
        return true;
    }
    let scope = scope_dir_of(plugin);
    match approval::dir_hash(&plugin.root) {
        Ok(h) => approval::is_approved(&scope, &plugin.manifest.name, &plugin.manifest.version, &h),
        Err(_) => false,
    }
}

/// approval check bound to bytes we already hold.
///
/// the loader opens the runner first (pinning the inode), reads its bytes
/// through that fd, and passes them here with the fd's fstat metadata. the
/// dir hash is recomputed with the runner's bytes+identity from that pinned
/// read, not a fresh path open, then compared to the store. a `true` means
/// the exact bytes the caller holds were operator-approved: rewriting the
/// runner file in place (same inode, new bytes) after the read can't pass,
/// since we hash what we read, not what the path resolves to now. the caller
/// must exec a sealed copy of `runner_bytes` - never re-open the path - for
/// this to hold.
///
/// `canon_root`/`canon_runner` must be the canonicalized plugin root and
/// runner path (the same pair `spawn_verified` containment-checked). bundled
/// plugins skip the check, like [`is_approved`].
pub fn is_approved_with_pinned_runner(
    plugin: &DiscoveredPlugin,
    canon_root: &Path,
    canon_runner: &Path,
    runner_bytes: &[u8],
    runner_md: &std::fs::Metadata,
) -> bool {
    if is_canonical_bundled(plugin) {
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

/// record operator approval; caller must have shown [`warning_text`] and
/// gotten explicit consent first
pub fn record_approval(plugin: &DiscoveredPlugin) -> Result<ApprovalRecord, PantheonError> {
    approval::record_approval_for(
        &scope_dir_of(plugin),
        &plugin.manifest.name,
        &plugin.manifest.version,
        &plugin.root,
    )
}

/// revoke a recorded approval; plugin stays installed but won't verify again
/// until re-approved
pub fn revoke_approval(plugin: &DiscoveredPlugin) -> Result<bool, PantheonError> {
    approval::revoke_approval_for(&scope_dir_of(plugin), &plugin.manifest.name)
}

/// plugins installed but not approved (or whose approval lapsed after a code
/// change); they won't verify until approved
pub fn pending_approvals(plugins: &[DiscoveredPlugin]) -> Vec<&DiscoveredPlugin> {
    plugins
        .iter()
        .filter(|p| !is_canonical_bundled(p) && !is_approved(p))
        .collect()
}
