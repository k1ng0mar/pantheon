//! The writing half of the migration pipeline: backup -> stage -> commit.
//!
//! Nothing here runs unless the caller explicitly applies a plan. The order
//! is fixed:
//!
//! ```text
//! backup(plan)  -> manifest   snapshot every target we would overwrite
//! stage(plan)   -> staged     copy every import into a staging tree, budgeted
//! validate      -> report     prove the staged tree has the right shape
//! commit(staged)-> report     rename the staged tree into place, atomically
//! ```
//!
//! Three guarantees:
//!
//! - **Transactional.** `apply` stages everything under
//!   `<dest>/.migrate-stage/<id>/` first. The live tree is untouched until
//!   every item has staged and validated; commit is a sequence of renames,
//!   and any commit error triggers an automatic rollback from the backup
//!   pre-images. A failed apply leaves the target untouched — there are no
//!   manual-restore instructions because there is nothing manual to do.
//! - **Budgeted.** Every byte staged or backed up is charged against
//!   [`StageBudgets`] (default 1 GiB total, 100k files, 100 MiB per file).
//!   Exceeding a budget aborts cleanly: the staging tree is discarded and
//!   the live tree is untouched.
//! - **No silent partial copy.** `copy_tree` records every skipped symlink
//!   and unreadable entry in the report rather than copying a partial tree.
//!
//! Budgets can be tuned with [`StageBudgets::from_env`]
//! (`PANTHEON_MIGRATE_MAX_BYTES`, `PANTHEON_MIGRATE_MAX_FILES`,
//! `PANTHEON_MIGRATE_MAX_FILE_BYTES`).

use super::{merr, Action, ItemKind, MigrationPlan, PlanItem, Targets};
use pantheon_api::error::PantheonError;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// budgets
// ---------------------------------------------------------------------------

/// Guardrails on how much disk one migration may consume. Every byte copied
/// by backup or staging is charged; exceeding any budget aborts cleanly with
/// the live tree untouched.
#[derive(Debug, Clone)]
pub struct StageBudgets {
    /// Cap on the total bytes copied by one backup+apply pass.
    pub max_total_bytes: u64,
    /// Cap on the number of files copied by one backup+apply pass.
    pub max_files: u64,
    /// Cap on any single file. A source tree with one enormous file fails
    /// fast instead of filling the disk.
    pub max_single_file_bytes: u64,
}

impl Default for StageBudgets {
    fn default() -> Self {
        Self {
            max_total_bytes: 1024 * 1024 * 1024,      // 1 GiB
            max_files: 100_000,                       // 100k files
            max_single_file_bytes: 100 * 1024 * 1024, // 100 MiB
        }
    }
}

impl StageBudgets {
    /// Defaults, overridden by the environment when set:
    /// `PANTHEON_MIGRATE_MAX_BYTES`, `PANTHEON_MIGRATE_MAX_FILES`,
    /// `PANTHEON_MIGRATE_MAX_FILE_BYTES` (plain byte counts).
    pub fn from_env() -> Self {
        let mut b = Self::default();
        if let Some(v) = std::env::var("PANTHEON_MIGRATE_MAX_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            b.max_total_bytes = v;
        }
        if let Some(v) = std::env::var("PANTHEON_MIGRATE_MAX_FILES")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            b.max_files = v;
        }
        if let Some(v) = std::env::var("PANTHEON_MIGRATE_MAX_FILE_BYTES")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            b.max_single_file_bytes = v;
        }
        b
    }

    /// No limits. For tests and for callers that enforce their own budgets.
    pub fn unlimited() -> Self {
        Self {
            max_total_bytes: u64::MAX,
            max_files: u64::MAX,
            max_single_file_bytes: u64::MAX,
        }
    }
}

/// Running counters for one backup+stage pass.
#[derive(Debug, Default)]
pub struct BudgetUsage {
    pub bytes: u64,
    pub files: u64,
}

impl StageBudgets {
    pub(crate) fn charge(
        &self,
        usage: &mut BudgetUsage,
        size: u64,
        what: &str,
    ) -> Result<(), PantheonError> {
        if size > self.max_single_file_bytes {
            return Err(merr(
                "MIGRATE_BUDGET_FILE_SIZE",
                format!(
                    "{what}: {size} bytes exceeds the per-file budget of {} bytes",
                    self.max_single_file_bytes
                ),
                "raise PANTHEON_MIGRATE_MAX_FILE_BYTES or exclude the file from the migration",
            ));
        }
        usage.files += 1;
        if usage.files > self.max_files {
            return Err(merr(
                "MIGRATE_BUDGET_FILES",
                format!(
                    "{what}: file count budget of {} exceeded",
                    self.max_files
                ),
                "raise PANTHEON_MIGRATE_MAX_FILES or migrate fewer categories",
            ));
        }
        usage.bytes = usage.bytes.saturating_add(size);
        if usage.bytes > self.max_total_bytes {
            return Err(merr(
                "MIGRATE_BUDGET_BYTES",
                format!(
                    "{what}: total byte budget of {} exceeded",
                    self.max_total_bytes
                ),
                "raise PANTHEON_MIGRATE_MAX_BYTES or migrate fewer categories",
            ));
        }
        Ok(())
    }
}

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

/// Snapshot every existing import target so an apply can be undone.
///
/// Targets that do not exist yet are not backed up — there is nothing to
/// restore, and an empty placeholder would be noise in the manifest.
///
/// The copy is charged against the default [`StageBudgets`]; see
/// [`backup_with_budgets`].
pub fn backup(plan: &MigrationPlan, targets: &Targets) -> Result<BackupManifest, PantheonError> {
    backup_with_budgets(plan, targets, &StageBudgets::default())
}

/// As [`backup`], with explicit budgets. A budget breach aborts before
/// anything is staged: the partial backup tree is removed, so a failed
/// backup leaves no residue behind.
pub fn backup_with_budgets(
    plan: &MigrationPlan,
    targets: &Targets,
    budgets: &StageBudgets,
) -> Result<BackupManifest, PantheonError> {
    let id = format!("{}", now_ms());
    let root = targets.backup_root().join(&id);
    let mut usage = BudgetUsage::default();

    let outcome: Result<Vec<BackupEntry>, PantheonError> = (|| {
        let mut entries = Vec::new();
        for target in backup_targets(plan, targets) {
            let target_path = PathBuf::from(&target);
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
            copy_any(&target_path, &dest, budgets, &mut usage)?;
            entries.push(BackupEntry {
                target,
                backup: dest.to_string_lossy().to_string(),
            });
        }
        Ok(entries)
    })();

    let entries = match outcome {
        Ok(e) => e,
        Err(e) => {
            // Clean abort: a half-written backup must not linger as a
            // plausible-looking restore point.
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_file(targets.backup_root().join(format!("{id}.json")));
            return Err(e);
        }
    };

    let manifest = BackupManifest {
        id,
        created_at_ms: now_ms(),
        source: plan.source.clone(),
        entries,
    };
    if !manifest.is_empty() {
        let mpath = targets
            .backup_root()
            .join(format!("{}.json", manifest.id));
        std::fs::write(
            &mpath,
            serde_json::to_string_pretty(&manifest).unwrap_or_default(),
        )
        .map_err(|e| io("MIGRATE_BACKUP_WRITE", e, mpath.display().to_string()))?;
    }
    Ok(manifest)
}

/// Every live path the apply may overwrite: the file copier's targets plus
/// the artefacts the bridges write. A bridge target with a pre-image is what
/// makes commit-time rollback able to restore e.g. the previous `.env`.
fn backup_targets(plan: &MigrationPlan, targets: &Targets) -> Vec<String> {
    let mut out: Vec<String> = import_targets(plan)
        .into_iter()
        .map(|(_, t)| (*t).to_string())
        .collect();
    let dd = &targets.data_dir;
    for item in &plan.items {
        if !matches!(item.action, Action::Import { .. }) || !is_bridged(item.kind) {
            continue;
        }
        match item.kind {
            ItemKind::Mcp => out.push(
                dd.join("mcp")
                    .join(format!("{}.json", plan.source))
                    .to_string_lossy()
                    .to_string(),
            ),
            ItemKind::Session => out.push(
                dd.join("imported-sessions")
                    .join(&plan.source)
                    .to_string_lossy()
                    .to_string(),
            ),
            ItemKind::Credentials => {
                out.push(
                    crate::carry::pantheon_env_path(dd)
                        .to_string_lossy()
                        .to_string(),
                );
                out.push(
                    dd.join("credentials")
                        .join(format!("{}.json", plan.source))
                        .to_string_lossy()
                        .to_string(),
                );
            }
            ItemKind::Provider => {
                out.push(
                    crate::providers::provider_sidecar_path(dd)
                        .to_string_lossy()
                        .to_string(),
                );
                // Only touched with --merge-providers, but the pre-image is
                // a few kilobytes and keeps backup() independent of the flag.
                out.push(dd.join("config.toml").to_string_lossy().to_string());
            }
            _ => {}
        }
    }
    out.sort();
    out.dedup();
    out
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
    ///
    /// This is the manual escape hatch. The normal path never needs it:
    /// [`apply_with`] rolls back automatically when a commit fails.
    pub fn rollback(&self, manifest: &BackupManifest) -> Result<usize, PantheonError> {
        let mut n = 0;
        for e in &manifest.entries {
            restore_backup_entry(e)?;
            n += 1;
        }
        Ok(n)
    }
}

// ---------------------------------------------------------------------------
// path helpers
// ---------------------------------------------------------------------------

/// Existence including dangling symlinks (`Path::exists` follows links and
/// reports false for a dangling one).
fn path_exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

/// Remove a file, dir, or symlink without following the link. A symlink is
/// never dereferenced: it is unlinked like a file.
fn remove_path(p: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(p)?.file_type().is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

/// Restore one backup entry over its live target. Restoring is never
/// budget-limited: refusing to roll back for budget reasons would be worse
/// than the disk use.
fn restore_backup_entry(entry: &BackupEntry) -> Result<(), PantheonError> {
    let target = PathBuf::from(&entry.target);
    let backup = PathBuf::from(&entry.backup);
    if !backup.exists() {
        return Err(merr(
            "MIGRATE_ROLLBACK_NO_BACKUP",
            format!("pre-image missing for {}", entry.target),
            "the backup dir may have been pruned; restore by hand from migrate-backups/",
        ));
    }
    if path_exists(&target) {
        remove_path(&target)
            .map_err(|e| io("MIGRATE_ROLLBACK_REMOVE", e, target.display().to_string()))?;
    }
    let budgets = StageBudgets::unlimited();
    let mut usage = BudgetUsage::default();
    copy_any(&backup, &target, &budgets, &mut usage)?;
    Ok(())
}

/// Byte comparison for the unchanged fast path. Sizes differ → not equal;
/// otherwise a full read of both sides.
fn files_equal(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if ma.len() != mb.len() {
        return false;
    }
    let (Ok(ra), Ok(rb)) = (std::fs::read(a), std::fs::read(b)) else {
        return false;
    };
    ra == rb
}

// ---------------------------------------------------------------------------
// stage -> validate -> commit
// ---------------------------------------------------------------------------

/// One artefact staged for commit: where it sits in the staging tree, where
/// it will land, and whether the live target already exists (i.e. whether a
/// backup pre-image covers it).
#[derive(Debug, Clone)]
pub struct StagedItem {
    pub kind: ItemKind,
    pub source: String,
    /// Path inside `<dest>/.migrate-stage/<id>/`.
    pub staged: PathBuf,
    /// The live target the commit will write.
    pub live: PathBuf,
    pub existed: bool,
    pub skipped: Vec<String>,
    /// The report outcome for this item. `None` for bridge artefacts, whose
    /// outcome comes from the bridge run itself.
    pub outcome: Option<ApplyOutcome>,
}

/// The result of [`stage`]: everything that staged cleanly, plus the
/// terminal outcomes (memory-plane, unchanged, per-item failures) that need
/// no commit.
#[derive(Debug)]
pub struct Staged {
    pub id: String,
    pub source: String,
    pub manifest: BackupManifest,
    /// Distinct `<dest>/.migrate-stage/<id>` roots, for cleanup.
    stage_roots: Vec<PathBuf>,
    pub items: Vec<StagedItem>,
    /// Bridge outcomes, with targets rewritten from staged to live paths.
    pub bridge_outcomes: Vec<ApplyOutcome>,
    /// Outcomes that need no commit.
    pub terminal: Vec<ApplyOutcome>,
    /// False when any item failed to stage or staged validation failed. When
    /// false, nothing is committed: the live tree stays untouched.
    pub complete: bool,
}

impl Staged {
    /// Discard the staging tree. The live tree was never touched, so there
    /// is nothing to roll back. The now-empty `.migrate-stage` parent is
    /// removed too, so a clean abort leaves no residue.
    pub fn discard(&self) {
        let mut parents: HashSet<PathBuf> = HashSet::new();
        for root in &self.stage_roots {
            let _ = std::fs::remove_dir_all(root);
            if let Some(p) = root.parent() {
                parents.insert(p.to_path_buf());
            }
        }
        for p in parents {
            let _ = std::fs::remove_dir(p);
        }
    }

    /// The report for an apply that staged but did not commit: every staged
    /// outcome is withheld because nothing landed.
    fn incomplete_report(&self) -> ApplyReport {
        let mut outcomes = self.terminal.clone();
        outcomes.extend(self.bridge_outcomes.clone());
        ApplyReport {
            source: self.source.clone(),
            backup_id: self.manifest.id.clone(),
            outcomes,
            complete: false,
        }
    }
}

/// Which staging root a live target stages under, and its path relative to
/// that root. The staging root lives inside the destination dir itself, so
/// the commit rename never crosses a filesystem boundary.
fn stage_slot(
    targets: &Targets,
    stage_id: &str,
    live: &Path,
) -> Result<(PathBuf, PathBuf), PantheonError> {
    // The more specific prefix wins: ext_dir defaults to a subdir of
    // data_dir, and a target under it must stage under ext_dir so a
    // separately-mounted extensions dir still renames within itself.
    let base = if live.starts_with(&targets.ext_dir) && targets.ext_dir != targets.data_dir {
        &targets.ext_dir
    } else if live.starts_with(&targets.data_dir) {
        &targets.data_dir
    } else {
        return Err(merr(
            "MIGRATE_TARGET_OUTSIDE",
            format!("{} is under neither the data dir nor the extensions dir", live.display()),
            "the plan target is inconsistent with the targets; re-plan",
        ));
    };
    let rel = live
        .strip_prefix(base)
        .map_err(|_| {
            merr(
                "MIGRATE_TARGET_OUTSIDE",
                format!("{} is not under {}", live.display(), base.display()),
                "the plan target is inconsistent with the targets; re-plan",
            )
        })?
        .to_path_buf();
    Ok((base.join(".migrate-stage").join(stage_id), rel))
}

/// Stage every import of the plan into `<dest>/.migrate-stage/<id>/`.
///
/// The live tree is not touched. Hard failures (IO errors, budget breaches)
/// return `Err` after discarding the staging tree; per-item content failures
/// (a vanished source, an unparseable bridge input) are recorded as failed
/// outcomes and set `complete` to false, which makes [`apply_with`] skip
/// the commit entirely.
pub(crate) fn stage(
    plan: &MigrationPlan,
    targets: &Targets,
    manifest: &BackupManifest,
    budgets: &StageBudgets,
    merge_providers: bool,
) -> Result<Staged, PantheonError> {
    let stage_id = format!("{}-stage", now_ms());
    let mut usage = BudgetUsage::default();
    let mut staged = Staged {
        id: stage_id.clone(),
        source: plan.source.clone(),
        manifest: manifest.clone(),
        stage_roots: Vec::new(),
        items: Vec::new(),
        bridge_outcomes: Vec::new(),
        terminal: Vec::new(),
        complete: true,
    };
    // Any `?` below must leave no residue: the error path discards the
    // staging tree. `roots` is drained into `staged.stage_roots` on every
    // exit so the discard sees every root that was created.
    let mut roots: HashSet<PathBuf> = HashSet::new();
    let result: Result<(), PantheonError> = (|| {
        for (item, target) in import_targets(plan) {
            let src = PathBuf::from(&item.path);
            let live = PathBuf::from(target);
            let (stage_root, rel) = stage_slot(targets, &stage_id, &live)?;
            roots.insert(stage_root.clone());

            if !path_exists(&src) {
                staged.complete = false;
                staged.terminal.push(ApplyOutcome {
                    kind: item.kind,
                    source: item.path.clone(),
                    target: (*target).to_string(),
                    status: ApplyStatus::SourceUnreadable,
                    detail: "source vanished between plan and apply".to_string(),
                    skipped: Vec::new(),
                });
                continue;
            }

            let existed = path_exists(&live);
            // Unchanged fast path: a byte-identical file needs no staging
            // and no commit.
            if existed && src.is_file() && live.is_file() && files_equal(&src, &live) {
                staged.terminal.push(ApplyOutcome {
                    kind: item.kind,
                    source: item.path.clone(),
                    target: (*target).to_string(),
                    status: ApplyStatus::Unchanged,
                    detail: "already byte-identical to the source; left alone".to_string(),
                    skipped: Vec::new(),
                });
                continue;
            }

            let staged_path = stage_root.join(&rel);
            if let Some(parent) = staged_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| io("MIGRATE_STAGE_MKDIR", e, parent.display().to_string()))?;
            }
            // A copy failure here is a hard error, not a per-item one: the
            // item cannot be staged, so the whole apply aborts and the
            // staging tree is discarded. (A vanished source, above, is the
            // per-item case: it is recorded and the run is reported
            // incomplete.)
            let mut skipped = Vec::new();
            if src.is_dir() {
                copy_tree(&src, &staged_path, &mut skipped, budgets, &mut usage)?;
            } else {
                let size = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
                budgets.charge(&mut usage, size, &src.display().to_string())?;
                std::fs::copy(&src, &staged_path).map_err(|e| {
                    io(
                        "MIGRATE_STAGE_COPY",
                        e,
                        format!("{} -> {}", src.display(), staged_path.display()),
                    )
                })?;
            }
            let status = if existed {
                ApplyStatus::Replaced
            } else {
                ApplyStatus::Created
            };
            staged.items.push(StagedItem {
                kind: item.kind,
                source: item.path.clone(),
                staged: staged_path,
                live,
                existed,
                skipped: skipped.clone(),
                outcome: Some(ApplyOutcome {
                    kind: item.kind,
                    source: item.path.clone(),
                    target: (*target).to_string(),
                    status,
                    detail: if existed {
                        format!("replaced (pre-image in backup {})", manifest.id)
                    } else {
                        "created".to_string()
                    },
                    skipped,
                }),
            });
        }

        // Memory-plane items are reported so the count is honest, but not
        // written here: the memory write path is capability-gated and
        // belongs to the CLI.
        for item in &plan.items {
            if let Action::Import { target } = &item.action {
                if target.starts_with("memory://") {
                    staged.terminal.push(ApplyOutcome {
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

        stage_bridges(plan, targets, &stage_id, &mut roots, &mut staged, budgets, &mut usage, merge_providers)?;

        // Staged validation: prove the staged tree has the shape `validate`
        // will demand of the live tree, before anything is committed.
        for item in &staged.items {
            let (status, detail) = check_shape(item.kind, &item.staged);
            if !matches!(status, ValidateStatus::Ok) {
                staged.complete = false;
                staged.terminal.push(ApplyOutcome {
                    kind: item.kind,
                    source: item.source.clone(),
                    target: item.live.to_string_lossy().to_string(),
                    status: ApplyStatus::Failed,
                    detail: format!("staged validation: {detail}"),
                    skipped: Vec::new(),
                });
            }
        }

        Ok(())
    })();
    staged.stage_roots = roots.into_iter().collect();
    if let Err(e) = result {
        staged.discard();
        return Err(e);
    }
    Ok(staged)
}

/// Run the bridges (MCP, sessions, credentials, providers) into the staging
/// tree instead of the live data dir.
///
/// Bridges derive every path from the data dir they are given, so passing
/// the staging root redirects them wholesale. Two bridges merge into live
/// files rather than writing fresh ones (`.env`, `config.toml`): their live
/// pre-images are copied into staging first, so the merge sees the real
/// current content and the commit replaces the live file atomically.
#[allow(clippy::too_many_arguments)]
fn stage_bridges(
    plan: &MigrationPlan,
    targets: &Targets,
    stage_id: &str,
    roots: &mut HashSet<PathBuf>,
    staged: &mut Staged,
    budgets: &StageBudgets,
    usage: &mut BudgetUsage,
    merge_providers: bool,
) -> Result<(), PantheonError> {
    if !plan.items.iter().any(|i| {
        is_bridged(i.kind) && matches!(i.action, Action::Import { .. })
    }) {
        return Ok(());
    }
    let stage_data = targets.data_dir.join(".migrate-stage").join(stage_id);
    roots.insert(stage_data.clone());
    std::fs::create_dir_all(&stage_data)
        .map_err(|e| io("MIGRATE_STAGE_MKDIR", e, stage_data.display().to_string()))?;

    // Pre-seed the merge bases: the bridges read them, and an unchanged base
    // is dropped before commit so it never touches the live file.
    struct Preseed {
        kind: ItemKind,
        rel: PathBuf,
        live: PathBuf,
    }
    let mut preseeds: Vec<Preseed> = Vec::new();
    let mut seed = |kind: ItemKind, live: PathBuf, preseeds: &mut Vec<Preseed>| -> Result<(), PantheonError> {
        if !live.is_file() {
            return Ok(());
        }
        let rel = live
            .strip_prefix(&targets.data_dir)
            .map_err(|_| {
                merr(
                    "MIGRATE_TARGET_OUTSIDE",
                    format!("{} is not under the data dir", live.display()),
                    "re-plan with consistent targets",
                )
            })?
            .to_path_buf();
        let dest = stage_data.join(&rel);
        let size = std::fs::metadata(&live).map(|m| m.len()).unwrap_or(0);
        budgets.charge(usage, size, &live.display().to_string())?;
        std::fs::copy(&live, &dest)
            .map_err(|e| io("MIGRATE_STAGE_SEED", e, live.display().to_string()))?;
        preseeds.push(Preseed { kind, rel, live });
        Ok(())
    };
    if plan.items.iter().any(|i| {
        i.kind == ItemKind::Credentials && matches!(i.action, Action::Import { .. })
    }) {
        seed(
            ItemKind::Credentials,
            crate::carry::pantheon_env_path(&targets.data_dir),
            &mut preseeds,
        )?;
    }
    if merge_providers
        && plan.items.iter().any(|i| {
            i.kind == ItemKind::Provider && matches!(i.action, Action::Import { .. })
        })
    {
        seed(
            ItemKind::Provider,
            targets.data_dir.join("config.toml"),
            &mut preseeds,
        )?;
    }

    let before: HashSet<PathBuf> = snapshot_files(&stage_data).into_iter().collect();
    let staged_targets = Targets::new(stage_data.clone(), stage_data.join("extensions"));
    let outcomes = apply_bridges(plan, &staged_targets, merge_providers, budgets, usage)?;

    // Whatever the bridges wrote (minus pre-seeds) is discovered by diffing
    // the staging tree: bridges decide their own artefact names, and the
    // diff sees all of them without this function having to know.
    //
    // One exception: the session bridge writes `imported-sessions/<source>/`
    // file by file, but the backup snapshots the whole `<source>` dir. The
    // staged unit must be the whole dir, or a commit rollback could not find
    // per-file pre-images. So per-file session entries collapse to their
    // quarantine root.
    let after = snapshot_files(&stage_data);
    let mut rels: Vec<PathBuf> = Vec::new();
    let mut session_dirs: HashSet<PathBuf> = HashSet::new();
    for rel in &after {
        if before.contains(rel) {
            continue;
        }
        if let Some(dir) = quarantine_collapse(rel) {
            session_dirs.insert(dir);
        } else {
            rels.push(rel.clone());
        }
    }
    rels.extend(session_dirs);
    rels.sort();
    for rel in &rels {
        let staged_path = stage_data.join(rel);
        if staged_path
            .extension()
            .map(|e| e == "pre-migrate")
            .unwrap_or(false)
        {
            // `merge_into_live_config` leaves a pre-image beside the staged
            // config; the backup manifest already covers the live file.
            let _ = std::fs::remove_file(&staged_path);
            continue;
        }
        let live = targets.data_dir.join(rel);
        staged.items.push(StagedItem {
            kind: ItemKind::Opaque, // refined below from the outcome
            source: plan.source.clone(),
            staged: staged_path,
            live,
            existed: false, // refined below
            skipped: Vec::new(),
            outcome: None,
        });
    }
    // Pre-seeds: commit only when the bridge actually changed them.
    for p in preseeds {
        let staged_path = stage_data.join(&p.rel);
        if !staged_path.exists() {
            continue;
        }
        if files_equal(&staged_path, &p.live) {
            let _ = std::fs::remove_file(&staged_path);
            continue;
        }
        staged.items.push(StagedItem {
            kind: p.kind,
            source: plan.source.clone(),
            staged: staged_path,
            live: p.live,
            existed: true,
            skipped: Vec::new(),
            outcome: None,
        });
    }
    // Fill in kind/existed now that the live tree is still pristine.
    for item in staged.items.iter_mut().filter(|i| i.outcome.is_none()) {
        item.existed = path_exists(&item.live);
    }

    // Rewrite bridge outcome targets from staged to live paths. A Failed
    // bridge outcome aborts the commit: nothing staged so far lands.
    for mut o in outcomes {
        let target = PathBuf::from(&o.target);
        if let Ok(rel) = target.strip_prefix(&stage_data) {
            o.target = targets.data_dir.join(rel).to_string_lossy().to_string();
        }
        if o.status == ApplyStatus::Failed {
            staged.complete = false;
            staged.terminal.push(o);
        } else {
            // Attribute the staged artefacts to the bridge kind that wrote
            // them, so staged validation checks the right shape.
            staged.bridge_outcomes.push(o);
        }
    }
    // Attribute kinds to diff-discovered items from the bridge outcomes that
    // produced them: match by live target prefix is unnecessary — instead,
    // walk the outcomes and tag items whose live path equals the outcome
    // target, sits under it, or contains it (the session quarantine commits
    // as one dir while its outcome names the manifest inside it).
    for o in &staged.bridge_outcomes {
        let ot = PathBuf::from(&o.target);
        for item in staged.items.iter_mut().filter(|i| i.outcome.is_none()) {
            if item.live == ot || item.live.starts_with(&ot) || ot.starts_with(&item.live) {
                item.kind = o.kind;
            }
        }
    }
    Ok(())
}

/// `imported-sessions/<source>/…` collapses to `imported-sessions/<source>`,
/// so the staged (and backup, and rollback) unit is the whole quarantine
/// dir rather than individual transcript files.
fn quarantine_collapse(rel: &Path) -> Option<PathBuf> {
    let mut parts = rel.components();
    if parts.next()?.as_os_str() != "imported-sessions" {
        return None;
    }
    let source = parts.next()?;
    if parts.next().is_none() {
        return None;
    }
    Some(PathBuf::from("imported-sessions").join(source))
}

/// Relative file paths under `root`, sorted. Symlinks are never followed.
fn snapshot_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(p);
            } else if ft.is_file() {
                if let Ok(rel) = p.strip_prefix(root) {
                    out.push(rel.to_path_buf());
                }
            }
        }
    }
    out.sort();
    out
}

/// Commit every staged item into place: remove-then-rename, so a replace
/// never merges into the old tree and leaves stale files behind.
///
/// Any commit error triggers an automatic rollback — committed replaces are
/// restored from their backup pre-images, committed creates are removed —
/// and the error is returned. There are no manual-restore instructions
/// because there is nothing manual to do.
pub(crate) fn commit_staged(staged: &Staged) -> Result<ApplyReport, PantheonError> {
    let mut committed: Vec<&StagedItem> = Vec::new();
    let mut failed: Option<(&StagedItem, PantheonError)> = None;

    for item in &staged.items {
        if path_exists(&item.live) {
            if let Err(e) = remove_path(&item.live) {
                failed = Some((
                    item,
                    io("MIGRATE_COMMIT_REMOVE", e, item.live.display().to_string()),
                ));
                break;
            }
        }
        if let Some(parent) = item.live.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                failed = Some((
                    item,
                    io("MIGRATE_COMMIT_MKDIR", e, parent.display().to_string()),
                ));
                break;
            }
        }
        match std::fs::rename(&item.staged, &item.live) {
            Ok(()) => committed.push(item),
            Err(e) => {
                failed = Some((
                    item,
                    io(
                        "MIGRATE_COMMIT_RENAME",
                        e,
                        format!("{} -> {}", item.staged.display(), item.live.display()),
                    ),
                ));
                break;
            }
        }
    }

    if let Some((item, e)) = failed {
        // Auto-rollback, best effort, in reverse commit order. The failed
        // item's live target was already removed above when it existed, so
        // it rolls back like a committed replace.
        let mut to_undo: Vec<&StagedItem> = committed;
        to_undo.push(item);
        let mut rb_err: Option<PantheonError> = None;
        for it in to_undo.iter().rev() {
            if let Err(re) = rollback_staged_item(it, &staged.manifest) {
                if rb_err.is_none() {
                    rb_err = Some(re);
                }
            }
        }
        staged.discard();
        let mut detail = format!("commit failed, rolled back automatically: {e}");
        if let Some(re) = rb_err {
            detail.push_str(&format!(" (rollback also hit an error: {re})"));
        }
        return Err(merr(
            "MIGRATE_COMMIT",
            detail,
            "no changes were committed; pre-images were restored from the backup",
        ));
    }

    staged.discard();
    let mut outcomes: Vec<ApplyOutcome> = staged
        .items
        .iter()
        .filter_map(|i| i.outcome.clone())
        .collect();
    outcomes.extend(staged.bridge_outcomes.clone());
    outcomes.extend(staged.terminal.clone());
    Ok(ApplyReport {
        source: staged.source.clone(),
        backup_id: staged.manifest.id.clone(),
        outcomes,
        complete: true,
    })
}

/// Undo one staged item after a failed commit: remove what the commit
/// placed, then restore the pre-image when the target previously existed.
fn rollback_staged_item(
    item: &StagedItem,
    manifest: &BackupManifest,
) -> Result<(), PantheonError> {
    if path_exists(&item.live) {
        remove_path(&item.live)
            .map_err(|e| io("MIGRATE_ROLLBACK_REMOVE", e, item.live.display().to_string()))?;
    }
    if item.existed {
        let want = item.live.to_string_lossy().to_string();
        let entry = manifest.entries.iter().find(|e| e.target == want).ok_or_else(|| {
            merr(
                "MIGRATE_ROLLBACK_NO_PREIMAGE",
                format!("no backup pre-image for {}", item.live.display()),
                "the backup manifest is incomplete; restore by hand from migrate-backups/",
            )
        })?;
        restore_backup_entry(entry)?;
    }
    Ok(())
}

/// Write every import in the plan, transactionally: stage everything into
/// `<dest>/.migrate-stage/<id>/`, validate the staged tree, then commit it
/// with renames. The live tree is untouched until the commit, and any
/// failure — staging, validation, or commit — leaves it untouched: staging
/// failures discard the stage, commit failures roll back automatically.
///
/// Three item kinds do not go through the file copier, because their source
/// is a file we refuse to copy: `Mcp` and `Session` are bridged by parsing
/// the source into a Pantheon-shaped artefact, and a credential manifest is
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
    apply_with_budgets(
        plan,
        targets,
        manifest,
        merge_providers,
        &StageBudgets::default(),
    )
}

/// As [`apply_with`], with explicit [`StageBudgets`]. A budget breach aborts
/// cleanly: the staging tree is discarded and the live tree is untouched.
pub fn apply_with_budgets(
    plan: &MigrationPlan,
    targets: &Targets,
    manifest: &BackupManifest,
    merge_providers: bool,
    budgets: &StageBudgets,
) -> Result<ApplyReport, PantheonError> {
    let staged = stage(plan, targets, manifest, budgets, merge_providers)?;
    if !staged.complete {
        // Per-item failures: nothing was committed, so there is nothing to
        // roll back. Discard the stage and report honestly.
        let report = staged.incomplete_report();
        staged.discard();
        return Ok(report);
    }
    commit_staged(&staged)
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
/// skills that already landed. (The staged apply treats a Failed outcome as
/// a reason to skip the commit, so nothing partial lands.)
fn apply_bridges(
    plan: &MigrationPlan,
    targets: &Targets,
    merge_providers: bool,
    budgets: &StageBudgets,
    usage: &mut BudgetUsage,
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
            ItemKind::Session => bridge_sessions(plan.source.as_str(), src, dd, budgets, usage),
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

fn bridge_sessions(
    source: &str,
    src: &Path,
    dd: &Path,
    budgets: &StageBudgets,
    usage: &mut BudgetUsage,
) -> Result<ApplyOutcome, PantheonError> {
    let imp = crate::carry::write_session_import_with_budget(dd, source, src, budgets, usage)?;
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

/// The shape one import target must have. Shared by [`validate`] (live
/// tree) and staged validation (staging tree): the same bar, before and
/// after the commit.
fn check_shape(kind: ItemKind, path: &Path) -> (ValidateStatus, String) {
    if !path.exists() {
        return (ValidateStatus::Missing, "not on disk".to_string());
    }
    if kind == ItemKind::Skill {
        if path.join("SKILL.md").is_file() {
            (ValidateStatus::Ok, "SKILL.md present".to_string())
        } else {
            (
                ValidateStatus::Incomplete,
                "skill dir has no SKILL.md".to_string(),
            )
        }
    } else if kind == ItemKind::Extension {
        if path.is_dir() {
            (ValidateStatus::Ok, "extension dir present".to_string())
        } else {
            (
                ValidateStatus::Incomplete,
                "extension is not a directory".to_string(),
            )
        }
    } else if path.is_file() {
        (ValidateStatus::Ok, "file present".to_string())
    } else {
        (ValidateStatus::Ok, "present".to_string())
    }
}

/// Re-read every import target and prove it landed with the right shape.
pub fn validate(plan: &MigrationPlan) -> ValidateReport {
    let mut outcomes = Vec::new();
    for (item, target) in import_targets(plan) {
        let p = Path::new(target);
        let (status, detail) = check_shape(item.kind, p);
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

fn copy_any(
    src: &Path,
    dst: &Path,
    budgets: &StageBudgets,
    usage: &mut BudgetUsage,
) -> Result<(), PantheonError> {
    if src.is_dir() {
        let mut ignored = Vec::new();
        copy_tree(src, dst, &mut ignored, budgets, usage)
    } else {
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)
                .map_err(|e| io("MIGRATE_COPY_MKDIR", e, p.display().to_string()))?;
        }
        let size = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        budgets.charge(usage, size, &src.display().to_string())?;
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
    budgets: &StageBudgets,
    usage: &mut BudgetUsage,
) -> Result<(), PantheonError> {
    copy_tree_at(src, dst, 0, skipped, budgets, usage)
}

fn copy_tree_at(
    src: &Path,
    dst: &Path,
    depth: usize,
    skipped: &mut Vec<String>,
    budgets: &StageBudgets,
    usage: &mut BudgetUsage,
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
            copy_tree_at(&from, &to, depth + 1, skipped, budgets, usage)?;
        } else if ft.is_file() {
            if let Some(p) = to.parent() {
                std::fs::create_dir_all(p)
                    .map_err(|e| io("MIGRATE_COPY_MKDIR", e, p.display().to_string()))?;
            }
            let size = std::fs::metadata(&from).map(|m| m.len()).unwrap_or(0);
            budgets.charge(usage, size, &from.display().to_string())?;
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
