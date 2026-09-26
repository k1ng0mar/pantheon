//! `pantheon repair <check|runs>` — find and fix the things a crash leaves behind.
//!
//! Nothing in the repo did this. `reset` deletes state, which is the
//! opposite of repair: a user whose run is stuck in `running` after a crash
//! had exactly two options, do nothing or lose the ledger. And a corrupted
//! database surfaced as a confusing read error downstream with no way to tell
//! "your data is damaged" from "this query has a bug".
//!
//! `check` is read-only and safe to run any time. `runs` is the only
//! mutating half, it appends real events rather than rewriting rows, and it
//! refuses while a run holds a live lease.

use crate::config_doc::Config;
use pantheon_runtime::Supervisor;

fn usage() -> &'static str {
    "usage:\n  \
     pantheon repair check              # integrity + stuck runs (read-only)\n  \
     pantheon repair runs               # settle runs stranded by a crash\n  \
     pantheon repair runs <run_id>      # settle one named run"
}

/// One problem worth showing the operator.
struct Finding {
    severity: &'static str,
    what: String,
    fix: String,
}

pub fn cmd_repair(args: &[String]) {
    let sub = match args.get(2).map(String::as_str) {
        Some(s) => s,
        None => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };
    let dd = crate::data_dir();
    // Load config so a broken config is reported here rather than as a
    // confusing "no such table" from the ledger.
    let _ = Config::load_or_report(&dd);

    let sup = match Supervisor::open(dd.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("repair: open ledger: {e}");
            std::process::exit(1);
        }
    };

    match sub {
        "check" => check(&sup),
        "runs" => repair_runs(&sup, args.get(3).map(String::as_str)),
        other => {
            eprintln!("repair: unknown subcommand '{other}'");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}

/// Read-only diagnosis. Exits 1 when something is wrong, so it is usable as a
/// health gate in a script, and 0 when clean.
fn check(sup: &Supervisor) {
    let mut findings: Vec<Finding> = Vec::new();

    match sup.integrity_check() {
        Ok(rows) => {
            let clean = rows.len() == 1 && rows[0] == "ok";
            if !clean {
                findings.push(Finding {
                    severity: "error",
                    what: format!("sqlite integrity_check reported: {}", rows.join("; ")),
                    fix: "restore from a backup, or `pantheon reset --state` if the run history is expendable"
                        .into(),
                });
            }
        }
        Err(e) => findings.push(Finding {
            severity: "error",
            what: format!("integrity_check could not run: {e}"),
            fix: "the database may be unreadable; `pantheon doctor` reports the open path".into(),
        }),
    }

    match sup.stuck_runs() {
        Ok(runs) => {
            for r in runs {
                // A stuck row with a live lease is a session that is genuinely
                // still working. Reporting it as a fault would train the
                // operator to ignore this output.
                let live = sup.has_active_lease(&r.0).unwrap_or(false);
                if live {
                    continue;
                }
                findings.push(Finding {
                    severity: "warn",
                    what: format!("run {} is still '{}' with no live lease", r.0, r.1),
                    fix: format!("pantheon repair runs {}", r.0),
                });
            }
        }
        Err(e) => findings.push(Finding {
            severity: "error",
            what: format!("could not list runs: {e}"),
            fix: "run `pantheon doctor`".into(),
        }),
    }

    if findings.is_empty() {
        println!("ok — ledger integrity clean, no stranded runs");
        return;
    }
    let errors = findings.iter().filter(|f| f.severity == "error").count();
    for f in &findings {
        println!("{}: {}", f.severity, f.what);
        println!("  fix: {}", f.fix);
    }
    if errors > 0 {
        std::process::exit(1);
    }
}

/// Settle runs stranded in a non-terminal state by a crash.
fn repair_runs(sup: &Supervisor, only: Option<&str>) {
    let targets = match sup.stuck_runs() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("repair: list runs: {e}");
            std::process::exit(1);
        }
    };
    let targets: Vec<_> = match only {
        Some(id) => targets.into_iter().filter(|r| r.0 == id).collect(),
        None => targets,
    };
    if targets.is_empty() {
        println!("no stranded runs");
        return;
    }

    let mut settled = 0;
    let mut refused = 0;
    for r in &targets {
        // A live lease means a session really is working on it. Skipping
        // rather than failing keeps one busy run from blocking the repair of
        // every other stranded run.
        match sup.has_active_lease(&r.0) {
            Ok(true) => {
                println!("skip {} — a session holds its lease", r.0);
                refused += 1;
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!("skip {} — cannot read lease state: {e}", r.0);
                refused += 1;
            }
        }
        let reason = format!("run was left '{}' with no live lease", r.1);
        match sup.settle_stuck_run(&r.0, &reason) {
            Ok(()) => {
                println!("settled {} (was '{}')", r.0, r.1);
                settled += 1;
            }
            Err(e) => {
                eprintln!("repair: {}: {e}", r.0);
                refused += 1;
            }
        }
    }
    println!("{settled} settled, {refused} skipped");
    if settled == 0 && refused > 0 {
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "repair_cli_tests.rs"]
mod tests;
