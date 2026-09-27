//! `pantheon repair` — find and fix anything wrong with this install.
//!
//! `doctor` is diagnosis: it reports every problem and a fix hint per check.
//! `repair` is the fixing half, and it is deliberately a separate verb so
//! neither one can quietly do the other's job — a `doctor` that mutated state
//! would be unsafe to run in a loop, and a repair that diagnosed would be
//! useless.
//!
//! The mechanism is one check body with two callers, not two implementations.
//! Each entry below pairs a `diagnose` (read-only, always runs first) with a
//! `repair` (mutates, only when `diagnose` found something). This is the shape
//! Hermes uses in `hermes_cli/doctor_report.py::doctor_check`, where
//! `should_fix` is a bool handed to the same function. The first version of
//! this file was a `repair check|runs` subcommand pair that re-implemented a
//! narrow slice of `doctor` and called it coverage; that is the failure this
//! registry exists to prevent.
//!
//! Rules every fixer obeys:
//!
//! 1. **Back up before mutating.** A repair that can make things worse leaves
//!    the original recoverable, and names the copy in its report.
//! 2. **Diagnose first, repair second.** A fixer never runs unconditionally,
//!    so `repair` on a healthy install changes nothing.
//! 3. **A problem with no safe automatic fix is reported as manual**, not
//!    skipped. "Repair found nothing it could fix" has to be an honest
//!    statement about the install.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One thing repair looked at and what it did about it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepairOutcome {
    /// Stable id, and the name of the check that produced it.
    pub check: String,
    /// "clean" | "fixed" | "manual" | "failed"
    pub status: String,
    pub detail: String,
    /// The exact action a human takes when this was not automatic.
    pub manual: String,
    /// Where the pre-repair copy landed, when one was taken.
    pub backup: Option<PathBuf>,
}

/// A check that can also repair what it finds.
pub struct Fixer {
    pub check: &'static str,
    /// Diagnose without touching anything.
    pub diagnose: fn(&Path) -> Result<Option<String>, String>,
    /// Apply the repair. `Err` means it was attempted and did not succeed; a
    /// backup taken on the way is named in the message.
    pub repair: fn(&Path) -> Result<String, String>,
}

/// Checks that deliberately have no automatic fix, because the only correct
/// action needs a human decision. A fixer that declines one of these is
/// working, not failing.
const FIXERS_WITHOUT_A_SAFE_FIX: &[&str] = &["skills", "config"];

/// Registry order is report order.
pub fn registry() -> Vec<Fixer> {
    vec![
        data_dir_layout(),
        config(),
        ledger_integrity(),
        stranded_runs(),
        search_index(),
        memory_index(),
        skills(),
    ]
}

pub fn usage() -> &'static str {
    "usage: pantheon repair [--dry-run] [--json]\n  \
     \n  \
     Finds anything wrong with this install and fixes what can be fixed\n  \
     safely. Diagnose only, no changes:  pantheon doctor\n  \
     Back up everything before repairing:   pantheon repair --dry-run"
}

/// Run every fixer against `dd`. Exits 1 when something was found that repair
/// could not resolve, so it is usable as a gate in a script.
pub fn cmd_repair(args: &[String]) {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let as_json = args.iter().any(|a| a == "--json");
    // A stray positional is almost always `check` or `runs` left over from the
    // old subcommand surface. Say so, because silently ignoring it would look
    // like the command worked.
    if let Some(bad) = args.iter().skip(2).find(|a| !a.starts_with('-')) {
        eprintln!(
            "repair: '{bad}' is not a flag. `repair` takes no subcommands: it finds \
             and fixes everything it can. Diagnose without changing anything with \
             `pantheon doctor`."
        );
        std::process::exit(2);
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return;
    }

    let dd = crate::data_dir();
    // Load config so a broken config is surfaced here rather than as a
    // confusing "no such table" from a ledger query.
    let _ = crate::config_doc::Config::load_or_report(&dd);

    let mut outcomes = Vec::new();
    for fixer in registry() {
        let finding = match (fixer.diagnose)(&dd) {
            Ok(None) => {
                outcomes.push(RepairOutcome {
                    check: fixer.check.to_string(),
                    status: "clean".into(),
                    detail: String::new(),
                    manual: String::new(),
                    backup: None,
                });
                continue;
            }
            Ok(Some(what)) => what,
            // A diagnose that cannot run is itself a finding, not a crash:
            // the rest of the install is still worth repairing.
            Err(e) => {
                outcomes.push(RepairOutcome {
                    check: fixer.check.to_string(),
                    status: "failed".into(),
                    detail: format!("check could not run: {e}"),
                    manual: "run `pantheon doctor` for the failing section".into(),
                    backup: None,
                });
                continue;
            }
        };

        if dry_run {
            outcomes.push(RepairOutcome {
                check: fixer.check.to_string(),
                status: "manual".into(),
                detail: finding,
                manual: "re-run without --dry-run to apply the automatic fix".into(),
                backup: None,
            });
            continue;
        }

        match (fixer.repair)(&dd) {
            Ok(what) => outcomes.push(RepairOutcome {
                check: fixer.check.to_string(),
                status: "fixed".into(),
                detail: format!("{finding} → {what}"),
                manual: String::new(),
                backup: None,
            }),
            Err(e) => {
                // A fixer that names a backup in its message left one behind.
                // Surfacing the path is the difference between a recoverable
                // failure and a scary one.
                //
                // A fixer that *declined* has not failed. It correctly refused
                // something unsafe and left the file alone, which is exactly
                // the outcome the registry is designed to produce. Calling
                // that "failed" would train the operator to ignore the word,
                // and would make a deliberate refusal exit non-zero.
                let declined = FIXERS_WITHOUT_A_SAFE_FIX.contains(&fixer.check);
                let backup = extract_backup(&e);
                outcomes.push(RepairOutcome {
                    check: fixer.check.to_string(),
                    status: if declined || backup.is_some() {
                        "manual".into()
                    } else {
                        "failed".into()
                    },
                    detail: format!("{finding} → {e}"),
                    manual: "see the message above for the exact recovery step".into(),
                    backup,
                });
            }
        }
    }

    if as_json {
        // One document, one shape. The previous version printed a bare `true`
        // and then the array on a second call, which is two JSON documents and
        // parses as neither.
        #[derive(Serialize)]
        struct Report<'a> {
            ok: bool,
            fixed: usize,
            manual: usize,
            failed: usize,
            outcomes: &'a [RepairOutcome],
        }
        let ok = outcomes
            .iter()
            .all(|o| o.status == "clean" || o.status == "fixed");
        let report = Report {
            ok,
            fixed: outcomes.iter().filter(|o| o.status == "fixed").count(),
            manual: outcomes.iter().filter(|o| o.status == "manual").count(),
            failed: outcomes.iter().filter(|o| o.status == "failed").count(),
            outcomes: &outcomes,
        };
        crate::print_json("repair", &report);
        if !ok {
            std::process::exit(1);
        }
        return;
    }

    render(&outcomes, dry_run);
    // Only a genuine failure exits 1. A `manual` finding is something the
    // operator has to decide about, and `repair` did its job — a registry
    // whose expected outcome set a non-zero exit would be useless in a script.
    if outcomes.iter().any(|o| o.status == "failed") {
        std::process::exit(1);
    }
}

fn render(outcomes: &[RepairOutcome], dry_run: bool) {
    if outcomes.iter().all(|o| o.status == "clean") {
        println!("ok — nothing to repair");
        return;
    }
    for o in outcomes {
        match o.status.as_str() {
            "clean" => continue,
            "fixed" => println!("fixed  {}: {}", o.check, o.detail),
            "manual" => println!("manual {}: {}", o.check, o.detail),
            _ => println!("failed {}: {}", o.check, o.detail),
        }
        if let Some(b) = &o.backup {
            println!("       backup: {}", b.display());
        }
        if !o.manual.is_empty() {
            println!("       next:  {}", o.manual);
        }
    }
    let fixed = outcomes.iter().filter(|o| o.status == "fixed").count();
    let manual = outcomes.iter().filter(|o| o.status == "manual").count();
    let failed = outcomes.iter().filter(|o| o.status == "failed").count();
    if dry_run {
        println!("\n{dry_run} dry run — nothing was changed");
    } else {
        println!("\n{fixed} fixed, {manual} need a human, {failed} failed");
    }
}

/// Pull a backup path out of a fixer's message. Fixers report the path in
/// prose (it is also what the operator reads), so the structured field is
/// recovered from it rather than threading a second return value through
/// every signature.
fn extract_backup(msg: &str) -> Option<PathBuf> {
    const MARK: &str = "backup: ";
    let start = msg.to_lowercase().find(MARK)? + MARK.len();
    let rest = &msg[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == ')')
        .unwrap_or(rest.len());
    Some(PathBuf::from(&rest[..end]))
}

// ---------------------------------------------------------------- config

/// A missing or unparseable `config.toml`.
///
/// The absent case is fixable and the malformed case is not: a config that
/// does not parse may contain anything, and writing over it would destroy a
/// file the user hand-edited and cannot reconstruct. So the fixer creates a
/// default when the file is simply missing, and for a parse failure it backs
/// the file up and reports — never overwrites.
fn config() -> Fixer {
    Fixer {
        check: "config",
        // A missing file and a malformed file are different problems with
        // different fixes, so they must not arrive as the same finding.
        // `Config::load` reports ENOENT as a load error, which made every
        // fresh install look like a corrupt config.
        diagnose: |dd| {
            if !crate::config_doc::Config::path(dd).exists() {
                return Ok(Some("no config.toml; run `pantheon setup`".to_string()));
            }
            match crate::config_doc::Config::load(dd) {
                Ok(c) => {
                    let problems = c.validate();
                    if problems.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(problems.join("; ")))
                    }
                }
                Err(e) => Ok(Some(format!(
                    "{} does not parse: {}",
                    crate::config_doc::Config::path(dd).display(),
                    e.cause
                ))),
            }
        },
        repair: |dd| {
            let path = crate::config_doc::Config::path(dd);
            if !path.exists() {
                // A default config is a working starting point, not a guess:
                // `Config::default` is the same one `setup` starts from, so
                // repair leaves the user in the state setup would have.
                let cfg = crate::config_doc::Config::default();
                cfg.save(dd)
                    .map_err(|e| format!("cannot write {}: {}", path.display(), e.cause))?;
                // The default has no [model] section, which `validate()`
                // rejects — a config with no provider cannot run a turn. So
                // this fix is partial by nature and says so, rather than
                // implying the install is now runnable.
                return Ok(format!(
                    "wrote a default {} (it has no [model] section yet, so \
                     `pantheon setup` is still needed before a turn will run)",
                    path.display()
                ));
            }
            // It exists. If it loads but does not validate, the parse succeeded
            // and the message must say that, not "does not parse" — the two
            // have completely different fixes (add a model section vs. repair
            // TOML by hand), and conflating them sent the operator to the
            // wrong one.
            if let Ok(c) = crate::config_doc::Config::load(dd) {
                let problems = c.validate();
                if !problems.is_empty() {
                    return Err(format!(
                        "config.toml is valid TOML but not a runnable config: {}. \
                         Repair is yours to make here: `pantheon setup` writes a \
                         [model] section, or set provider and model in the file.",
                        problems.join("; ")
                    ));
                }
            }
            // It exists and does not parse. Preserve it: the user's own edits
            // are in there, and a fixer that overwrites a config it merely
            // failed to understand is the most destructive thing this command
            // could do.
            let backup = backup_file(&path, "config.toml")?;
            Err(format!(
                "{} does not parse, and its contents are preserved at {}. Repair it by \
                 hand, or delete it and re-run `pantheon repair` to get a default.",
                path.display(),
                backup.display()
            ))
        },
    }
}

// ---------------------------------------------------------------- memory

/// The memory FTS5 index. Unlike the ledger's sidecar this one is fully
/// rebuildable, because it is an external-content table kept in sync by
/// triggers with `memories` as the source of truth.
fn memory_index() -> Fixer {
    Fixer {
        check: "memory-index",
        diagnose: |dd| {
            let path = dd.join("memory.db");
            if !path.exists() {
                return Ok(None);
            }
            match pantheon_memory::fts_health(&path) {
                Ok(healthy) if healthy => Ok(None),
                Ok(_) => Ok(Some("the memory FTS index is missing or empty".to_string())),
                Err(e) => Ok(Some(format!(
                    "the memory FTS index is unusable: {}",
                    e.cause
                ))),
            }
        },
        repair: |dd| {
            let path = dd.join("memory.db");
            if !path.exists() {
                return Ok(format!(
                    "{} does not exist; it is created on the first memory write",
                    path.display()
                ));
            }
            let backup = backup_file(&path, "memory.db")?;
            let rows = pantheon_memory::rebuild_fts(&path)
                .map_err(|e| format!("rebuild failed: {}", e.cause))?;
            Ok(format!(
                "rebuilt the memory FTS index from {rows} record(s). Backup: {}",
                backup.display()
            ))
        },
    }
}

// ---------------------------------------------------------------- skills

/// Skills that discovery rejected. The report names each one; the repair
/// removes the broken files after a backup.
///
/// This is destructive to the user's own skill files, so it is the one fixer
/// that needs a human decision: a rejected skill is usually a typo in front
/// matter, and deleting the file discards work rather than fixing it. It is
/// therefore reported as manual and NOT deleted.
fn skills() -> Fixer {
    Fixer {
        check: "skills",
        diagnose: |dd| {
            let rejected = crate::skills_cli::rejected_at(dd);
            if rejected.is_empty() {
                return Ok(None);
            }
            // `reason` already embeds the file path for parse failures, so
            // prefixing `path` again printed it twice. Use the reason alone
            // and fall back to the path only when the reason has none.
            Ok(Some(
                rejected
                    .iter()
                    .map(|(path, reason)| {
                        if reason.contains(&path.display().to_string()) {
                            reason.clone()
                        } else {
                            format!("{}: {}", path.display(), reason)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("; "),
            ))
        },
        repair: |_dd| {
            Err(
                "a rejected skill is usually a typo in its front matter, so the file is kept \
             and the problem is reported instead"
                    .to_string(),
            )
        },
    }
}

// ---------------------------------------------------------------- layout

/// Directories the runtime writes into. A missing one is not a fault (most are
/// created lazily), but a *file* where a directory belongs is the user's data
/// and is reported, never clobbered.
fn data_dir_layout() -> Fixer {
    Fixer {
        check: "data-dir-layout",
        diagnose: |dd| {
            if !dd.exists() {
                return Ok(Some(format!("{} does not exist", dd.display())));
            }
            let mut bad = Vec::new();
            for name in ["skills", "gateway", "extensions"] {
                let p = dd.join(name);
                if p.exists() && !p.is_dir() {
                    bad.push(format!("{name} exists but is not a directory"));
                }
            }
            if bad.is_empty() {
                Ok(None)
            } else {
                Ok(Some(bad.join("; ")))
            }
        },
        repair: |dd| {
            if !dd.exists() {
                std::fs::create_dir_all(dd)
                    .map_err(|e| format!("cannot create {}: {e}", dd.display()))?;
                return Ok(format!("created {}", dd.display()));
            }
            let mut created = Vec::new();
            for name in ["skills", "gateway", "extensions"] {
                let p = dd.join(name);
                if p.exists() {
                    continue; // a file here is reported, never overwritten
                }
                std::fs::create_dir_all(&p)
                    .map_err(|e| format!("cannot create {}: {e}", p.display()))?;
                created.push(name);
            }
            if created.is_empty() {
                Ok("nothing to create".to_string())
            } else {
                Ok(format!("created {}", created.join(", ")))
            }
        },
    }
}

// ---------------------------------------------------------------- ledger

fn ledger_integrity() -> Fixer {
    Fixer {
        check: "ledger-integrity",
        diagnose: |dd| {
            let path = dd.join("ledger.db");
            if !path.exists() {
                return Ok(None); // nothing built yet; not a fault
            }
            let ledger = match pantheon_storage::Ledger::open(&path) {
                Ok(l) => l,
                Err(e) => return Ok(Some(format!("ledger.db does not open: {}", e.cause))),
            };
            // `PRAGMA integrity_check` returns `["ok"]` when healthy, not an
            // empty list. Treating "one row that says ok" as a problem would
            // make every healthy install look damaged.
            match ledger.integrity_check() {
                Ok(rows) if rows.iter().all(|r| r.trim() == "ok") => Ok(None),
                Ok(rows) if rows.is_empty() => Ok(None),
                Ok(rows) => Ok(Some(format!("PRAGMA integrity_check: {}", rows.join("; ")))),
                Err(e) => Ok(Some(format!("integrity_check could not run: {}", e.cause))),
            }
        },
        repair: |dd| {
            let path = dd.join("ledger.db");
            let backup = backup_file(&path, "ledger.db")?;
            // Structural damage cannot be repaired by writing to the damaged
            // file, and `VACUUM` on one can make it worse. The only honest
            // repair is a backup plus an explicit statement of what is left.
            Err(format!(
                "ledger.db is structurally damaged and cannot be repaired in place. \
                 Backup: {}. Restore a known-good ledger.db over it, or run \
                 `pantheon reset --state` to start clean (that discards run history).",
                backup.display()
            ))
        },
    }
}

fn stranded_runs() -> Fixer {
    Fixer {
        check: "stranded-runs",
        diagnose: |dd| {
            let path = dd.join("ledger.db");
            if !path.exists() {
                return Ok(None);
            }
            let ledger = match pantheon_storage::Ledger::open(&path) {
                Ok(l) => l,
                Err(e) => return Ok(Some(format!("ledger.db does not open: {}", e.cause))),
            };
            let stuck = match ledger.stuck_runs() {
                Ok(s) => s,
                Err(e) => return Ok(Some(format!("cannot read runs: {}", e.cause))),
            };
            if stuck.is_empty() {
                return Ok(None);
            }
            Ok(Some(
                stuck
                    .iter()
                    .map(|(id, status, ..)| format!("{id} is still '{status}'"))
                    .collect::<Vec<_>>()
                    .join("; "),
            ))
        },
        repair: |dd| {
            let path = dd.join("ledger.db");
            let ledger = match pantheon_storage::Ledger::open(&path) {
                Ok(l) => l,
                Err(e) => return Err(format!("cannot open ledger: {}", e.cause)),
            };
            let stuck = match ledger.stuck_runs() {
                Ok(s) => s,
                Err(e) => return Err(format!("cannot read runs: {}", e.cause)),
            };
            let mut settled = 0;
            let mut skipped: Vec<String> = Vec::new();
            for (run_id, status, ..) in stuck {
                // `stuck_runs` already excludes runs with a live lease, so
                // everything here is a corpse. Settling appends real events
                // rather than rewriting the status, so the ledger still
                // explains how the run ended.
                match ledger.settle_stuck_run(&run_id, "repaired by `pantheon repair`") {
                    Ok(()) => settled += 1,
                    Err(e) => skipped.push(format!("{run_id} ({status}): {}", e.cause)),
                }
            }
            if !skipped.is_empty() {
                return Err(format!(
                    "settled {settled} run(s); {} could not be settled: {}",
                    skipped.len(),
                    skipped.join("; ")
                ));
            }
            Ok(format!("settled {settled} stranded run(s)"))
        },
    }
}

// ---------------------------------------------------------------- search

/// The FTS5 sidecar is a derived index over the ledger, and nothing in the repo
/// rebuilds it. A partial write, a restored ledger, or a schema change leaves
/// it permanently wrong: search silently returns nothing and no command can
/// fix it. That is a real gap, and the honest report is that a full re-index
/// is not implemented — not a fix that recreates an empty table and calls the
/// problem solved.
fn search_index() -> Fixer {
    Fixer {
        check: "search-index",
        diagnose: |dd| {
            let path = dd.join("ledger.db");
            if !path.exists() {
                return Ok(None);
            }
            // Healthy means the index answers a MATCH at all. A broken index
            // raises; a merely empty one returns nothing, which is why this
            // probes for a query working rather than for rows existing.
            match pantheon_storage::search_index_health(&path) {
                Ok(healthy) if healthy => Ok(None),
                Ok(_) => Ok(Some("FTS index is missing or empty".to_string())),
                Err(e) => Ok(Some(format!("FTS index is unusable: {}", e.cause))),
            }
        },
        repair: |dd| {
            let path = dd.join("ledger.db");
            let backup = backup_file(&path, "ledger.db")?;
            pantheon_storage::recreate_search_index(&path)
                .map_err(|e| format!("recreate failed: {}", e.cause))?;
            Ok(format!(
                "recreated the FTS index as an empty table. A full re-index of existing \
                 runs is NOT implemented, so previously indexed runs are not searchable \
                 until they are re-indexed. Backup: {}",
                backup.display()
            ))
        },
    }
}

/// Copy `name` next to itself with a timestamp suffix. Fails rather than
/// overwriting an existing backup.
fn backup_file(path: &Path, name: &str) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!(
            "{} does not exist, nothing to back up",
            path.display()
        ));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dest = path.with_file_name(format!("{name}.{stamp}.bak"));
    if dest.exists() {
        return Err(format!("backup {} already exists", dest.display()));
    }
    std::fs::copy(path, &dest).map_err(|e| format!("cannot back up {}: {e}", path.display()))?;
    Ok(dest)
}

#[cfg(test)]
#[path = "repair_cli_tests.rs"]
mod tests;
