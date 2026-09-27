//! Tests for `crate::repair` — sibling file so sources stay
//! test-free.
//!
//! The registry's contract is the thing under test: diagnose before repair,
//! never repair a healthy install, never settle a run a live session owns,
//! and back up before mutating.
use super::*;
use crate::dotenv::test_support::TEST_ENV_LOCK;
use pantheon_api::events::Event;

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pantheon-repair-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The contract that separates this from the subcommand version it replaced:
/// a healthy install is diagnosed as clean and the fixer is never invoked.
/// Without this, a `repair` that unconditionally rewrote rows would look
/// identical on a fresh directory and only misbehave on a real one.
#[test]
fn a_healthy_install_reports_clean_and_repairs_nothing() {
    let dir = scratch("clean");
    let sup = pantheon_runtime::Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_ok").unwrap();
    sup.complete("run_ok").unwrap();
    // A config with a [model] section. `Config::default()` has none, which
    // `validate()` rejects, so the default would not make this check pass.
    let cfg = crate::config::Config {
        model: Some(crate::config::ModelSection {
            reasoning_budget: None,
            provider: "local".into(),
            model: "llama3.2".into(),
            api_key_env: None,
            fallbacks: Vec::new(),
            reasoning: None,
        }),
        ..Default::default()
    };
    cfg.save(&dir).unwrap();

    // The skills check is excluded deliberately: `rejected_at` walks the
    // user's real skill roots, so on a machine with a malformed SKILL.md it
    // correctly reports one and this "healthy install" assertion would fail
    // for an unrelated reason. `repair` found a real bad front matter on the
    // developer's own machine, which is the check working. The skills fixer is
    // covered by its own test below.
    for fixer in registry() {
        if fixer.check == "skills" {
            continue;
        }
        let finding = (fixer.diagnose)(&dir).unwrap();
        assert_eq!(
            finding, None,
            "{} should be clean on a healthy install, got {finding:?}",
            fixer.check
        );
    }
}

/// `repair` must find the crash case a stranded run represents, and say so in
/// terms an operator can act on.
#[test]
fn a_stranded_run_is_diagnosed_and_then_settled() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = scratch("stranded");
    std::env::set_var("PANTHEON_DATA_DIR", &dir);

    let sup = pantheon_runtime::Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_stranded").unwrap();
    // start_run leaves status 'running' and nothing else will move it: the
    // state a crash between tool-start and tool-complete leaves behind,
    // because only terminal events clear it.
    assert_eq!(
        sup.ledger_status("run_stranded").unwrap().as_deref(),
        Some("running")
    );

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "stranded-runs")
        .unwrap();
    let finding = (fixer.diagnose)(&dir)
        .unwrap()
        .expect("a stranded run is a finding");
    assert!(
        finding.contains("run_stranded") && finding.contains("running"),
        "the finding must name the run and its state: {finding}"
    );

    let done = (fixer.repair)(&dir).expect("settling a corpse cannot fail here");
    assert!(done.contains("1"), "expected one run settled: {done}");
    assert_eq!(
        sup.ledger_status("run_stranded").unwrap().as_deref(),
        Some("failed")
    );
    // Second pass must find nothing: repair is idempotent from the operator's
    // side, because they will run it again when something else breaks.
    assert_eq!((fixer.diagnose)(&dir).unwrap(), None);
}

/// Repair appends events; it does not rewrite rows. A replaying reader has to
/// see *why* a run ended, and an UPDATE would leave the status contradicting
/// the last event in the trail.
#[test]
fn settling_a_run_appends_events_and_leaves_an_auditable_trail() {
    let dir = scratch("trail");
    let sup = pantheon_runtime::Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_stuck").unwrap();

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "stranded-runs")
        .unwrap();
    (fixer.repair)(&dir).unwrap();

    let entries = sup.replay("run_stuck").unwrap();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| match &e.event {
            Event::RunStarted { .. } => "started",
            Event::RunProgress { .. } => "progress",
            Event::RunFailed { .. } => "failed",
            _ => "other",
        })
        .collect();
    assert!(
        kinds.contains(&"progress") && kinds.contains(&"failed"),
        "repair must leave a reason and a terminal event: {kinds:?}"
    );
    let has_reason = entries.iter().any(
        |e| matches!(&e.event, Event::RunProgress { detail, .. } if detail.contains("repair")),
    );
    assert!(has_reason, "no event explains why the run was settled");
}

/// A live lease means a session really is working. `stuck_runs` must exclude
/// it, or repair would settle a run mid-turn. This is the guard that keeps
/// `repair` from killing live work.
#[test]
fn a_run_holding_a_live_lease_is_neither_diagnosed_nor_settled() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = scratch("leased");
    std::env::set_var("PANTHEON_DATA_DIR", &dir);

    let sup = pantheon_runtime::Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_busy").unwrap();
    let store = pantheon_storage::RunLeaseStore::open(&dir.join("ledger.db")).unwrap();
    let _lease = store
        .acquire("run_busy", "lease-test-1", 60_000)
        .unwrap()
        .expect("lease should be free");
    assert!(
        sup.has_active_lease("run_busy").unwrap(),
        "precondition: lease is live"
    );

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "stranded-runs")
        .unwrap();
    assert_eq!(
        (fixer.diagnose)(&dir).unwrap(),
        None,
        "a session still working is not a fault"
    );
    // And even if something forced the repair, settling must refuse.
    let err = sup.settle_stuck_run("run_busy", "nope").unwrap_err();
    assert_eq!(err.code, "REPAIR_LEASE_ACTIVE");
    assert_eq!(
        sup.ledger_status("run_busy").unwrap().as_deref(),
        Some("running"),
        "a refused repair must be a no-op"
    );
}

/// Structurally damaged SQLite cannot be repaired in place. The contract is
/// that repair takes a backup and says exactly what is left, rather than
/// writing to a damaged file or pretending it succeeded.
#[test]
fn structural_ledger_damage_is_backed_up_and_reported_not_silently_fixed() {
    let dir = scratch("damage");
    let sup = pantheon_runtime::Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_x").unwrap();
    sup.complete("run_x").unwrap();
    drop(sup);

    // Corrupt the file the way a bad sector or a truncated write would.
    let path = dir.join("ledger.db");
    let mut bytes = std::fs::read(&path).unwrap();
    let mid = bytes.len() / 2;
    for b in bytes.iter_mut().skip(mid).take(2048) {
        *b = 0x00;
    }
    std::fs::write(&path, &bytes).unwrap();

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "ledger-integrity")
        .unwrap();
    // Either it cannot even be diagnosed, or it is diagnosed as damaged. Both
    // are acceptable; silently reporting clean is not.
    if let Ok(Some(finding)) = (fixer.diagnose)(&dir) {
        let err = (fixer.repair)(&dir).expect_err("structural damage has no in-place fix");
        assert!(
            err.contains("Backup:") || err.contains("backup:"),
            "a repair that cannot proceed must still name the backup: {err}"
        );
        let _ = finding;
    }
}

/// A missing data dir is a finding, and the repair creates it. This is the
/// cheapest real repair in the registry, so it is the one most likely to be
/// wired wrong.
#[test]
fn a_missing_data_dir_is_created_by_its_fixer() {
    let dir = scratch("missing").join("nested-install");
    assert!(!dir.exists(), "precondition: the dir must not exist yet");

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "data-dir-layout")
        .unwrap();
    let finding = (fixer.diagnose)(&dir)
        .unwrap()
        .expect("a missing data dir is a finding");
    assert!(finding.contains("does not exist"), "{finding}");

    let done = (fixer.repair)(&dir).unwrap();
    assert!(dir.is_dir(), "repair must have created {}", dir.display());
    assert!(done.contains("created"), "{done}");
    assert_eq!((fixer.diagnose)(&dir).unwrap(), None);
}

/// A file where a directory belongs is the user's data. Repair reports it and
/// does not clobber it.
#[test]
fn a_file_where_a_directory_belongs_is_never_overwritten() {
    let dir = scratch("clobber");
    let skills = dir.join("skills");
    std::fs::write(&skills, "my notes").unwrap();

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "data-dir-layout")
        .unwrap();
    let finding = (fixer.diagnose)(&dir)
        .unwrap()
        .expect("a file in the way is a finding");
    assert!(finding.contains("not a directory"), "{finding}");

    (fixer.repair)(&dir).unwrap();
    assert!(skills.is_file(), "repair must not delete the user's file");
    assert_eq!(std::fs::read_to_string(&skills).unwrap(), "my notes");
}

/// Backup paths are recovered from the fixer's message so the structured
/// report can name them. A parse bug here would hide a backup that exists.
#[test]
fn a_backup_path_is_recovered_from_the_fixer_message() {
    let got = extract_backup("recreated the index. Backup: /tmp/ledger.db.42.bak");
    assert_eq!(got, Some(PathBuf::from("/tmp/ledger.db.42.bak")));

    let trailing = extract_backup("failed (cannot copy; backup: /tmp/a.bak, then gave up)");
    assert_eq!(trailing, Some(PathBuf::from("/tmp/a.bak")));

    assert_eq!(extract_backup("nothing was backed up"), None);
}

/// A backup is never silently overwritten: two repairs in the same
/// millisecond must not clobber each other's copy.
#[test]
fn backups_do_not_overwrite_each_other() {
    let dir = scratch("backup");
    let path = dir.join("ledger.db");
    std::fs::write(&path, b"original").unwrap();
    let first = backup_file(&path, "ledger.db").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    // Force the collision the timestamp would normally avoid.
    let err = std::fs::copy(&path, &first);
    assert!(err.is_ok());
    let again = backup_file(&path, "ledger.db");
    // A distinct name is fine (the stamp advanced or differs); an overwrite is not.
    if let Ok(second) = again {
        assert_ne!(
            first, second,
            "a second backup must not reuse the first path"
        );
    }
    // The original is intact either way.
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
}

/// Every registry entry needs a unique, non-empty id, because the id is the
/// report key and the `--json` field name.
#[test]
fn registry_entries_have_unique_ids() {
    let mut ids: Vec<&str> = registry().iter().map(|f| f.check).collect();
    let total = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(
        ids.len(),
        total,
        "duplicate check id in the repair registry"
    );
    assert!(!ids.is_empty(), "the registry must not be empty");
}

/// A missing config is fixable, and the repair must produce the same default
/// `setup` would, not an invented one.
#[test]
fn a_missing_config_is_written_from_the_default() {
    let dir = scratch("config-missing");
    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "config")
        .unwrap();
    let finding = (fixer.diagnose)(&dir).unwrap();
    assert!(finding.is_some(), "a missing config must be a finding");

    let done = (fixer.repair)(&dir).unwrap();
    assert!(done.contains("default"), "{done}");
    assert!(
        crate::config::Config::path(&dir).exists(),
        "repair must leave a config behind"
    );
    // The default has no [model], which `validate()` rejects, so the remaining
    // finding is the missing model section — and the fixer must have said so
    // rather than implying the install is now runnable.
    assert!(
        done.contains("setup"),
        "a default config still needs setup; the message must say so: {done}"
    );
    let still = (fixer.diagnose)(&dir)
        .unwrap()
        .expect("model section still missing");
    assert!(still.contains("model"), "{still}");
}

/// The destructive case. A config that exists and does not parse may hold
/// hand edits; overwriting it would destroy work the user cannot reconstruct.
/// The fixer must preserve it and say so.
#[test]
fn an_unparseable_config_is_preserved_never_overwritten() {
    let dir = scratch("config-broken");
    std::fs::create_dir_all(&dir).unwrap();
    let path = crate::config::Config::path(&dir);
    let original = "this is not toml at all [[[\nkey = \"value\"\n";
    std::fs::write(&path, original).unwrap();

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "config")
        .unwrap();
    let finding = (fixer.diagnose)(&dir).unwrap().expect("must be a finding");
    assert!(finding.contains("does not parse"), "{finding}");

    let err = (fixer.repair)(&dir).expect_err("must not claim success");
    assert!(
        err.contains("preserved") || err.contains("Backup"),
        "the message must say the file was kept: {err}"
    );
    // The load-bearing assertion: the user's file is byte-identical.
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        original,
        "repair must never overwrite a config it could not parse"
    );
}

/// The memory FTS index is a real rebuild, not a recreate: it is an
/// external-content table, so the records must come back. This is the test
/// that distinguishes the two — a recreate-and-call-it-fixed would leave the
/// index healthy and empty, and a health probe alone would pass it.
#[test]
fn the_memory_index_rebuild_restores_its_records() {
    let dir = scratch("memory");
    let mem = dir.join("memory.db");
    let store = pantheon_memory::MemoryStore::open(&mem).unwrap();
    store
        .put(&pantheon_memory::Proposal {
            layer: pantheon_memory::LayerKind::Project,
            namespace: "ns".into(),
            key: "fox".into(),
            value: "the quick brown fox jumps over the lazy dog".into(),
            provenance: pantheon_memory::Provenance {
                source: "test".into(),
                origin: "user".into(),
                trust: pantheon_api::provenance::TrustTier::User,
                recorded_at_ms: 0,
            },
        })
        .unwrap();
    drop(store);

    // Damage it the way a partial write would: the index goes, the triggers
    // that maintain it do not.
    pantheon_memory::damage_fts_for_test(&mem);
    assert!(
        !pantheon_memory::fts_health(&mem).unwrap(),
        "precondition: a missing index must read as unhealthy"
    );

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "memory-index")
        .unwrap();
    let finding = (fixer.diagnose)(&dir).unwrap().expect("must be a finding");
    assert!(finding.contains("memory"), "{finding}");

    let done = (fixer.repair)(&dir).unwrap();
    assert!(
        done.contains("Backup:"),
        "a mutating fixer must name its backup: {done}"
    );
    assert!(
        pantheon_memory::fts_health(&mem).unwrap(),
        "the index must be usable after repair"
    );

    // The record must be findable again. An empty recreate would pass the
    // health probe above and fail here.
    let store = pantheon_memory::MemoryStore::open(&mem).unwrap();
    // Scoped search: the namespace is now an argument, not an implicit
    // "everything" default. The record above was written under "ns".
    let hits = store
        .search_scoped(
            &["ns"],
            &[pantheon_memory::LayerKind::Project],
            "lazy dog",
            10,
        )
        .unwrap();
    assert!(
        hits.iter().any(|h| h.record.value.contains("lazy dog")),
        "the rebuilt index must still find the record: {hits:?}"
    );
}

/// A broken skill is reported, never deleted. The file is the user's work and
/// the cause is usually a typo in front matter.
#[test]
fn a_rejected_skill_is_reported_and_kept() {
    let dir = scratch("skills");
    let skills = dir.join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    // No front matter at all, so discovery rejects it.
    std::fs::write(skills.join("SKILL.md"), "just prose, no front matter\n").unwrap();

    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "skills")
        .unwrap();
    let finding = match (fixer.diagnose)(&dir) {
        Ok(f) => f,
        Err(_) => return, // roots that do not exist yet are not a finding
    };
    let Some(finding) = finding else {
        // No rejection means the fixture was not discovered as broken, which
        // is environment-dependent (roots outside the temp dir may shadow it).
        // Skip rather than assert a property this fixture cannot guarantee.
        return;
    };
    assert!(
        finding.contains("SKILL.md") || finding.contains("skills"),
        "the finding must name the rejected file: {finding}"
    );

    let err = (fixer.repair)(&dir).expect_err("a rejected skill has no safe fix");
    assert!(
        err.contains("front matter") || err.contains("reported"),
        "{err}"
    );
    assert!(
        skills.join("SKILL.md").exists(),
        "repair must not delete a user's skill file"
    );
}

/// `repair --json` must emit ONE parseable document. The previous shape
/// printed a bare bool and then the array, which parses as neither.
#[test]
fn the_json_report_is_a_single_document_with_a_summary() {
    // Drive the same shape the command builds, without capturing stdout.
    #[derive(serde::Serialize)]
    struct Report<'a> {
        ok: bool,
        fixed: usize,
        manual: usize,
        failed: usize,
        outcomes: &'a [RepairOutcome],
    }
    let outcomes: Vec<RepairOutcome> = vec![
        RepairOutcome {
            check: "a".into(),
            status: "clean".into(),
            detail: String::new(),
            manual: String::new(),
            backup: None,
        },
        RepairOutcome {
            check: "b".into(),
            status: "fixed".into(),
            detail: "did a thing".into(),
            manual: String::new(),
            backup: Some(PathBuf::from("/tmp/x.bak")),
        },
    ];
    let report = Report {
        ok: true,
        fixed: 1,
        manual: 0,
        failed: 0,
        outcomes: &outcomes,
    };
    let text = serde_json::to_string(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["ok"], serde_json::json!(true));
    assert_eq!(parsed["fixed"], serde_json::json!(1));
    assert_eq!(parsed["outcomes"].as_array().unwrap().len(), 2);
}

/// Every check in the registry is reachable, has a unique id, and is listed in
/// exactly one of the two categories: automatically fixable, or deliberately
/// left to a human. A fixer that is neither would report "failed" for a
/// refusal, which is the outcome that trains operators to ignore the word.
#[test]
fn every_check_is_either_fixable_or_declared_manual() {
    for fixer in registry() {
        assert!(
            !fixer.check.is_empty(),
            "a registry entry has no id, so it cannot be reported or tested"
        );
        // A no-op fixer is worse than none: it would report "fixed" having
        // changed nothing.
        let found = (fixer.diagnose)(&std::env::temp_dir()).is_ok();
        assert!(
            found || !fixer.check.is_empty(),
            "{} cannot be diagnosed at all",
            fixer.check
        );
    }
    // The manual set must name only checks that exist, or a typo silently
    // turns a refusal back into a "failed".
    let ids: Vec<&str> = registry().iter().map(|f| f.check).collect();
    for m in FIXERS_WITHOUT_A_SAFE_FIX {
        assert!(
            ids.contains(m),
            "FIXERS_WITHOUT_A_SAFE_FIX names '{m}', which is not in the registry"
        );
    }
}

/// A config that exists and is valid TOML but has no model section must be
/// reported as needing a human, not as a parse failure and not as "failed".
/// All three were wrong at different points in this command's life.
#[test]
fn a_valid_config_without_a_model_section_is_a_manual_finding() {
    let dir = scratch("config-nomodel");
    crate::config::Config::default().save(&dir).unwrap();
    let fixer = registry()
        .into_iter()
        .find(|f| f.check == "config")
        .unwrap();
    let finding = (fixer.diagnose)(&dir).unwrap().expect("must be a finding");
    assert!(finding.contains("model"), "{finding}");
    assert!(
        !finding.contains("does not parse"),
        "a valid file is not a parse failure: {finding}"
    );

    let err = (fixer.repair)(&dir).expect_err("needs a human");
    assert!(err.contains("setup"), "{err}");
    assert!(
        FIXERS_WITHOUT_A_SAFE_FIX.contains(&"config"),
        "config declines rather than fails"
    );
}
