//! Shared on-disk swarm manifest: the `<data_dir>/swarms/<id>.json` store
//! that both the CLI `swarm` verb and the dashboard orchestrator read and
//! write, so `swarm list` / `swarm status` see every swarm no matter
//! which entry point created it.
//!
//! The orchestrator's in-memory records are the source of truth while a
//! swarm is live; this manifest is the durable index. Writes are atomic
//! (tmp file then rename), matching the rest of the codebase.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// One agent in a manifest. `label` is `{swarm_id}:{role}:{i}` for CLI
/// swarms; the orchestrator writes the same shape so the loader is
/// source-agnostic. `#[serde(default)]` on every optional field means a
/// partial or older manifest still parses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmAgentManifest {
    pub label: String,
    pub run_id: String,
    pub role: String,
    /// `"running"` while the turn executes, then `"ok"` / `"error"`.
    #[serde(default)]
    pub status: String,
}

/// Durable swarm manifest. Written under `<data_dir>/swarms/<swarm_id>.json`.
/// The CLI loader and the orchestrator's persistence both speak this shape.
/// `created_ms` is shared: the CLI sets it at spawn, the orchestrator at
/// create, and both loaders sort by it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmManifest {
    pub swarm_id: String,
    pub task: String,
    pub roles: Vec<String>,
    pub agents: Vec<SwarmAgentManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    /// Unix milliseconds when the swarm was created. `#[serde(default)]`
    /// keeps pre-timestamp manifests loadable; they sort at the epoch.
    #[serde(default)]
    pub created_ms: u64,
    /// `"running"` while work proceeds, then `"complete"` / `"partial"` /
    /// `"failed"`. `#[serde(default)]` so pre-status manifests still parse.
    #[serde(default)]
    pub status: String,
}

/// `<data_dir>/swarms` - the one place every swarm manifest lives.
pub fn swarms_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("swarms")
}

/// Persist a manifest atomically: write to a unique tmp file, then rename
/// over the target. Creates the `swarms/` dir on demand. Unique tmp names
/// keep two writers racing on the same id from stealing each other's file.
pub fn write_manifest(data_dir: &Path, m: &SwarmManifest) -> Result<(), String> {
    if m.swarm_id.is_empty() || m.swarm_id.contains('/') || m.swarm_id.contains('\\') {
        return Err(format!("invalid swarm id {:?}", m.swarm_id));
    }
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = swarms_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("swarms dir: {e}"))?;
    let path = dir.join(format!("{}.json", m.swarm_id));
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(
        "{}.json.tmp.{}.{seq}",
        m.swarm_id,
        std::process::id()
    ));
    let raw = serde_json::to_string_pretty(m).map_err(|e| format!("serialize manifest: {e}"))?;
    std::fs::write(&tmp, raw)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Load every manifest in `swarms/`, newest first. Unreadable or malformed
/// files are skipped, never fatal: the index is best-effort, a corrupt
/// row must not break `swarm list`.
pub fn load_all_manifests(data_dir: &Path) -> Vec<SwarmManifest> {
    let dir = swarms_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<SwarmManifest> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Ok(m) = serde_json::from_str::<SwarmManifest>(&raw) {
            out.push(m);
        }
    }
    out.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| b.swarm_id.cmp(&a.swarm_id))
    });
    out
}

/// Read one manifest by id. Rejects path separators: callers pass user-facing
/// swarm ids and must never be able to escape `swarms/`.
pub fn read_manifest(data_dir: &Path, id: &str) -> Option<SwarmManifest> {
    if id.is_empty() || id.contains('/') || id.contains('\\') {
        return None;
    }
    let path = swarms_dir(data_dir).join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(id: &str, created_ms: u64) -> SwarmManifest {
        SwarmManifest {
            swarm_id: id.to_string(),
            task: format!("task {id}"),
            roles: vec!["default".to_string()],
            agents: vec![SwarmAgentManifest {
                label: format!("{id}:default:0"),
                run_id: format!("run-{id}"),
                role: "default".to_string(),
                status: "ok".to_string(),
            }],
            delivery: None,
            created_ms,
            status: "complete".to_string(),
        }
    }

    #[test]
    fn manifests_round_trip_and_sort_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_manifest(dir.path(), &manifest("sw_a", 100)).expect("write a");
        write_manifest(dir.path(), &manifest("sw_b", 300)).expect("write b");
        write_manifest(dir.path(), &manifest("sw_c", 200)).expect("write c");

        let all = load_all_manifests(dir.path());
        let ids: Vec<&str> = all.iter().map(|m| m.swarm_id.as_str()).collect();
        assert_eq!(ids, vec!["sw_b", "sw_c", "sw_a"]);

        let a = read_manifest(dir.path(), "sw_a").expect("read a");
        assert_eq!(a.created_ms, 100);
        assert_eq!(a.agents[0].run_id, "run-sw_a");
    }

    #[test]
    fn legacy_manifest_without_created_ms_still_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = swarms_dir(dir.path()).join("sw_old.json");
        std::fs::create_dir_all(swarms_dir(dir.path())).expect("mkdir");
        std::fs::write(
            &path,
            r#"{"swarm_id":"sw_old","task":"t","roles":[],"agents":[],"status":"partial"}"#,
        )
        .expect("write");
        let loaded = read_manifest(dir.path(), "sw_old").expect("parse legacy");
        assert_eq!(loaded.created_ms, 0);
        assert_eq!(loaded.status, "partial");
    }

    #[test]
    fn read_manifest_rejects_path_traversal() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_manifest(dir.path(), "../outside").is_none());
        assert!(read_manifest(dir.path(), "a/b").is_none());
        assert!(read_manifest(dir.path(), "").is_none());
    }

    #[test]
    fn write_manifest_rejects_unsafe_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut m = manifest("../escape", 1);
        assert!(write_manifest(dir.path(), &m).is_err());
        m.swarm_id = "safe".to_string();
        assert!(write_manifest(dir.path(), &m).is_ok());
    }
}
