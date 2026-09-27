//! Tests for detect / analyze / plan / provenance — the read-only half.
//!
//! Pure parsing/mapping tests stay here. The detect/classify tests on
//! fixture trees moved to `eval/tests/migration_detect.rs`; the
//! `backup_root_lives_under_the_data_dir` tautology was deleted.
//!
//! `item_name_uses_dir_name_or_file_stem` also stays: `item_name` is a
//! private helper not reachable through the public API.
use super::*;
use std::path::PathBuf;

#[test]
fn source_kind_parses_aliases() {
    assert_eq!(SourceKind::parse("hermes"), Some(SourceKind::Hermes));
    assert_eq!(SourceKind::parse("OpenClaw"), Some(SourceKind::OpenClaw));
    assert_eq!(SourceKind::parse("omp"), Some(SourceKind::Omp));
    assert_eq!(SourceKind::parse("oh-my-pi"), Some(SourceKind::Omp));
    assert_eq!(SourceKind::parse("pi"), Some(SourceKind::Omp));
    assert_eq!(SourceKind::parse("nope"), None);
}

// ---------------------------------------------------------------------------
// secrets
// ---------------------------------------------------------------------------

#[test]
fn secret_paths_are_never_importable() {
    for n in [".env", "auth.json", "broker.token", "google_token.json"] {
        assert!(is_secret_path(Path::new(n)), "{n} must be a secret");
    }
    assert!(is_secret_path(Path::new("x/agent.db")));
    assert!(is_secret_path(Path::new("x/id_ed25519")));
    assert!(is_secret_path(Path::new("x/key.pem")));
    assert!(!is_secret_path(Path::new("SKILL.md")));
    assert!(!is_secret_path(Path::new("plugin.yaml")));
}

#[test]
fn config_files_that_may_hold_keys_are_flagged() {
    assert!(config_may_hold_keys(Path::new("models.yml")));
    assert!(config_may_hold_keys(Path::new("config.yaml")));
    assert!(!config_may_hold_keys(Path::new("SKILL.md")));
}

#[test]
fn targets_respect_ext_dir_override() {
    let t = Targets::new(PathBuf::from("/d"), PathBuf::from("/e"));
    assert_eq!(t.dir_for(ItemKind::Skill), Some(PathBuf::from("/d/skills")));
    assert_eq!(
        t.dir_for(ItemKind::Extension),
        Some(PathBuf::from("/e")),
        "extensions honour PANTHEON_EXT_DIR"
    );
    assert_eq!(t.dir_for(ItemKind::Secret), None);
    assert_eq!(t.dir_for(ItemKind::Memory), None);
}

#[test]
fn item_name_uses_dir_name_or_file_stem() {
    let d = Detected {
        kind: ItemKind::Skill,
        path: "/src/skills/demo".into(),
        mappable: true,
        note: String::new(),
    };
    assert_eq!(item_name(&d), "demo");
    let f = Detected {
        kind: ItemKind::Rule,
        path: "/src/rules/style.md".into(),
        mappable: true,
        note: String::new(),
    };
    assert_eq!(item_name(&f), "style");
}
