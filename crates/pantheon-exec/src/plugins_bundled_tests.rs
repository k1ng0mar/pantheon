use super::*;

/// Scratch data dir for one test. Unique per call so tests stay
/// parallel-safe.
fn scratch_data_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "pantheon-bundled-plugins-{tag}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Build a synthetic registry entry. The sha256 is computed with the
/// same canonical function the build script uses, so the entry is
/// internally consistent; tests that need a mismatch tamper with it
/// afterwards.
fn test_entry(
    dir_name: &'static str,
    kind: &'static str,
    manifest_file: &'static str,
    files: &'static [BundledPluginFile],
) -> BundledPlugin {
    let pairs: Vec<(&str, &str)> = files.iter().map(|f| (f.path, f.content)).collect();
    let sha = plugin_hash::canonical_sha256(&pairs);
    BundledPlugin {
        dir_name,
        kind,
        manifest_file,
        sha256: Box::leak(sha.into_boxed_str()),
        files,
    }
}

const TOOL_MANIFEST: &str =
    "name: demo-tool\nversion: \"1.2.0\"\ndescription: A demo tool plugin.\n";
const HOOK_MANIFEST: &str =
        "name: demo-hook\nversion: \"0.4.0\"\ndescription: A demo hook plugin.\nprovides_hooks:\n  - pre_llm_call\n";

#[test]
fn seed_materializes_tool_plugin() {
    static FILES: &[BundledPluginFile] = &[
        BundledPluginFile {
            path: "manifest.yaml",
            content: TOOL_MANIFEST,
        },
        BundledPluginFile {
            path: "run.sh",
            content: "#!/bin/sh\necho hi\n",
        },
        BundledPluginFile {
            path: "lib/util.py",
            content: "# util\n",
        },
    ];
    let entry = test_entry("demo-tool", "tool", "manifest.yaml", FILES);
    let dd = scratch_data_dir("seed-tool");

    let seeded = seed_bundled_plugin(&dd, &entry).unwrap();
    assert_eq!(seeded.as_deref(), Some("demo-tool"));

    let root = dd.join("plugins").join("bundled").join("demo-tool");
    assert_eq!(
        std::fs::read_to_string(root.join("manifest.yaml")).unwrap(),
        TOOL_MANIFEST
    );
    assert_eq!(
        std::fs::read_to_string(root.join("lib").join("util.py")).unwrap(),
        "# util\n"
    );
    // Runner scripts land executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "run.sh is not executable");
    }
    // The embedded manifest parses: the seeded plugin is listable.
    let info = parse_bundled_info(&entry).unwrap();
    assert_eq!(info.name, "demo-tool");
    assert_eq!(info.kind, BundledPluginKind::Tool);
    assert_eq!(info.version, "1.2.0");
}

#[test]
fn seed_materializes_hook_plugin() {
    static FILES: &[BundledPluginFile] = &[
        BundledPluginFile {
            path: "plugin.yaml",
            content: HOOK_MANIFEST,
        },
        BundledPluginFile {
            path: "__init__.py",
            content: "# hook\n",
        },
    ];
    let entry = test_entry("demo-hook", "hook", "plugin.yaml", FILES);
    let dd = scratch_data_dir("seed-hook");

    let seeded = seed_bundled_plugin(&dd, &entry).unwrap();
    assert_eq!(seeded.as_deref(), Some("demo-hook"));

    // Hook plugins land in the extensions tree the runtime scans.
    let root = dd.join("extensions").join("bundled").join("demo-hook");
    assert_eq!(
        std::fs::read_to_string(root.join("plugin.yaml")).unwrap(),
        HOOK_MANIFEST
    );
    let info = parse_bundled_info(&entry).unwrap();
    assert_eq!(info.name, "demo-hook");
    assert_eq!(info.kind, BundledPluginKind::Hook);
    assert_eq!(info.version, "0.4.0");
    assert_eq!(info.description, "A demo hook plugin.");
}

#[test]
fn seed_never_overwrites_existing_dir() {
    static FILES: &[BundledPluginFile] = &[BundledPluginFile {
        path: "manifest.yaml",
        content: TOOL_MANIFEST,
    }];
    let entry = test_entry("demo-tool", "tool", "manifest.yaml", FILES);
    let dd = scratch_data_dir("seed-keep");

    let target = dd.join("plugins").join("bundled").join("demo-tool");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("sentinel.txt"), "operator edit").unwrap();

    let seeded = seed_bundled_plugin(&dd, &entry).unwrap();
    assert_eq!(seeded, None, "existing dir must be reported as kept");
    // Operator content untouched, plugin files not written.
    assert_eq!(
        std::fs::read_to_string(target.join("sentinel.txt")).unwrap(),
        "operator edit"
    );
    assert!(!target.join("manifest.yaml").exists());
}

#[test]
fn seed_hash_mismatch_fails_closed() {
    static FILES: &[BundledPluginFile] = &[BundledPluginFile {
        path: "manifest.yaml",
        content: TOOL_MANIFEST,
    }];
    let mut entry = test_entry("demo-tool", "tool", "manifest.yaml", FILES);
    // Tamper with the expected digest: the re-hash must catch it.
    entry.sha256 = Box::leak("0".repeat(64).into_boxed_str());
    let dd = scratch_data_dir("seed-mismatch");

    let err = seed_bundled_plugin(&dd, &entry).unwrap_err();
    assert!(
        err.cause.contains("sha256 mismatch"),
        "unexpected error: {err}"
    );
    // Fail closed: no half-materialized dir left behind.
    assert!(
        !dd.join("plugins")
            .join("bundled")
            .join("demo-tool")
            .exists(),
        "mismatched plugin dir was not deleted"
    );
}

#[test]
fn seed_rejects_unknown_kind() {
    static FILES: &[BundledPluginFile] = &[BundledPluginFile {
        path: "manifest.yaml",
        content: TOOL_MANIFEST,
    }];
    let entry = test_entry("demo-tool", "mystery", "manifest.yaml", FILES);
    let dd = scratch_data_dir("seed-kind");
    let err = seed_bundled_plugin(&dd, &entry).unwrap_err();
    assert_eq!(err.code, "PLUGIN_SEED_KIND");
}

#[test]
fn parse_bundled_info_rejects_bad_manifests() {
    // Invalid YAML.
    static BAD: &[BundledPluginFile] = &[BundledPluginFile {
        path: "manifest.yaml",
        content: "[[[not yaml",
    }];
    let entry = test_entry("bad", "tool", "manifest.yaml", BAD);
    assert!(parse_bundled_info(&entry).is_none());
    // Missing manifest file in the entry.
    static EMPTY: &[BundledPluginFile] = &[];
    let entry = test_entry("empty", "tool", "manifest.yaml", EMPTY);
    assert!(parse_bundled_info(&entry).is_none());
    // Empty name.
    static NONAME: &[BundledPluginFile] = &[BundledPluginFile {
        path: "manifest.yaml",
        content: "name: \"\"\n",
    }];
    let entry = test_entry("noname", "tool", "manifest.yaml", NONAME);
    assert!(parse_bundled_info(&entry).is_none());
    // Unknown kind token.
    static FILES: &[BundledPluginFile] = &[BundledPluginFile {
        path: "manifest.yaml",
        content: TOOL_MANIFEST,
    }];
    let entry = test_entry("x", "widget", "manifest.yaml", FILES);
    assert!(parse_bundled_info(&entry).is_none());
}

#[test]
fn write_plugin_enabled_round_trips_through_config() {
    let dd = scratch_data_dir("enable-roundtrip");
    // Enable stamps enabled + kind + version.
    write_plugin_enabled(&dd, "demo-tool", "tool", "1.2.0", true).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    let doc: toml::Value = text.parse().unwrap();
    assert_eq!(doc["plugins"]["demo-tool"]["enabled"].as_bool(), Some(true));
    assert_eq!(doc["plugins"]["demo-tool"]["kind"].as_str(), Some("tool"));
    assert_eq!(
        doc["plugins"]["demo-tool"]["version"].as_str(),
        Some("1.2.0")
    );
    // Disable flips the flag and keeps the stamps.
    write_plugin_enabled(&dd, "demo-tool", "tool", "1.2.0", false).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    let doc: toml::Value = text.parse().unwrap();
    assert_eq!(
        doc["plugins"]["demo-tool"]["enabled"].as_bool(),
        Some(false)
    );
    assert_eq!(doc["plugins"]["demo-tool"]["kind"].as_str(), Some("tool"));
    // Unrelated config survives the edit.
    std::fs::write(
        dd.join("config.toml"),
        "[goal]\nmax_iterations = 25\n[plugins]\n",
    )
    .unwrap();
    write_plugin_enabled(&dd, "other", "hook", "2.0", true).unwrap();
    let text = std::fs::read_to_string(dd.join("config.toml")).unwrap();
    let doc: toml::Value = text.parse().unwrap();
    assert_eq!(doc["goal"]["max_iterations"].as_integer(), Some(25));
    assert_eq!(doc["plugins"]["other"]["enabled"].as_bool(), Some(true));
}

#[test]
fn write_plugin_enabled_rejects_bad_slug() {
    let dd = scratch_data_dir("enable-badslug");
    let err = write_plugin_enabled(&dd, "../evil", "tool", "1", true).unwrap_err();
    assert_eq!(err.code, "PLUGIN_BAD_NAME");
    assert!(!dd.join("config.toml").exists());
}

#[test]
fn set_bundled_enabled_rejects_names_outside_registry() {
    // The build-time registry is empty in this checkout (content
    // workers add the plugin dirs), so every name is unknown.
    let dd = scratch_data_dir("enable-unknown");
    let err = set_bundled_enabled(&dd, "nope", true).unwrap_err();
    assert_eq!(err.code, "PLUGIN_UNKNOWN_PLUGIN");
}

#[test]
fn discover_plugins_scans_bundled_subdir() {
    let dd = scratch_data_dir("discover-bundled");
    // Hand-place a bundled tool plugin; seed is a no-op with the
    // empty build-time registry, so this exercises the scan alone.
    let dir = dd.join("plugins").join("bundled").join("demo-tool");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.yaml"), TOOL_MANIFEST).unwrap();

    let found = discover_plugins(&dd, &dd);
    let plugin = found
        .iter()
        .find(|p| p.manifest.name == "demo-tool")
        .expect("bundled subdir plugin not discovered");
    assert_eq!(plugin.location, PluginLocation::User);
    assert!(plugin.root.ends_with("plugins/bundled/demo-tool"));
}

#[test]
fn canonical_sha256_is_order_independent_and_content_sensitive() {
    let a = plugin_hash::canonical_sha256(&[("b.txt", "2"), ("a.txt", "1")]);
    let b = plugin_hash::canonical_sha256(&[("a.txt", "1"), ("b.txt", "2")]);
    assert_eq!(a, b, "hash must not depend on input order");
    let c = plugin_hash::canonical_sha256(&[("a.txt", "1"), ("b.txt", "3")]);
    assert_ne!(a, c, "hash must change with content");
    let d = plugin_hash::canonical_sha256(&[("a.txt", "1")]);
    assert_ne!(a, d, "hash must change with the file set");
    assert_eq!(a.len(), 64, "expected hex sha256");
}

#[test]
fn bundled_plugin_kind_tokens_round_trip() {
    assert_eq!(BundledPluginKind::Tool.as_str(), "tool");
    assert_eq!(BundledPluginKind::Hook.as_str(), "hook");
    assert_eq!(
        BundledPluginKind::parse("tool"),
        Some(BundledPluginKind::Tool)
    );
    assert_eq!(
        BundledPluginKind::parse("hook"),
        Some(BundledPluginKind::Hook)
    );
    assert_eq!(BundledPluginKind::parse("widget"), None);
}
