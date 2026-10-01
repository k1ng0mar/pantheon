//! Behavioral tests for the unified approval store (`pantheon-api::approval`)
//! and its two thin adapters (`pantheon-exec::plugin_approval` for tool
//! plugins, `pantheon-extensions` for hook plugins).
//!
//! Policy: only small deterministic unit tests live beside the code.
//! Everything behavioral — filesystem, stores, processes — lives here and
//! runs via `cargo test -p pantheon-eval`.

use pantheon_api::approval::{self, ApprovalStore};
use std::path::Path;
use tempfile::tempdir;

/// Manifest-free loader for store-level tests: dir name is the plugin name.
fn loader(dir: &Path) -> Option<(String, String)> {
    let name = dir.file_name()?.to_str()?.to_string();
    Some((name, "1.0".to_string()))
}

fn plugin_dir(scope: &Path, name: &str, code: &str) -> std::path::PathBuf {
    let d = scope.join(name);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("run.sh"), code).unwrap();
    d
}

#[test]
fn hash_changes_when_code_changes() {
    let d = tempdir().unwrap();
    let p = plugin_dir(d.path(), "p", "echo one");
    let h1 = approval::dir_hash(&p).unwrap();
    std::fs::write(p.join("run.sh"), "echo two").unwrap();
    let h2 = approval::dir_hash(&p).unwrap();
    assert_ne!(h1, h2, "approval binding must notice modified code");
}

#[test]
fn hash_is_stable_for_unchanged_code() {
    let d = tempdir().unwrap();
    let p = plugin_dir(d.path(), "p", "echo one");
    assert_eq!(
        approval::dir_hash(&p).unwrap(),
        approval::dir_hash(&p).unwrap()
    );
}

#[test]
fn approval_lapses_after_code_change() {
    let scope = tempdir().unwrap();
    let p = plugin_dir(scope.path(), "p", "echo one");
    let rec = approval::record_approval_for(scope.path(), "p", "1.0", &p).unwrap();
    assert!(approval::is_approved(
        scope.path(),
        "p",
        "1.0",
        &approval::dir_hash(&p).unwrap()
    ));
    assert_eq!(rec.plugin, "p");
    // Modify the code: the recorded approval no longer matches.
    std::fs::write(p.join("run.sh"), "echo evil").unwrap();
    assert!(
        !approval::is_approved(scope.path(), "p", "1.0", &approval::dir_hash(&p).unwrap()),
        "approval must lapse when plugin code changes after consent"
    );
}

#[test]
fn approval_lapses_after_version_bump() {
    let scope = tempdir().unwrap();
    let p = plugin_dir(scope.path(), "p", "echo one");
    let hash = approval::dir_hash(&p).unwrap();
    approval::record_approval_for(scope.path(), "p", "1.0", &p).unwrap();
    assert!(approval::is_approved(scope.path(), "p", "1.0", &hash));
    assert!(
        !approval::is_approved(scope.path(), "p", "2.0", &hash),
        "approval is bound to the approved version"
    );
}

#[test]
fn revoke_removes_approval() {
    let scope = tempdir().unwrap();
    let p = plugin_dir(scope.path(), "p", "echo one");
    let hash = approval::dir_hash(&p).unwrap();
    approval::record_approval_for(scope.path(), "p", "1.0", &p).unwrap();
    assert!(approval::revoke_approval_for(scope.path(), "p").unwrap());
    assert!(!approval::is_approved(scope.path(), "p", "1.0", &hash));
    assert!(!approval::revoke_approval_for(scope.path(), "p").unwrap());
}

#[test]
fn pending_lists_unapproved_and_record_clears_it() {
    let scope = tempdir().unwrap();
    plugin_dir(scope.path(), "aaa", "echo a");
    plugin_dir(scope.path(), "zzz", "echo z");
    let pending = approval::pending_approvals(scope.path(), &loader);
    let names: Vec<_> = pending.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["aaa", "zzz"]);
    // record_approval refuses unknown names (no blank-check consent).
    let err = approval::record_approval(scope.path(), "ghost", &loader).unwrap_err();
    assert_eq!(err.code, "APPROVAL_NOT_PENDING");
    approval::record_approval(scope.path(), "aaa", &loader).unwrap();
    let pending = approval::pending_approvals(scope.path(), &loader);
    let names: Vec<_> = pending.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["zzz"]);
}

#[test]
fn bundled_plugins_are_never_pending() {
    let scope = tempdir().unwrap();
    let bundled = scope.path().join("bundled");
    plugin_dir(&bundled, "first-party", "echo one");
    plugin_dir(scope.path(), "third-party", "echo one");
    let pending = approval::pending_approvals(scope.path(), &loader);
    let names: Vec<_> = pending.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["third-party"]);
}

#[test]
fn store_survives_reopen() {
    let scope = tempdir().unwrap();
    let p = plugin_dir(scope.path(), "p", "echo one");
    let hash = approval::dir_hash(&p).unwrap();
    approval::record_approval_for(scope.path(), "p", "1.0", &p).unwrap();
    let reopened = ApprovalStore::open(scope.path());
    assert!(reopened.is_approved("p", "1.0", &hash));
}

// --- exec adapter: DiscoveredPlugin mapping onto the shared store ---

use pantheon_exec::plugins::{discover_plugins, DiscoveredPlugin, PluginLocation};

fn write_exec_plugin(dir: &Path, name: &str) {
    let p = dir.join(name);
    std::fs::create_dir_all(&p).unwrap();
    let manifest = pantheon_exec::plugins::PluginManifest {
        name: name.to_string(),
        description: "test".into(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: "".into(),
        capabilities: vec![],
        env_vars: vec![],
        runner: "run.sh".into(),
        enabled: false,
    };
    std::fs::write(
        p.join("manifest.yaml"),
        serde_yaml::to_string(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(p.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
}

#[test]
fn exec_adapter_roundtrip_and_lapse() {
    use pantheon_exec::plugin_approval as pa;
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    write_exec_plugin(&plugins, "tp");
    let found: Vec<DiscoveredPlugin> = discover_plugins(&data, d.path());
    assert_eq!(found.len(), 1);
    let plugin = &found[0];
    assert!(!pa::is_approved(plugin));
    assert_eq!(pa::pending_approvals(&found).len(), 1);
    let rec = pa::record_approval(plugin).unwrap();
    assert_eq!(rec.plugin, "tp");
    assert!(pa::is_approved(plugin));
    assert!(pa::pending_approvals(&found).is_empty());
    // Code change lapses the approval.
    std::fs::write(plugin.root.join("run.sh"), "#!/bin/sh\necho evil\n").unwrap();
    assert!(!pa::is_approved(plugin));
    assert!(pa::revoke_approval(plugin).is_ok());
}

#[test]
fn exec_adapter_bundled_needs_no_approval() {
    use pantheon_exec::plugin_approval as pa;
    use pantheon_exec::plugins::load_plugin;
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let bundled = data.join("plugins").join("bundled");
    write_exec_plugin(&bundled, "bp");
    // Bundled plugins live one level deeper than user plugins, so they are
    // not found by the one-level `discover_plugins` scan; load directly.
    let plugin =
        load_plugin(&bundled.join("bp"), PluginLocation::User).expect("bundled plugin loads");
    assert!(pa::is_bundled(&plugin));
    assert!(pa::is_approved(&plugin));
    assert!(pa::pending_approvals(std::slice::from_ref(&plugin)).is_empty());
}

// --- extensions adapter: plugin.yaml loader onto the shared store ---

fn write_hook_plugin(ext_dir: &Path, name: &str) {
    let p = ext_dir.join(name);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(
        p.join("plugin.yaml"),
        format!("name: {name}\nversion: 0.1.0\nhooks: []\n"),
    )
    .unwrap();
    std::fs::write(p.join("hook.py"), "print('hi')\n").unwrap();
}

#[test]
fn extensions_adapter_pending_and_approve() {
    let d = tempdir().unwrap();
    let ext = d.path().join("extensions");
    std::fs::create_dir_all(&ext).unwrap();
    write_hook_plugin(&ext, "hext");
    let pending = pantheon_extensions::pending_approvals(&ext);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].name, "hext");
    let rec = pantheon_extensions::record_approval(&ext, "hext").unwrap();
    assert_eq!(rec.plugin, "hext");
    assert_eq!(rec.version, "0.1.0");
    assert!(pantheon_extensions::pending_approvals(&ext).is_empty());
    // Approval is bound to the content hash: modify the plugin, it lapses.
    std::fs::write(ext.join("hext").join("hook.py"), "print('evil')\n").unwrap();
    assert_eq!(pantheon_extensions::pending_approvals(&ext).len(), 1);
}

#[test]
fn extensions_adapter_revoke() {
    let d = tempdir().unwrap();
    let ext = d.path().join("extensions");
    std::fs::create_dir_all(&ext).unwrap();
    write_hook_plugin(&ext, "hext");
    pantheon_extensions::record_approval(&ext, "hext").unwrap();
    assert!(pantheon_extensions::revoke_approval(&ext, "hext").unwrap());
    assert_eq!(pantheon_extensions::pending_approvals(&ext).len(), 1);
}

#[cfg(unix)]
mod symlinks {
    use super::*;
    use std::os::unix::fs::symlink;

    fn link_plugin(dir: &Path) -> std::path::PathBuf {
        let p = plugin_dir(dir, "p", "echo one");
        std::fs::write(p.join("data.txt"), "payload").unwrap();
        p
    }

    #[test]
    fn hash_detects_internal_symlink_retarget() {
        let d = tempdir().unwrap();
        let p = link_plugin(d.path());
        symlink("data.txt", p.join("alias")).unwrap();
        let h1 = approval::dir_hash(&p).unwrap();
        assert_eq!(
            h1,
            approval::dir_hash(&p).unwrap(),
            "stable while untouched"
        );
        // Retarget the link at an internal file: the digest must change
        // even though every real file's bytes are identical.
        std::fs::remove_file(p.join("alias")).unwrap();
        symlink("run.sh", p.join("alias")).unwrap();
        let h2 = approval::dir_hash(&p).unwrap();
        assert_ne!(h1, h2, "retargeted internal symlink must change the digest");
    }

    #[test]
    fn escaping_symlink_hash_binds_target_contents() {
        let d = tempdir().unwrap();
        let p = link_plugin(d.path());
        // Outside file: its contents feed the plugin hash, so rewriting it
        // after approval lapses the binding.
        let outside = d.path().join("secret.txt");
        std::fs::write(&outside, "top-secret-contents").unwrap();
        symlink(&outside, p.join("leak")).unwrap();
        let h1 = approval::dir_hash(&p).unwrap();
        std::fs::write(&outside, "different-secret-contents").unwrap();
        assert_ne!(
            h1,
            approval::dir_hash(&p).unwrap(),
            "rewriting an escaping link's target must change the plugin hash"
        );
        // Retargeting the link itself is also detected.
        std::fs::remove_file(p.join("leak")).unwrap();
        symlink("/etc/hostname", p.join("leak")).unwrap();
        assert_ne!(
            h1,
            approval::dir_hash(&p).unwrap(),
            "retargeted escaping symlink must change the digest"
        );
    }

    #[test]
    fn dir_and_broken_symlinks_fail_closed() {
        let d = tempdir().unwrap();
        let p = link_plugin(d.path());
        let sub = p.join("sub");
        std::fs::create_dir(&sub).unwrap();
        symlink("sub", p.join("dirlink")).unwrap();
        symlink("no-such-target", p.join("broken")).unwrap();
        // A link to a directory or a dangling link cannot be bound to
        // content: hashing fails closed instead of recording the link text.
        assert!(approval::dir_hash(&p).is_err());
    }
}
