//! `pantheon reset`: scoped destructive cleanup with confirmation.
//!
//! --config   delete config.toml + memory-backend.toml (fast to redo)
//! --state    delete ledger.db, memory.db, gateway cursors (run history)
//! --everything   everything --config and --state remove, and nothing more
//!
//! The three flags are mutually exclusive; passing two is an error rather
//! than a silent precedence rule.
//!
//! State resets are refused while any active lease exists, and also refused
//! when the lease store cannot be read: reset under a live session would
//! corrupt the durability guarantees, and "cannot prove it is safe" is not
//! the same as "it is safe". Confirmation is interactive by default; --yes
//! skips it for scripts.
//!
//! Safewrite staging and signed-URL state are NOT removed by --everything.
//! They are per-path working state, not configuration, and blowing them away
//! alongside a config reset would discard in-flight edits the user may still
//! want to apply.

use pantheon_core::error::PantheonError;
use pantheon_storage::RunLeaseStore;
use std::path::{Path, PathBuf};

fn target_files(data_dir: &Path, scope: &str) -> Vec<PathBuf> {
    let mut v = match scope {
        "config" => vec![
            data_dir.join("config.toml"),
            data_dir.join("memory-backend.toml"),
        ],
        "state" => vec![
            data_dir.join("ledger.db"),
            data_dir.join("memory.db"),
            data_dir.join("gateway").join("tg-cursor"),
            data_dir.join("gateway").join("discord-cursor"),
        ],
        _ => vec![
            data_dir.join("config.toml"),
            data_dir.join("memory-backend.toml"),
            data_dir.join("ledger.db"),
            data_dir.join("memory.db"),
            data_dir.join("gateway").join("tg-cursor"),
            data_dir.join("gateway").join("discord-cursor"),
        ],
    };
    v.sort();
    v.dedup();
    v.retain(|p| p.exists());
    v
}

/// Whether a run lease is still active, i.e. whether a destructive state
/// reset must be refused.
///
/// Returns `Err` when the lease store cannot be read. The caller must treat
/// that as "refuse", not "proceed": an unreadable ledger is exactly the case
/// where we cannot prove no session is live, and deleting `ledger.db` under a
/// running session destroys its recovery guarantees. Failing open here means
/// the one case that most needs the guard is the one case that skips it.
pub fn active_lease_exists(data_dir: &Path) -> Result<bool, PantheonError> {
    let path = data_dir.join("ledger.db");
    if !path.exists() {
        return Ok(false);
    }
    let store = RunLeaseStore::open(&path)?;
    Ok(!store.list_active()?.is_empty())
}

pub fn cmd_reset(args: &[String]) {
    // Collect every scope flag first. Chaining `if/else if` meant
    // `--config --state` silently reset only the config and said nothing
    // about the state the user also asked to clear.
    let wanted: Vec<&str> = ["config", "state", "everything"]
        .into_iter()
        .filter(|s| args.iter().any(|a| a == &format!("--{s}")))
        .collect();
    let scope = match wanted.as_slice() {
        [] => {
            eprintln!("usage: pantheon reset --config|--state|--everything [--yes]");
            std::process::exit(2);
        }
        // `--everything` is a superset, so it wins over the narrower flags
        // rather than being silently ignored behind them.
        [s] => *s,
        many => {
            eprintln!(
                "reset: {} are mutually exclusive; pass exactly one",
                many.iter()
                    .map(|s| format!("--{s}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            std::process::exit(2);
        }
    };
    let data_dir = crate::data_dir();
    let targets = target_files(&data_dir, scope);
    if targets.is_empty() {
        println!("nothing to reset for scope {scope}");
        return;
    }
    if scope != "config" {
        match active_lease_exists(&data_dir) {
            Ok(true) => {
                eprintln!("reset: a run lease is active; stop the session first");
                std::process::exit(1);
            }
            Ok(false) => {}
            Err(e) => {
                eprintln!(
                    "reset: cannot read the lease store, refusing to delete state: {e}\n\
                     hint: if no session is running, remove the ledger yourself"
                );
                std::process::exit(1);
            }
        }
    }
    let assume_yes = args.iter().any(|a| a == "--yes");
    if !assume_yes {
        println!("about to delete:");
        for t in &targets {
            println!("  {}", t.display());
        }
        print!("type 'reset' to confirm: ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut buf = String::new();
        let _ = std::io::stdin().read_line(&mut buf);
        if buf.trim() != "reset" {
            println!("aborted");
            return;
        }
    }
    let mut deleted = 0;
    for t in &targets {
        match std::fs::remove_file(t) {
            Ok(()) => deleted += 1,
            Err(e) => eprintln!("reset: could not delete {}: {e}", t.display()),
        }
    }
    println!("deleted {deleted} files ({scope})");
}

#[cfg(test)]
#[path = "reset_cli_tests.rs"]
mod tests;
