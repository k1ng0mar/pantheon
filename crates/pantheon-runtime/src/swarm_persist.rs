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
/// `created_ms` lives on the CLI's `SwarmRecord` (the spawn path sets it);
/// the orchestrator leaves it unset, so it is not part of this shared struct.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmManifest {
    pub swarm_id: String,
    pub task: String,
    pub roles: Vec<String>,
    pub agents: Vec<SwarmAgentManifest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<String>,
    /// `"running"` while work proceeds, then `"complete"` / `"partial"` /
    /// `"failed"`. `#[serde(default)]` so pre-status manifests still parse.
    #[serde(default)]
    pub status: String,
}

/// `<data_dir>/swarms` - the one place every swarm manifest lives.
pub fn swarms_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("swarms")
}

/// Persist a manifest atomically: write to a tmp file, then rename over the
/// target. Creates the `swarms/` dir on demand.
pub fn write_manifest(data_dir: &Path, m: &SwarmManifest) -> Result<(), String> {
    let dir = swarms_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("swarms dir: {e}"))?;
    let path = dir.join(format!("{}.json", m.swarm_id));
    let tmp = dir.join(format!("{}.json.tmp", m.swarm_id));
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
    out.sort_by(|a, b| b.swarm_id.cmp(&a.swarm_id));
    out
}

/// Read one manifest by id.
pub fn read_manifest(data_dir: &Path, id: &str) -> Option<SwarmManifest> {
    let path = swarms_dir(data_dir).join(format!("{id}.json"));
    let raw = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}
