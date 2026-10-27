//! Shared small helpers. `now_ms`, `project_root`, `atomic_write`, and
//! `parent_of` were each copied byte-identical into several route
//! modules; they live here once.

use std::path::{Path, PathBuf};

/// Wall-clock milliseconds since the Unix epoch. Never panics: a clock
/// before the epoch (or an unreadable one) reads as 0.
///
/// Thin re-export of the canonical `pantheon_api::logging::now_ms` rather
/// than a ninth copy of the same five lines.
pub(crate) use pantheon_api::logging::now_ms;

/// The project working directory: the process's current dir, or `.` when
/// it cannot be read.
pub(crate) fn project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Atomic config write: unique temp file in the same directory +
/// fsync + rename, so a crash mid-write never leaves a half-written
/// file, and the existing file's permissions survive the rename.
pub(crate) fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    // Unique temp name (pid + nanos): the old fixed `*.toml.tmp` name
    // let concurrent writers clobber each other's temp file. Mode:
    // preserve the existing file's mode instead of loosening it to the
    // umask default on rename (a 0600 config.toml used to come back
    // 0644); a brand-new file keeps the umask default, as before.
    // fsync file + dir so the rename survives a crash.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_file_name(format!(
        "{}.{}-{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config".to_string()),
        std::process::id(),
        nanos
    ));
    #[cfg(unix)]
    let existing_mode = std::fs::metadata(path)
        .ok()
        .map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.mode() & 0o777
        });
    let write = (|| -> Result<(), String> {
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        if let Some(mode) = existing_mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
                .map_err(|e| e.to_string())?;
        }
        std::fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(parent) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

/// Navigate (and create) intermediate tables for a dotted path. Returns
/// the parent table and the leaf key.
pub(crate) fn parent_of<'a>(
    doc: &'a mut toml::Value,
    path: &str,
) -> Result<(&'a mut toml::map::Map<String, toml::Value>, String), String> {
    let mut parts: Vec<&str> = path.split('.').collect();
    let leaf = parts.pop().ok_or_else(|| "empty path".to_string())?;
    if leaf.is_empty() {
        return Err("empty path segment".to_string());
    }
    let mut cur = doc;
    for part in parts {
        if part.is_empty() {
            return Err("empty path segment".to_string());
        }
        cur = cur
            .as_table_mut()
            .ok_or_else(|| format!("'{path}' walks through a non-table"))?
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        if !cur.is_table() {
            return Err(format!("'{path}' walks through a non-table value"));
        }
    }
    let table = cur
        .as_table_mut()
        .ok_or_else(|| format!("'{path}' has no parent table"))?;
    Ok((table, leaf.to_string()))
}

/// Round a USD cost to 4 decimal places for JSON output. Four, not two:
/// single model calls routinely cost fractions of a cent, and a frontend
/// `toFixed(2)` display formats identically either way.
pub(crate) fn round_cost_usd(v: f64) -> f64 {
    (v * 10000.0).round() / 10000.0
}

/// Default agent/namespace name when none is configured.
pub(crate) const DEFAULT_AGENT_NAME: &str = "nyx";
