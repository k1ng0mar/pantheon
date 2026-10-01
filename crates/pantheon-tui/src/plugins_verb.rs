//! `pantheon plugins`: list, enable/disable, install, and the interactive toggle.
//!
//! Four plugin sources feed one merged view:
//!
//! 1. The embedded bundled-plugin registry (`pantheon_exec::plugins`,
//!    tool and hook kinds) — toggled through the `[plugins]` config
//!    table via [`pantheon_exec::plugins::set_bundled_enabled`].
//! 2. The extensions bundled catalog (`pantheon_extensions`, e.g.
//!    `time-gap`) — toggled through the same `[plugins]` config table
//!    via [`pantheon_extensions::set_enabled`]. Both bundled sources
//!    share one enablement state: the config file wins over any
//!    manifest flag.
//! 3. Third-party tool plugins from
//!    [`pantheon_exec::plugins::discover_plugins`] — toggled by
//!    rewriting the manifest `enabled` flag in place (their gate is the
//!    approval store, enforced at spawn).
//! 4. Third-party hook plugins from the extensions dir — approval-gated;
//!    listed read-only here, toggled with
//!    `pantheon extensions approve <name>`.

use std::path::{Path, PathBuf};

/// How one row is toggled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toggle {
    /// Bundled plugin from the embedded registry (tool or hook):
    /// `[plugins.<name>]` via `pantheon_exec::plugins::set_bundled_enabled`.
    BundledRegistry,
    /// Hook plugin from the extensions catalog: `[plugins.<name>]` via
    /// `pantheon_extensions::set_enabled`.
    ExtensionsCatalog,
    /// Third-party tool plugin: manifest `enabled` flag in place.
    ManifestFlag,
    /// Third-party hook plugin: the approval store gates it; not
    /// toggleable from here.
    ApprovalGated,
}

/// One row of the merged plugin view.
#[derive(Debug, Clone)]
pub(crate) struct PluginRow {
    pub name: String,
    /// "tool" | "hook".
    pub kind: &'static str,
    pub version: String,
    pub description: String,
    pub bundled: bool,
    pub enabled: bool,
    pub toggle: Toggle,
}

/// Shorten a description to one line for table display.
fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() <= max {
        return flat;
    }
    let mut cut = max;
    while cut > 0 && !flat.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", flat[..cut].trim_end())
}

/// Is the bundled plugin `name` enabled? The `[plugins.<name>]` config
/// entry wins when present; when absent the bundled manifest's own
/// `enabled` flag (`manifest_default`) is the default. A missing or
/// unparsable config fails closed (disabled) — a typo must never
/// silently flip a plugin on.
fn config_enabled(data_dir: &Path, name: &str, manifest_default: bool) -> bool {
    pantheon_extensions::bundled::load_config(data_dir)
        .map(|c| pantheon_extensions::bundled::is_enabled_with_default(&c, name, manifest_default))
        .unwrap_or(false)
}

/// The merged plugin view: bundled (embedded registry + extensions
/// catalog) and third-party discovered plugins, sorted by name. The
/// embedded registry wins name collisions so a plugin that exists in
/// both catalogs renders once.
pub(crate) fn collect_rows(data_dir: &Path) -> Vec<PluginRow> {
    let mut rows: Vec<PluginRow> = Vec::new();
    let seen = |rows: &[PluginRow], name: &str| rows.iter().any(|r| r.name == name);

    // 1. Embedded bundled-plugin registry (tool + hook kinds).
    for info in pantheon_exec::plugins::bundled_plugin_infos() {
        rows.push(PluginRow {
            name: info.name.clone(),
            kind: info.kind.as_str(),
            version: info.version,
            description: info.description,
            bundled: true,
            enabled: config_enabled(data_dir, &info.name, info.enabled),
            toggle: Toggle::BundledRegistry,
        });
    }
    // 2. Extensions bundled catalog (e.g. time-gap).
    for p in pantheon_extensions::bundled_plugins() {
        if seen(&rows, &p.name) {
            continue;
        }
        rows.push(PluginRow {
            name: p.name.clone(),
            kind: p.kind.as_str(),
            version: p.version,
            description: p.description,
            bundled: true,
            enabled: config_enabled(data_dir, &p.name, p.enabled),
            toggle: Toggle::ExtensionsCatalog,
        });
    }
    // 3. Third-party tool plugins. (Discovery also best-effort seeds
    // the bundled registry first; bundled paths are skipped here —
    // they are covered by sources 1 and 2 above.)
    let project_root = std::env::current_dir().unwrap_or_else(|_| data_dir.to_path_buf());
    for p in pantheon_exec::plugins::discover_plugins(data_dir, &project_root) {
        if pantheon_exec::plugin_approval::is_bundled(&p) {
            continue;
        }
        if seen(&rows, &p.manifest.name) {
            continue;
        }
        rows.push(PluginRow {
            name: p.manifest.name.clone(),
            kind: "tool",
            version: p.manifest.version.clone(),
            description: p.manifest.description.clone(),
            bundled: false,
            enabled: p.manifest.enabled,
            toggle: Toggle::ManifestFlag,
        });
    }
    // 4. Third-party hook plugins: top-level dirs of the extensions dir
    // with a plugin.yaml, minus `bundled/`. Enabled = approved in the
    // approval store for the current dir hash (best-effort).
    let ext_dir = crate::terminal::ext_dir();
    if let Ok(entries) = std::fs::read_dir(&ext_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || !path.join("plugin.yaml").exists() {
                continue;
            }
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "bundled" || n.starts_with('.'))
            {
                continue;
            }
            let manifest =
                match pantheon_extensions::PluginManifest::load(&path.join("plugin.yaml")) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
            if seen(&rows, &manifest.name) {
                continue;
            }
            let enabled = pantheon_api::approval::dir_hash(&path)
                .map(|h| {
                    pantheon_api::approval::is_approved(
                        &ext_dir,
                        &manifest.name,
                        &manifest.version,
                        &h,
                    )
                })
                .unwrap_or(false);
            rows.push(PluginRow {
                name: manifest.name,
                kind: "hook",
                version: manifest.version,
                description: manifest.description,
                bundled: false,
                enabled,
                toggle: Toggle::ApprovalGated,
            });
        }
    }

    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Toggle one row. Returns a human confirmation on success.
pub(crate) fn apply_toggle(
    data_dir: &Path,
    row: &PluginRow,
    enabled: bool,
) -> Result<String, String> {
    match row.toggle {
        Toggle::BundledRegistry => {
            pantheon_exec::plugins::set_bundled_enabled(data_dir, &row.name, enabled)
                .map_err(|e| e.to_string())?;
        }
        Toggle::ExtensionsCatalog => {
            pantheon_extensions::set_enabled(data_dir, &row.name, enabled)
                .map_err(|e| e.to_string())?;
        }
        Toggle::ManifestFlag => {
            let project_root = std::env::current_dir().unwrap_or_else(|_| data_dir.to_path_buf());
            let found = pantheon_exec::plugins::discover_plugins(data_dir, &project_root);
            let plugin = found
                .iter()
                .find(|p| {
                    p.manifest.name == row.name && !pantheon_exec::plugin_approval::is_bundled(p)
                })
                .ok_or_else(|| format!("plugin '{}' is no longer discovered", row.name))?;
            pantheon_exec::plugins::set_enabled(plugin, enabled).map_err(|e| e.to_string())?;
        }
        Toggle::ApprovalGated => {
            return Err(format!(
                "third-party hook plugin '{}' is approval-gated; use `pantheon extensions approve {}`",
                row.name, row.name
            ));
        }
    }
    Ok(format!(
        "{} {}",
        if enabled { "enabled" } else { "disabled" },
        row.name
    ))
}

fn print_list(rows: &[PluginRow]) {
    if rows.is_empty() {
        println!("no plugins installed");
        return;
    }
    println!(
        "  {:<26} {:<4} {:<10} {:<8}  {}",
        "NAME", "KIND", "VERSION", "STATUS", "DESCRIPTION"
    );
    for r in rows {
        let status = if r.enabled { "enabled" } else { "disabled" };
        let tag = if r.bundled { " [bundled]" } else { "" };
        println!(
            "  {:<26} {:<4} {:<10} {:<8}  {}{}",
            r.name,
            r.kind,
            r.version,
            status,
            one_line(&r.description, 60),
            tag
        );
    }
}

fn cmd_list(data_dir: &Path) {
    print_list(&collect_rows(data_dir));
}

fn plugins_help() {
    eprintln!("usage: pantheon plugins <list|enable|disable|install> ...");
    eprintln!("  (no subcommand)      interactive toggle screen (TTY) or list (piped)");
    eprintln!("  list                 bundled + third-party plugins, with kind/version/status");
    eprintln!("  enable <name>        enable a plugin");
    eprintln!("  disable <name>       disable a plugin");
    eprintln!("  install <name>       install a plugin from the catalog by name");
    eprintln!();
    eprintln!("bundled plugins toggle through [plugins.<name>] in config.toml;");
    eprintln!("third-party tool plugins toggle their manifest flag in place;");
    eprintln!("third-party hook plugins are approval-gated");
    eprintln!("(`pantheon extensions approve <name>`).");
}

/// Entry point called from the verb dispatch in `terminal::run`.
pub fn cmd_plugins(args: &[String]) {
    let dd = crate::terminal::data_dir();
    match args.get(2).map(String::as_str) {
        Some("list") => cmd_list(&dd),
        Some(cmd @ ("enable" | "disable")) => {
            if args.len() < 4 {
                eprintln!("usage: pantheon plugins {cmd} <name>");
                std::process::exit(2);
            }
            let name = &args[3];
            let enabled = cmd == "enable";
            let rows = collect_rows(&dd);
            let row = rows.iter().find(|r| r.name == *name).unwrap_or_else(|| {
                eprintln!("plugins {cmd}: no plugin named '{name}'");
                std::process::exit(1);
            });
            match apply_toggle(&dd, row, enabled) {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("plugins {cmd}: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("install") => {
            if args.len() < 4 {
                eprintln!("usage: pantheon plugins install <name>");
                eprintln!(
                    "catalog plugins: {}",
                    pantheon_exec::plugins::catalog_names().join(", ")
                );
                std::process::exit(2);
            }
            let name = &args[3];
            let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            pantheon_exec::plugins::install_catalog(name, &dd, &project_root).unwrap_or_else(|e| {
                eprintln!("plugins install: {e}");
                std::process::exit(1);
            });
            println!("installed {name}");
        }
        Some("--help") | Some("-h") => {
            plugins_help();
            std::process::exit(0);
        }
        Some(other) => {
            eprintln!("plugins: unknown subcommand {other:?}");
            plugins_help();
            std::process::exit(2);
        }
        // Bare `pantheon plugins`: the interactive toggle screen on a
        // TTY (Hermes-style), plain list when piped.
        None => {
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                crate::plugin_toggle::run_toggle_screen(&dd);
            } else {
                cmd_list(&dd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch data dir for one test. Unique per call so tests stay
    /// parallel-safe.
    fn scratch_data_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-plugins-verb-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_tool_plugin(dd: &Path, name: &str, enabled: bool) {
        let dir = dd.join("plugins").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.yaml"),
            format!("name: {name}\nversion: \"1.0\"\ndescription: Test plugin {name}.\nenabled: {enabled}\n"),
        )
        .unwrap();
    }

    #[test]
    fn one_line_truncates_and_flattens() {
        assert_eq!(one_line("hello", 60), "hello");
        assert_eq!(one_line("a\nb  c", 60), "a b c");
        let long = "x".repeat(100);
        let cut = one_line(&long, 60);
        assert!(cut.len() <= 61 + 3, "unexpected length: {}", cut.len());
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn collect_rows_lists_bundled_and_third_party() {
        let dd = scratch_data_dir("rows");
        // A third-party tool plugin on disk.
        write_tool_plugin(&dd, "acme-tool", true);

        let rows = collect_rows(&dd);
        // The extensions catalog's time-gap entry is always present.
        let time_gap = rows
            .iter()
            .find(|r| r.name == "time-gap")
            .expect("time-gap missing from plugin list");
        assert_eq!(time_gap.kind, "hook");
        assert!(time_gap.bundled);
        assert_eq!(time_gap.toggle, Toggle::ExtensionsCatalog);
        assert!(!time_gap.enabled, "bundled plugins ship disabled");
        // The third-party tool plugin shows with its manifest state.
        let acme = rows
            .iter()
            .find(|r| r.name == "acme-tool")
            .expect("third-party plugin missing from plugin list");
        assert_eq!(acme.kind, "tool");
        assert!(!acme.bundled);
        assert!(acme.enabled);
        assert_eq!(acme.toggle, Toggle::ManifestFlag);
        // Sorted by name.
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn collect_rows_dedups_bundled_sources() {
        // time-gap exists in the extensions catalog; the merged view
        // must contain it exactly once even though two bundled sources
        // are merged.
        let dd = scratch_data_dir("dedup");
        let rows = collect_rows(&dd);
        assert_eq!(rows.iter().filter(|r| r.name == "time-gap").count(), 1);
    }

    #[test]
    fn enable_disable_round_trips_through_config() {
        let dd = scratch_data_dir("toggle");
        // time-gap is an extensions-catalog hook plugin: toggling goes
        // through the [plugins] config table.
        let rows = collect_rows(&dd);
        let row = rows.iter().find(|r| r.name == "time-gap").unwrap();

        apply_toggle(&dd, row, true).unwrap();
        let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
        let doc: toml::Value = text.parse().unwrap();
        assert_eq!(doc["plugins"]["time-gap"]["enabled"].as_bool(), Some(true));

        let rows = collect_rows(&dd);
        let row = rows.iter().find(|r| r.name == "time-gap").unwrap();
        assert!(row.enabled, "list must reflect the config write");

        apply_toggle(&dd, row, false).unwrap();
        let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
        let doc: toml::Value = text.parse().unwrap();
        assert_eq!(doc["plugins"]["time-gap"]["enabled"].as_bool(), Some(false));
    }

    #[test]
    fn toggle_manifest_flag_for_third_party_tool() {
        let dd = scratch_data_dir("manifest-toggle");
        write_tool_plugin(&dd, "acme-tool", false);
        let rows = collect_rows(&dd);
        let row = rows.iter().find(|r| r.name == "acme-tool").unwrap();

        apply_toggle(&dd, row, true).unwrap();
        // The manifest flag was rewritten in place.
        let rows = collect_rows(&dd);
        let row = rows.iter().find(|r| r.name == "acme-tool").unwrap();
        assert!(row.enabled);
        apply_toggle(&dd, row, false).unwrap();
        let rows = collect_rows(&dd);
        let row = rows.iter().find(|r| r.name == "acme-tool").unwrap();
        assert!(!row.enabled);
    }

    #[test]
    fn toggle_unknown_name_is_an_error() {
        let dd = scratch_data_dir("toggle-unknown");
        let row = PluginRow {
            name: "nope".to_string(),
            kind: "tool",
            version: String::new(),
            description: String::new(),
            bundled: false,
            enabled: false,
            toggle: Toggle::BundledRegistry,
        };
        let err = apply_toggle(&dd, &row, true).unwrap_err();
        assert!(err.contains("not a bundled plugin"), "unexpected: {err}");
    }

    #[test]
    fn toggle_approval_gated_hook_is_rejected() {
        let dd = scratch_data_dir("toggle-gated");
        let row = PluginRow {
            name: "acme-hook".to_string(),
            kind: "hook",
            version: "1.0".to_string(),
            description: String::new(),
            bundled: false,
            enabled: false,
            toggle: Toggle::ApprovalGated,
        };
        let err = apply_toggle(&dd, &row, true).unwrap_err();
        assert!(err.contains("approval-gated"), "unexpected: {err}");
    }
}
