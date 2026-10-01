//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral — SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem — lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Bundled skill seeding tests. All run against a temp data dir through the
//! public `seed_bundled_skills` entry point plus real discovery, so the
//! assertions cover the production path, not a near-identical impl.

use pantheon_exec::bundled_skills::*;
use pantheon_exec::skills::{parse_skill, skill_body};
use std::fs;
use std::path::Path;

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
        parse_skill(s.content, path)
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
    let found = pantheon_exec::skills::discover_skills_ext(t.path(), t.path(), &[]);
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
    let outcome = out
        .iter()
        .find(|(name, _)| name == "design-references")
        .map(|(_, o)| *o)
        .expect("design-references missing from seed output");
    assert_eq!(outcome, SeedOutcome::KeptUserCopy);
    let body = fs::read_to_string(&path).unwrap();
    assert!(body.contains("User's edit."));
    assert!(
        !body.contains("purple gradient"),
        "reseed clobbered user edit"
    );
}

#[test]
fn stamped_copy_round_trips_through_parse() {
    let t = data();
    seed_bundled_skills(t.path());
    let raw = fs::read_to_string(skill_file(&t, "design-references")).unwrap();
    let path = skill_file(&t, "design-references");
    let skill = parse_skill(&raw, &path).unwrap();
    assert_eq!(skill.meta.name, "design-references");
    let body = skill_body(&skill).unwrap();
    assert!(body.contains("## The registry"));
}
