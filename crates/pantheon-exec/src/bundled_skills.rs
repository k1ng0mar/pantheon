//! Bundled skills: shipped inside the binary, materialized into
//! `<data_dir>/skills/` on first use so the standard discovery sees them.
//!
//! A bundled skill is ordinary `SKILL.md` data once on disk. It is not a
//! capability: `skill_read` stays gated on FilesystemRead and the model
//! reads it like any other skill. Bundling solves only one problem: a fresh
//! install has an empty skills dir, so the shipped knowledge would never
//! reach the session.
//!
//! Layout: `crates/pantheon-exec/bundled-skills/<name>/SKILL.md`, embedded
//! with `include_str!` at compile time. Seeding is write-if-missing with a
//! content stamp, so a user editing the materialized copy is never
//! overwritten, and a version bump in the binary refreshes only skills the
//! user has not touched.

use crate::skills::parse_skill;
use pantheon_core::error::{Layer, PantheonError};
use std::fs;
use std::path::Path;

/// One bundled skill: name plus the full SKILL.md text.
pub struct BundledSkill {
    /// Skill directory name under `<data_dir>/skills/`.
    pub dir_name: &'static str,
    /// Complete SKILL.md content, frontmatter included.
    pub content: &'static str,
}

/// The skills this binary ships. Add an entry and a matching
/// `include_str!` here; discovery does the rest.
pub fn bundled_skills() -> Vec<BundledSkill> {
    vec![BundledSkill {
        dir_name: "design-references",
        content: include_str!("../bundled-skills/design-references/SKILL.md"),
    }]
}

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check bundled skill content",
        "",
    )
}

/// Validate a bundled skill through the same parser discovery uses, so a
/// malformed entry fails loudly instead of seeding a skill that then
/// silently fails to load.
fn validate(content: &str) -> Result<(), PantheonError> {
    let path = Path::new("bundled-skills/<name>/SKILL.md");
    let skill = parse_skill(content, path)?;
    if skill.meta.name.trim().is_empty() {
        return Err(serr(
            "SKILL_NO_NAME",
            "bundled skill frontmatter has no name".into(),
        ));
    }
    Ok(())
}

/// Materialize every bundled skill into `<data_dir>/skills/<name>/`.
///
/// Per skill:
/// - target missing: write it (first run).
/// - target exists with the stamp comment matching this build: rewrite it
///   (binary updated, user never edited).
/// - target exists with a different stamp: leave it alone (user edit wins).
fn seed_bundled_skill(
    skills_dir: &Path,
    bundled: &BundledSkill,
) -> Result<SeedOutcome, PantheonError> {
    validate(bundled.content)?;
    let dir = skills_dir.join(bundled.dir_name);
    let target = dir.join("SKILL.md");
    let stamp = stamp_for(bundled.content);

    if !target.exists() {
        fs::create_dir_all(&dir).map_err(|e| {
            serr(
                "SKILL_SEED_IO",
                format!("cannot create {}: {e}", dir.display()),
            )
        })?;
        fs::write(&target, stamped(bundled.content, &stamp)).map_err(|e| {
            serr(
                "SKILL_SEED_IO",
                format!("cannot write {}: {e}", target.display()),
            )
        })?;
        return Ok(SeedOutcome::Created);
    }

    let existing = fs::read_to_string(&target).map_err(|e| {
        serr(
            "SKILL_SEED_IO",
            format!("cannot read {}: {e}", target.display()),
        )
    })?;
    if existing.contains(&stamp) {
        fs::write(&target, stamped(bundled.content, &stamp)).map_err(|e| {
            serr(
                "SKILL_SEED_IO",
                format!("cannot refresh {}: {e}", target.display()),
            )
        })?;
        Ok(SeedOutcome::Refreshed)
    } else {
        Ok(SeedOutcome::KeptUserCopy)
    }
}

/// Marker comment that records which bundled content version a materialized
/// copy holds. Sits at the very end, below everything the parser and the
/// model care about.
fn stamp_for(content: &str) -> String {
    format!("<!-- pantheon:bundled:{} -->", simple_hash(content))
}

fn stamped(content: &str, stamp: &str) -> String {
    format!("{content}\n{stamp}\n")
}

/// FNV-1a over the SKILL.md content, hex-encoded. Content-addressed, not
/// hand-versioned: any edit to the source file changes the stamp.
fn simple_hash(content: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

/// What happened to one bundled skill during seeding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedOutcome {
    /// Wrote a fresh copy.
    Created,
    /// Rewrote an untouched copy to match this build.
    Refreshed,
    /// Left the on-disk copy alone; a user edit is present.
    KeptUserCopy,
}

/// Seed all bundled skills into `data_dir/skills` and return one outcome
/// per skill, in `bundled_skills()` order. Called from session construction
/// before discovery runs.
pub fn seed_bundled_skills(data_dir: &Path) -> Vec<(String, SeedOutcome)> {
    let skills_dir = data_dir.join("skills");
    bundled_skills()
        .into_iter()
        .map(|s| {
            let outcome = seed_bundled_skill(&skills_dir, &s);
            match outcome {
                Ok(o) => (s.dir_name.to_string(), o),
                // A failed seed must not take the session down: log-and-
                // continue, discovery still sees whatever is on disk.
                Err(e) => {
                    eprintln!("bundled skill '{}' failed to seed: {e}", s.dir_name);
                    (s.dir_name.to_string(), SeedOutcome::KeptUserCopy)
                }
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "bundled_skills_tests.rs"]
mod tests;
