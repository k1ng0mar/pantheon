//! Load-bearing migration invariants moved out of the crate per the
//! test-hygiene policy: everything here exercises the public API only
//! (`pantheon_migration::{...}`, `pantheon_migration::compat::...`).
//! Run with `cargo test -p pantheon-eval`.
use pantheon_extensions::hooks::Hook;
use pantheon_migration::compat::{map_hook, scan_registrations, HookMap, KNOWN_FOREIGN_EVENTS};
use pantheon_migration::*;
use std::fs;
use std::path::{Path, PathBuf};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon-migration-extra-{}-{}-{}",
        name,
        std::process::id(),
        now_ms()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn targets(d: &Path) -> Targets {
    Targets::new(d.join("data"), d.join("data").join("extensions"))
}

/// A hermes source root with one good skill and one good extension.
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

fn tiny_budgets() -> StageBudgets {
    StageBudgets {
        max_total_bytes: 10,
        max_files: 1_000_000,
        max_single_file_bytes: 1_000_000,
    }
}

// ---------------------------------------------------------------------------
// apply: credentials bridge
// ---------------------------------------------------------------------------

#[test]
fn source_env_is_merged_into_the_key_store_never_copied() {
    // The load-bearing regression: the copier once copied the *source* .env
    // over `<data_dir>/.env`, silently undoing the merge, the never-clobber
    // rule, and the 0600 mode in a single write. Bridged kinds may have
    // import targets, but they must never be file-copied.
    let d = tmp("env-merge");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::write(
        d.join(".env"),
        "OPENAI_API_KEY=sk-sourcevalue12345\nHOME=/root\n# a comment\n",
    )
    .unwrap();
    fs::create_dir_all(d.join("sessions")).unwrap();
    fs::write(d.join("sessions/t.jsonl"), "{\"r\":\"u\"}\n").unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    assert!(p.items.iter().any(|i| i.kind == ItemKind::Credentials));
    assert!(p.items.iter().any(|i| i.kind == ItemKind::Session));

    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();
    assert!(r.complete, "{:?}", r.outcomes);

    // The credential was merged into pantheon's own store; the unclassified
    // name stayed behind; the source file was not copied wholesale.
    let store = fs::read_to_string(t.data_dir.join(".env")).unwrap();
    // The canonical dotenv writer quotes values; what must hold is
    // that the carried value is byte-identical to the source value.
    let source_value = fs::read_to_string(d.join(".env"))
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("OPENAI_API_KEY="))
        .expect("fixture carries the key")
        .trim()
        .to_string();
    let carried = store
        .lines()
        .find_map(|l| l.trim().strip_prefix("OPENAI_API_KEY="))
        .map(|v| v.trim().trim_matches('"').to_string());
    assert_eq!(
        carried.as_deref(),
        Some(source_value.as_str()),
        "the source credential must be merged into the key store"
    );
    assert!(!store.contains("HOME"), "a non-credential must not carry");
    assert!(!store.contains("a comment"), "comments are not keys");

    // The session dir was bridged, not copied as files.
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

    // And validate agrees no bridge artefact is missing.
    let v = validate(&p);
    assert!(v.complete, "{}", v.render());
}

#[test]
fn apply_never_clobbers_an_existing_pantheon_key() {
    let d = tmp("env-noclobber");
    fs::write(d.join(".env"), "OPENAI_API_KEY=sk-sourcevalue12345\n").unwrap();
    let t = targets(&d);
    fs::create_dir_all(&t.data_dir).unwrap();
    fs::write(
        t.data_dir.join(".env"),
        "OPERATOR_KEY=keepme\nOPENAI_API_KEY=sk-alreadyset99999\n",
    )
    .unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    // The credentials bridge merges into the live .env, so the backup must
    // hold its pre-image, or a later rollback could not restore it.
    let m = backup(&p, &t).unwrap();
    assert!(
        m.entries.iter().any(|e| e.target.ends_with(".env")),
        "the live .env must have a pre-image: {:?}",
        m.entries.iter().map(|e| &e.target).collect::<Vec<_>>()
    );

    let r = apply(&p, &t, &m).unwrap();
    assert!(r.complete, "{:?}", r.outcomes);
    let store = fs::read_to_string(t.data_dir.join(".env")).unwrap();
    assert!(
        store.contains("OPENAI_API_KEY=sk-alreadyset99999"),
        "the operator's key must survive a migration: {store}"
    );
    assert!(!store.contains("sk-sourcevalue12345"));
    assert!(store.contains("OPERATOR_KEY=keepme"));
    let cred = r
        .outcomes
        .iter()
        .find(|o| o.kind == ItemKind::Credentials)
        .unwrap();
    assert!(cred.detail.contains("already present"), "{}", cred.detail);
}

// ---------------------------------------------------------------------------
// apply: atomicity
// ---------------------------------------------------------------------------

#[test]
fn a_staging_failure_commits_nothing_and_names_the_vanished_source() {
    // One good skill and one vanished source: the good item must not land
    // just because it staged first, and the vanished item must be loud, not
    // silent.
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
    let gone = r
        .outcomes
        .iter()
        .find(|o| o.kind == ItemKind::Skill && o.target.ends_with("skills/gone"))
        .unwrap();
    assert_eq!(gone.status, ApplyStatus::SourceUnreadable);
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
fn apply_validate_round_trip_then_rollback_restores_the_pre_image() {
    let d = tmp("roundtrip");
    fixture(&d);
    let t = targets(&d);
    // A previous import already landed: apply must replace it (no merge
    // into the old tree) and the backup must point at the pre-image.
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(t.data_dir.join("skills/demo/SKILL.md"), "OLD\n").unwrap();
    fs::write(t.data_dir.join("skills/demo/stale.md"), "stale\n").unwrap();

    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    assert!(!m.id.is_empty());
    assert!(m.entries.iter().any(|e| e.target.ends_with("skills/demo")));

    let r = apply(&p, &t, &m).unwrap();
    assert!(r.complete, "{:?}", r.outcomes);
    assert_eq!(r.failures(), 0);
    let skill = r
        .outcomes
        .iter()
        .find(|o| o.kind == ItemKind::Skill)
        .unwrap();
    assert_eq!(skill.status, ApplyStatus::Replaced);
    assert!(skill.detail.contains(&m.id), "{}", skill.detail);
    assert!(t.data_dir.join("skills/demo/SKILL.md").is_file());
    assert!(t.data_dir.join("skills/demo/references/extra.md").is_file());
    assert!(
        !t.data_dir.join("skills/demo/stale.md").exists(),
        "replace must not merge into the old tree"
    );

    // Validate re-reads what landed: a partial import would be loud.
    let v = validate(&p);
    assert!(v.complete, "{}", v.render());
    assert_eq!(v.problems(), 0);

    // Rollback restores exactly the pre-image and removes what apply added.
    let n = r.rollback(&m).unwrap();
    assert!(n >= 1);
    assert_eq!(
        fs::read_to_string(t.data_dir.join("skills/demo/SKILL.md")).unwrap(),
        "OLD\n"
    );
    assert!(
        !t.data_dir.join("skills/demo/references").exists(),
        "what apply added must be gone"
    );
}

#[test]
fn a_budget_breach_aborts_with_nothing_committed() {
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

    // The backup itself is budget-guarded too, and a failed backup leaves
    // no partial tree behind.
    fs::create_dir_all(t.data_dir.join("skills/demo")).unwrap();
    fs::write(t.data_dir.join("skills/demo/SKILL.md"), vec![b'y'; 1024]).unwrap();
    let before: usize = fs::read_dir(t.backup_root())
        .map(|rd| rd.count())
        .unwrap_or(0);
    let err = backup_with_budgets(&p, &t, &tiny_budgets()).expect_err("backup exceeds budget");
    assert!(err.to_string().contains("budget"), "{err}");
    let after: Vec<_> = fs::read_dir(t.backup_root())
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).collect())
        .unwrap_or_default();
    assert_eq!(
        after.len(),
        before,
        "a failed backup must leave no residue: {after:?}"
    );
}

#[test]
fn apply_never_dereferences_symlinks() {
    // A symlink inside an imported tree must never be followed: following
    // it would copy content from outside the source root.
    let d = tmp("symlink");
    let src = d.join("skills/demo");
    fs::create_dir_all(&src).unwrap();
    fs::write(src.join("SKILL.md"), "---\nname: demo\n---\n").unwrap();
    let outside = d.join("outside.txt");
    fs::write(&outside, "secret\n").unwrap();
    std::os::unix::fs::symlink(&outside, src.join("link.md")).unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    assert!(p.items.iter().any(|i| i.kind == ItemKind::Skill));
    let m = backup(&p, &t).unwrap();
    let r = apply(&p, &t, &m).unwrap();
    assert!(r.complete, "{:?}", r.outcomes);

    let landed = t.data_dir.join("skills/demo");
    assert!(landed.join("SKILL.md").is_file());
    assert!(
        !landed.join("link.md").exists(),
        "a symlink must never be dereferenced into the target"
    );
}

// ---------------------------------------------------------------------------
// validate routing
// ---------------------------------------------------------------------------

#[test]
fn validate_flags_missing_targets_and_routes_memory_plane_items() {
    let d = tmp("validate-routing");
    let t = targets(&d);

    let missing = MigrationPlan {
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
    let v = validate(&missing);
    assert!(!v.complete, "a missing target is a loud failure");
    assert_eq!(v.outcomes[0].status, ValidateStatus::Missing);

    let memory = MigrationPlan {
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
    let v = validate(&memory);
    assert!(v.complete, "routing to the memory plane is not a failure");
    assert_eq!(v.outcomes[0].status, ValidateStatus::NotApplicable);
}

// ---------------------------------------------------------------------------
// compat: hook mapping
// ---------------------------------------------------------------------------

#[test]
fn mapped_hook_is_a_promise_it_must_be_wired() {
    // `HookMap::Mapped` tells the operator their handler will run. The
    // invariant: whatever the table says, a mapped hook is always wired,
    // and a hook with no fire site is never reported as mapped.
    for e in KNOWN_FOREIGN_EVENTS {
        if let HookMap::Mapped(h) = map_hook(e) {
            assert!(h.is_wired(), "{e} mapped to unwired {}", h.name());
        }
    }

    // The load-bearing tool hooks map for real.
    assert_eq!(
        map_hook("before_tool_call"),
        HookMap::Mapped(Hook::PreToolCall)
    );
    assert_eq!(
        map_hook("after_tool_call"),
        HookMap::Mapped(Hook::PostToolCall)
    );
    // OMP compaction start/end collapse onto the single wired hook.
    assert_eq!(
        map_hook("auto_compaction_start"),
        HookMap::Mapped(Hook::OnCompaction)
    );
    assert_eq!(
        map_hook("auto_compaction_end"),
        HookMap::Mapped(Hook::OnCompaction)
    );
    assert!(Hook::OnCompaction.is_wired());

    // `pre_gateway_dispatch` is declared but has no fire site: mapping it
    // would promise the operator their handler runs, and it would not.
    for ev in [
        "message_received",
        "pre_gateway_dispatch",
        "gateway_dispatch",
        "before_dispatch",
        // The retry-fallback walk never reaches the core fan-out, so there
        // is no fire site either: reasoned-about Unsupported, never mapped.
        "auto_retry_start",
        "auto_retry_end",
        "retry_fallback_applied",
        "message_sending",
        "some_future_event",
    ] {
        assert_eq!(
            map_hook(ev),
            HookMap::Unsupported { nearest: None },
            "{ev} must not claim a mapped hook"
        );
    }

    // Spelling normalization: case and separators never change the mapping.
    for spelling in [
        "before_prompt_build",
        "BEFORE_PROMPT_BUILD",
        "before-prompt-build",
        " before_prompt_build ",
    ] {
        assert_eq!(
            map_hook(spelling),
            HookMap::Mapped(Hook::PreLlmCall),
            "{spelling}"
        );
    }
}

#[test]
fn scan_registrations_parses_real_plugin_shapes() {
    // Shaped like the actual openclaw-lark extension.
    let js = r#"
        api.on('before_tool_call', (event, ctx) => {});
        api.on('after_tool_call', (event, ctx) => {});
    "#;
    let (events, _) = scan_registrations(js);
    assert_eq!(events, vec!["before_tool_call", "after_tool_call"]);

    // Double-quoted spellings work too.
    let (events, _) = scan_registrations(r#"api.on("before_prompt_build", async (e,c) => {})"#);
    assert_eq!(events, vec!["before_prompt_build"]);

    // Registration is not confused with reads, and repeats are deduplicated.
    let (events, methods) = scan_registrations("const x = api.runtime; api.logger.info('hi');");
    assert!(events.is_empty());
    assert!(methods.is_empty());
    let (events, _) =
        scan_registrations("api.on('message_received', f); api.on('message_received', g);");
    assert_eq!(events.len(), 1);

    // Refusable capability methods are picked up alongside events.
    let (events, methods) = scan_registrations(
        "api.registerProvider({}); api.registerTool({}); api.on('before_prompt_build', f);",
    );
    assert_eq!(events, vec!["before_prompt_build"]);
    assert!(methods.contains(&"registerProvider".to_string()));
    assert!(methods.contains(&"registerTool".to_string()));
    assert!(!methods.contains(&"registerCli".to_string()));
}
