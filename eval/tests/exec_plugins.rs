//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem - lives here and
//! runs via `cargo test -p pantheon-eval`.

use pantheon_api::capability::Capability;
use pantheon_exec::plugins::*;
use std::path::Path;
use tempfile::tempdir;

/// Discovery also returns the bundled first-party plugins seeded
/// into the data dir, so tests select the plugin they wrote by name
/// rather than assuming it is the only (or first) hit.
fn only_plugin<'a>(found: &'a [DiscoveredPlugin], name: &str) -> &'a DiscoveredPlugin {
    found
        .iter()
        .find(|p| p.manifest.name == name)
        .unwrap_or_else(|| panic!("plugin {name} not discovered"))
}

#[test]
fn discover_finds_user_plugin() {
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    write_plugin(
        &plugins,
        "demo",
        "run.sh",
        vec![ToolCapability {
            name: "demo_ping".into(),
            capability: Capability::ShellExecute,
            description: "ping".into(),
            parameters: serde_json::json!({}),
        }],
    );
    let found = discover_plugins(&data, d.path());
    let demo = only_plugin(&found, "demo");
    assert_eq!(demo.location, PluginLocation::User);
}

#[test]
fn discover_ignores_dirs_without_manifest() {
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::create_dir_all(plugins.join("no-manifest")).unwrap();
    let found = discover_plugins(&data, d.path());
    assert!(
        found.iter().all(|p| p.root != plugins.join("no-manifest")),
        "a directory without a manifest is never discovered as a plugin"
    );
}

#[test]
fn verify_rejects_missing_runner() {
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    write_plugin(&plugins, "bad", "run.sh", vec![]);
    // Delete the runner file after creation.
    std::fs::remove_file(plugins.join("bad").join("run.sh")).unwrap();
    let found = discover_plugins(&data, d.path());
    let plugin = only_plugin(&found, "bad");
    pantheon_exec::plugin_approval::record_approval(plugin).unwrap();
    let err = verify_plugin(plugin).unwrap_err();
    assert_eq!(err.code, "PLUGIN_NO_RUNNER");
}

#[test]
fn verify_rejects_missing_shebang() {
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    write_plugin(&plugins, "noshebang", "run.sh", vec![]);
    // Remove shebang line.
    let p = plugins.join("noshebang").join("run.sh");
    std::fs::write(&p, "echo no shebang").unwrap();
    let found = discover_plugins(&data, d.path());
    let plugin = only_plugin(&found, "noshebang");
    pantheon_exec::plugin_approval::record_approval(plugin).unwrap();
    let err = verify_plugin(plugin).unwrap_err();
    assert_eq!(err.code, "PLUGIN_NO_SHEBANG");
}

#[test]
fn verify_rejects_missing_required_env() {
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    write_plugin(&plugins, "envcheck", "run.sh", vec![]);
    // Patch the manifest to require an env var.
    let p = plugins.join("envcheck");
    std::fs::write(
            p.join("manifest.yaml"),
            "name: envcheck\ndescription: ''\nversion: '0.1.0'\nrunner: run.sh\nenv_vars:\n  - name: MY_API_KEY\n    required: true\n",
        )
        .unwrap();
    // Ensure MY_API_KEY is not in the environment.
    std::env::remove_var("MY_API_KEY");
    let found = discover_plugins(&data, d.path());
    let plugin = only_plugin(&found, "envcheck");
    pantheon_exec::plugin_approval::record_approval(plugin).unwrap();
    let err = verify_plugin(plugin).unwrap_err();
    assert_eq!(err.code, "PLUGIN_MISSING_ENV");
}

#[test]
fn verify_rejects_unapproved_plugin() {
    // The privilege gate itself: a third-party plugin with no recorded
    // approval is refused with PLUGIN_NOT_APPROVED, even with a valid runner.
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    write_plugin(&plugins, "shady", "run.sh", vec![]);
    let found = discover_plugins(&data, d.path());
    let err = verify_plugin(only_plugin(&found, "shady")).unwrap_err();
    assert_eq!(err.code, "PLUGIN_NOT_APPROVED");
}

#[test]
fn tool_allowed_filters_by_capability() {
    let d = tempdir().unwrap();
    let data = d.path().join("data");
    let plugins = data.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    write_plugin(
        &plugins,
        "capprod",
        "run.sh",
        vec![ToolCapability {
            name: "safe_read".into(),
            capability: Capability::ShellExecute,
            description: "s".into(),
            parameters: serde_json::json!({}),
        }],
    );
    let found = discover_plugins(&data, d.path());
    let plugin = only_plugin(&found, "capprod");
    // default policy denies everything.
    let policy = pantheon_api::capability::Policy::default();
    assert!(!tool_allowed(plugin, "safe_read", &policy));
    let coder = pantheon_api::capability::Policy::coder();
    assert!(tool_allowed(plugin, "safe_read", &coder));
}

#[test]
fn verify_rejects_dotdot_runner() {
    let d = tempdir().unwrap();
    for bad in ["../evil.sh", "sub/../../evil.sh"] {
        let err = verify_plugin(&plugin_with_runner(d.path(), bad)).unwrap_err();
        assert_eq!(err.code, "PLUGIN_UNSAFE_RUNNER", "{bad:?}");
    }
}

#[cfg(windows)]
#[test]
fn verify_rejects_dotdot_runner_windows_separator() {
    // On Windows `\` is a path separator, so `..\evil.sh` is a traversal.
    let d = tempdir().unwrap();
    let err = verify_plugin(&plugin_with_runner(d.path(), "..\\evil.sh")).unwrap_err();
    assert_eq!(err.code, "PLUGIN_UNSAFE_RUNNER");
}

#[test]
fn verify_rejects_absolute_runner() {
    let d = tempdir().unwrap();
    let err = verify_plugin(&plugin_with_runner(d.path(), "/bin/sh")).unwrap_err();
    assert_eq!(err.code, "PLUGIN_UNSAFE_RUNNER");
}

#[test]
fn verify_rejects_empty_runner() {
    let d = tempdir().unwrap();
    for bad in ["", "   "] {
        let err = verify_plugin(&plugin_with_runner(d.path(), bad)).unwrap_err();
        assert_eq!(err.code, "PLUGIN_UNSAFE_RUNNER", "{bad:?}");
    }
}

#[cfg(unix)]
#[test]
fn verify_rejects_symlinked_runner_escape() {
    // The runner is a relative symlink pointing outside the plugin dir:
    // the lexical check passes, the canonical containment check must not.
    let d = tempdir().unwrap();
    let plugin = plugin_with_runner(d.path(), "link.sh");
    std::os::unix::fs::symlink("/bin/true", plugin.root.join("link.sh")).unwrap();
    let err = verify_plugin(&plugin).unwrap_err();
    assert_eq!(err.code, "PLUGIN_UNSAFE_RUNNER");
}

#[test]
fn verify_accepts_nested_relative_runner() {
    let d = tempdir().unwrap();
    let plugin = plugin_with_runner(d.path(), "bin/run.sh");
    std::fs::create_dir_all(plugin.root.join("bin")).unwrap();
    std::fs::write(
        plugin.root.join("bin").join("run.sh"),
        "#!/bin/sh\necho hi\n",
    )
    .unwrap();
    let out = verify_plugin(&plugin).unwrap();
    // verify_plugin returns the CANONICAL runner path (symlinks resolved):
    // the containment proof is about this path, so it is the only safe
    // exec target.
    assert_eq!(
        out,
        plugin
            .root
            .canonicalize()
            .unwrap()
            .join("bin")
            .join("run.sh")
    );
}

fn write_plugin(dir: &Path, name: &str, runner: &str, caps: Vec<ToolCapability>) {
    let p = dir.join(name);
    std::fs::create_dir_all(&p).unwrap();
    let manifest = PluginManifest {
        name: name.to_string(),
        description: "test".into(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: "".into(),
        capabilities: caps,
        env_vars: vec![],
        runner: runner.into(),
        enabled: false,
    };
    std::fs::write(
        p.join("manifest.yaml"),
        serde_yaml::to_string(&manifest).unwrap(),
    )
    .unwrap();
    if !runner.is_empty() {
        std::fs::write(p.join(runner), "#!/bin/sh\necho hi\n").unwrap();
    }
}

fn plugin_with_runner(root: &Path, runner: &str) -> DiscoveredPlugin {
    // First-party ("bundled") location: skips the third-party approval gate
    // so these runner-safety tests don't depend on the approval store.
    let proot = root.join("bundled").join("t");
    std::fs::create_dir_all(&proot).unwrap();
    DiscoveredPlugin {
        manifest: PluginManifest {
            name: "t".into(),
            description: String::new(),
            version: "0.1.0".into(),
            sha: None,
            maintainer: String::new(),
            capabilities: vec![],
            env_vars: vec![],
            runner: runner.into(),
            enabled: false,
        },
        location: PluginLocation::User,
        root: proot,
    }
}
