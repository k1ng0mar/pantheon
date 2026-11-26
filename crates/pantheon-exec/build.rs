//! Generate the bundled-skills and bundled-plugins registries.
//!
//! Walks `bundled-skills/*/` and `bundled-plugins/*/` and embeds text files
//! via `include_str!` into registries in OUT_DIR. Add a directory to add a
//! skill or plugin; no hand-edited registry entries.
//!
//! Logic lives in `build-support/gen.rs` etc, shared with unit tests via
//! `include!` (build scripts can't depend on the crate they're building).

include!("build-support/gen.rs");
include!("build-support/plugin_hash.rs");
include!("build-support/gen_plugins.rs");

fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.join("bundled-skills");
    // cargo watches the dir, so any skill edit re-runs this
    println!("cargo:rerun-if-changed={}", root.display());
    let (defs, skipped) = discover_bundled_skills(&root);
    for s in &skipped {
        println!(
            "cargo:warning=bundled skill '{}': skipping non-UTF8 file '{}' (only text files are embedded)",
            s.skill, s.rel
        );
    }
    let src = render_registry_rs(&defs);
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap())
        .join("bundled_skills_registry.rs");
    std::fs::write(&out, src).unwrap();

    // --- bundled plugins section (do not touch the skills section above) ---
    let plugins_root = manifest_dir.join("bundled-plugins");
    println!("cargo:rerun-if-changed={}", plugins_root.display());
    let (pdefs, pskipped) = discover_bundled_plugins(&plugins_root);
    for s in &pskipped {
        println!(
            "cargo:warning=bundled plugin '{}': skipping non-UTF8/symlink file '{}' (only text files are embedded)",
            s.plugin, s.rel
        );
    }
    let psrc = self::render_plugin_registry_rs(&pdefs);
    let pout = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap())
        .join("bundled_plugins_registry.rs");
    std::fs::write(&pout, psrc).unwrap();
}
