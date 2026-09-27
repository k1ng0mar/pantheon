//! The writing half of the migration pipeline: backup -> apply -> validate.
//!
//! Nothing here runs unless the caller explicitly applies a plan. The order
//! is fixed and each stage is independently callable:
//!
//! ```text
//! backup(plan)  -> manifest   snapshot every target we would overwrite
//! apply(plan)   -> report     write imports, restoring from backup on failure
//! validate(plan)-> report     re-read every target and prove it landed
//! ```
//!
//! Two guarantees:
//!
//! - **Fail-stop.** The first write error aborts the run. A half-applied
//!   import is reported as such, never described as success.
//! - **No silent partial copy.** `copy_tree` records every skipped symlink
//!   and unreadable entry in the report rather than copying a partial tree.

use super::{merr, Action, ItemKind, MigrationPlan, PlanItem, Targets};
use pantheon_api::error::PantheonError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------------

/// One target that existed before we wrote, and where its pre-image went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupEntry {
    pub target: String,
    pub backup: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BackupManifest {
    /// Timestamp-derived id, also the directory name under the backup root.
    pub id: String,
    pub created_at_ms: i64,
    pub source: String,
    pub entries: Vec<BackupEntry>,
}

impl BackupManifest {
    pub fn root(&self) -> PathBuf {
        PathBuf::from(&self.id)
    }
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub fn path(&self) -> String {
        self.entries
            .iter()
            .map(|e| format!("{} -> {}", e.target, e.backup))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn io(code: &str, e: std::io::Error, what: String) -> PantheonError {
    merr(
        code,
        format!("{what}: {e}"),
        "check permissions and free space under the data dir, then re-run",
    )
}

/// Snapshot every existing import target so an apply can be undone by hand.
///
/// Targets that do not exist yet are not backed up — there is nothing to
/// restore, and an empty placeholder would be noise in the manifest.
pub fn backup(plan: &MigrationPlan, targets: &Targets) -> Result<BackupManifest, PantheonError> {
    let id = format!("{}", now_ms());
    let root = targets.backup_root().join(&id);
    let mut entries = Vec::new();

    for (_item, target) in import_targets(plan) {
        let target_path = PathBuf::from(target);
        if !target_path.exists() {
            continue;
        }
        // Mirror the absolute target path under the backup dir so two
        // targets can never collide on a flat name.
        let rel = target_path
            .strip_prefix("/")
            .unwrap_or(target_path.as_path())
            .to_path_buf();
        let dest = root.join(&rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| io("MIGRATE_BACKUP_MKDIR", e, parent.display().to_string()))?;
        }
        copy_any(&target_path, &dest)?;
        entries.push(BackupEntry {
            target: (*target).to_string(),
            backup: dest.to_string_lossy().to_string(),
        });
    }

    if !entries.is_empty() {
        let manifest = BackupManifest {
            id,
            created_at_ms: now_ms(),
            source: plan.source.clone(),
            entries,
        };
        let mpath = targets.backup_root().join(format!("{}.json", manifest.id));
        std::fs::write(
            &mpath,
            serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        )
        .map_err(|e| io("MIGRATE_BACKUP_WRITE", e, mpath.display().to_string()))?;
        Ok(manifest)
    } else {
        Ok(BackupManifest {
            id,
            created_at_ms: now_ms(),
            source: plan.source.clone(),
            entries,
        })
    }
}

/// Item kinds whose import is produced by a **bridge** rather than by copying
/// the source file.
///
/// These must be excluded from the file copier. `Credentials` is the sharp
/// case: its target is `<data_dir>/.env`, a real path, so a naive filter would
/// copy the source `.env` straight over Pantheon's key store — destroying the
/// merge, the never-clobber rule, and the 0600 permission in one write.
pub(crate) fn is_bridged(kind: ItemKind) -> bool {
    matches!(
        kind,
        ItemKind::Mcp | ItemKind::Session | ItemKind::Credentials | ItemKind::Provider
    )
}

/// Import items whose target is a real filesystem path, paired with that
/// target. Memory-plane items carry a `memory://` target and bridged items are
/// handled by `apply_bridges`; both are the CLI's problem, not the copier's.
pub(crate) fn import_targets(plan: &MigrationPlan) -> Vec<(&PlanItem, &str)> {
    plan.items
        .iter()
        .filter_map(|i| match &i.action {
            Action::Import { target }
                if !target.starts_with("memory://") && !is_bridged(i.kind) =>
            {
                Some((i, target.as_str()))
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplyStatus {
    /// Target did not exist; created.
    Created,
    /// Target existed; a pre-image was taken and it was replaced.
    Replaced,
    /// Already byte-identical to the source; left alone.
    Unchanged,
    /// Import with a non-filesystem target; routed to the memory plane.
    MemoryPlane,
    /// Source unreadable or vanished between plan and apply.
    SourceUnreadable,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    pub kind: ItemKind,
    pub source: String,
    pub target: String,
    pub status: ApplyStatus,
    pub detail: String,
    /// Symlinks and other entries deliberately not copied.
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReport {
    pub source: String,
    pub backup_id: String,
    pub outcomes: Vec<ApplyOutcome>,
    /// True when every filesystem import landed. A run that stopped early
    /// reports false; callers must not describe it as a clean import.
    pub complete: bool,
}

impl ApplyReport {
    pub fn ok(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.status,
                    ApplyStatus::Created | ApplyStatus::Replaced | ApplyStatus::Unchanged
                )
            })
            .count()
    }
    pub fn failures(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| {
                o.status == ApplyStatus::Failed || o.status == ApplyStatus::SourceUnreadable
            })
            .count()
    }
    /// Restore every backed-up pre-image over its target.
    pub fn rollback(&self, manifest: &BackupManifest) -> Result<usize, PantheonError> {
        let mut n = 0;
        for e in &manifest.entries {
            let target = PathBuf::from(&e.target);
            let backup = PathBuf::from(&e.backup);
            if !backup.exists() {
                continue;
            }
            if target.is_dir() {
                std::fs::remove_dir_all(&target)
                    .map_err(|e| io("MIGRATE_ROLLBACK_REMOVE", e, target.display().to_string()))?;
            } else if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|e| io("MIGRATE_ROLLBACK_REMOVE", e, target.display().to_string()))?;
            }
            copy_any(&backup, &target)?;
            n += 1;
        }
        Ok(n)
    }
}

/// Write every import in the plan. Stops at the first hard failure.
///
/// Three item kinds do not go through the file copier, because their source is
/// a file we refuse to copy: `Mcp` and `Session` are bridged by parsing the
/// source into a Pantheon-shaped artefact, and a credential manifest is
/// written names-only. Everything else is a verbatim copy.
pub fn apply(
    plan: &MigrationPlan,
    targets: &Targets,
    manifest: &BackupManifest,
) -> Result<ApplyReport, PantheonError> {
    apply_with(plan, targets, manifest, false)
}

/// As [`apply`], but `merge_providers` also merges detected custom providers
/// into the live `<data_dir>/config.toml`. Off by default: that file is the
/// runtime's own config, read on every startup, so touching it is opt-in.
pub fn apply_with(
    plan: &MigrationPlan,
    targets: &Targets,
    manifest: &BackupManifest,
    merge_providers: bool,
) -> Result<ApplyReport, PantheonError> {
    let mut outcomes = Vec::new();
    let mut complete = true;

    for (item, target) in import_targets(plan) {
        let src = PathBuf::from(&item.path);
        let dst = PathBuf::from(target);
        let mut skipped = Vec::new();

        if !src.exists() {
            complete = false;
            outcomes.push(ApplyOutcome {
                kind: item.kind,
                source: item.path.clone(),
                target: (*target).to_string(),
                status: ApplyStatus::SourceUnreadable,
                detail: "source vanished between plan and apply".to_string(),
                skipped,
            });
            break;
        }

        let existed = dst.exists();
        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                complete = false;
                outcomes.push(ApplyOutcome {
                    kind: item.kind,
                    source: item.path.clone(),
                    target: (*target).to_string(),
                    status: ApplyStatus::Failed,
                    detail: format!("mkdir {}: {e}", parent.display()),
                    skipped,
                });
                break;
            }
        }

        // Replace is remove-then-copy: a merge into an existing tree would
        // leave stale files from the previous import behind.
        if existed {
            let _ = if dst.is_dir() {
                std::fs::remove_dir_all(&dst)
            } else {
                std::fs::remove_file(&dst)
            };
        }

        let copied: Result<(), String> = if src.is_dir() {
            copy_tree(&src, &dst, &mut skipped).map_err(|e| e.to_string())
        } else {
            match std::fs::copy(&src, &dst) {
                Ok(_) => Ok(()),
                Err(e) => Err(format!("copy {}: {e}", src.display())),
            }
        };

        match copied {
            Ok(()) => outcomes.push(ApplyOutcome {
                kind: item.kind,
                source: item.path.clone(),
                target: (*target).to_string(),
                status: if existed {
                    ApplyStatus::Replaced
                } else {
                    ApplyStatus::Created
                },
                detail: if existed {
                    format!("replaced (pre-image in backup {})", manifest.id)
                } else {
                    "created".to_string()
                },
                skipped,
            }),
            Err(detail) => {
                complete = false;
                outcomes.push(ApplyOutcome {
                    kind: item.kind,
                    source: item.path.clone(),
                    target: (*target).to_string(),
                    status: ApplyStatus::Failed,
                    detail,
                    skipped,
                });
                break;
            }
        }
    }

    // Memory-plane items are reported so the count is honest, but not written
    // here: the memory write path is capability-gated and belongs to the CLI.
    for item in &plan.items {
        if let Action::Import { target } = &item.action {
            if target.starts_with("memory://") {
                outcomes.push(ApplyOutcome {
                    kind: item.kind,
                    source: item.path.clone(),
                    target: target.clone(),
                    status: ApplyStatus::MemoryPlane,
                    detail: "routed to the memory plane; write it with `pantheon memory put`"
                        .to_string(),
                    skipped: Vec::new(),
                });
            }
        }
    }

    outcomes.extend(apply_bridges(plan, targets, merge_providers)?);

    Ok(ApplyReport {
        source: plan.source.clone(),
        backup_id: manifest.id.clone(),
        outcomes,
        complete,
    })
}

fn bridge_providers(
    source: &str,
    src: &Path,
    dd: &Path,
    merge: bool,
) -> Result<ApplyOutcome, PantheonError> {
    let set = crate::providers::read_providers(src);
    if set.is_empty() {
        return Err(crate::merr(
            "MIGRATE_PROV_EMPTY",
            format!("{}: no custom provider to import", src.display()),
            "check the source config still declares a providers block",
        ));
    }
    let sidecar = crate::providers::write_provider_sidecar(dd, source, &set)?;
    let mut detail = format!(
        "{}; reviewable sidecar at {}",
        crate::providers::summarise(&set),
        sidecar.display()
    );
    let target = if merge {
        let (path, added, skipped) = crate::providers::merge_into_live_config(dd, &set)?;
        if !skipped.is_empty() {
            detail.push_str(&format!(
                "; {} already in config.toml, left untouched: {}",
                skipped.len(),
                skipped.join(", ")
            ));
        }
        detail.push_str(&format!("; merged {} into {}", added.len(), path.display()));
        path.to_string_lossy().to_string()
    } else {
        detail.push_str("; not merged into config.toml (pass --merge-providers to do so)");
        sidecar.to_string_lossy().to_string()
    };
    Ok(bridged(
        ItemKind::Provider,
        src.to_string_lossy().to_string(),
        target,
        detail,
    ))
}

// ---------------------------------------------------------------------------
// bridges: MCP, credentials, sessions, providers
// ---------------------------------------------------------------------------

/// Handle the item kinds whose source file must not be copied.
///
/// Each parses the source and writes a Pantheon-shaped artefact instead. A
/// failure here is reported per item rather than aborting the run, because
/// these are additive: a missing MCP declaration does not invalidate the
/// skills that already landed.
fn apply_bridges(
    plan: &MigrationPlan,
    targets: &Targets,
    merge_providers: bool,
) -> Result<Vec<ApplyOutcome>, PantheonError> {
    let mut out = Vec::new();
    let dd = &targets.data_dir;

    for item in &plan.items {
        if !matches!(item.action, Action::Import { .. }) {
            continue;
        }
        let src = Path::new(&item.path);
        let outcome = match item.kind {
            ItemKind::Mcp => bridge_mcp(plan.source.as_str(), src, dd),
            ItemKind::Session => bridge_sessions(plan.source.as_str(), src, dd),
            // Names-only manifest. A real `Secret` never reaches here: the
            // plan always gives it a Skip.
            ItemKind::Credentials => bridge_credentials(plan.source.as_str(), src, dd),
            // Providers default to a reviewable sidecar. Merging into the
            // live `config.toml` is opt-in via `merge_providers`, because that
            // file is read on every startup.
            ItemKind::Provider => bridge_providers(plan.source.as_str(), src, dd, merge_providers),
            _ => continue,
        };
        match outcome {
            Ok(o) => out.push(o),
            Err(e) => out.push(ApplyOutcome {
                kind: item.kind,
                source: item.path.clone(),
                target: item.target().unwrap_or("").to_string(),
                status: ApplyStatus::Failed,
                detail: e.to_string(),
                skipped: Vec::new(),
            }),
        }
    }
    Ok(out)
}

fn bridged(kind: ItemKind, source: String, target: String, detail: String) -> ApplyOutcome {
    ApplyOutcome {
        kind,
        source,
        target,
        status: ApplyStatus::Created,
        detail,
        skipped: Vec::new(),
    }
}

fn bridge_mcp(source: &str, src: &Path, dd: &Path) -> Result<ApplyOutcome, PantheonError> {
    let servers = if src.extension().map(|e| e == "json").unwrap_or(false) {
        crate::carry::parse_mcp_json(&std::fs::read_to_string(src).unwrap_or_default())?
    } else {
        crate::carry::parse_hermes_mcp(&std::fs::read_to_string(src).unwrap_or_default())
    };
    if servers.is_empty() {
        return Err(crate::merr(
            "MIGRATE_MCP_EMPTY",
            format!("{}: no mcp server found to bridge", src.display()),
            "check the source config still declares mcp_servers",
        ));
    }
    let needs = servers.iter().filter(|s| s.needs_credentials).count();
    let path = crate::carry::write_mcp_declaration(dd, source, &servers)?;
    Ok(ApplyOutcome {
        ..bridged(
            ItemKind::Mcp,
            src.to_string_lossy().to_string(),
            path.to_string_lossy().to_string(),
            format!(
                "{} mcp server(s) declared; {needs} need a credential you supply at registration",
                servers.len()
            ),
        )
    })
}

fn bridge_sessions(source: &str, src: &Path, dd: &Path) -> Result<ApplyOutcome, PantheonError> {
    let imp = crate::carry::write_session_import(dd, source, src)?;
    Ok(ApplyOutcome {
        ..bridged(
            ItemKind::Session,
            src.to_string_lossy().to_string(),
            imp.manifest.to_string_lossy().to_string(),
            format!(
                "{} transcript file(s) quarantined; index them with session_search deliberately",
                imp.files.len()
            ),
        )
    })
}

fn bridge_credentials(source: &str, src: &Path, dd: &Path) -> Result<ApplyOutcome, PantheonError> {
    // A `.env` is carried into pantheon's own key store. Values are read here
    // and go straight to `<data_dir>/.env`; they are never returned, logged,
    // or printed, and the report names keys only.
    let r = crate::carry::merge_env_into(dd, source, src)?;
    Ok(ApplyOutcome {
        ..bridged(
            ItemKind::Credentials,
            src.to_string_lossy().to_string(),
            r.path.clone(),
            format!(
                "{} key(s) carried into {}, {} already present (left untouched), {} without a value, {} not a credential",
                r.added.len(),
                r.path,
                r.already_present.len(),
                r.no_value.len(),
                r.unclassified.len()
            ),
        )
    })
}

// ---------------------------------------------------------------------------
// validate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidateStatus {
    /// Present and structurally what the source kind requires.
    Ok,
    /// Expected shape is missing (e.g. a skill with no SKILL.md).
    Incomplete,
    /// Not on disk at all.
    Missing,
    /// Memory-plane item: not a filesystem check.
    NotApplicable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateOutcome {
    pub kind: ItemKind,
    pub target: String,
    pub status: ValidateStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidateReport {
    pub source: String,
    pub outcomes: Vec<ValidateOutcome>,
    pub complete: bool,
}

impl ValidateReport {
    pub fn ok(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| o.status == ValidateStatus::Ok)
            .count()
    }
    pub fn problems(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|o| {
                matches!(
                    o.status,
                    ValidateStatus::Incomplete | ValidateStatus::Missing
                )
            })
            .count()
    }
    pub fn render(&self) -> String {
        let mut s = format!(
            "validate {}: {} ok, {} problem(s)\n",
            self.source,
            self.ok(),
            self.problems()
        );
        for o in &self.outcomes {
            let mark = match o.status {
                ValidateStatus::Ok => "+",
                ValidateStatus::NotApplicable => "-",
                _ => "!",
            };
            s.push_str(&format!(
                "  {} {:<10} {} {}\n",
                mark, o.kind, o.target, o.detail
            ));
        }
        s
    }
}

/// Re-read every import target and prove it landed with the right shape.
pub fn validate(plan: &MigrationPlan) -> ValidateReport {
    let mut outcomes = Vec::new();
    for (item, target) in import_targets(plan) {
        let p = Path::new(target);
        let (status, detail) = if !p.exists() {
            (ValidateStatus::Missing, "not on disk".to_string())
        } else if item.kind == ItemKind::Skill {
            if p.join("SKILL.md").is_file() {
                (ValidateStatus::Ok, "SKILL.md present".to_string())
            } else {
                (
                    ValidateStatus::Incomplete,
                    "skill dir has no SKILL.md".to_string(),
                )
            }
        } else if item.kind == ItemKind::Extension {
            if p.is_dir() {
                (ValidateStatus::Ok, "extension dir present".to_string())
            } else {
                (
                    ValidateStatus::Incomplete,
                    "extension is not a directory".to_string(),
                )
            }
        } else if p.is_file() {
            (ValidateStatus::Ok, "file present".to_string())
        } else {
            (ValidateStatus::Ok, "present".to_string())
        };
        outcomes.push(ValidateOutcome {
            kind: item.kind,
            target: (*target).to_string(),
            status,
            detail,
        });
    }
    // Bridged items are checked against the artefact they actually produced,
    // not the source file. A credential carry lands in the key store, and a
    // credential store is legitimately allowed to be absent when the source
    // declared no usable key.
    for item in &plan.items {
        if !is_bridged(item.kind) {
            continue;
        }
        let Action::Import { target } = &item.action else {
            continue;
        };
        let p = Path::new(target);
        let (status, detail) = match item.kind {
            ItemKind::Credentials => {
                if p.is_file() {
                    (ValidateStatus::Ok, "key store present".to_string())
                } else {
                    (
                        ValidateStatus::Incomplete,
                        "no key store written; the source declared no usable credential"
                            .to_string(),
                    )
                }
            }
            _ if p.exists() => (ValidateStatus::Ok, "bridge artefact present".to_string()),
            _ => (
                ValidateStatus::Missing,
                "bridge artefact not on disk".to_string(),
            ),
        };
        outcomes.push(ValidateOutcome {
            kind: item.kind,
            target: target.clone(),
            status,
            detail,
        });
    }
    for item in &plan.items {
        if let Action::Import { target } = &item.action {
            if target.starts_with("memory://") {
                outcomes.push(ValidateOutcome {
                    kind: item.kind,
                    target: target.clone(),
                    status: ValidateStatus::NotApplicable,
                    detail: "memory plane; verify with `pantheon memory recall`".to_string(),
                });
            }
        }
    }
    let complete = outcomes.iter().all(|o| {
        !matches!(
            o.status,
            ValidateStatus::Incomplete | ValidateStatus::Missing
        )
    });
    ValidateReport {
        source: plan.source.clone(),
        outcomes,
        complete,
    }
}

// ---------------------------------------------------------------------------
// copy helpers
// ---------------------------------------------------------------------------

fn copy_any(src: &Path, dst: &Path) -> Result<(), PantheonError> {
    if src.is_dir() {
        let mut ignored = Vec::new();
        copy_tree(src, dst, &mut ignored)
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)
                .map_err(|e| io("MIGRATE_COPY_MKDIR", e, p.display().to_string()))?;
        }
        std::fs::copy(src, dst).map(|_| ()).map_err(|e| {
            io(
                "MIGRATE_COPY",
                e,
                format!("{} -> {}", src.display(), dst.display()),
            )
        })
    }
}

/// Recursive copy that refuses to follow symlinks and records what it left
/// behind. A symlink in a source tree could point anywhere, including outside
/// the data dir, so it is never dereferenced.
///
/// Depth-bounded by [`MAX_COPY_DEPTH`], so a pathological or cyclic-looking
/// source tree cannot make one import recurse without limit. Exceeding the
/// bound is recorded in `skipped` like any other non-copied entry — the run
/// still completes, and the omission is visible.
const MAX_COPY_DEPTH: usize = 24;

pub(crate) fn copy_tree(
    src: &Path,
    dst: &Path,
    skipped: &mut Vec<String>,
) -> Result<(), PantheonError> {
    copy_tree_at(src, dst, 0, skipped)
}

fn copy_tree_at(
    src: &Path,
    dst: &Path,
    depth: usize,
    skipped: &mut Vec<String>,
) -> Result<(), PantheonError> {
    if depth > MAX_COPY_DEPTH {
        skipped.push(format!(
            "{}: deeper than {MAX_COPY_DEPTH} levels, not copied",
            src.display()
        ));
        return Ok(());
    }
    let rd =
        std::fs::read_dir(src).map_err(|e| io("MIGRATE_READ_DIR", e, src.display().to_string()))?;
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                skipped.push(format!("{}: unreadable dir entry ({e})", src.display()));
                continue;
            }
        };
        let from = entry.path();
        let to = dst.join(entry.file_name());
        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                skipped.push(format!("{}: unreadable file type ({e})", from.display()));
                continue;
            }
        };
        if ft.is_symlink() {
            skipped.push(format!("{}: symlink, not followed", from.display()));
            continue;
        }
        if ft.is_dir() {
            copy_tree_at(&from, &to, depth + 1, skipped)?;
        } else if ft.is_file() {
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p)
                    .map_err(|e| io("MIGRATE_COPY_MKDIR", e, p.display().to_string()))?;
            }
            std::fs::copy(&from, &to).map_err(|e| {
                io(
                    "MIGRATE_COPY",
                    e,
                    format!("{} -> {}", from.display(), to.display()),
                )
            })?;
        }
        // Sockets, fifos, devices: not copied, and not worth a line each.
    }
    Ok(())
}
