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
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].manifest.name, "demo");
    assert_eq!(found[0].location, PluginLocation::User);
}

#[test]
fn discover_ignores_dirs_without_manifest() {
    let d = tempdir().unwrap();
    let plugins = d.path().join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::create_dir_all(plugins.join("no-manifest")).unwrap();
    assert!(discover_plugins(&plugins, d.path()).is_empty());
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
    let err = verify_plugin(&found[0]).unwrap_err();
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
    let err = verify_plugin(&found[0]).unwrap_err();
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
    let err = verify_plugin(&found[0]).unwrap_err();
    assert_eq!(err.code, "PLUGIN_MISSING_ENV");
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
    let plugin = &found[0];
    // default policy denies everything.
    let policy = pantheon_api::capability::Policy::default();
    assert!(!tool_allowed(plugin, "safe_read", &policy));
    let coder = pantheon_api::capability::Policy::coder();
    assert!(tool_allowed(plugin, "safe_read", &coder));
}
