//! Tests for `pantheon_exec::plugins::tests` — sibling file so sources stay test-free.
use super::*;
use tempfile::tempdir;

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

// ---- manifest runner path traversal regression tests ----

fn plugin_with_runner(root: &Path, runner: &str) -> DiscoveredPlugin {
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
        root: root.to_path_buf(),
    }
}

#[test]
fn plugin_install_dir_rejects_bad_name() {
    let d = tempdir().unwrap();
    let long = "x".repeat(65);
    for bad in ["../evil", "..", "/abs", "a/b", "", long.as_str()] {
        let err = plugin_install_dir(d.path(), bad).unwrap_err();
        assert_eq!(err.code, "PLUGIN_BAD_NAME", "{bad:?}");
    }
    assert!(!d.path().join("evil").exists(), "rejection must precede any write");
}

#[test]
fn plugin_install_dir_stays_under_plugins_root() {
    let d = tempdir().unwrap();
    let dest = plugin_install_dir(d.path(), "good-name_2").unwrap();
    assert!(
        dest.ends_with("plugins/good-name_2"),
        "unexpected dest: {}",
        dest.display()
    );
}
