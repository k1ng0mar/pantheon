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
}
