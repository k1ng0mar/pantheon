//! Atomic SQLite snapshots for `pantheon backup`.
//!
//! Every database Pantheon writes lives directly under the data dir as
//! `<name>.db`. A snapshot is taken with SQLite's `VACUUM INTO`, which
//! writes a transactionally consistent copy while the live database keeps
//! serving readers and writers — no lock dance, no half-written file.
//! Restore is a plain file copy back over the live path (done while no
//! Pantheon process holds the database, which the CLI enforces).

use std::path::{Path, PathBuf};

/// SQLite databases Pantheon owns, in backup order (ledger first: it is
/// the one a restore is most likely about).
pub const KNOWN_DB_NAMES: &[&str] = &[
    "ledger.db",
    "memory.db",
    "collaboration.db",
    "claims.db",
    "ideas.db",
];

/// All `<name>.db` files directly under `data_dir`: the known list first
/// (stable order), then any other top-level `*.db` a future version adds,
/// sorted. Subdirectories are not scanned — nothing Pantheon writes puts
/// a database below the top level today, and blindly snapshotting e.g. a
/// browser profile would be wrong.
pub fn discover_dbs(data_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = KNOWN_DB_NAMES
        .iter()
        .map(|n| data_dir.join(n))
        .filter(|p| p.is_file())
        .collect();
    if let Ok(entries) = std::fs::read_dir(data_dir) {
        let mut extra: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension().map(|x| x == "db").unwrap_or(false)
                    && !out.iter().any(|k| k == p)
            })
            .collect();
        extra.sort();
        out.extend(extra);
    }
    out
}

/// Write an atomic, transactionally consistent copy of the SQLite database
/// at `src` to `dest` via `VACUUM INTO`. The destination's parent must
/// exist. Fails closed: any error names the database.
pub fn snapshot_db(src: &Path, dest: &Path) -> Result<(), String> {
    // rusqlite's open() creates a missing file, so check first: snapshotting
    // a database that does not exist must fail loudly, not back up an
    // empty database over a real backup.
    if !src.is_file() {
        return Err(format!("snapshot {}: no such database", src.display()));
    }
    let conn =
        rusqlite::Connection::open(src).map_err(|e| format!("open {}: {e}", src.display()))?;
    // VACUUM INTO takes a string literal, not a bound parameter: double
    // the quotes. Paths Pantheon generates never contain quotes, but a
    // hand-set PANTHEON_DATA_DIR could.
    let literal = dest.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{literal}'"))
        .map_err(|e| format!("snapshot {}: {e}", src.display()))?;
    Ok(())
}

/// Copy a backup file back over the live database path. Refuses when the
/// destination's parent is missing so a typo cannot scatter files.
pub fn restore_db(src: &Path, dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        if !parent.is_dir() {
            return Err(format!(
                "restore: parent of {} does not exist",
                dest.display()
            ));
        }
    }
    std::fs::copy(src, dest).map_err(|e| format!("restore {}: {e}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pantheon-backup-test-{name}"));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn snapshot_round_trips_data() {
        let d = tmp("roundtrip");
        let src = d.join("ledger.db");
        {
            let c = rusqlite::Connection::open(&src).unwrap();
            c.execute_batch("CREATE TABLE t(x TEXT); INSERT INTO t VALUES ('hello');")
                .unwrap();
        }
        let dest = d.join("snap.db");
        snapshot_db(&src, &dest).unwrap();
        let c = rusqlite::Connection::open(&dest).unwrap();
        let v: String = c.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(v, "hello");
    }

    #[test]
    fn snapshot_missing_source_errors() {
        let d = tmp("missing");
        let err = snapshot_db(&d.join("nope.db"), &d.join("out.db")).unwrap_err();
        assert!(err.contains("nope.db"), "unexpected: {err}");
    }

    #[test]
    fn discover_finds_known_and_extra_dbs() {
        let d = tmp("discover");
        for n in ["ledger.db", "memory.db", "extra.db", "notes.txt"] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let found: Vec<String> = discover_dbs(&d)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(found, vec!["ledger.db", "memory.db", "extra.db"]);
    }
}
