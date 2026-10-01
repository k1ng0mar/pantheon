//! The nightly repair loop's durable tool-disable list.
//!
//! When the nightly pass disables a broken tool it removes it from the
//! live registry *and* records the name here
//! (`<data_dir>/nightly/disabled-tools.json`), so the next registry build
//! skips it. [`crate::session::Session::build_tool_registry`] consults this
//! list — the registry build is the one constructor, so the skip lives
//! there, not in every caller.
//!
//! The file is a JSON array of `{name, reason, at_ms}` records. Reads are
//! fail-open: a missing or corrupt file yields an empty list, so a broken
//! disable list can never brick the registry build. Writes are atomic
//! (temp file + rename), mirroring the other data-dir stores.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct DisabledTool {
    name: String,
    reason: String,
    at_ms: i64,
}

/// Path of the disable list: `<data_dir>/nightly/disabled-tools.json`.
pub fn disabled_tools_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nightly").join("disabled-tools.json")
}

/// Names the nightly disabled, in record order. Missing or corrupt file =
/// empty (fail-open: the registry build must never fail on this).
pub fn load_disabled_tools(data_dir: &Path) -> Vec<String> {
    read_records(&disabled_tools_path(data_dir))
        .into_iter()
        .map(|e| e.name)
        .collect()
}

/// Record one nightly tool disable. Idempotent on the name: re-disabling
/// updates the reason and timestamp instead of duplicating the row.
pub fn record_disabled_tool(data_dir: &Path, name: &str, reason: &str) -> Result<(), String> {
    let path = disabled_tools_path(data_dir);
    let mut records = read_records(&path);
    let at_ms = now_ms();
    match records.iter_mut().find(|e| e.name == name) {
        Some(e) => {
            e.reason = reason.to_string();
            e.at_ms = at_ms;
        }
        None => records.push(DisabledTool {
            name: name.to_string(),
            reason: reason.to_string(),
            at_ms,
        }),
    }
    write_records(&path, &records)
}

fn read_records(path: &Path) -> Vec<DisabledTool> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn write_records(path: &Path, records: &[DisabledTool]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create nightly dir: {e}"))?;
    }
    let text =
        serde_json::to_string_pretty(records).map_err(|e| format!("encode disable list: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("write disable list: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("publish disable list: {e}"))?;
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
