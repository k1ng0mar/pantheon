//! Bundled skill seeding tests. All run against a temp data dir through the
//! public `seed_bundled_skills` entry point plus real discovery, so the
//! assertions cover the production path, not a near-identical impl.

use super::*;
use std::fs;

fn data() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn skills_dir(t: &tempfile::TempDir) -> std::path::PathBuf {
    t.path().join("skills")
}

fn skill_file(t: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    skills_dir(t).join(name).join("SKILL.md")
}

#[test]
fn bundled_entries_parse() {
    for s in bundled_skills() {
        let path = Path::new("bundled");
        super::super::skills::parse_skill(s.content, path)
            .unwrap_or_else(|e| panic!("bundled skill {} invalid: {e}", s.dir_name));
    }
}

#[test]
fn first_run_creates_and_discovery_sees_it() {
    let t = data();
    let out = seed_bundled_skills(t.path());
    assert!(!out.is_empty(), "no bundled skills registered");
    for (name, outcome) in &out {
        assert_eq!(*outcome, SeedOutcome::Created, "{name}");
        assert!(skill_file(&t, name).exists(), "{name} not written");
    }
    let found = super::super::skills::discover_skills_ext(t.path(), t.path(), &[]);
    for (name, _) in &out {
        assert!(
            found.iter().any(|s| s.meta.name == *name),
            "{name} not discovered after seeding"
        );
    }
}

#[test]
fn second_run_is_idempotent_refresh() {
    let t = data();
    seed_bundled_skills(t.path());
    let out = seed_bundled_skills(t.path());
    for (name, outcome) in &out {
        assert_eq!(*outcome, SeedOutcome::Refreshed, "{name}");
    }
}

#[test]
fn user_edit_wins_over_reseed() {
    let t = data();
    seed_bundled_skills(t.path());
    let path = skill_file(&t, "design-references");
    fs::write(
        &path,
        "---\nname: design-references\ndescription: edited\n---\n\nUser's edit.\n",
    )
    .unwrap();
    let out = seed_bundled_skills(t.path());
    assert_eq!(out[0].1, SeedOutcome::KeptUserCopy);
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("User's edit."));
    assert!(
        !body.contains("purple gradient"),
        "reseed clobbered user edit"
    );
}

#[test]
fn stamp_changes_when_content_changes() {
    let a = stamp_for("hello");
    let b = stamp_for("hello ");
    assert_ne!(a, b);
}

#[test]
fn stamped_copy_round_trips_through_parse() {
    let t = data();
    seed_bundled_skills(t.path());
    let raw = fs::read_to_string(skill_file(&t, "design-references")).unwrap();
    let path = skill_file(&t, "design-references");
    let skill = super::super::skills::parse_skill(&raw, &path).unwrap();
    assert_eq!(skill.meta.name, "design-references");
    let body = super::super::skills::skill_body(&skill).unwrap();
    assert!(body.contains("## The registry"));
}

#[test]
fn registry_lists_77_entries() {
    for s in bundled_skills() {
        if s.dir_name != "design-references" {
            continue;
        }
        let rows = s.content.matches("| https://").count();
        assert_eq!(rows, 77, "registry row count drifted");
    }
}
