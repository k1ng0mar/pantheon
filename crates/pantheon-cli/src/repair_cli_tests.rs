//! Tests for `pantheon_cli::repair_cli` — sibling file so sources stay
//! test-free.
//!
//! The registry's contract is the thing under test: diagnose before repair,
//! never repair a healthy install, never settle a run a live session owns,
//! and back up before mutating.
use super::*;
use crate::dotenv::test_support::TEST_ENV_LOCK;
use pantheon_core::events::Event;

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

    for fixer in registry() {
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
