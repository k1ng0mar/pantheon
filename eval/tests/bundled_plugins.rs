//! Behavioral tests for the bundled plugin catalog and the single
//! enablement state (`pantheon-extensions::bundled`).
//!
//! Policy: only small deterministic unit tests live beside the code.
//! Everything behavioral - filesystem, config round-trips - lives here
//! and runs via `cargo test -p pantheon-eval`.

use pantheon_api::capability::{Capability, Policy};
use pantheon_api::config::Config;
use pantheon_extensions::bundled;
use pantheon_tools::builtins::{register_builtins_with, BuiltinOptions};
use pantheon_tools::tools::ToolRegistry;
use tempfile::tempdir;

#[test]
fn catalog_matches_the_vendored_manifest() {
    let plugins = bundled::bundled_plugins();
    assert_eq!(plugins.len(), 1, "add a row only with a vendored manifest");
    let p = &plugins[0];
    assert_eq!(p.name, "time-gap");
    assert_eq!(p.kind, bundled::PluginKind::Hook);
    // Parsed from vendor/time-gap-pantheon/plugin.yaml at compile time.
    let manifest: pantheon_extensions::PluginManifest =
        serde_yaml::from_str(include_str!("../../vendor/time-gap-pantheon/plugin.yaml")).unwrap();
    assert_eq!(p.version, manifest.version);
    assert_eq!(p.description, manifest.description);
}

#[test]
fn set_enabled_writes_the_config_entry() {
    let d = tempdir().unwrap();
    bundled::set_enabled(d.path(), "time-gap", true).unwrap();
    let text = std::fs::read_to_string(d.path().join("config.toml")).unwrap();
    let doc: toml::Value = text.parse().unwrap();
    let entry = &doc["plugins"]["time-gap"];
    assert_eq!(entry["enabled"].as_bool(), Some(true));
    assert_eq!(entry["kind"].as_str(), Some("hook"));
    assert_eq!(entry["version"].as_str(), Some("2.1.0"));
    // The config struct reads it back as enabled.
    let config = Config::load(d.path()).unwrap();
    assert!(config.plugin_enabled("time-gap"));
    assert!(bundled::is_enabled(&config, "time-gap"));
}

#[test]
fn set_enabled_preserves_unrelated_config() {
    let d = tempdir().unwrap();
    std::fs::write(
        d.path().join("config.toml"),
        "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n\n[tools]\nterminal = true\n",
    )
    .unwrap();
    bundled::set_enabled(d.path(), "time-gap", true).unwrap();
    let config = Config::load(d.path()).unwrap();
    assert_eq!(config.model.as_ref().unwrap().provider, "openai");
    assert!(config.plugin_enabled("time-gap"));
    // Disabling keeps the entry (self-describing) and flips the flag.
    bundled::set_enabled(d.path(), "time-gap", false).unwrap();
    let config = Config::load(d.path()).unwrap();
    assert!(!config.plugin_enabled("time-gap"));
    let text = std::fs::read_to_string(d.path().join("config.toml")).unwrap();
    let doc: toml::Value = text.parse().unwrap();
    assert_eq!(doc["plugins"]["time-gap"]["kind"].as_str(), Some("hook"));
}

#[test]
fn unknown_plugin_names_are_rejected() {
    let d = tempdir().unwrap();
    let err = bundled::set_enabled(d.path(), "evil-plugin", true).unwrap_err();
    assert_eq!(err.code, "PLUGIN_UNKNOWN_PLUGIN");
    // Nothing was written.
    assert!(!d.path().join("config.toml").exists());
}

#[test]
fn bundled_plugins_ship_disabled() {
    // No config at all: every catalog plugin disabled (fail closed).
    let d = tempdir().unwrap();
    let config = bundled::load_config(d.path());
    assert!(config.is_none());
    let empty = Config::default();
    for p in bundled::bundled_plugins() {
        assert!(!bundled::is_enabled(&empty, &p.name));
    }
    assert!(!empty.plugin_enabled("time-gap"));
    assert!(!empty.plugin_enabled("not-a-plugin"));
}

#[test]
fn missing_enabled_flag_means_disabled() {
    let d = tempdir().unwrap();
    std::fs::write(
        d.path().join("config.toml"),
        "[plugins.time-gap]\nkind = \"hook\"\n",
    )
    .unwrap();
    let config = Config::load(d.path()).unwrap();
    assert!(!bundled::is_enabled(&config, "time-gap"));
}

#[test]
fn malformed_config_fails_closed_without_exiting() {
    let d = tempdir().unwrap();
    std::fs::write(d.path().join("config.toml"), "this is [not valid toml").unwrap();
    // Must not process-exit (unlike Config::load_or_report): a typo fails
    // closed to disabled.
    assert!(bundled::load_config(d.path()).is_none());
}

fn registry_with_data_dir(d: &std::path::Path) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            data_dir: Some(d.to_path_buf()),
            ..Default::default()
        },
    );
    reg
}

#[test]
fn enable_plugin_tool_is_approval_gated() {
    let d = tempdir().unwrap();
    let reg = registry_with_data_dir(d.path());
    assert!(reg.names().contains(&"enable_plugin".to_string()));
    // The tool requires the PluginEnable capability, which the coder
    // policy marks Approval: the agent loop parks for a human before it
    // can run. Never silent.
    assert_eq!(
        reg.capability_of("enable_plugin"),
        Some(Capability::PluginEnable)
    );
    assert_eq!(
        Policy::coder().check(&Capability::PluginEnable),
        pantheon_api::capability::Decision::Approval
    );
}

#[test]
fn enable_plugin_tool_enables_only_catalog_plugins() {
    let d = tempdir().unwrap();
    let reg = registry_with_data_dir(d.path());
    // Arbitrary names are refused: no agent path to install plugins.
    let err = reg
        .execute("enable_plugin", r#"{"name":"evil"}"#)
        .unwrap_err();
    assert_eq!(err.code, "TOOL_UNKNOWN_PLUGIN");
    assert!(!d.path().join("config.toml").exists());
    // A catalog plugin flips the single enablement state.
    let out = reg
        .execute("enable_plugin", r#"{"name":"time-gap"}"#)
        .unwrap();
    assert!(out.contains("time-gap"), "unexpected output: {out}");
    let config = Config::load(d.path()).unwrap();
    assert!(config.plugin_enabled("time-gap"));
}

#[test]
fn enable_plugin_tool_not_registered_without_data_dir() {
    let mut reg = ToolRegistry::new();
    register_builtins_with(&mut reg, BuiltinOptions::default());
    assert!(
        !reg.names().contains(&"enable_plugin".to_string()),
        "the tool must not be advertised when it cannot write the config"
    );
}

#[test]
fn seed_materializes_vendored_files_inert() {
    let d = tempdir().unwrap();
    let ext_dir = d.path().join("extensions");
    let seeded = bundled::seed(&ext_dir).unwrap();
    assert_eq!(seeded, vec!["time-gap".to_string()]);
    let target = ext_dir.join("bundled").join("time-gap");
    assert!(target.join("plugin.yaml").exists());
    let init = std::fs::read_to_string(target.join("__init__.py")).unwrap();
    assert!(
        init.contains("register"),
        "vendored hook source must land intact"
    );
    // Seeding never overwrites an existing install.
    std::fs::write(target.join("operator-note.txt"), "mine").unwrap();
    let seeded_again = bundled::seed(&ext_dir).unwrap();
    assert!(seeded_again.is_empty(), "second seed must be a no-op");
    assert!(target.join("operator-note.txt").exists());
}

#[test]
fn seeded_plugin_loads_only_when_config_enabled() {
    use pantheon_extensions::{ExtensionManager, RunnerConfig};

    // Disabled (no config entry): seeded files sit inert, nothing loads.
    let d = tempdir().unwrap();
    let ext_dir = d.path().join("extensions");
    bundled::seed(&ext_dir).unwrap();
    let mut mgr = ExtensionManager::new(RunnerConfig::default());
    mgr.load_dir(&ext_dir).unwrap();
    assert!(
        !mgr.names().contains(&"time-gap".to_string()),
        "bundled plugins ship disabled; seeding must not enable"
    );

    // Enabled via the shared config state: the same manager loads it.
    // The manager derives the data dir as the extensions dir's parent,
    // so config.toml sits next to extensions/, the real layout.
    let d2 = tempdir().unwrap();
    std::fs::write(
        d2.path().join("config.toml"),
        "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n\n[plugins.time-gap]\nenabled = true\n",
    )
    .unwrap();
    let ext_dir2 = d2.path().join("extensions");
    bundled::seed(&ext_dir2).unwrap();
    let mut mgr2 = ExtensionManager::new(RunnerConfig::default());
    mgr2.load_dir(&ext_dir2).unwrap();
    assert!(
        mgr2.names().contains(&"time-gap".to_string()),
        "an enabled catalog entry must load from the seeded copy"
    );
}
