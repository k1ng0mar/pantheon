//! Tests for the writing half: backup -> apply -> validate -> rollback.
use super::*;
use crate::apply::{commit_staged, copy_tree, import_targets, is_bridged, stage};
use crate::{
    analyze, plan, Action, Detected, ItemKind, MigrationPlan, PlanItem, SourceKind, Targets,
};
use std::fs;
use std::path::{Path, PathBuf};

fn now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon-apply-{}-{}-{}",
        name,
        std::process::id(),
        now()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn targets(d: &Path) -> Targets {
    Targets::new(d.join("data"), d.join("data").join("extensions"))
}

/// A source tree with one good skill and one good extension.
fn fixture(d: &Path) {
    fs::create_dir_all(d.join("skills/demo/references")).unwrap();
    fs::write(
        d.join("skills/demo/SKILL.md"),
        "---\nname: demo\n---\nbody\n",
    )
    .unwrap();
    fs::write(d.join("skills/demo/references/extra.md"), "extra\n").unwrap();
    fs::create_dir_all(d.join("plugins/p1")).unwrap();
    fs::write(d.join("plugins/p1/plugin.yaml"), "name: p1\n").unwrap();
    fs::write(
        d.join("plugins/p1/__init__.py"),
        "def register(ctx): pass\n",
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

#[test]
fn backup_only_snapshots_targets_that_exist() {
    let d = tmp("bk");
    fixture(&d);
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    assert!(m.is_empty(), "nothing exists yet, so nothing to back up");
    assert!(!m.id.is_empty());
}

#[test]
fn backup_captures_existing_target_pre_image() {
    let d = tmp("bk2");
    fixture(&d);
    let t = targets(&d);
    // Pretend a previous import already landed.
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(t.data_dir.join("skills/demo/SKILL.md"), "OLD\n").unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    assert_eq!(m.len(), 1);
    let e = &m.entries[0];
    assert!(e.target.ends_with("skills/demo"));
    // The pre-image of a directory target is a directory.
    let pre = Path::new(&e.backup);
    assert!(pre.is_dir(), "{}", pre.display());
    assert_eq!(fs::read_to_string(pre.join("SKILL.md")).unwrap(), "OLD\n");
    // Manifest is written next to the backup dir.
    let mpath = t.backup_root().join(format!("{}.json", m.id));
    assert!(mpath.is_file());
    assert!(fs::read_to_string(&mpath).unwrap().contains("skills/demo"));
}

#[test]
fn backup_paths_do_not_collide_across_targets() {
    let d = tmp("bk3");
    fs::create_dir_all(d.join("skills/a")).unwrap();
    fs::write(d.join("skills/a/SKILL.md"), "---\nname: a\n---\n").unwrap();
    fs::create_dir_all(d.join("skills/b")).unwrap();
    fs::write(d.join("skills/b/SKILL.md"), "---\nname: b\n---\n").unwrap();
    let t = targets(&d);
    for n in ["a", "b"] {
        fs::create_dir_all(t.data_dir.join(format!("skills/{n}"))).unwrap();
        fs::write(t.data_dir.join(format!("skills/{n}/SKILL.md")), "OLD\n").unwrap();
    }
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    assert_eq!(m.len(), 2);
    assert_ne!(m.entries[0].backup, m.entries[1].backup);
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

#[test]
fn apply_creates_targets_and_copies_the_whole_tree() {
    let d = tmp("ap1");
    fixture(&d);
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();

    assert!(r.complete, "report: {:?}", r.outcomes);
    assert_eq!(r.failures(), 0);
    assert_eq!(r.ok(), 2, "one skill, one extension");
    let landed = t.data_dir.join("skills/demo/SKILL.md");
    assert!(landed.is_file());
    assert!(t.data_dir.join("skills/demo/references/extra.md").is_file());
    assert!(t.ext_dir.join("p1/plugin.yaml").is_file());
    assert!(r
        .outcomes
        .iter()
        .all(|o| matches!(o.status, ApplyStatus::Created)));
}

#[test]
fn apply_replaces_and_points_at_the_backup() {
    let d = tmp("ap2");
    fixture(&d);
    let t = targets(&d);
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(t.data_dir.join("skills/demo/SKILL.md"), "OLD\n").unwrap();
    // A stale file that must not survive the replace.
    fs::write(t.data_dir.join("skills/demo/stale.md"), "stale\n").unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();

    let out = r
        .outcomes
        .iter()
        .find(|o| o.kind == ItemKind::Skill)
        .unwrap();
    assert_eq!(out.status, ApplyStatus::Replaced);
    assert!(out.detail.contains(&m.id));
    assert!(t.data_dir.join("skills/demo/SKILL.md").is_file());
    assert!(
        !t.data_dir.join("skills/demo/stale.md").exists(),
        "replace must not merge into the old tree"
    );
}

#[test]
fn apply_reports_a_vanished_source_and_stops() {
    let d = tmp("ap3");
    let t = targets(&d);
    let missing = PlanItem {
        kind: ItemKind::Skill,
        path: d.join("skills/gone").to_string_lossy().to_string(),
        action: Action::Import {
            target: t.data_dir.join("skills/gone").to_string_lossy().to_string(),
        },
        note: String::new(),
    };
    let p = MigrationPlan {
        source: "hermes".into(),
        root: d.to_string_lossy().to_string(),
        source_version: None,
        items: vec![missing],
    };
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();
    assert!(!r.complete);
    assert_eq!(r.failures(), 1);
    assert_eq!(r.outcomes[0].status, ApplyStatus::SourceUnreadable);
}

#[test]
fn apply_routes_memory_plane_items_without_writing_them() {
    let d = tmp("ap4");
    let t = targets(&d);
    let p = MigrationPlan {
        source: "hermes".into(),
        root: d.to_string_lossy().to_string(),
        source_version: None,
        items: vec![PlanItem {
            kind: ItemKind::Memory,
            path: d.join("MEMORY.md").to_string_lossy().to_string(),
            action: Action::Import {
                target: "memory://memory".into(),
            },
            note: String::new(),
        }],
    };
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();
    assert_eq!(r.outcomes[0].status, ApplyStatus::MemoryPlane);
    assert_eq!(r.ok(), 0, "a memory item is not a filesystem write");
    assert!(r.complete, "routing is not a failure");
}

#[test]
fn a_bridged_target_is_never_reached_by_the_file_copier() {
    // Regression: `import_targets` used to filter only on `memory://`, so the
    // copier also copied the *source* `.env` over `<data_dir>/.env`. That
    // silently undid the merge, the never-clobber rule, and the 0600 mode in a
    // single write. Every bridged kind must be excluded from the copier.
    let d = tmp("bridged-not-copied");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::write(d.join(".env"), "OPENAI_API_KEY=sk-sourcevalue12345\n").unwrap();
    fs::create_dir_all(d.join("sessions")).unwrap();
    fs::write(d.join("sessions/t.jsonl"), "{\"r\":\"u\"}\n").unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);

    // The copier's work list is the invariant that matters: a bridged kind may
    // legitimately *have* an import target (that is how the plan says "this
    // will be produced by a bridge"), but it must never be copied.
    for (item, _target) in import_targets(&p) {
        assert!(
            !is_bridged(item.kind),
            "{} must not be in the file copier's work list",
            item.kind
        );
    }
    let bridged_in_plan: Vec<ItemKind> = p
        .items
        .iter()
        .filter(|i| is_bridged(i.kind) && i.target().is_some())
        .map(|i| i.kind)
        .collect();
    assert!(
        bridged_in_plan.contains(&ItemKind::Credentials),
        "the fixture has a .env to bridge: {bridged_in_plan:?}"
    );
    assert!(
        bridged_in_plan.contains(&ItemKind::Session),
        "the fixture has a session dir to bridge: {bridged_in_plan:?}"
    );

    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();

    // The key store holds the merged credential, and HOME never arrived.
    let store = fs::read_to_string(t.data_dir.join(".env")).unwrap();
    assert!(store.contains("OPENAI_API_KEY=sk-sourcevalue12345"));
    assert!(!store.contains("HOME"));

    // The session dir was bridged, not copied as a config file.
    assert!(t
        .data_dir
        .join("imported-sessions/hermes/manifest.json")
        .is_file());
    let sess = r
        .outcomes
        .iter()
        .find(|o| o.kind == ItemKind::Session)
        .unwrap();
    assert!(sess.target.ends_with("manifest.json"), "{}", sess.target);

    // And validate agrees: no bridge artefact is reported missing.
    let v = validate(&p);
    assert!(v.complete, "{}", v.render());
}

#[test]
fn a_source_env_is_merged_into_the_key_store_not_copied() {
    let d = tmp("ap5");
    let src = d.join(".env");
    fs::write(
        &src,
        "OPENAI_API_KEY=sk-sourcevalue12345\nHOME=/root\n# a comment\n",
    )
    .unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();

    // Exactly one credential outcome, targeting pantheon's own store.
    let cred = r
        .outcomes
        .iter()
        .find(|o| o.kind == ItemKind::Credentials)
        .expect("the .env should be carried");
    assert_eq!(cred.target, t.data_dir.join(".env").to_string_lossy());

    let store = fs::read_to_string(t.data_dir.join(".env")).unwrap();
    assert!(store.contains("OPENAI_API_KEY=sk-sourcevalue12345"));
    assert!(!store.contains("HOME"), "a non-credential must not carry");
    assert!(!store.contains("a comment"), "comments are not keys");

    // The source file was not copied wholesale anywhere.
    for e in fs::read_dir(&t.data_dir).unwrap().flatten() {
        let pth = e.path();
        if pth.is_file() && pth != t.data_dir.join(".env") {
            let body = fs::read_to_string(&pth).unwrap_or_default();
            assert!(
                !body.contains("sk-sourcevalue12345"),
                "value leaked into {}",
                pth.display()
            );
        }
    }
}

#[test]
fn apply_does_not_clobber_an_existing_pantheon_key() {
    let d = tmp("ap-noclobber");
    fs::write(d.join(".env"), "OPENAI_API_KEY=sk-sourcevalue12345\n").unwrap();
    let t = targets(&d);
    fs::create_dir_all(&t.data_dir).unwrap();
    fs::write(
        t.data_dir.join(".env"),
        "OPENAI_API_KEY=sk-alreadyset99999\n",
    )
    .unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();

    let store = fs::read_to_string(t.data_dir.join(".env")).unwrap();
    assert!(
        store.contains("OPENAI_API_KEY=sk-alreadyset99999"),
        "the operator's key must survive a migration: {store}"
    );
    let cred = r
        .outcomes
        .iter()
        .find(|o| o.kind == ItemKind::Credentials)
        .unwrap();
    assert!(cred.detail.contains("already present"), "{}", cred.detail);
}

#[test]
fn copy_tree_skips_symlinks_and_records_them() {
    let d = tmp("sym");
    let src = d.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("real.md"), "real\n").unwrap();
    let outside = d.join("outside.txt");
    fs::write(&outside, "secret\n").unwrap();
    std::os::unix::fs::symlink(&outside, src.join("link.md")).unwrap();

    let mut skipped = Vec::new();
    copy_tree(
        &src,
        &d.join("dst"),
        &mut skipped,
        &StageBudgets::unlimited(),
        &mut BudgetUsage::default(),
    )
    .unwrap();
    assert!(d.join("dst/real.md").is_file());
    assert!(
        !d.join("dst/link.md").exists(),
        "a symlink must never be dereferenced"
    );
    assert_eq!(skipped.len(), 1);
    assert!(skipped[0].contains("symlink"));
}

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

#[test]
fn validate_passes_after_a_clean_apply() {
    let d = tmp("va1");
    fixture(&d);
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    apply(&p, &t, &m).unwrap();
    let v = validate(&p);
    assert!(v.complete, "{}", v.render());
    assert_eq!(v.problems(), 0);
    assert!(v.render().contains("SKILL.md present"));
}

#[test]
fn validate_flags_a_skill_dir_with_no_skill_md() {
    let d = tmp("va2");
    let t = targets(&d);
    fs::create_dir_all(t.data_dir.join("skills/hollow")).unwrap();
    let p = MigrationPlan {
        source: "omp".into(),
        root: d.to_string_lossy().to_string(),
        source_version: None,
        items: vec![PlanItem {
            kind: ItemKind::Skill,
            path: d.join("skills/hollow").to_string_lossy().to_string(),
            action: Action::Import {
                target: t
                    .data_dir
                    .join("skills/hollow")
                    .to_string_lossy()
                    .to_string(),
            },
            note: String::new(),
        }],
    };
    let v = validate(&p);
    assert!(!v.complete);
    assert_eq!(v.problems(), 1);
    assert!(v.render().contains("no SKILL.md"));
}

#[test]
fn validate_flags_a_missing_target() {
    let d = tmp("va3");
    let t = targets(&d);
    let p = MigrationPlan {
        source: "omp".into(),
        root: d.to_string_lossy().to_string(),
        source_version: None,
        items: vec![PlanItem {
            kind: ItemKind::Agent,
            path: "/nowhere/scout.md".into(),
            action: Action::Import {
                target: t
                    .data_dir
                    .join("agents/scout.md")
                    .to_string_lossy()
                    .to_string(),
            },
            note: String::new(),
        }],
    };
    let v = validate(&p);
    assert!(!v.complete);
    assert_eq!(v.outcomes[0].status, ValidateStatus::Missing);
}

#[test]
fn validate_marks_memory_plane_not_applicable() {
    let d = tmp("va4");
    let p = MigrationPlan {
        source: "hermes".into(),
        root: d.to_string_lossy().to_string(),
        source_version: None,
        items: vec![PlanItem {
            kind: ItemKind::Persona,
            path: d.join("SOUL.md").to_string_lossy().to_string(),
            action: Action::Import {
                target: "memory://persona".into(),
            },
            note: String::new(),
        }],
    };
    let v = validate(&p);
    assert!(v.complete);
    assert_eq!(v.outcomes[0].status, ValidateStatus::NotApplicable);
}

// ---------------------------------------------------------------------------
// rollback
// ---------------------------------------------------------------------------

#[test]
fn rollback_restores_the_pre_image() {
    let d = tmp("rb");
    fixture(&d);
    let t = targets(&d);
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(t.data_dir.join("skills/demo/SKILL.md"), "OLD\n").unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();
    assert!(t.data_dir.join("skills/demo/references/extra.md").is_file());

    let n = r.rollback(&m).unwrap();
    assert!(n >= 1);
    assert_eq!(
        fs::read_to_string(t.data_dir.join("skills/demo/SKILL.md")).unwrap(),
        "OLD\n"
    );
    assert!(
        !t.data_dir.join("skills/demo/references/extra.md").exists(),
        "rollback removes what apply added"
    );
}

// ---------------------------------------------------------------------------
// end to end
// ---------------------------------------------------------------------------

#[test]
fn full_pipeline_detect_plan_backup_apply_validate() {
    let d = tmp("e2e");
    let src = d.join("src");
    fixture(&src);
    fs::write(src.join("config.yaml"), "x: 1\n").unwrap();
    fs::write(src.join(".env"), "OPENAI_API_KEY=sk-pipelinevalue1\n").unwrap();
    fs::create_dir_all(src.join("agents")).unwrap();
    fs::write(src.join("agents/scout.md"), "---\nname: scout\n---\n").unwrap();

    // 1. detect
    let kinds = detect(&src);
    assert!(kinds.contains(&SourceKind::Hermes));

    // 2. analyze + 3. plan
    let t = targets(&d);
    let detected = analyze(&src, SourceKind::Hermes);
    let p = plan(&src, SourceKind::Hermes, &t);
    assert_eq!(p.items.len(), detected.len());

    // 4. dry run renders without touching anything
    let rendered = render(&p);
    assert!(rendered.contains("to import"));
    assert!(
        !t.data_dir.join("skills/demo").exists(),
        "dry run wrote nothing"
    );

    // 5. backup (nothing exists)  6. apply
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();
    assert!(r.complete, "{:?}", r.outcomes);

    // 7. validate
    let v = validate(&p);
    assert!(v.complete, "{}", v.render());

    // A classified credential landed in pantheon's own key store; nothing
    // else did, and the source file was not copied wholesale.
    let store = t.data_dir.join(".env");
    assert!(store.is_file(), "the credential should be carried");
    assert!(fs::read_to_string(&store)
        .unwrap()
        .contains("sk-pipelinevalue1"));
    // The fixture's auth.json is still a real secret: reported, never copied.
    let skipped: Vec<&Detected> = detected
        .iter()
        .filter(|x| x.kind == ItemKind::Secret)
        .collect();
    assert!(!skipped.is_empty(), "auth.json must still be a skip");
    assert_eq!(p.skipped(), skipped.len());
}

#[test]
fn copy_tree_is_depth_bounded() {
    // A pathological source tree must not make one import recurse without
    // limit. Exceeding the bound is recorded as a skip, not a silent drop and
    // not a hard failure.
    let d = tmp("deep-tree");
    let src = d.join("deep");
    let mut p = src.clone();
    std::fs::create_dir_all(&p).unwrap();
    for _ in 0..40 {
        p = p.join("n");
        std::fs::create_dir_all(&p).unwrap();
    }
    std::fs::write(p.join("leaf.txt"), "bottom\n").unwrap();

    let mut skipped = Vec::new();
    copy_tree(
        &src,
        &d.join("dst"),
        &mut skipped,
        &StageBudgets::unlimited(),
        &mut BudgetUsage::default(),
    )
    .unwrap();
    assert!(
        skipped.iter().any(|s| s.contains("deeper than")),
        "the depth bound should have been reported: {skipped:?}"
    );
    assert!(
        !d.join("dst").join("n").exists() || !skipped.is_empty(),
        "the run must complete rather than recurse forever"
    );
}

#[test]
fn copy_tree_still_copies_normal_depths() {
    let d = tmp("normal-tree");
    let src = d.join("skill");
    std::fs::create_dir_all(src.join("a/b/c")).unwrap();
    std::fs::write(src.join("a/b/c/deep.txt"), "kept\n").unwrap();
    let mut skipped = Vec::new();
    copy_tree(
        &src,
        &d.join("out"),
        &mut skipped,
        &StageBudgets::unlimited(),
        &mut BudgetUsage::default(),
    )
    .unwrap();
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(
        std::fs::read_to_string(d.join("out/a/b/c/deep.txt")).unwrap(),
        "kept\n"
    );
}
// ---------------------------------------------------------------------------
// transactional apply: stage -> validate -> commit
// ---------------------------------------------------------------------------

fn tiny_budgets() -> StageBudgets {
    StageBudgets {
        max_total_bytes: 10,
        max_files: 1_000_000,
        max_single_file_bytes: 1_000_000,
    }
}

#[test]
fn a_staging_failure_commits_nothing() {
    // One good skill and one vanished source: the good item must not land
    // just because it staged first. The old code committed as it went.
    let d = tmp("txn-stage-fail");
    fixture(&d);
    let t = targets(&d);
    let mut p = plan(&d, SourceKind::Hermes, &t);
    p.items.push(PlanItem {
        kind: ItemKind::Skill,
        path: d.join("skills/gone").to_string_lossy().to_string(),
        action: Action::Import {
            target: t.data_dir.join("skills/gone").to_string_lossy().to_string(),
        },
        note: String::new(),
    });
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();
    assert!(!r.complete);
    assert_eq!(r.failures(), 1);
    assert!(
        !t.data_dir.join("skills/demo").exists(),
        "a failed apply must leave the target untouched"
    );
    assert!(
        !t.data_dir.join(".migrate-stage").exists(),
        "the staging tree must be discarded"
    );
}

#[test]
fn a_commit_failure_rolls_back_automatically() {
    // Two skills stage cleanly; sabotage the second staged tree so its
    // commit rename fails. The first skill must be rolled back, and the
    // pre-existing target restored from its pre-image.
    let d = tmp("txn-commit-fail");
    fixture(&d);
    fs::create_dir_all(d.join("skills/b")).unwrap();
    fs::write(d.join("skills/b/SKILL.md"), "---\nname: b\n---\n").unwrap();
    let t = targets(&d);
    // A previous import already landed under skills/demo.
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(t.data_dir.join("skills/demo/SKILL.md"), "OLD\n").unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    assert!(
        m.entries.iter().any(|e| e.target.ends_with("skills/demo")),
        "the pre-image must exist for the rollback"
    );

    let staged = stage(&p, &t, &m, &StageBudgets::default(), false).unwrap();
    assert!(staged.complete);
    assert!(staged.items.len() >= 2);
    // Sabotage: remove one staged tree so its rename fails. The extension
    // commits last (plan order), so the skills are already committed and
    // must be rolled back.
    let victim = staged
        .items
        .iter()
        .find(|i| i.live.ends_with("p1"))
        .expect("extension p1 staged");
    fs::remove_dir_all(&victim.staged).unwrap();

    let err = commit_staged(&staged).expect_err("the sabotaged rename must fail");
    assert!(
        err.to_string().contains("rolled back"),
        "the error must say the rollback happened: {err}"
    );

    // the sabotaged extension never landed...
    assert!(
        !t.ext_dir.join("p1").exists(),
        "the sabotaged item must not land"
    );
    // ...the created skill was removed by the rollback...
    assert!(
        !t.data_dir.join("skills/b").exists(),
        "a created item must be removed by the rollback"
    );
    // ...the earlier-committed skill demo was rolled back to its pre-image...
    assert_eq!(
        fs::read_to_string(t.data_dir.join("skills/demo/SKILL.md")).unwrap(),
        "OLD\n",
        "the pre-image must be restored"
    );
    assert!(
        !t.data_dir.join("skills/demo/references").exists(),
        "what the commit added must be gone"
    );
    // ...and no staging residue remains.
    assert!(!t.data_dir.join(".migrate-stage").exists());
}

#[test]
fn a_commit_failure_removes_created_targets() {
    // Same sabotage, but the first item is a create (no pre-image): the
    // rollback must remove it rather than restore it.
    let d = tmp("txn-commit-create");
    fixture(&d);
    fs::create_dir_all(d.join("skills/b")).unwrap();
    fs::write(d.join("skills/b/SKILL.md"), "---\nname: b\n---\n").unwrap();
    let t = targets(&d);

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    let staged = stage(&p, &t, &m, &StageBudgets::default(), false).unwrap();
    // Sabotage the extension: it commits last, so the skill is already
    // committed and must be rolled back.
    let victim = staged
        .items
        .iter()
        .find(|i| i.live.ends_with("p1"))
        .expect("extension p1 staged");
    fs::remove_dir_all(&victim.staged).unwrap();

    commit_staged(&staged).expect_err("the sabotaged rename must fail");
    assert!(
        !t.data_dir.join("skills/demo").exists(),
        "a created target must be removed by the rollback"
    );
    assert!(!t.ext_dir.join("p1").exists());
}

#[test]
fn budget_breach_aborts_cleanly() {
    let d = tmp("txn-budget-bytes");
    fixture(&d);
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();

    let err = apply_with_budgets(&p, &t, &m, false, &tiny_budgets())
        .expect_err("10 bytes cannot hold a skill");
    assert!(
        err.to_string().contains("budget"),
        "the error must name the budget: {err}"
    );
    assert!(
        !t.data_dir.join("skills").exists(),
        "a budget abort must commit nothing"
    );
    assert!(!t.data_dir.join(".migrate-stage").exists());
}

#[test]
fn single_file_budget_rejects_one_big_file() {
    let d = tmp("txn-budget-file");
    fs::create_dir_all(d.join("skills/big")).unwrap();
    fs::write(d.join("skills/big/SKILL.md"), "---\nname: big\n---\n").unwrap();
    fs::write(d.join("skills/big/blob.bin"), vec![b'x'; 1024]).unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();

    let budgets = StageBudgets {
        max_single_file_bytes: 100,
        ..StageBudgets::unlimited()
    };
    let err = apply_with_budgets(&p, &t, &m, false, &budgets).expect_err("1 KiB > 100 B budget");
    assert!(err.to_string().contains("per-file"), "{err}");
    assert!(!t.data_dir.join("skills").exists());
}

#[test]
fn file_count_budget_rejects_many_files() {
    let d = tmp("txn-budget-count");
    fs::create_dir_all(d.join("skills/many")).unwrap();
    fs::write(d.join("skills/many/SKILL.md"), "---\nname: many\n---\n").unwrap();
    for i in 0..5 {
        fs::write(d.join(format!("skills/many/f{i}.md")), "x\n").unwrap();
    }
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();

    let budgets = StageBudgets {
        max_files: 3,
        ..StageBudgets::unlimited()
    };
    let err = apply_with_budgets(&p, &t, &m, false, &budgets).expect_err("6 files > 3 budget");
    assert!(err.to_string().contains("file count"), "{err}");
    assert!(!t.data_dir.join("skills").exists());
}

#[test]
fn backup_breach_leaves_no_partial_backup() {
    // A pre-existing 1 KiB target with a 10-byte budget: the backup itself
    // must fail, and the half-written backup tree must be removed.
    let d = tmp("txn-budget-backup");
    fixture(&d);
    let t = targets(&d);
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(t.data_dir.join("skills/demo/SKILL.md"), vec![b'y'; 1024]).unwrap();
    let p = plan(&d, SourceKind::Hermes, &t);

    let before: usize = fs::read_dir(t.backup_root())
        .map(|rd| rd.count())
        .unwrap_or(0);
    let err = backup_with_budgets(&p, &t, &tiny_budgets()).expect_err("backup exceeds budget");
    assert!(err.to_string().contains("budget"), "{err}");
    let after: Vec<_> = fs::read_dir(t.backup_root())
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert_eq!(before, 0, "precondition: backup root starts empty");
    assert!(
        after.is_empty(),
        "a failed backup must leave no residue: {after:?}"
    );
}

#[test]
fn backup_snapshots_bridge_targets_for_rollback() {
    // The credentials bridge merges into the live .env, so the backup must
    // hold its pre-image — otherwise a commit rollback could not restore it.
    let d = tmp("txn-backup-bridges");
    fs::write(d.join(".env"), "OPENAI_API_KEY=sk-sourcevalue12345\n").unwrap();
    let t = targets(&d);
    fs::create_dir_all(&t.data_dir).unwrap();
    fs::write(t.data_dir.join(".env"), "OPERATOR_KEY=keepme\n").unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    assert!(
        m.entries.iter().any(|e| e.target.ends_with(".env")),
        "the live .env must have a pre-image: {:?}",
        m.entries.iter().map(|e| &e.target).collect::<Vec<_>>()
    );

    // And the merged commit keeps the operator's key.
    let r = apply(&p, &t, &m).unwrap();
    assert!(r.complete, "{:?}", r.outcomes);
    let store = fs::read_to_string(t.data_dir.join(".env")).unwrap();
    assert!(store.contains("OPERATOR_KEY=keepme"));
    assert!(store.contains("OPENAI_API_KEY=sk-sourcevalue12345"));
}

#[test]
fn byte_identical_files_are_left_alone() {
    let d = tmp("txn-unchanged");
    fixture(&d);
    let t = targets(&d);
    // Pre-seed the live target with byte-identical content.
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(
        t.data_dir.join("skills/demo/SKILL.md"),
        "---\nname: demo\n---\nbody\n",
    )
    .unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    // Narrow the plan to the single identical file so the dir case (which
    // always re-stages) does not muddy the assertion.
    let single = MigrationPlan {
        source: p.source.clone(),
        root: p.root.clone(),
        source_version: p.source_version.clone(),
        items: vec![PlanItem {
            kind: ItemKind::Skill,
            path: d.join("skills/demo/SKILL.md").to_string_lossy().to_string(),
            action: Action::Import {
                target: t
                    .data_dir
                    .join("skills/demo/SKILL.md")
                    .to_string_lossy()
                    .to_string(),
            },
            note: String::new(),
        }],
    };
    let m = backup(&single, &t).unwrap();
    let r = apply(&single, &t, &m).unwrap();
    assert!(r.complete);
    assert_eq!(r.outcomes[0].status, ApplyStatus::Unchanged);
}

#[test]
fn budgets_default_to_sane_values() {
    let b = StageBudgets::default();
    assert_eq!(b.max_total_bytes, 1024 * 1024 * 1024);
    assert_eq!(b.max_files, 100_000);
    assert_eq!(b.max_single_file_bytes, 100 * 1024 * 1024);
    let u = StageBudgets::unlimited();
    assert_eq!(u.max_total_bytes, u64::MAX);
}

#[test]
fn a_failed_commit_rolls_back_the_session_quarantine_as_one_dir() {
    // The session bridge writes `imported-sessions/<source>/` file by file;
    // the staged (and backup, and rollback) unit is the whole dir.
    let d = tmp("txn-session-rollback");
    fs::create_dir_all(d.join("sessions")).unwrap();
    fs::write(
        d.join("sessions/t.jsonl"),
        "{\"role\":\"user\",\"content\":\"new transcript\"}\n",
    )
    .unwrap();
    fs::write(d.join(".env"), "OPENAI_API_KEY=sk-sourcevalue12345\n").unwrap();

    let t = targets(&d);
    // Pre-existing live quarantine with an old transcript, and a live .env
    // the credentials bridge will merge into (and commit after the
    // quarantine, so sabotaging it rolls the quarantine back).
    let live_q = t.data_dir.join("imported-sessions/hermes");
    fs::create_dir_all(&live_q).unwrap();
    fs::write(
        live_q.join("old.jsonl"),
        "{\"role\":\"user\",\"content\":\"old transcript\"}\n",
    )
    .unwrap();
    fs::write(t.data_dir.join(".env"), "OPERATOR_KEY=keepme\n").unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    assert!(p.items.iter().any(|i| i.kind == ItemKind::Session));
    let m = backup(&p, &t).unwrap();
    assert!(
        m.entries
            .iter()
            .any(|e| e.target.ends_with("imported-sessions/hermes")),
        "the quarantine must have a pre-image"
    );

    let staged = stage(&p, &t, &m, &StageBudgets::default(), false).unwrap();
    assert!(staged.complete);
    let sess_items: Vec<_> = staged
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Session)
        .collect();
    assert_eq!(
        sess_items.len(),
        1,
        "the quarantine stages as one dir, not per-file: {:?}",
        sess_items.iter().map(|i| &i.live).collect::<Vec<_>>()
    );
    assert!(sess_items[0].staged.is_dir());
    assert!(sess_items[0].existed, "the live quarantine pre-exists");

    // Sabotage the credentials commit: it is staged last, so the quarantine
    // is already committed when the rename fails.
    let victim = staged
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Credentials)
        .expect("credentials staged");
    assert!(victim.staged.is_file());
    fs::remove_file(&victim.staged).unwrap();

    let err = commit_staged(&staged).expect_err("the sabotaged rename must fail");
    assert!(err.to_string().contains("rolled back"), "{err}");

    // The quarantine is back to its pre-image, byte for byte.
    assert_eq!(
        fs::read_to_string(live_q.join("old.jsonl")).unwrap(),
        "{\"role\":\"user\",\"content\":\"old transcript\"}\n"
    );
    assert!(
        !live_q.join("t.jsonl").exists(),
        "what the commit added must be gone"
    );
    assert!(
        !live_q.join("manifest.json").exists(),
        "the staged manifest must be gone too"
    );
    // And the credentials merge was rolled back as well.
    assert_eq!(
        fs::read_to_string(t.data_dir.join(".env")).unwrap(),
        "OPERATOR_KEY=keepme\n"
    );
    assert!(!t.data_dir.join(".migrate-stage").exists());
}
