//! Migration (spec section 23): detect -> analyze -> plan -> dry-run ->
//! approval -> backup -> apply -> validate.
//!
//! Wave 2 scope: detection, analysis, plan, and dry-run over real on-disk
//! layouts (Hermes, OpenClaw). Nothing on the source side is written; every
//! unmappable item is archived into the plan instead of silently dropped.
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Which system we are importing from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceKind { Hermes, OpenClaw }

impl SourceKind {
    pub fn name(&self) -> &'static str {
        match self { SourceKind::Hermes => "hermes", SourceKind::OpenClaw => "openclaw" }
    }
}

/// Provenance carried by everything imported (spec section 23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    pub source_path: String,
    pub imported_at_ms: i64,
}

/// One discovered item on the source side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detected {
    pub kind: String,
    pub path: String,
    pub mappable: bool,
    pub note: String,
}

/// What a dry-run would do, item by item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action { Import { target: String }, Archive { reason: String } }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub kind: String,
    pub path: String,
    pub action: Action,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub source: String,
    pub root: String,
    pub items: Vec<PlanItem>,
}

impl MigrationPlan {
    pub fn imports(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.action, Action::Import { .. })).count()
    }
    pub fn archived(&self) -> usize {
        self.items.iter().filter(|i| matches!(i.action, Action::Archive { .. })).count()
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Detect a source root without assuming a fixed absolute layout.
/// Hermes ships `config.yaml` + `plugins/` + `skills/`; OpenClaw ships
/// `openclaw.json`-style plugin manifests (`openclaw.plugin.json`).
pub fn detect(root: &Path) -> Vec<SourceKind> {
    let mut found = Vec::new();
    if root.join("config.yaml").exists() || root.join("plugins").is_dir() {
        found.push(SourceKind::Hermes);
    }
    if root.join("skills").is_dir() || root.join("plugins").is_dir() {
        // OpenClaw layouts put plugin manifests inside plugins/.
        if plugins_are_openclaw(root) || root.join("clawhub.json").exists() {
            found.push(SourceKind::OpenClaw);
        }
    }
    found
}

fn plugins_are_openclaw(root: &Path) -> bool {
    let dir = root.join("plugins");
    let Ok(rd) = std::fs::read_dir(&dir) else { return false; };
    for e in rd.flatten() {
        if e.path().join("openclaw.plugin.json").exists() { return true; }
    }
    false
}

/// Analyse a source root: what exists, what maps, what has to be archived.
/// Read-only. No writes anywhere on the source side.
pub fn analyze(root: &Path, kind: SourceKind) -> Vec<Detected> {
    let mut out = Vec::new();
    match kind {
        SourceKind::Hermes => {
            let plugins = root.join("plugins");
            if let Ok(rd) = std::fs::read_dir(&plugins) {
                for e in rd.flatten() {
                    let p = e.path();
                    if !p.is_dir() { continue; }
                    let has_manifest = p.join("plugin.yaml").exists();
                    let has_ts = p.join("index.ts").exists();
                    let mappable = has_manifest && p.join("__init__.py").exists() && !has_ts;
                    out.push(Detected {
                        kind: "plugin".into(),
                        path: p.to_string_lossy().to_string(),
                        mappable,
                        note: if mappable {
                            "python register(ctx) hook plugin; imports natively".into()
                        } else if has_ts && !p.join("__init__.py").exists() {
                            "typescript entry; needs OpenClaw-compat adapter".into()
                        } else {
                            "no __init__.py or no plugin.yaml; archive".into()
                        },
                    });
                }
            }
            let skills = root.join("skills");
            if let Ok(rd) = std::fs::read_dir(&skills) {
                for e in rd.flatten() {
                    let p = e.path();
                    let is_dir = p.is_dir();
                    out.push(Detected {
                        kind: "skill".into(),
                        path: p.to_string_lossy().to_string(),
                        mappable: is_dir && p.join("SKILL.md").exists(),
                        note: if is_dir && p.join("SKILL.md").exists() {
                            "portable SKILL.md; Tier 1 drop-in".into()
                        } else { "no SKILL.md; archive".into() },
                    });
                }
            }
            for extra in ["MEMORY.md", "USER.md", "SOUL.md"] {
                let p = root.join(extra);
                if p.exists() {
                    out.push(Detected {
                        kind: "memory".into(), path: p.to_string_lossy().to_string(),
                        mappable: true,
                        note: "memory file; imports into the memory plane with provenance".into(),
                    });
                }
            }
        }
        SourceKind::OpenClaw => {
            let plugins = root.join("plugins");
            if let Ok(rd) = std::fs::read_dir(&plugins) {
                for e in rd.flatten() {
                    let p = e.path();
                    if !p.is_dir() { continue; }
                    let has_manifest = p.join("openclaw.plugin.json").exists();
                    out.push(Detected {
                        kind: "openclaw-plugin".into(),
                        path: p.to_string_lossy().to_string(),
                        mappable: !has_manifest,
                        note: if has_manifest {
                            "openclaw.plugin.json; goes through the compat adapter".into()
                        } else { "no openclaw.plugin.json; archive".into() },
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Build the dry-run plan. Unmappable items are archived with a reason,
/// never dropped. `target_root` is where imports would land.
pub fn plan(root: &Path, kind: SourceKind, target_root: &Path) -> MigrationPlan {
    let detected = analyze(root, kind);
    let mut items = Vec::new();
    for d in detected {
        let file_name = Path::new(&d.path)
            .file_name().map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "item".into());
        let action = if d.mappable {
            Action::Import { target: target_root.join(&d.kind).join(&file_name)
                .to_string_lossy().to_string() }
        } else {
            Action::Archive { reason: d.note.clone() }
        };
        items.push(PlanItem { kind: d.kind, path: d.path, action });
    }
    MigrationPlan { source: kind.name().into(), root: root.to_string_lossy().to_string(), items }
}

/// Provenance for everything the plan would import.
pub fn provenance(root: &Path, kind: SourceKind) -> Provenance {
    Provenance {
        source: kind.name().into(),
        source_path: root.to_string_lossy().to_string(),
        imported_at_ms: now_ms(),
    }
}

/// Render a plan for humans (dry-run output).
pub fn render(p: &MigrationPlan) -> String {
    let mut s = format!("migrate {} from {}\n", p.source, p.root);
    s.push_str(&format!("  {} to import, {} to archive\n", p.imports(), p.archived()));
    for i in &p.items {
        match &i.action {
            Action::Import { target } => s.push_str(&format!("  + {:<10} {} -> {}\n", i.kind, i.path, target)),
            Action::Archive { reason } => s.push_str(&format!("  ~ {:<10} {} (archive: {})\n", i.kind, i.path, reason)),
        }
    }
    s
}

#[cfg(test)]
mod tests {
        use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pantheon-migrate-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_hermes_layout() {
        let d = tmp("hermes");
        fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
        fs::create_dir_all(d.join("plugins/p1")).unwrap();
        assert_eq!(detect(&d), vec![SourceKind::Hermes]);
    }

    #[test]
    fn detects_openclaw_plugins() {
        let d = tmp("openclaw");
        fs::create_dir_all(d.join("plugins/soul")).unwrap();
        fs::write(d.join("plugins/soul/openclaw.plugin.json"), "{}").unwrap();
        assert!(detect(&d).contains(&SourceKind::OpenClaw));
    }

    #[test]
    fn plan_imports_python_plugins_and_archives_missing_entries() {
        let d = tmp("plan");
        let good = d.join("plugins/good");
        fs::create_dir_all(&good).unwrap();
        fs::write(good.join("plugin.yaml"), "name: good\n").unwrap();
        fs::write(good.join("__init__.py"), "def register(ctx): pass\n").unwrap();
        let bad = d.join("plugins/bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("plugin.yaml"), "name: bad\n").unwrap();
        let p = plan(&d, SourceKind::Hermes, Path::new("/tmp/pantheon-target"));
        assert_eq!(p.imports(), 1);
        assert_eq!(p.archived(), 1);
        assert!(render(&p).contains("archive:"));
    }

    #[test]
    fn provenance_records_source_and_version() {
        let d = tmp("prov");
        let pr = provenance(&d, SourceKind::Hermes);
        assert_eq!(pr.source, "hermes");
        assert!(pr.imported_at_ms > 0);
    }
}
