//! `pantheon backup`: atomic snapshots of every SQLite database Pantheon
//! owns, plus restore.
//!
//! Snapshots go to `<data_dir>/backups/<UTC-timestamp>/` (overridable with
//! `--dir`), one file per database plus a `manifest.json`. Each snapshot
//! is taken with SQLite `VACUUM INTO`, so it is transactionally consistent
//! even while the gateway or a session is writing.
//!
//! Recommended schedule (a scheduled job runs agent task text, so the job
//! is a task that runs the verb):
//!   pantheon schedule "run `pantheon backup` in the shell" --every 24h --deliver log

use std::path::{Path, PathBuf};

pub fn usage() -> &'static str {
    "usage: pantheon backup [--dir DIR] [--list] [--restore DIR --yes]\n\
     \n\
     Snapshot every SQLite database under the data dir (ledger, memory,\n\
     collaboration, claims, ideas, ...) into backups/<UTC-timestamp>/ via\n\
     atomic VACUUM INTO copies, with a manifest.json per backup.\n\
     \n\
     --dir DIR       backup root (default: <data_dir>/backups)\n\
     --list          list existing backups, newest first\n\
     --restore DIR   copy a backup's databases back over the live ones.\n\
                     Destructive: requires --yes, and stop every Pantheon\n\
                     process first (serve, gateway, sessions).\n\
     \n\
     Recommended schedule:\n\
       pantheon schedule \"run `pantheon backup` in the shell\" --every 24h --deliver log"
}

fn utc_stamp() -> String {
    // No chrono in this crate; UTC from the system clock, formatted by hand.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since epoch -> civil date (Howard Hinnant's algorithm).
    let days = (secs / 86400) as i64;
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let t = secs % 86400;
    let hh = t / 3600;
    let mm = (t % 3600) / 60;
    let ss = t % 60;
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn stamp_dir_name() -> String {
    utc_stamp().replace([':', '-'], "").replace('T', "-")
}

fn manifest(_backup_dir: &Path, files: &[(String, u64)]) -> String {
    let entries: Vec<String> = files
        .iter()
        .map(|(n, b)| format!("    {{\"name\": \"{n}\", \"bytes\": {b}}}"))
        .collect();
    format!(
        "{{\n  \"created_utc\": \"{}\",\n  \"pantheon_version\": \"{}\",\n  \"files\": [\n{}\n  ]\n}}\n",
        utc_stamp(),
        env!("CARGO_PKG_VERSION"),
        entries.join(",\n")
    )
}

fn list_backups(root: &Path) {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|r| {
            r.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir() && p.join("manifest.json").is_file())
                .collect()
        })
        .unwrap_or_default();
    dirs.sort();
    dirs.reverse();
    if dirs.is_empty() {
        println!("no backups in {}", root.display());
        return;
    }
    for d in dirs {
        let name = d.file_name().unwrap_or_default().to_string_lossy();
        let manifest_text = std::fs::read_to_string(d.join("manifest.json")).unwrap_or_default();
        let created: String = manifest_text
            .lines()
            .find_map(|l| {
                let l = l.trim();
                l.strip_prefix("\"created_utc\":")
                    .map(|v| v.trim().trim_matches(['"', ',', ' ']).to_string())
            })
            .unwrap_or_default();
        let count = manifest_text.matches("\"name\":").count();
        println!("{name}  {created}  {count} databases");
    }
}

fn cmd_restore(dir: &str, yes: bool, data_dir: &Path) {
    let src = PathBuf::from(dir);
    let manifest_path = src.join("manifest.json");
    if !manifest_path.is_file() {
        eprintln!("backup: {dir} is not a pantheon backup (no manifest.json)");
        std::process::exit(1);
    }
    if !yes {
        eprintln!("backup: --restore overwrites the live databases.");
        eprintln!("stop every Pantheon process first (serve, gateway, sessions),");
        eprintln!("then re-run with --yes to confirm.");
        std::process::exit(2);
    }
    // Refuse while a live database looks open-for-write by someone else:
    // a best-effort guard, not a lock - the operator stops the processes.
    let manifest_text = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        eprintln!("backup: cannot read {}: {e}", manifest_path.display());
        std::process::exit(1);
    });
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap_or_else(|e| {
        eprintln!("backup: cannot parse {}: {e}", manifest_path.display());
        std::process::exit(1);
    });
    let mut names: Vec<String> = Vec::new();
    for f in manifest
        .get("files")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let Some(n) = f.get("name").and_then(|v| v.as_str()) {
            names.push(n.to_string());
        }
    }
    if names.is_empty() {
        eprintln!("backup: manifest lists no databases; nothing to restore");
        std::process::exit(1);
    }
    for name in &names {
        // Stay inside the backup dir: manifest names are plain file names.
        if name.contains('/') || name.contains('\\') || name == "manifest.json" {
            eprintln!("backup: refusing suspicious manifest entry {name:?}");
            std::process::exit(1);
        }
        let from = src.join(name);
        let to = data_dir.join(name);
        if !from.is_file() {
            eprintln!("backup: backup is missing {name}; aborting (nothing restored)");
            std::process::exit(1);
        }
        pantheon_storage::backup::restore_db(&from, &to).unwrap_or_else(|e| {
            eprintln!("backup: {e}");
            std::process::exit(1);
        });
        println!("restored {name}");
    }
    println!("restore complete from {}", src.display());
}

pub fn cmd_backup(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return;
    }
    let flag_val = |name: &str| {
        let mut it = args.iter().peekable();
        while let Some(a) = it.next() {
            if a == name {
                return it.next().cloned();
            }
            if let Some(v) = a.strip_prefix(&format!("{name}=")) {
                return Some(v.to_string());
            }
        }
        None
    };
    let data_dir = crate::terminal::data_dir();
    let root = flag_val("--dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("backups"));

    if args.iter().any(|a| a == "--list") {
        list_backups(&root);
        return;
    }
    if let Some(dir) = flag_val("--restore") {
        let yes = args.iter().any(|a| a == "--yes");
        cmd_restore(&dir, yes, &data_dir);
        return;
    }
    if let Some(other) = args.iter().skip(2).find(|a| a.starts_with("--")) {
        eprintln!("backup: unknown flag '{other}'");
        eprintln!("{}", usage());
        std::process::exit(2);
    }

    let dbs = pantheon_storage::backup::discover_dbs(&data_dir);
    if dbs.is_empty() {
        println!("no databases found under {}", data_dir.display());
        return;
    }
    let dest = root.join(stamp_dir_name());
    if let Err(e) = std::fs::create_dir_all(&dest) {
        eprintln!("backup: cannot create {}: {e}", dest.display());
        std::process::exit(1);
    }
    let mut files: Vec<(String, u64)> = Vec::new();
    for src in &dbs {
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let target = dest.join(&name);
        match pantheon_storage::backup::snapshot_db(src, &target) {
            Ok(()) => {
                let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                println!("snapshotted {name} ({bytes} bytes)");
                files.push((name, bytes));
            }
            Err(e) => {
                eprintln!("backup: {e}");
                std::process::exit(1);
            }
        }
    }
    if let Err(e) = std::fs::write(dest.join("manifest.json"), manifest(&dest, &files)) {
        eprintln!("backup: snapshots taken but manifest failed: {e}");
        std::process::exit(1);
    }
    println!("backup complete: {}", dest.display());
}
