//! `pantheon reset`: scoped destructive cleanup with confirmation.
//!
//! --config   delete config.toml + memory-backend.toml (fast to redo)
//! --state    delete ledger.db, memory.db, gateway cursors (run history)
//! --everything   both, plus safewrite staging and signed-URL state
//!
//! Runs are refused while any active lease exists: reset under a live
//! session would corrupt the durability guarantees. Confirmation is
//! interactive by default; --yes skips it for scripts.

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

/// Refuse when a lease row is still unexpired: another supervisor may be
/// mid-run and deleting the ledger under it breaks recovery guarantees.
pub fn active_lease_exists(data_dir: &Path) -> bool {
    let path = data_dir.join("ledger.db");
    if !path.exists() {
        return false;
    }
    match RunLeaseStore::open(&path) {
        Ok(store) => match store.list_active() {
            Ok(leases) => !leases.is_empty(),
            Err(_) => false, // unreadable store: let the delete proceed
        },
        Err(_) => false,
    }
}

pub fn cmd_reset(args: &[String]) {
    let scope = if args.iter().any(|a| a == "--config") {
        "config"
    } else if args.iter().any(|a| a == "--state") {
        "state"
    } else if args.iter().any(|a| a == "--everything") {
        "everything"
    } else {
        eprintln!("usage: pantheon reset --config|--state|--everything [--yes]");
        std::process::exit(2);
    };
    let data_dir = crate::data_dir();
    let targets = target_files(&data_dir, scope);
    if targets.is_empty() {
        println!("nothing to reset for scope {scope}");
        return;
    }
    if scope != "config" && active_lease_exists(&data_dir) {
        eprintln!("reset: a run lease is active; stop the session first");
        std::process::exit(1);
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
