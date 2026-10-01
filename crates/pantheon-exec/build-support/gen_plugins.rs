/* Bundled-plugin registry generator.

Walks `bundled-plugins/<name>/`, collecting every UTF-8 text file,
and renders the Rust registry source with `include_str!` embeddings
plus a per-plugin sha256 over the (relative path, content) pairs.
Seeding (`pantheon_exec::plugins::seed_bundled_plugins`) re-hashes the
materialized directory against the embedded digest and fails closed
on mismatch.

Each plugin directory holds EITHER `manifest.yaml` (tool plugin,
Pantheon tool-plugin schema) OR `plugin.yaml` (hook plugin,
extensions shape). Directories with neither are ignored; directories
with both are skipped with a warning.

Content workers add plugins by adding directories — no code changes,
no per-plugin hand edits. Only UTF-8 text files are embedded; anything
else is reported as skipped so a stray binary never breaks the build.
Build/test artifacts (`__pycache__/`, `*.pyc`, dotfiles) are excluded
silently instead — they are never content, so warning about them
would be noise.

NOTE: this file is `include!`d by build.rs (alongside `plugin_hash.rs`,
which provides `canonical_sha256`), so it must not start with inner
(`//!`) doc comments. */

/* Which half of the plugin system a bundled plugin belongs to. */
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundledPluginKind {
    Tool,
    Hook,
}

impl BundledPluginKind {
    /* Stable lowercase token, also used in the generated registry. */
    pub fn as_str(self) -> &'static str {
        match self {
            BundledPluginKind::Tool => "tool",
            BundledPluginKind::Hook => "hook",
        }
    }

    /* The manifest file that declares this kind. */
    pub fn manifest_file(self) -> &'static str {
        match self {
            BundledPluginKind::Tool => "manifest.yaml",
            BundledPluginKind::Hook => "plugin.yaml",
        }
    }
}

/* One bundled plugin found on disk. */
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledPluginDef {
    /* Directory name under `bundled-plugins/` (also the seed target name). */
    pub dir_name: String,
    pub kind: BundledPluginKind,
    /* (relative path with `/` separators, absolute path, UTF-8 content). */
    pub files: Vec<BundledPluginFileDef>,
}

/* One embedded text file. */
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledPluginFileDef {
    pub rel: String,
    pub abs: std::path::PathBuf,
    pub content: String,
}

/* Non-UTF8 files met during the walk: skipped with a build warning. */
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedPluginFile {
    pub plugin: String,
    pub rel: String,
}

/* Plugin-dir slug rule, mirroring `pantheon_exec::skills::valid_slug`
(ASCII alphanumerics plus `-`/`_`, 1..=64). The build script cannot
depend on the crate it builds, so the rule is duplicated here. */
fn valid_plugin_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
}

/* Canonical content hash over (relative path, content) pairs. Lives in
`plugin_hash.rs`, shared between build.rs (hash of what gets embedded)
and the seed-time re-hash in `pantheon_exec::plugins` (hash of what
landed on disk) — see that file for the canonical form. */

/* Discover bundled plugins under `root`
(`crates/pantheon-exec/bundled-plugins`). Returns `(plugins,
skipped_files)`, plugins sorted by dir name for a deterministic
registry. */
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

/* Build and test artifacts that must never enter the embedded bundle,
even if a content worker runs a test suite inside the plugin dir:
`__pycache__/` bytecode caches, `*.pyc` files, and dotfiles/dotdirs.
These are skipped SILENTLY — not recorded as skipped, no build
warning — because they are never plugin content; warning about them
would just be noise on every build. (Non-UTF8 files and symlinks,
by contrast, are recorded and warned about, since those may be
content the worker intended to ship.) */
fn is_artifact_name(name: &str) -> bool {
    name == "__pycache__" || name.ends_with(".pyc") || name.starts_with('.')
}

/* Recursively collect UTF-8 text files under `dir`; `rel_prefix` is the
already-accumulated relative path ("" at the top). */
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
        // Symlinks are never embedded: the registry must be self-contained
        // bytes, not pointers into the build machine's filesystem.
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

/* Render the registry Rust source. Each file is embedded with
`include_str!` at an ABSOLUTE path, so content is read at crate
compile time (fresh on every build) rather than serialized into the
generated source as escaped literals. The sha256 is computed over the
contents read here at build time; seeding re-hashes the materialized
files with the same canonical form. */
pub fn render_plugin_registry_rs(defs: &[BundledPluginDef]) -> String {
    let mut out = String::from("// @generated by crates/pantheon-exec/build.rs — do not edit.\n");
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
