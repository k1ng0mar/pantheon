//! Pre-state hashing for file-writing tool calls.
//!
//! When a tool call that writes a file parks for approval, we record the
//! SHA-256 of the target file's current contents. On resume, we re-read
//! and compare. A mismatch means the file changed between plan and apply:
//! the grant is stale, the call is rejected, and the agent must re-plan.
//!
//! This is optimistic concurrency control. It closes the gap where a user
//! approves a diff, edits the file manually while the run is parked, and
//! the stale write clobbers their changes on resume.

use sha2::{Digest, Sha256};
use std::path::Path;

/// SHA-256 hex of a file's contents, or None if the file does not exist.
pub fn hash_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    Some(out)
}

/// Extract the target file path from a tool call's arguments, if the tool
/// writes to a file. Returns None for read-only tools.
///
/// Covers: write, edit, patch, and any tool whose args carry a `path`
/// field that maps to a write operation.
pub fn file_write_target(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    let path = args.get("path").or_else(|| args.get("file_path"))?;
    let path_str = path.as_str()?;

    match tool_name {
        "write" | "write_file" | "edit" | "patch" | "create_file" | "notebook_edit" => {
            Some(path_str.to_string())
        }
        _ => {
            // For tools we don't know, only treat it as a write target if
            // the args explicitly say so.
            let writes = args
                .get("_write")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if writes {
                Some(path_str.to_string())
            } else {
                None
            }
        }
    }
}

/// Check whether a file's current hash matches the recorded pre-state hash.
/// Returns Ok(()) on match or when no pre-state was recorded.
/// Returns Err with a description on mismatch.
pub fn verify_pre_state(path: &Path, recorded_hash: &str) -> Result<(), String> {
    let current = hash_file(path).unwrap_or_default();
    if current == recorded_hash {
        Ok(())
    } else {
        Err(format!(
            "file changed between approval and apply: {} (expected {}, got {})",
            path.display(),
            if recorded_hash.is_empty() {
                "did not exist"
            } else {
                recorded_hash
            },
            if current.is_empty() {
                "does not exist"
            } else {
                &current
            },
        ))
    }
}
