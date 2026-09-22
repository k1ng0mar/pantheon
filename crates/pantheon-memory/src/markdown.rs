//! Native MEMORY.md interface (Hermes-style flat file).
//!
//! Auto-syncs Agent-layer records with a markdown file at the project root.
//! Format: each record becomes a section with `# <key>` heading followed by
//! the value as the body. Editing MEMORY.md and re-importing keeps the
//! store in sync; programmatic writes also write through to the file.
//!
//! Use cases:
//! - Quick visual inspection: `cat MEMORY.md`
//! - Git diffable like any other doc
//! - Bootstrap from existing MEMORY.md: import on session start
//! - Drift-free: store is source of truth, file is a projection

use crate::{LayerKind, MemoryStore, Proposal, Provenance};
use pantheon_core::error::{Layer, PantheonError};
use std::path::Path;

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Memory,
        false,
        cause,
        "check the memory.md path and permissions",
        "",
    )
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fnv1a_hex(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

fn file_hash(path: &Path) -> Result<String, PantheonError> {
    let bytes = std::fs::read(path).map_err(|e| merr("MEM_SYNC_READ", format!("{e}")))?;
    Ok(fnv1a_hex(&bytes))
}

/// Result of a sync pass: import/export decisions made and the next
/// `MEMORY.md` snapshot written to disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// True when the on-disk file differed from the last imported hash
    /// and the diff was applied to the store.
    pub imported: bool,
    /// True when the store changed after import (or had no prior file)
    /// and the export was written back to disk.
    pub exported: bool,
    /// Hash of the file that was imported (or the existing one if no
    /// import happened this round).
    pub file_hash: String,
}

/// Conflict: both the on-disk file and the store changed since the last
/// sync. The caller must reconcile manually (import, export, or merge)
/// before silent data loss becomes a problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncConflict {
    pub file_hash: String,
    pub store_hash: String,
}

fn store_hash(store: &MemoryStore, namespace: &str) -> Result<String, PantheonError> {
    let rows = store.list_agent(namespace)?;
    let md = render_agent(&rows);
    Ok(fnv1a_hex(md.as_bytes()))
}

/// One sync pass: read `path` and the store, import file changes, then
/// export the store if it changed since the last known file hash.
///
/// `last_known_hash` is the hash recorded from the previous sync. Pass
/// `None` on a fresh install. Returns the new file hash so the caller
/// can store it for the next pass.
pub fn sync(
    store: &MemoryStore,
    policy: &pantheon_core::capability::Policy,
    namespace: &str,
    path: &Path,
    last_known_hash: Option<&str>,
) -> Result<SyncReport, PantheonError> {
    let file_exists = path.exists();
    let cur_hash = if file_exists {
        file_hash(path)?
    } else {
        "".into()
    };

    let mut imported = false;

    // Step 1: file changes since last sync?
    let file_changed = last_known_hash
        .map(|h| h != cur_hash)
        .unwrap_or(file_exists);
    if file_exists && file_changed {
        import_agent(store, policy, namespace, path)?;
        imported = true;
    }

    // Step 2: store changed since last sync?
    let store_hash_now = store_hash(store, namespace)?;
    let store_changed = last_known_hash.map(|h| h != store_hash_now).unwrap_or(true);
    if !imported && !store_changed {
        return Ok(SyncReport {
            imported: false,
            exported: false,
            file_hash: cur_hash,
        });
    }

    // Step 3: write the projection back. Atomic via tmp+rename.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| merr("MEM_SYNC_MKDIR", format!("{e}")))?;
        }
    }
    let tmp = path.with_extension("md.sync.tmp");
    let rows = store.list_agent(namespace)?;
    let md = render_agent(&rows);
    std::fs::write(&tmp, md).map_err(|e| merr("MEM_SYNC_WRITE", format!("{e}")))?;
    std::fs::rename(&tmp, path).map_err(|e| merr("MEM_SYNC_RENAME", format!("{e}")))?;
    let exported = true;

    Ok(SyncReport {
        imported,
        exported,
        file_hash: file_hash(path)?,
    })
}

/// Detect a conflict between the on-disk file and the store. The caller
/// must reconcile before calling `sync`, otherwise the write-back clobbers
/// one side.
pub fn detect_conflict(
    store: &MemoryStore,
    namespace: &str,
    path: &Path,
    last_known_hash: Option<&str>,
) -> Result<Option<SyncConflict>, PantheonError> {
    let file_exists = path.exists();
    let cur_hash = if file_exists {
        file_hash(path)?
    } else {
        "".into()
    };
    let store_hash_now = store_hash(store, namespace)?;
    let file_changed = last_known_hash
        .map(|h| h != cur_hash)
        .unwrap_or(file_exists);
    let store_changed = last_known_hash
        .map(|h| h != store_hash_now)
        .unwrap_or(false);
    if file_changed && store_changed {
        Ok(Some(SyncConflict {
            file_hash: cur_hash,
            store_hash: store_hash_now,
        }))
    } else {
        Ok(None)
    }
}

/// Render Agent-layer records for one namespace as markdown.
/// Sections: `# <key>` heading, value as the body.
pub fn render_agent(records: &[(String, String)]) -> String {
    let mut out = String::new();
    out.push_str("# Agent memory\n\n");
    for (k, v) in records {
        out.push_str(&format!("# {k}\n\n{v}\n\n"));
    }
    out
}

/// Export the Agent-layer records for one namespace to a markdown file.
pub fn export_agent(
    store: &MemoryStore,
    namespace: &str,
    path: &Path,
) -> Result<(), PantheonError> {
    let rows = list_agent_records(store, namespace)?;
    let md = render_agent(&rows);
    std::fs::write(path, md)
        .map_err(|e| merr("MEM_EXPORT", format!("write {}: {e}", path.display())))
}

/// List Agent-layer records and export them as `MEMORY.md`.
pub fn list_agent_records(
    store: &MemoryStore,
    namespace: &str,
) -> Result<Vec<(String, String)>, PantheonError> {
    store.list_agent(namespace)
}

/// Parse a markdown file into Agent-layer records. Splits on `# <key>`
/// headings; the body until the next heading becomes the value.
pub fn parse_md(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("# ") {
            if let Some((key, body)) = current.take() {
                out.push((key, body.trim().to_string()));
            }
            let key = rest.trim().to_string();
            if key != "Agent memory" {
                current = Some((key, String::new()));
            }
        } else if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((key, body)) = current {
        out.push((key, body.trim().to_string()));
    }
    out
}

/// Import Agent-layer records from a markdown file. Each parsed section
/// becomes a Proposal that goes through the standard write path
/// (propose -> policy -> provenance -> validation).
pub fn import_agent(
    store: &MemoryStore,
    policy: &pantheon_core::capability::Policy,
    namespace: &str,
    path: &Path,
) -> Result<usize, PantheonError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| merr("MEM_IMPORT_READ", format!("read {}: {e}", path.display())))?;
    let parsed = parse_md(&text);
    let mut written = 0;
    for (key, value) in parsed {
        let p = Proposal {
            layer: LayerKind::Agent,
            namespace: namespace.to_string(),
            key,
            value,
            provenance: Provenance {
                source: "import".into(),
                origin: "memory.md".into(),
                recorded_at_ms: now_ms(),
            },
        };
        crate::propose_write(store, policy, p, 4096)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LayerKind;

    #[test]
    fn parse_round_trips_simple_records() {
        let md = "# Agent memory\n\n# city\n\nKano\n\n# tz\n\nAfrica/Lagos\n";
        let parsed = parse_md(md);
        assert_eq!(
            parsed,
            vec![
                ("city".into(), "Kano".into()),
                ("tz".into(), "Africa/Lagos".into()),
            ]
        );
    }

    #[test]
    fn parse_handles_missing_top_heading() {
        let md = "# city\n\nKano\n";
        let parsed = parse_md(md);
        assert_eq!(parsed, vec![("city".into(), "Kano".into())]);
    }

    #[test]
    fn parse_handles_multiline_body() {
        let md = "# notes\n\nline one\nline two\n\n# end\n\n";
        let parsed = parse_md(md);
        assert_eq!(parsed[0].0, "notes");
        assert!(parsed[0].1.contains("line one"));
        assert!(parsed[0].1.contains("line two"));
    }

    #[test]
    fn render_then_parse_round_trip() {
        let rows = vec![
            ("city".to_string(), "Kano".to_string()),
            ("tz".to_string(), "Africa/Lagos".to_string()),
        ];
        let md = render_agent(&rows);
        let parsed = parse_md(&md);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "city");
        assert_eq!(parsed[1].1, "Africa/Lagos");
    }

    #[test]
    fn import_proposals_go_through_policy_gate() {
        // Use a fresh in-memory store and a policy that grants MemoryWrite.
        let store = MemoryStore::open_in_memory().unwrap();
        let policy = pantheon_core::capability::Policy::coder()
            .allow(pantheon_core::capability::Capability::MemoryWrite);
        let tmp = std::env::temp_dir().join(format!(
            "pantheon-mem-md-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&tmp, "# city\n\nKano\n\n# tz\n\nAfrica/Lagos\n").unwrap();
        let n = import_agent(&store, &policy, "nyx", &tmp).unwrap();
        assert_eq!(n, 2);
        let layers = [LayerKind::Agent];
        let hits = crate::recall(&store, &policy, &layers, "city", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.value, "Kano");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn import_refuses_without_write_capability() {
        let store = MemoryStore::open_in_memory().unwrap();
        // Default `coder()` does NOT grant MemoryWrite.
        let policy = pantheon_core::capability::Policy::coder();
        let tmp = std::env::temp_dir().join(format!(
            "pantheon-mem-md-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&tmp, "# city\n\nKano\n").unwrap();
        let err = import_agent(&store, &policy, "nyx", &tmp).unwrap_err();
        assert_eq!(err.code, "MEM_NO_CAPABILITY");
        let _ = std::fs::remove_file(&tmp);
    }

    fn writer_policy() -> pantheon_core::capability::Policy {
        use pantheon_core::capability::Capability as C;
        pantheon_core::capability::Policy::coder().allow(C::MemoryWrite)
    }

    fn fresh_md(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-mdsync-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sync_creates_file_on_fresh_install() {
        let dir = fresh_md("fresh");
        let store = MemoryStore::open_in_memory().unwrap();
        let md = dir.join("MEMORY.md");
        let report = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
        assert!(!report.imported);
        assert!(report.exported);
        assert!(md.exists());
        let text = std::fs::read_to_string(&md).unwrap();
        assert!(text.contains("# Agent memory"));
    }

    #[test]
    fn sync_imports_when_file_changes() {
        let dir = fresh_md("import");
        let store = MemoryStore::open_in_memory().unwrap();
        let md = dir.join("MEMORY.md");
        // Initial sync seeds the file from the (empty) store.
        let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
        assert!(r1.exported);
        // User edits the file outside the harness.
        std::fs::write(&md, "# city\n\nKano\n").unwrap();
        // Next sync imports the new content and re-exports.
        let r2 = sync(&store, &writer_policy(), "nyx", &md, Some(&r1.file_hash)).unwrap();
        assert!(r2.imported);
        assert!(r2.exported);
        let layers = [LayerKind::Agent];
        let hits = crate::recall(&store, &writer_policy(), &layers, "city", 10).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn sync_noop_when_nothing_changed() {
        let dir = fresh_md("noop");
        let store = MemoryStore::open_in_memory().unwrap();
        let md = dir.join("MEMORY.md");
        let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
        let r2 = sync(&store, &writer_policy(), "nyx", &md, Some(&r1.file_hash)).unwrap();
        assert!(!r2.imported);
        assert!(!r2.exported);
    }

    #[test]
    fn detect_conflict_when_both_sides_changed() {
        let dir = fresh_md("conflict");
        let store = MemoryStore::open_in_memory().unwrap();
        let md = dir.join("MEMORY.md");
        let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
        // File changes outside.
        std::fs::write(&md, "# from_file\n\nX\n").unwrap();
        // Store changes inside (a new record through propose_write).
        crate::propose_write(
            &store,
            &writer_policy(),
            crate::Proposal {
                layer: LayerKind::Agent,
                namespace: "nyx".into(),
                key: "from_store".into(),
                value: "Y".into(),
                provenance: Provenance {
                    source: "test".into(),
                    origin: "test".into(),
                    recorded_at_ms: 0,
                },
            },
            4096,
        )
        .unwrap();
        let conflict = detect_conflict(&store, "nyx", &md, Some(&r1.file_hash)).unwrap();
        assert!(conflict.is_some(), "expected conflict to be detected");
    }

    #[test]
    fn sync_no_conflict_when_only_store_changed() {
        let dir = fresh_md("store_only");
        let store = MemoryStore::open_in_memory().unwrap();
        let md = dir.join("MEMORY.md");
        let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
        crate::propose_write(
            &store,
            &writer_policy(),
            crate::Proposal {
                layer: LayerKind::Agent,
                namespace: "nyx".into(),
                key: "new".into(),
                value: "value".into(),
                provenance: Provenance {
                    source: "test".into(),
                    origin: "test".into(),
                    recorded_at_ms: 0,
                },
            },
            4096,
        )
        .unwrap();
        // File is unchanged from the last sync.
        let conflict = detect_conflict(&store, "nyx", &md, Some(&r1.file_hash)).unwrap();
        assert!(conflict.is_none());
        let r2 = sync(&store, &writer_policy(), "nyx", &md, Some(&r1.file_hash)).unwrap();
        assert!(!r2.imported);
        assert!(r2.exported);
        // After sync, the file reflects the new record.
        let text = std::fs::read_to_string(&md).unwrap();
        assert!(text.contains("new"));
    }
}
