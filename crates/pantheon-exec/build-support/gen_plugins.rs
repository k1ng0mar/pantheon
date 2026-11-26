/* Bundled-plugin registry generator.

Walks `bundled-plugins/<name>/`, embeds every UTF-8 text file with
`include_str!`, and computes a sha256 over the (relative path, content) pairs.
Seeding (`pantheon_exec::plugins::seed_bundled_plugins`) re-hashes what landed
on disk against that digest and fails closed on mismatch.

A plugin dir holds either `manifest.yaml` (tool plugin, Pantheon tool-plugin
schema) or `plugin.yaml` (hook plugin, extensions shape). Neither = ignored,
both = skipped w/ a warning.

Add a dir to add a plugin. `__pycache__/`, `*.pyc` and dotfiles are dropped
silently - never content, warning about them every build would be noise.

NOTE: included by build.rs next to `plugin_hash.rs` (which provides
`canonical_sha256`), so no inner (`//!`) doc comments here. */

/* Which half of the plugin system a bundled plugin belongs to. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundledPluginKind {
    Tool,
    Hook,
}

impl BundledPluginKind {
    /* Lowercase token for the generated registry and `as_str()`. */
    pub fn as_str(self) -> &'static str {
        match self {
            BundledPluginKind::Tool => "tool",
            BundledPluginKind::Hook => "hook",
        }
    }

    /* manifest that declares this kind */
    pub fn manifest_file(self) -> &'static str {
        match self {
            BundledPluginKind::Tool => "manifest.yaml",
            BundledPluginKind::Hook => "plugin.yaml",
        }
    }
}

/* one plugin dir found on disk */
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledPluginDef {
    /* dir name under `bundled-plugins/` (also the seed target name) */
    pub dir_name: String,
    pub kind: BundledPluginKind,
    /* (relative path with `/` separators, absolute path, UTF-8 content) */
    pub files: Vec<BundledPluginFileDef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledPluginFileDef {
    pub rel: String,
    pub abs: std::path::PathBuf,
    pub content: String,
}

/* non-UTF8 / unreadable files, skipped w/ a build warning */
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedPluginFile {
    pub plugin: String,
    pub rel: String,
}

/* plugin-dir slug: same rule as `pantheon_exec::skills::valid_slug` (ASCII
alnum + `-`/`_`, 1..=64). build scripts can't depend on the crate they build,
so it's duplicated here. */
fn valid_plugin_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
}

/* content hash lives in `plugin_hash.rs`, shared between build.rs (what gets
embedded) and seed-time (what landed on disk). same canonical form both sides. */

/* find plugins under `root` (`crates/pantheon-exec/bundled-plugins`);
sorted by dir name so the registry is deterministic */
pub fn discover_bundled_plugins(
    root: &std::path::Path,
) -> (Vec<BundledPluginDef>, Vec<SkippedPluginFile>) {
    let mut plugins = Vec::new();
    let mut skipped = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return (plugins, skipped);
    };
    let mut dirs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for dir in dirs {
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        if !valid_plugin_slug(name) {
            eprintln!("bundled-plugins: skipping directory with invalid slug: {name:?}");
            continue;
        }
        let has_tool_manifest = dir.join("manifest.yaml").is_file();
        let has_hook_manifest = dir.join("plugin.yaml").is_file();
        let kind = match (has_tool_manifest, has_hook_manifest) {
            (true, true) => {
                eprintln!(
                    "bundled-plugins: '{name}' has both manifest.yaml and plugin.yaml; skipping (one manifest per plugin)"
                );
                continue;
            }
            (true, false) => BundledPluginKind::Tool,
            (false, true) => BundledPluginKind::Hook,
            (false, false) => continue,
        };
        let mut files = Vec::new();
        collect_plugin_text_files(&dir, "", name, &mut files, &mut skipped);
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        plugins.push(BundledPluginDef {
            dir_name: name.to_string(),
            kind,
            files,
        });
    }
    (plugins, skipped)
}

/* never embed build/test leftovers, even if a content worker ran tests in
the plugin dir: `__pycache__/`, `*.pyc`, dotfiles. skipped SILENTLY - no
warning, never content. non-UTF8 and symlinks are recorded instead since
those might be content the worker meant to ship. */
fn is_artifact_name(name: &str) -> bool {
    name == "__pycache__" || name.ends_with(".pyc") || name.starts_with('.')
}

/* walk `dir` for UTF-8 text; `rel_prefix` is the relative path so far ("" at top) */
fn collect_plugin_text_files(
    dir: &std::path::Path,
    rel_prefix: &str,
    plugin: &str,
    files: &mut Vec<BundledPluginFileDef>,
    skipped: &mut Vec<SkippedPluginFile>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if is_artifact_name(name) {
            continue;
        }
        // no symlinks: registry should be self-contained bytes, not pointers
        // into whoever's build machine
        if p.is_symlink() {
            skipped.push(SkippedPluginFile {
                plugin: plugin.to_string(),
                rel: join_rel(rel_prefix, name),
            });
            continue;
        }
        if p.is_dir() {
            collect_plugin_text_files(&p, &join_rel(rel_prefix, name), plugin, files, skipped);
        } else if p.is_file() {
            let rel = join_rel(rel_prefix, name);
            match std::fs::read(&p) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(content) => files.push(BundledPluginFileDef {
                        rel,
                        abs: p,
                        content,
                    }),
                    Err(_) => skipped.push(SkippedPluginFile {
                        plugin: plugin.to_string(),
                        rel,
                    }),
                },
                Err(_) => skipped.push(SkippedPluginFile {
                    plugin: plugin.to_string(),
                    rel,
                }),
            }
        }
    }
}

fn join_rel(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

/* render registry source. absolute `include_str!` paths -> content is read at
compile time, fresh each build. sha256 is over what's read here; seeding
re-hashes the materialized files with the same canonical form. */
pub fn render_plugin_registry_rs(defs: &[BundledPluginDef]) -> String {
    let mut out = String::from("// @generated by crates/pantheon-exec/build.rs - do not edit.\n");
    out.push_str(
        "pub struct BundledPluginFile {\n    pub path: &'static str,\n    pub content: &'static str,\n}\n",
    );
    out.push_str(
        "pub struct BundledPlugin {\n    pub dir_name: &'static str,\n    pub kind: &'static str,\n    pub manifest_file: &'static str,\n    pub sha256: &'static str,\n    pub files: &'static [BundledPluginFile],\n}\n",
    );
    out.push_str("pub fn bundled_plugins() -> Vec<BundledPlugin> {\n    vec![\n");
    for d in defs {
        let pairs: Vec<(&str, &str)> = d
            .files
            .iter()
            .map(|f| (f.rel.as_str(), f.content.as_str()))
            .collect();
        let sha = canonical_sha256(&pairs);
        out.push_str(&format!(
            "        BundledPlugin {{\n            dir_name: {:?},\n            kind: {:?},\n            manifest_file: {:?},\n            sha256: {:?},\n            files: &[\n",
            d.dir_name,
            d.kind.as_str(),
            d.kind.manifest_file(),
            sha,
        ));
        for f in &d.files {
            out.push_str(&format!(
                "                BundledPluginFile {{ path: {:?}, content: include_str!({:?}) }},\n",
                f.rel,
                f.abs.to_string_lossy(),
            ));
        }
        out.push_str("            ],\n        },\n");
    }
    out.push_str("    ]\n}\n");
    out
}
