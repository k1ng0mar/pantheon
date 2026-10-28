//! The nightly repair loop's durable tool-disable list.
//!
//! When the nightly pass disables a broken tool it removes it from the
//! live registry *and* records the name here
//! (`<data_dir>/nightly/disabled-tools.json`), so the next registry build
//! skips it. [`crate::session::Session::build_tool_registry`] consults this
//! list - the registry build is the one constructor, so the skip lives
//! there, not in every caller.
//!
//! The file is a JSON array of `{name, reason, at_ms}` records. Writes are
//! atomic (temp file + rename), mirroring the other data-dir stores.
//!
//! Missing vs corrupt are different states, and the old code conflated
//! them: a missing file is normal (no tool has ever been disabled) and
//! reads as empty, but a corrupt file used to read as empty too -
//! silently re-enabling every tool the nightly pass had contained,
//! including a dangerous one it disabled for a reason. A corrupt list
//! now warns loudly at the registry build (log + `/status`
//! diagnostic) and refuses to be overwritten, so the corruption is
//! visible instead of paved over. The build still proceeds - it cannot
//! know which tools the corrupt list named - but it never proceeds
//! quietly.

use pantheon_api::logging::now_ms;
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

/// Names the nightly disabled, in record order. Missing file = empty
/// (no tool has ever been disabled). A corrupt file also yields empty
/// here - use [`load_disabled_tools_status`] when the distinction
/// matters, which is everywhere the list gates a dangerous tool.
pub fn load_disabled_tools(data_dir: &Path) -> Vec<String> {
    load_disabled_tools_status(data_dir).0
}

/// Names the nightly disabled, plus whether the list file exists but
/// failed to parse. `true` means containment may be lost: tools the
/// nightly pass disabled could be live again, and callers must warn
/// loudly rather than proceed as if the list were simply empty.
pub fn load_disabled_tools_status(data_dir: &Path) -> (Vec<String>, bool) {
    let (records, corrupt) = read_records_status(&disabled_tools_path(data_dir));
    (records.into_iter().map(|e| e.name).collect(), corrupt)
}

/// Record one nightly tool disable. Idempotent on the name: re-disabling
/// updates the reason and timestamp instead of duplicating the row.
/// Refuses to overwrite a corrupt list: writing a fresh list over it
/// would silently discard the containment record it once held.
pub fn record_disabled_tool(data_dir: &Path, name: &str, reason: &str) -> Result<(), String> {
    let path = disabled_tools_path(data_dir);
    let (mut records, corrupt) = read_records_status(&path);
    if corrupt {
        return Err(format!(
            "refusing to overwrite corrupt disabled-tools list {}; repair or remove it by hand first",
            path.display()
        ));
    }
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

/// Read the list, distinguishing "no file" (clean empty) from "file
/// present but unreadable or unparseable" (corrupt).
fn read_records_status(path: &Path) -> (Vec<DisabledTool>, bool) {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(records) => (records, false),
            Err(_) => (Vec::new(), true),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
        Err(_) => (Vec::new(), true),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(content: Option<&str>) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let data_dir = dir.path().to_path_buf();
        if let Some(text) = content {
            let path = disabled_tools_path(&data_dir);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, text).unwrap();
        }
        (dir, data_dir)
    }

    #[test]
    fn missing_list_is_clean_empty_not_corrupt() {
        let (_dir, data_dir) = dir_with(None);
        let (names, corrupt) = load_disabled_tools_status(&data_dir);
        assert!(names.is_empty());
        assert!(!corrupt, "a missing list is normal, not corruption");
    }

    #[test]
    fn corrupt_list_is_flagged_and_refuses_overwrite() {
        let (_dir, data_dir) = dir_with(Some("{not json"));
        let (names, corrupt) = load_disabled_tools_status(&data_dir);
        assert!(names.is_empty());
        assert!(corrupt, "an unparseable list must be flagged");
        let err = record_disabled_tool(&data_dir, "tool-x", "broken")
            .expect_err("a corrupt list must not be overwritten");
        assert!(err.contains("corrupt"), "got: {err}");
        assert_eq!(
            std::fs::read_to_string(disabled_tools_path(&data_dir)).unwrap(),
            "{not json",
            "the corrupt file must be untouched by the refused write"
        );
    }

    #[test]
    fn valid_list_round_trips_and_dedupes_on_name() {
        let (_dir, data_dir) = dir_with(None);
        record_disabled_tool(&data_dir, "tool-x", "first").unwrap();
        record_disabled_tool(&data_dir, "tool-x", "second").unwrap();
        let (names, corrupt) = load_disabled_tools_status(&data_dir);
        assert_eq!(names, vec!["tool-x".to_string()]);
        assert!(!corrupt);
    }
}
