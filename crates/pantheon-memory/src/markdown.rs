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
use pantheon_api::error::{Layer, PantheonError};
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
    let rows = store.list_agent_meta(namespace)?;
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
    policy: &pantheon_api::capability::Policy,
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
    let rows = store.list_agent_meta(namespace)?;
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
/// Sections: `# <key>` heading, value as the body. Value lines that look
/// like headings are escaped with a leading backslash so a value holding
/// `# something` survives a render/parse round-trip as one record.
/// Render one record. `trust` becomes a provenance footer that parse_md
/// reads back; legacy 2-tuple callers get `user` (human-edited file).
pub fn render_record(key: &str, value: &str, trust: pantheon_api::provenance::TrustTier) -> String {
    let escaped = value
        .lines()
        .map(|l| {
            if l.starts_with("# ") || l == "#" {
                format!("\\{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("# {key}\n\n{{trust={}}}\n{}\n\n", trust.as_str(), escaped)
}

pub fn render_agent(records: &[(String, String, pantheon_api::provenance::TrustTier)]) -> String {
    let mut out = String::new();
    // Sentinel header: unambiguous, and a key named "Agent memory" still
    // round-trips because the header is not a `# ` heading.
    out.push_str("<!-- pantheon:agent-memory v2 -->\n\n# Agent memory\n\n");
    for (k, v, t) in records {
        out.push_str(&render_record(k, v, *t));
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
) -> Result<Vec<(String, String, pantheon_api::provenance::TrustTier)>, PantheonError> {
    store.list_agent_meta(namespace)
}

/// Parse a markdown file into Agent-layer records. Splits on `# <key>`
/// headings; the body until the next heading becomes the value. Value
/// lines escaped at render time (leading backslash before a heading-like
/// line) are unescaped here. The sentinel header marks v1 exports; for
/// legacy files without it, the first `# Agent memory` line is treated as
/// the document header and skipped, so a literal key named `Agent memory`
/// in a v1 file round-trips.
/// One parsed record: key, value, and the trust tier from its
/// provenance footer when the file carries one (v2 sentinel). Legacy
/// files (v1 or header-less) yield `None` and the importer decides.
pub fn parse_md_meta(
    content: &str,
) -> Vec<(String, String, Option<pantheon_api::provenance::TrustTier>)> {
    let mut out = Vec::new();
    let mut current: Option<(String, String, Option<String>)> = None;
    let mut seen_header = false;
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("\\# ") {
            // Escaped heading-like line inside a value.
            if let Some((_, body, _)) = current.as_mut() {
                body.push_str(&format!("# {rest}\n"));
            }
            continue;
        }
        if line == "\\#" {
            if let Some((_, body, _)) = current.as_mut() {
                body.push_str("#\n");
            }
            continue;
        }
        // Provenance footer: `{trust=tier}` as the first line of a body.
        // Captured and stripped so it never shows up in the value.
        if let Some(rest) = line.strip_prefix("{trust=") {
            if let Some((_, body, trust)) = current.as_mut() {
                if body.trim().is_empty() && trust.is_none() {
                    if let Some(tier) = rest.strip_suffix('}') {
                        *trust = Some(tier.trim().to_string());
                        continue;
                    }
                }
            }
        }
        if let Some(rest) = line.strip_prefix("# ") {
            let key = rest.trim().to_string();
            // Document header: always skipped, sentinel or legacy. In a
            // sentinel file only the very first occurrence is the header;
            // later `# Agent memory` lines are real records.
            if key == "Agent memory" && !seen_header {
                seen_header = true;
                continue;
            }
            if let Some((key, body, trust)) = current.take() {
                let tier = trust.and_then(|t| pantheon_api::provenance::TrustTier::parse(&t));
                out.push((key, body.trim().to_string(), tier));
            }
            current = Some((key, String::new(), None));
        } else if let Some((_, body, _)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((key, body, trust)) = current {
        let tier = trust.and_then(|t| pantheon_api::provenance::TrustTier::parse(&t));
        out.push((key, body.trim().to_string(), tier));
    }
    out
}

/// Legacy two-tuple view over parse_md_meta (tests, simple importers).
pub fn parse_md(content: &str) -> Vec<(String, String)> {
    parse_md_meta(content)
        .into_iter()
        .map(|(k, v, _)| (k, v))
        .collect()
}

/// Import Agent-layer records from a markdown file. Each parsed section
/// becomes a Proposal that goes through the standard write path
/// (propose -> policy -> provenance -> validation).
///
/// Trust handling: a v2 file carries each record's tier and a hand-edited
/// or legacy file is human-authored, so both import at the file's tier
/// (`memory.md` is a human-authored origin in the write gate). The tier is
/// capped at the tier the store already has for the row — an import can
/// keep or lower the file's claimed tier, never upgrade it. The store's
/// own anti-clobber rule still applies on top: when the capped tier is
/// below the stored tier the row is held entirely.
pub fn import_agent(
    store: &MemoryStore,
    policy: &pantheon_api::capability::Policy,
    namespace: &str,
    path: &Path,
) -> Result<usize, PantheonError> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| merr("MEM_IMPORT_READ", format!("read {}: {e}", path.display())))?;
    let parsed = parse_md_meta(&text);
    let mut written = 0;
    for (key, value, trust) in parsed {
        // A v2 file carries each record's tier; a hand-edited or legacy
        // file is human-authored, so it imports as User. An Untrusted
        // record stays Untrusted: the round-trip must not launder trust.
        let tier = trust.unwrap_or(pantheon_api::provenance::TrustTier::User);
        // Never upgrade on import: cap the file's tier at the store's
        // existing tier for this row. New rows keep the file's tier.
        let tier = match store.get(LayerKind::Agent, namespace, &key)? {
            Some(existing) if existing.provenance.trust.rank() < tier.rank() => {
                existing.provenance.trust
            }
            _ => tier,
        };
        let p = Proposal {
            layer: LayerKind::Agent,
            namespace: namespace.to_string(),
            key,
            value,
            provenance: Provenance {
                source: "import".into(),
                origin: "memory.md".into(),
                trust: tier,
                recorded_at_ms: now_ms(),
            },
        };
        crate::propose_write(store, policy, p, 4096)?;
        written += 1;
    }
    Ok(written)
}

#[cfg(test)]
#[path = "markdown_tests.rs"]
mod tests;
