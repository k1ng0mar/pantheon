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
fn catalog_is_empty_since_time_gap_went_native() {
    // The bundled catalog's one entry (the Python time-gap hook) was
    // removed: native temporal awareness (`[temporal]`, on by default)
    // covers it in-process. The catalog machinery stays for the next
    // bundled plugin, so this pin flips the moment one ships.
    let plugins = bundled::bundled_plugins();
    assert!(
        plugins.is_empty(),
        "catalog expected empty; update this test alongside the new entry: {plugins:?}"
    );
}

#[test]
fn set_enabled_writes_nothing_without_catalog_entries() {
    // With an empty catalog every name is unknown, so no config is
    // written. The write path itself is exercised the moment a catalog
    // row returns; until then this is the observable contract.
    let d = tempdir().unwrap();
    let err = bundled::set_enabled(d.path(), "time-gap", true).unwrap_err();
    assert_eq!(err.code, "PLUGIN_UNKNOWN_PLUGIN");
    assert!(!d.path().join("config.toml").exists());
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
fn enable_plugin_tool_enables_only_bundled_names() {
    let d = tempdir().unwrap();
    let reg = registry_with_data_dir(d.path());
    // Arbitrary names are refused: no agent path to install plugins.
    let err = reg
        .execute("enable_plugin", r#"{"name":"evil"}"#)
        .unwrap_err();
    assert_eq!(err.code, "TOOL_UNKNOWN_PLUGIN");
    assert!(!d.path().join("config.toml").exists());
    // The vendored extensions catalog is empty (time-gap went native via
    // [temporal]), so a former catalog name is refused exactly like any
    // other unknown name.
    let err = reg
        .execute("enable_plugin", r#"{"name":"time-gap"}"#)
        .unwrap_err();
    assert_eq!(err.code, "TOOL_UNKNOWN_PLUGIN");
    // An embedded-registry plugin (compiled into the binary) enables
    // through the same single [plugins.<name>] state.
    let out = reg
        .execute("enable_plugin", r#"{"name":"noisegate"}"#)
        .unwrap();
    assert!(out.contains("noisegate"), "unexpected output: {out}");
    let config = Config::load(d.path()).unwrap();
    assert!(config.plugin_enabled("noisegate"));
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
fn seed_materializes_nothing_for_an_empty_catalog() {
    let d = tempdir().unwrap();
    let ext_dir = d.path().join("extensions");
    let seeded = bundled::seed(&ext_dir).unwrap();
    assert!(seeded.is_empty());
    assert!(
        !ext_dir.join("bundled").exists(),
        "an empty catalog must not create dirs"
    );
}
