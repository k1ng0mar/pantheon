//! Generate the bundled-skills registry.
//!
//! Walks `bundled-skills/*/` and embeds `SKILL.md` + `scripts/**` +
//! `references/**` (text files) via `include_str!` into a generated
//! registry source in OUT_DIR. Content workers add skills by adding
//! directories — no per-skill hand edits, no registry code changes.
//!
//! The walk/render logic lives in `build-support/gen.rs`, shared with
//! unit tests through `include!` (a build script cannot depend on the
//! crate it is building).

include!("build-support/gen.rs");
include!("build-support/plugin_hash.rs");
include!("build-support/gen_plugins.rs");

fn main() {
    let manifest_dir = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest_dir.join("bundled-skills");
    // Re-run when any bundled skill changes (cargo watches the dir).
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
    // Walks `bundled-plugins/*/` and embeds every text file plus a sha256
    // per plugin into a generated registry in OUT_DIR. Content workers add
    // plugins by adding directories — no per-plugin hand edits.
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
