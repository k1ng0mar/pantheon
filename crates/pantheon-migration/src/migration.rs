//! Migration merged into one file. External paths keep working through the
//! crate-root shims in lib.rs (same pattern as the providers merge).

pub mod apply {
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
    //!   pre-images. A failed apply leaves the target untouched - there are no
    //!   manual-restore instructions because there is nothing manual to do.
    //! - **Budgeted.** Every byte staged or backed up is charged against
    //!   [`StageBudgets`] (default 1 GiB total, 100k files, 100 MiB per file).
    //!   Exceeding a budget aborts cleanly: the staging tree is discarded and
    //!   the live tree is untouched.
    //! - **No silent partial copy.** `copy_tree` records every skipped symlink
    //!   and unreadable entry in the report rather than copying a partial tree.
    //!
    //! Budgets use the [`StageBudgets`] defaults (1 GiB total, 100k files,
    //! 100 MiB per file); callers that need different limits construct one
    //! explicitly.

    use crate::{merr, Action, ItemKind, MigrationPlan, PlanItem, Targets};
    use pantheon_api::error::PantheonError;
    use pantheon_api::logging::now_ms;
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
                    format!("{what}: file count budget of {} exceeded", self.max_files),
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

    fn io(code: &str, e: std::io::Error, what: String) -> PantheonError {
        merr(
            code,
            format!("{what}: {e}"),
            "check permissions and free space under the data dir, then re-run",
        )
    }

    /// Snapshot every existing import target so an apply can be undone.
    ///
    /// Targets that do not exist yet are not backed up - there is nothing to
    /// restore, and an empty placeholder would be noise in the manifest.
    ///
    /// The copy is charged against the default [`StageBudgets`]; see
    /// [`backup_with_budgets`].
    pub fn backup(
        plan: &MigrationPlan,
        targets: &Targets,
    ) -> Result<BackupManifest, PantheonError> {
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
            let mpath = targets.backup_root().join(format!("{}.json", manifest.id));
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
    /// copy the source `.env` straight over Pantheon's key store - destroying the
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
                format!(
                    "{} is under neither the data dir nor the extensions dir",
                    live.display()
                ),
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
                            detail:
                                "routed to the memory plane; write it with `pantheon memory put`"
                                    .to_string(),
                            skipped: Vec::new(),
                        });
                    }
                }
            }

            stage_bridges(
                plan,
                targets,
                &stage_id,
                &mut roots,
                &mut staged,
                budgets,
                &mut usage,
                merge_providers,
            )?;

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
        if !plan
            .items
            .iter()
            .any(|i| is_bridged(i.kind) && matches!(i.action, Action::Import { .. }))
        {
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
        let mut seed = |kind: ItemKind,
                        live: PathBuf,
                        preseeds: &mut Vec<Preseed>|
         -> Result<(), PantheonError> {
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
        if plan
            .items
            .iter()
            .any(|i| i.kind == ItemKind::Credentials && matches!(i.action, Action::Import { .. }))
        {
            seed(
                ItemKind::Credentials,
                crate::carry::pantheon_env_path(&targets.data_dir),
                &mut preseeds,
            )?;
        }
        if merge_providers
            && plan
                .items
                .iter()
                .any(|i| i.kind == ItemKind::Provider && matches!(i.action, Action::Import { .. }))
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
        // produced them: match by live target prefix is unnecessary - instead,
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

    /// `imported-sessions/<source>/...` collapses to `imported-sessions/<source>`,
    /// so the staged (and backup, and rollback) unit is the whole quarantine
    /// dir rather than individual transcript files.
    fn quarantine_collapse(rel: &Path) -> Option<PathBuf> {
        let mut parts = rel.components();
        if parts.next()?.as_os_str() != "imported-sessions" {
            return None;
        }
        let source = parts.next()?;
        parts.next()?;
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
    /// Any commit error triggers an automatic rollback - committed replaces are
    /// restored from their backup pre-images, committed creates are removed
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
            remove_path(&item.live).map_err(|e| {
                io(
                    "MIGRATE_ROLLBACK_REMOVE",
                    e,
                    item.live.display().to_string(),
                )
            })?;
        }
        if item.existed {
            let want = item.live.to_string_lossy().to_string();
            let entry = manifest
                .entries
                .iter()
                .find(|e| e.target == want)
                .ok_or_else(|| {
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
    /// failure - staging, validation, or commit - leaves it untouched: staging
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
                ItemKind::Provider => {
                    bridge_providers(plan.source.as_str(), src, dd, merge_providers)
                }
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

    fn bridge_credentials(
        source: &str,
        src: &Path,
        dd: &Path,
    ) -> Result<ApplyOutcome, PantheonError> {
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
    /// bound is recorded in `skipped` like any other non-copied entry - the run
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
        let rd = std::fs::read_dir(src)
            .map_err(|e| io("MIGRATE_READ_DIR", e, src.display().to_string()))?;
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
}

pub mod carry {
    //! Carrying the three surfaces a "just copy the files" migration misses:
    //! MCP servers, credentials, and session transcripts.
    //!
    //! Each one has the same shape of problem - the source representation cannot
    //! be dropped into the data dir, but *abandoning* it loses real state. So
    //! each gets a bridge: a declaration Pantheon can act on, written in
    //! Pantheon's own terms.
    //!
    //! - **MCP** (`mcp_servers` in a Hermes `config.yaml`, an `mcp.json`, a
    //!   `.mcp.json`): parsed into a Pantheon-shaped server list. Transport,
    //!   command, args, and url carry over. Headers, tokens, and env values do
    //!   not - they become a declared requirement instead.
    //! - **Credentials** (`.env`, `auth.json`, provider blocks): read as *names*.
    //!   The bridge emits a manifest saying which env var feeds which Pantheon
    //!   provider, so the operator can fill them through `pantheon model` rather
    //!   than inheriting a foreign vault. With `carry_credentials: true` the
    //!   values are written into Pantheon's own encrypted vault, which is the
    //!   only way a value moves - never a raw copy of the source `.env`.
    //! - **Sessions** (transcript dirs): copied into a quarantine path with a
    //!   manifest, so they can be indexed into `session_search` deliberately
    //!   rather than silently shadowing ledger-owned transcripts.

    use pantheon_api::error::{Layer, PantheonError};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn cerr(code: &str, cause: String, remediation: &str) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Runtime,
            false,
            cause,
            remediation,
            "see `pantheon migrate <source> --json` for the full plan",
        )
    }

    // ===========================================================================
    // MCP
    // ===========================================================================

    /// One MCP server, in Pantheon's terms.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct McpServer {
        /// The name Pantheon will register it under.
        pub name: String,
        /// `stdio` or `http`. Anything else is refused at the bridge.
        pub transport: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub command: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub args: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub url: Option<String>,
        /// Env var *names* the server needs. Values are never carried.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub requires_env: Vec<String>,
        /// True when the source declared a credential we refused to copy.
        #[serde(default)]
        pub needs_credentials: bool,
        /// Operator toggle. Defaults to true so every declaration written
        /// before this field existed reads as enabled. The launcher honors
        /// it: a disabled declaration is never configured.
        ///
        /// Deliberate asymmetry, documented not changed: config entries
        /// default to disabled ("fresh installs enable zero"), while
        /// migrated declarations default to enabled because a migration
        /// preserves the source's intent. Enabling is still not execution:
        /// an enabled declaration still needs an explicit
        /// `pantheon mcp approve <name>` before it ever launches.
        #[serde(default = "mcp_enabled_default")]
        pub enabled: bool,
    }

    /// `serde` default for [`McpServer::enabled`]: old declaration files have
    /// no such key and must read as enabled.
    fn mcp_enabled_default() -> bool {
        true
    }

    /// Readiness of one declared server: `None` means ready to register,
    /// `Some(blocker)` says why not. Single source of truth for the `/mcp`
    /// listing, `/mcp reload`, and the dashboard's MCP view.
    ///
    /// "Ready" means the launcher knows how to attach it (stdio with a
    /// command, http/sse with a url) and no credential is missing that only
    /// the operator can supply. Registration additionally needs an explicit
    /// `pantheon mcp approve <name>` - readiness is not approval.
    pub fn server_readiness(s: &McpServer) -> Option<String> {
        let blocker = match s.transport.as_str() {
            "stdio" => match s.command.as_deref() {
                Some(c) if !c.trim().is_empty() => String::new(),
                _ => "no command declared".to_string(),
            },
            "http" | "sse" => match s.url.as_deref() {
                Some(u) if !u.trim().is_empty() => String::new(),
                _ => "no url declared".to_string(),
            },
            other => format!("unsupported transport {other:?}"),
        };
        let blocker = if s.needs_credentials && blocker.is_empty() {
            if s.requires_env.is_empty() {
                "needs a credential (value not in the source)".to_string()
            } else {
                format!("needs a credential ({})", s.requires_env.join(", "))
            }
        } else {
            blocker
        };
        if blocker.is_empty() {
            None
        } else {
            Some(blocker)
        }
    }

    /// A `mcp.json`-shaped document, as Claude Code / omp / others write it.
    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct McpJson {
        #[serde(default, rename = "mcpServers", alias = "mcp_servers")]
        pub mcp_servers: BTreeMap<String, McpJsonEntry>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct McpJsonEntry {
        #[serde(default, rename = "type")]
        pub kind: Option<String>,
        #[serde(default)]
        pub command: Option<String>,
        #[serde(default)]
        pub args: Vec<String>,
        #[serde(default)]
        pub url: Option<String>,
        #[serde(default)]
        pub headers: BTreeMap<String, String>,
        #[serde(default)]
        pub env: BTreeMap<String, String>,
    }

    impl McpJsonEntry {
        /// Does this entry embed a credential inline?
        pub fn has_credential(&self) -> bool {
            !self.headers.is_empty() || self.env.values().any(|v| looks_secret(v))
        }

        /// Env var names the entry declares, minus the ones that are plainly not
        /// credentials (PATH, HOME, a base URL).
        pub fn credential_env_names(&self) -> Vec<String> {
            self.env
                .iter()
                .filter(|(_, v)| looks_secret(v))
                .map(|(k, _)| k.clone())
                .collect()
        }
    }

    /// Cheap "this looks like a credential" test, used so a manifest is not read
    /// deeply enough to have to trust its contents.
    pub fn looks_secret(v: &str) -> bool {
        let v = v.trim();
        if v.len() < 8 {
            return false;
        }
        if v.starts_with("${") || v.starts_with('$') {
            return false; // already an indirection
        }
        if v.starts_with("http://") || v.starts_with("https://") {
            return false;
        }
        let lower = v.to_ascii_lowercase();
        lower.contains("bearer")
            || lower.contains("token")
            || lower.contains("secret")
            || lower.contains("apikey")
            || (v.chars().filter(|c| c.is_ascii_digit()).count() >= 4
                && v.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)))
    }

    /// Parse an `mcp.json` document into Pantheon servers.
    pub fn parse_mcp_json(body: &str) -> Result<Vec<McpServer>, PantheonError> {
        let doc: McpJson = serde_json::from_str(body).map_err(|e| {
            cerr(
                "MIGRATE_MCP_PARSE",
                e.to_string(),
                "check the file is valid JSON",
            )
        })?;
        let mut out = Vec::new();
        for (name, e) in doc.mcp_servers {
            // Infer from the shape when `type` is absent: a url means http.
            let transport = match e.kind.as_deref() {
                None => if e.url.is_some() { "http" } else { "stdio" }.to_string(),
                Some("stdio") | Some("http") | Some("sse") => e.kind.clone().unwrap(),
                Some(other) => {
                    return Err(cerr(
                        "MIGRATE_MCP_TRANSPORT",
                        format!("{name}: unsupported transport {other:?}"),
                        "only stdio, http, and sse bridge to Pantheon",
                    ))
                }
            };
            let requires_env = e.credential_env_names();
            let needs_credentials = e.has_credential();
            out.push(McpServer {
                name,
                transport,
                command: e.command,
                args: e.args,
                url: e.url,
                requires_env,
                needs_credentials,
                enabled: true,
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Which key a bare `- ` list item currently belongs to, and which block
    /// scalar entries are nested in.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Block {
        /// Top level of a server: `command:`, `url:`, ...
        Server,
        /// Inside `args:`.
        Args,
        /// Inside `headers:`. Every entry is a credential.
        Headers,
        /// Inside `env:`. Only a secret-looking value counts.
        Env,
        /// Any other nested block.
        Other,
    }

    /// Parse the `mcp_servers:` block out of a Hermes `config.yaml` without a
    /// full YAML parse. Hermes writes a flat `name: {command, args, env, headers}`
    /// shape, so a line scan is enough and avoids deserialising a struct where a
    /// stray field could capture a value.
    ///
    /// A small state machine rather than a line matcher, because a bare `- item`
    /// only means an argument when it directly follows `args:`. Under `env:` or
    /// `headers:` it is a different list entirely, and folding those into `args`
    /// would hand the runtime a command line that was never in the source.
    pub fn parse_hermes_mcp(config_yaml: &str) -> Vec<McpServer> {
        let lines: Vec<&str> = config_yaml.lines().collect();
        let Some(start) = lines
            .iter()
            .position(|l| l.trim_start().starts_with("mcp_servers:"))
        else {
            return Vec::new();
        };
        let base = lines[start].len() - lines[start].trim_start().len();

        let mut servers: Vec<McpServer> = Vec::new();
        let mut cur: Option<McpServer> = None;
        let mut command: Option<String> = None;
        let mut url: Option<String> = None;
        let mut args: Vec<String> = Vec::new();
        // Which block the parser is currently inside.
        let mut block = Block::Server;

        macro_rules! flush {
            () => {
                if let Some(mut s) = cur.take() {
                    s.command = command.take();
                    s.url = url.take();
                    s.args = std::mem::take(&mut args);
                    if s.url.is_some() && s.transport == "stdio" {
                        s.transport = "http".into();
                    }
                    servers.push(s);
                }
            };
        }

        for line in &lines[start + 1..] {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let indent = line.len() - line.trim_start().len();
            if indent <= base {
                break; // block ended
            }

            // A new server at the first level under `mcp_servers:`.
            if indent == base + 2 && t.ends_with(':') && !t.starts_with('-') {
                flush!();
                let name = t.trim_end_matches(':').trim().trim_matches('"').to_string();
                cur = Some(McpServer {
                    name,
                    transport: "stdio".into(),
                    command: None,
                    args: Vec::new(),
                    url: None,
                    requires_env: Vec::new(),
                    needs_credentials: false,
                    enabled: true,
                });
                continue;
            }

            let Some(s) = cur.as_mut() else { continue };

            // A bare list item belongs to whatever block opened it. Only `args`
            // contributes to the command line; an `env:` or `headers:` list is not
            // the command line, and folding it in there would hand the runtime an
            // argv that was never in the source.
            if let Some(item) = t.strip_prefix("- ") {
                if block == Block::Args {
                    args.push(unquote(item));
                }
                continue;
            }

            // `key:` with no value opens a block; `key: value` sets a scalar.
            if let Some(key) = t.strip_suffix(':') {
                block = match key.trim() {
                    "args" => Block::Args,
                    "headers" => Block::Headers,
                    "env" => Block::Env,
                    _ => Block::Other,
                };
                continue;
            }

            let Some((key, value)) = t.split_once(':') else {
                block = Block::Server;
                continue;
            };
            let key = key.trim();
            let value = value.trim();
            let block = std::mem::replace(&mut block, Block::Server);

            match key {
                "command" => command = Some(unquote(value)),
                "url" => url = Some(unquote(value)),
                "args" if value.starts_with('[') => args = parse_flow_seq(value),
                _ => {
                    // Inside a headers/env block, or any scalar that looks like a
                    // bearer. Record the env var *name* when the source used an
                    // indirection; a literal token is flagged but never carried.
                    let credential = match block {
                        Block::Headers => true,
                        Block::Env => looks_secret(value),
                        _ => value.to_ascii_lowercase().contains("bearer") && looks_secret(value),
                    };
                    if credential {
                        s.needs_credentials = true;
                        if let Some(var) = indirection_name(value) {
                            push_unique(&mut s.requires_env, var);
                        } else if let Some(var) = bearer_env_name(value) {
                            push_unique(&mut s.requires_env, var);
                        }
                    }
                }
            }
        }
        flush!();
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        servers
    }

    /// The variable name inside a `${VAR}` or `$VAR` reference, if the value is
    /// purely an indirection. A value with surrounding literal text is not one.
    fn indirection_name(value: &str) -> Option<String> {
        let v = value.trim();
        if let Some(inner) = v.strip_prefix("${").and_then(|r| r.strip_suffix('}')) {
            let n = inner.trim();
            if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Some(n.to_string());
            }
        }
        None
    }

    /// The `${VAR}` name inside a `Bearer ${VAR}` style header value, where the
    /// literal and the reference are mixed.
    fn bearer_env_name(value: &str) -> Option<String> {
        let start = value.find("${")?;
        let rest = &value[start + 2..];
        let end = rest.find('}')?;
        let n = rest[..end].trim();
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            Some(n.to_string())
        } else {
            None
        }
    }

    fn push_unique(v: &mut Vec<String>, s: String) {
        if !v.contains(&s) {
            v.push(s);
        }
    }

    fn unquote(s: &str) -> String {
        s.trim().trim_matches('"').trim_matches('\'').to_string()
    }

    fn parse_flow_seq(s: &str) -> Vec<String> {
        s.trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(unquote)
            .filter(|x| !x.is_empty())
            .collect()
    }

    // ===========================================================================
    // Credentials
    // ===========================================================================

    /// One credential the source declares and Pantheon must be told to supply.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct CredentialMapping {
        /// The env var name in the source.
        pub env_var: String,
        /// Where it should be supplied from, when we can tell.
        pub target: CredentialTarget,
        /// Set when the source had a value for it. The value is never here.
        pub had_value: bool,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CredentialTarget {
        /// A catalogued Pantheon provider; supply via `pantheon model`.
        Provider,
        /// An MCP server header; supply when registering the server.
        McpAuth,
        /// A channel bot token (telegram, discord, ...).
        Channel,
        /// We could not classify it.
        Unclassified,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct CredentialManifest {
        pub source: String,
        /// Names only. Values live in the vault, never in this file.
        pub mappings: Vec<CredentialMapping>,
        /// Written only when `carry_credentials` was set.
        pub vault_written: Vec<String>,
    }

    impl CredentialManifest {
        pub fn unclassified(&self) -> usize {
            self.mappings
                .iter()
                .filter(|m| m.target == CredentialTarget::Unclassified)
                .count()
        }
    }

    /// Classify an env var name into a Pantheon credential target.
    ///
    /// Name-based on purpose: reading values to classify them would defeat the
    /// point of not copying them.
    pub fn classify_credential(name: &str) -> CredentialTarget {
        let n = name.to_ascii_uppercase();
        // MCP first: `MCP_COMPOSIO_API_KEY` ends in KEY and would otherwise be
        // filed as a model provider, which is exactly the wrong destination.
        if n.contains("MCP") {
            return CredentialTarget::McpAuth;
        }
        let last = n.rsplit('_').next().unwrap_or("");
        if matches!(
            last,
            "TOKEN" | "SECRET" | "KEY" | "PASSWORD" | "CREDENTIALS"
        ) {
            // Bot tokens belong to a channel; everything else looks like a
            // provider or service credential.
            if n.contains("TELEGRAM")
                || n.contains("DISCORD")
                || n.contains("SLACK")
                || n.contains("WHATSAPP")
                || n.contains("SIGNAL")
                || n.contains("MATRIX")
                || n.contains("IRC")
                || n.contains("FEISHU")
                || n.contains("WECOM")
                || n.contains("TEAMS")
                || n.contains("LINE_")
                || n.contains("NTFY")
            {
                return CredentialTarget::Channel;
            }
            return CredentialTarget::Provider;
        }
        CredentialTarget::Unclassified
    }

    /// One parsed `.env` entry: the name, and whether it carried a real value.
    /// The value itself is deliberately not returned - reading it is the
    /// caller's business, and it goes straight into a vault if anywhere.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct EnvEntry {
        pub name: String,
        pub had_value: bool,
        /// The raw value, `None` unless the caller explicitly asked for values.
        /// Populated only by [`parse_env_values`].
        pub value: Option<String>,
    }

    /// The value part of a dotenv line, with the rules
    /// `pantheon-tui::dotenv::parse_dotenv` uses: trim, strip an inline ` #`
    /// comment when the value is unquoted, then strip one layer of matching quotes.
    ///
    /// One implementation, used by both parsers in this crate, and pinned against
    /// the CLI's by a cross-crate test. The two drifted once: the CLI stripped the
    /// comment and this did not, so a key carried by `pantheon migrate` could land
    /// in `<data_dir>/.env` with the comment still attached.
    pub(crate) fn dotenv_value(raw: &str) -> String {
        let v = raw.trim();
        let v = if v.starts_with('"') || v.starts_with('\'') {
            v
        } else if let Some(hash) = v.find(" #") {
            v[..hash].trim_end()
        } else {
            v
        };
        if v.len() >= 2
            && ((v.starts_with('"') && v.ends_with('"'))
                || (v.starts_with('\'') && v.ends_with('\'')))
        {
            return v[1..v.len() - 1].to_string();
        }
        v.to_string()
    }

    /// Parse a `.env`-shaped file into names + whether each had a value.
    pub fn parse_env_names(body: &str) -> Vec<EnvEntry> {
        parse_env(body, false)
    }

    /// Parse a `.env`-shaped file **including values**. Only the credential-bridge
    /// path calls this, and it writes the result straight into an encrypted vault
    /// without echoing it. Nothing here prints, logs, or returns a value through
    /// a plan, a report, or an error.
    pub fn parse_env_values(body: &str) -> Vec<EnvEntry> {
        parse_env(body, true)
    }

    fn parse_env(body: &str, with_values: bool) -> Vec<EnvEntry> {
        let mut out = Vec::new();
        for line in body.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let t = t.strip_prefix("export ").unwrap_or(t);
            let Some((k, v)) = t.split_once('=') else {
                continue;
            };
            let k = k.trim();
            // A shell identifier, not just an alphanumeric run: a leading digit
            // means this was not a variable assignment.
            let mut chars = k.chars();
            let valid_start =
                matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_');
            if !valid_start || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let value = dotenv_value(v);
            let v = value.as_str();
            // Placeholders and indirections are "not really set".
            let real = !v.is_empty()
                && !v.starts_with("your_")
                && !v.starts_with("changeme")
                && !v.starts_with('<')
                && v != "null"
                && v != "TODO";
            out.push(EnvEntry {
                name: k.to_string(),
                had_value: real,
                value: if with_values && real {
                    Some(v.to_string())
                } else {
                    None
                },
            });
        }
        out
    }

    /// Build the credential manifest for a source's declared names.
    pub fn credential_manifest(source: &str, names: &[String]) -> CredentialManifest {
        CredentialManifest {
            source: source.to_string(),
            mappings: names
                .iter()
                .map(|n| CredentialMapping {
                    env_var: n.clone(),
                    target: classify_credential(n),
                    had_value: false,
                })
                .collect(),
            vault_written: Vec::new(),
        }
    }

    // ===========================================================================
    // Sessions
    // ===========================================================================

    /// A batch of imported transcript files waiting to be indexed.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SessionImport {
        pub source: String,
        pub files: Vec<PathBuf>,
        /// Written next to the transcripts.
        pub manifest: PathBuf,
        /// Transcript formats we recognised.
        pub formats: Vec<String>,
    }

    /// Recognise a transcript file by shape and count its records, without
    /// parsing the content into anything that could carry a secret onward.
    pub fn transcript_format(path: &Path) -> Option<&'static str> {
        let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
        if name.ends_with(".jsonl") {
            return Some("jsonl");
        }
        if name.ends_with(".json") {
            return Some("json");
        }
        None
    }

    // ===========================================================================
    // Writers - the bridge artefacts
    // ===========================================================================

    fn werr(code: &str, cause: String) -> PantheonError {
        cerr(
            code,
            cause,
            "check the data dir is writable and the source file still exists",
        )
    }

    /// Write a Pantheon-shaped MCP declaration. Transport, command, args, and url
    /// carry over; credentials become declared requirements.
    pub fn write_mcp_declaration(
        targets_data_dir: &Path,
        source: &str,
        servers: &[McpServer],
    ) -> Result<PathBuf, PantheonError> {
        if servers.is_empty() {
            return Ok(PathBuf::new());
        }
        let dir = targets_data_dir.join("mcp");
        std::fs::create_dir_all(&dir).map_err(|e| werr("MIGRATE_MCP_MKDIR", e.to_string()))?;
        let path = dir.join(format!("{}.json", source));
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "source": source,
            "note": "Generated by `pantheon migrate apply`. Transport/command/args carry over; \
                     credentials are declared, not copied. Register with `pantheon mcp`.",
            "servers": servers,
        }))
        .map_err(|e| werr("MIGRATE_MCP_ENCODE", e.to_string()))?;
        std::fs::write(&path, body).map_err(|e| werr("MIGRATE_MCP_WRITE", e.to_string()))?;
        Ok(path)
    }

    /// Write the credential manifest. **Names and targets only** - the file
    /// deliberately has no field a value could occupy.
    pub fn write_credential_manifest(
        targets_data_dir: &Path,
        source: &str,
        manifest: &CredentialManifest,
    ) -> Result<PathBuf, PantheonError> {
        let dir = targets_data_dir.join("credentials");
        std::fs::create_dir_all(&dir).map_err(|e| werr("MIGRATE_CRED_MKDIR", e.to_string()))?;
        let path = dir.join(format!("{}.json", source));
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "source": source,
            "note": "Names only. No value in this file, by construction. Values live in \
                     <data_dir>/.env, which is pantheon's own key store. Manage them \
                     with `pantheon model` or by hand.",
            "mappings": manifest.mappings,
        }))
        .map_err(|e| werr("MIGRATE_CRED_ENCODE", e.to_string()))?;
        std::fs::write(&path, body).map_err(|e| werr("MIGRATE_CRED_WRITE", e.to_string()))?;
        Ok(path)
    }

    // ===========================================================================
    // .env  ->  <data_dir>/.env
    // ===========================================================================

    /// Where a credential value landed, per key. **Names only** - this struct has
    /// no field a value could occupy, deliberately.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct EnvMergeReport {
        /// `<data_dir>/.env`, the file that was written.
        pub path: String,
        /// Written into the store by this migration.
        pub added: Vec<String>,
        /// Already present in `<data_dir>/.env`; left untouched. A migration must
        /// never clobber a key the operator already set in Pantheon.
        pub already_present: Vec<String>,
        /// Declared but empty or a placeholder in the source; nothing to carry.
        pub no_value: Vec<String>,
        /// Not a credential we recognise, so not carried.
        pub unclassified: Vec<String>,
    }

    /// The dotenv path Pantheon already uses, so an imported key is visible to
    /// `load_dotenv` and `pantheon model` with no extra wiring.
    pub fn pantheon_env_path(data_dir: &Path) -> PathBuf {
        data_dir.join(".env")
    }

    /// Read `KEY=value` pairs out of a dotenv file, last-wins per key. Mirrors
    /// `pantheon-tui`'s `parse_dotenv` so a key set twice behaves the same here.
    pub fn read_dotenv(path: &Path) -> Vec<(String, String)> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            let t = t.strip_prefix("export ").unwrap_or(t);
            let Some((k, v)) = t.split_once('=') else {
                continue;
            };
            let k = k.trim();
            if k.is_empty() {
                continue;
            }
            let value = dotenv_value(v);
            let v = value.as_str();
            match out.iter_mut().find(|(ek, _)| ek == k) {
                Some(slot) => slot.1 = v.to_string(),
                None => out.push((k.to_string(), v.to_string())),
            }
        }
        out
    }

    /// Owner-only, matching `restrict_permissions` in `pantheon-tui::dotenv`.
    /// Belt and braces: the dotenv writer below already creates the file with
    /// owner-only permissions; this keeps the historical guarantee even if the
    /// writer ever changes.
    #[cfg(unix)]
    fn restrict_permissions(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    fn restrict_permissions(_path: &Path) {}

    /// Carry a source `.env` into Pantheon's own key store.
    ///
    /// Rules, in order of importance:
    /// - Only keys that classify as credentials are carried. A source `PATH` or
    ///   `HOME` does not belong in a key store.
    /// - **An existing key is never clobbered.** It is reported as
    ///   `already_present` and the operator decides. A migration that overwrites a
    ///   key the operator set in Pantheon is a data-loss bug.
    /// - A value is written to `<data_dir>/.env` and nowhere else. It is never
    ///   returned in this report, never logged, and never printed.
    pub fn merge_env_into(
        data_dir: &Path,
        source: &str,
        source_env: &Path,
    ) -> Result<EnvMergeReport, PantheonError> {
        let body = std::fs::read_to_string(source_env).map_err(|e| {
            cerr(
                "MIGRATE_ENV_READ",
                format!("{}: {e}", source_env.display()),
                "check the source env file is readable",
            )
        })?;
        // Values are needed here and nowhere else; they go straight to the store.
        let entries = parse_env_values(&body);

        std::fs::create_dir_all(data_dir).map_err(|e| werr("MIGRATE_ENV_MKDIR", e.to_string()))?;
        let dest = pantheon_env_path(data_dir);
        let mut report = EnvMergeReport {
            path: dest.to_string_lossy().to_string(),
            ..Default::default()
        };

        let existing_keys: Vec<String> = read_dotenv(&dest).into_iter().map(|(k, _)| k).collect();

        for e in entries {
            if classify_credential(&e.name) == CredentialTarget::Unclassified {
                report.unclassified.push(e.name);
                continue;
            }
            let Some(value) = e.value else {
                report.no_value.push(e.name);
                continue;
            };
            if existing_keys.contains(&e.name) {
                report.already_present.push(e.name);
                continue;
            }
            // The store write goes through the canonical atomic dotenv writer
            // (temp + fsync + rename, owner-only, lossless quoting): the old
            // raw `format!("{key}={value}")` + `fs::write` path silently
            // truncated values like `sk-abc # def` on read-back.
            if value.contains('\n') || value.contains('\r') {
                return Err(werr(
                    "MIGRATE_ENV_WRITE",
                    format!("value for {} must be single-line", e.name),
                ));
            }
            pantheon_api::dotenv::upsert_dotenv(data_dir, &e.name, &value)
                .map_err(|e| werr("MIGRATE_ENV_WRITE", format!("{}: {e}", dest.display())))?;
            report.added.push(e.name);
        }

        if !report.added.is_empty() {
            restrict_permissions(&dest);
        }

        // The names-only manifest rides alongside, so the import is auditable
        // without ever opening the value store.
        let manifest = credential_manifest(source, &report.added);
        write_credential_manifest(data_dir, source, &manifest)?;

        Ok(report)
    }

    /// One read-back of a written MCP declaration.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct McpDeclaration {
        pub source: String,
        pub servers: Vec<McpServer>,
    }

    /// Read every MCP declaration under `<data_dir>/mcp/`, sorted by source.
    pub fn read_mcp_declarations(data_dir: &Path) -> Vec<McpDeclaration> {
        let dir = data_dir.join("mcp");
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
            .collect();
        paths.sort();
        let mut out = Vec::new();
        for p in paths {
            let file_name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let body = match std::fs::read_to_string(&p) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("mcp: declaration '{file_name}' unreadable: {e}");
                    continue;
                }
            };
            let doc: Doc = match serde_json::from_str(&body) {
                Ok(d) => d,
                Err(e) => {
                    // Never silently drop a broken file: warn with the
                    // file name + parse error, and keep a placeholder
                    // row so the file shows in every MCP view instead
                    // of vanishing. Transport "corrupt" is unlaunchable:
                    // the resolver skips unknown transports.
                    eprintln!("mcp: declaration '{file_name}' unparseable: {e}");
                    let stem = p
                        .file_stem()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| file_name.clone());
                    out.push(McpDeclaration {
                        source: stem.clone(),
                        servers: vec![McpServer {
                            name: format!("corrupt:{stem}"),
                            transport: "corrupt".to_string(),
                            command: None,
                            args: Vec::new(),
                            url: None,
                            requires_env: Vec::new(),
                            needs_credentials: false,
                            enabled: true,
                        }],
                    });
                    continue;
                }
            };
            if !doc.servers.is_empty() {
                out.push(McpDeclaration {
                    source: doc.source,
                    servers: doc.servers,
                });
            }
        }
        out
    }

    /// The on-disk shape of a written declaration.
    #[derive(Debug, Clone, Deserialize)]
    struct Doc {
        #[serde(default)]
        source: String,
        #[serde(default)]
        servers: Vec<McpServer>,
    }

    /// Copy transcript files into a quarantine path and write a manifest, so they
    /// can be indexed into `session_search` deliberately, with explicit budgets.
    /// A budget breach aborts the quarantine copy with an error; the caller
    /// discards the partial tree.
    pub fn write_session_import_with_budget(
        targets_data_dir: &Path,
        source: &str,
        from: &Path,
        budgets: &crate::apply::StageBudgets,
        usage: &mut crate::apply::BudgetUsage,
    ) -> Result<SessionImport, PantheonError> {
        let dir = targets_data_dir.join("imported-sessions").join(source);
        std::fs::create_dir_all(&dir).map_err(|e| werr("MIGRATE_SESS_MKDIR", e.to_string()))?;

        let mut files = Vec::new();
        let mut formats: Vec<String> = Vec::new();
        collect_transcripts(from, &dir, "", &mut files, &mut formats, 0, budgets, usage)?;

        let manifest = dir.join("manifest.json");
        let body = serde_json::to_string_pretty(&serde_json::json!({
            "source": source,
            "from": from.to_string_lossy(),
            "note": "Quarantined transcripts. Index them with session_search deliberately; \
                     the ledger owns live transcripts and will not read this path.",
            "files": files.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>(),
            "formats": formats,
        }))
        .map_err(|e| werr("MIGRATE_SESS_ENCODE", e.to_string()))?;
        std::fs::write(&manifest, body).map_err(|e| werr("MIGRATE_SESS_WRITE", e.to_string()))?;

        Ok(SessionImport {
            source: source.to_string(),
            files,
            manifest,
            formats,
        })
    }

    /// Depth-limited recursive transcript collection. Bounded so a runaway or
    /// symlinked source tree cannot turn one import into an unbounded copy.
    ///
    /// Eight parameters is over clippy's default limit, but this is a private
    /// recursive helper where every parameter is recursion state threaded
    /// through each level (`files`/`formats` accumulate, `depth` bounds,
    /// `budgets`/`usage` account). Grouping them into a struct would obscure
    /// the recursion without removing any complexity, so the lint is silenced
    /// here with justification.
    #[allow(clippy::too_many_arguments)]
    fn collect_transcripts(
        from: &Path,
        into: &Path,
        rel_prefix: &str,
        files: &mut Vec<PathBuf>,
        formats: &mut Vec<String>,
        depth: usize,
        budgets: &crate::apply::StageBudgets,
        usage: &mut crate::apply::BudgetUsage,
    ) -> Result<(), PantheonError> {
        if depth > 4 {
            return Ok(());
        }
        let rd = match std::fs::read_dir(from) {
            Ok(r) => r,
            Err(_) => return Ok(()),
        };
        for entry in rd.flatten() {
            let src = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue; // never follow out of the source tree
            }
            if ft.is_dir() {
                let child = entry.file_name().to_string_lossy().to_string();
                let next = if rel_prefix.is_empty() {
                    child
                } else {
                    format!("{rel_prefix}__{child}")
                };
                collect_transcripts(&src, into, &next, files, formats, depth + 1, budgets, usage)?;
                continue;
            }
            if !ft.is_file() {
                continue;
            }
            let Some(fmt) = transcript_format(&src) else {
                continue;
            };
            let name = src
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "record".into());
            // Flatten to a unique name. A bare `d{depth}-{name}` collides when two
            // subdirectories at the same depth hold a transcript with the same
            // file name, and the second copy silently overwrites the first.
            //
            // The prefix is the path *relative to the scan root*, accumulated as
            // the recursion descends. It must never be rebuilt from the absolute
            // source path: `Path::components()` yields `RootDir` for an absolute
            // path, and joining that in produces a leading "/", which makes
            // `into.join(prefix)` discard `into` entirely and resolve back to the
            // source file.
            let rel = if rel_prefix.is_empty() {
                name.clone()
            } else {
                format!("{rel_prefix}__{name}")
            };
            let safe: String = rel
                .chars()
                .map(|c| {
                    if c == '/' || c == '\\' || c == ':' {
                        '_'
                    } else {
                        c
                    }
                })
                .collect();
            let mut dest = into.join(&safe);
            // Belt and braces: never clobber, even if a name somehow repeats.
            let mut n = 2;
            while dest.exists() {
                let ext = std::path::Path::new(&safe)
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                dest = into.join(format!("{safe}-{n}{ext}"));
                n += 1;
            }
            let size = std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0);
            budgets.charge(usage, size, &src.display().to_string())?;
            std::fs::copy(&src, &dest)
                .map_err(|e| werr("MIGRATE_SESS_COPY", format!("{}: {e}", src.display())))?;
            files.push(dest);
            let tag = format!("{fmt}:d{depth}");
            if !formats.contains(&tag) {
                formats.push(tag);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod carry_env_tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        fn scratch(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-carry-env-{}-{}-{tag}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// End-to-end `migrate apply` credential path: `merge_env_into` is the
        /// exact function `apply.rs` calls for `ItemKind::Credentials`.
        /// Adversarial source values must land in `<data_dir>/.env` and read
        /// back byte-identical through the canonical parser. Before the quoting
        /// fix, a source `"sk-abc # def"` was written back raw and read back as
        /// `sk-abc` - silent truncation.
        #[test]
        fn merge_env_into_round_trips_adversarial_values() {
            let dir = scratch("roundtrip");
            let data_dir = dir.join("data");
            let source = dir.join("source.env");
            // Values are double-quoted in the source so the source parser hands
            // the writer the raw special characters; the writer must re-quote
            // them losslessly.
            let cases: Vec<(&str, &str)> = vec![
                ("MIG_A_API_KEY", "sk-abc # def"),
                ("MIG_B_API_KEY", "#leading"),
                ("MIG_C_API_KEY", "a\"b"),
                ("MIG_D_API_KEY", "c:\\path\\to"),
                ("MIG_E_API_KEY", "a=b"),
                ("MIG_F_API_KEY", "unicode-✓-•••-日本語"),
                ("MIG_G_API_KEY", "trailing "),
                ("MIG_H_API_KEY", "  padded  "),
                ("MIG_I_API_KEY", "semi;colon"),
                ("MIG_J_API_KEY", "dollar$back`tick"),
            ];
            let mut body = String::new();
            for (k, v) in &cases {
                body.push_str(&format!("{k}=\"{v}\"\n"));
            }
            // A non-credential must not be carried; a placeholder must not be.
            body.push_str("PATH=/usr/bin:/bin\n");
            body.push_str("MIG_Z_API_KEY=changeme\n");
            std::fs::write(&source, &body).unwrap();

            let report = merge_env_into(&data_dir, "test-source", &source).expect("merge");
            assert_eq!(
                report.added.len(),
                cases.len(),
                "all credentials carried: {report:?}"
            );
            assert!(report.unclassified.iter().any(|n| n == "PATH"));
            assert!(report.no_value.iter().any(|n| n == "MIG_Z_API_KEY"));

            let text = std::fs::read_to_string(data_dir.join(".env")).unwrap();
            let back = pantheon_api::dotenv::parse_dotenv(&text);
            for (k, v) in &cases {
                let got = back.iter().find(|(ek, _)| ek == k).map(|(_, ev)| ev);
                assert_eq!(
                    got,
                    Some(&v.to_string()),
                    "key {k} must round-trip byte-identical"
                );
            }
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// The never-clobber rule: a key already in `<data_dir>/.env` is left
        /// untouched and reported, never overwritten by a migration.
        #[test]
        fn merge_env_into_never_clobbers_existing_keys() {
            let dir = scratch("noclobber");
            let data_dir = dir.join("data");
            std::fs::create_dir_all(&data_dir).unwrap();
            pantheon_api::dotenv::upsert_dotenv(&data_dir, "MIG_A_API_KEY", "operator-set")
                .expect("seed");
            let source = dir.join("source.env");
            std::fs::write(
                &source,
                "MIG_A_API_KEY=\"new-value\"\nMIG_B_API_KEY=\"fresh\"\n",
            )
            .unwrap();

            let report = merge_env_into(&data_dir, "test-source", &source).expect("merge");
            assert_eq!(report.already_present, vec!["MIG_A_API_KEY".to_string()]);
            assert_eq!(report.added, vec!["MIG_B_API_KEY".to_string()]);
            assert_eq!(
                pantheon_api::dotenv::read_dotenv_value(&data_dir, "MIG_A_API_KEY").as_deref(),
                Some("operator-set"),
                "existing key must be untouched"
            );
            assert_eq!(
                pantheon_api::dotenv::read_dotenv_value(&data_dir, "MIG_B_API_KEY").as_deref(),
                Some("fresh")
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn corrupt_declaration_is_kept_as_placeholder_not_dropped() {
            let dir = scratch("corrupt-decl");
            let data_dir = dir.join("data");
            std::fs::create_dir_all(data_dir.join("mcp")).unwrap();
            std::fs::write(data_dir.join("mcp").join("broken.json"), "{not valid json").unwrap();
            let decls = read_mcp_declarations(&data_dir);
            assert_eq!(decls.len(), 1, "corrupt file must leave a placeholder row");
            assert_eq!(decls[0].servers.len(), 1);
            assert_eq!(decls[0].servers[0].transport, "corrupt");
            assert!(
                server_readiness(&decls[0].servers[0]).is_some(),
                "placeholder must read as blocked, never launchable"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod compat {
    //! Compat adapter: OpenClaw / OMP extensions -> Pantheon extensions.
    //!
    //! Both foreign ecosystems ship the same *idea* - a `register(api)` entry
    //! point that attaches handlers to named lifecycle events - under different
    //! manifests, different spellings, and a much larger event vocabulary. This
    //! module is the seam named in `docs/developer/architecture.md` section 8.
    //!
    //! The contract it upholds is deliberately narrow:
    //!
    //! - A foreign hook maps to a Pantheon hook **only** when the semantics match.
    //!   Nothing is approximated.
    //! - An unmappable hook is **reported, never dropped**. A plugin that asks for
    //!   `before_tool_call` and gets silence is worse than one that is refused,
    //!   so `CompatReport::unsupported` always names what was lost.
    //! - Capabilities Pantheon has no equivalent for (model providers, media
    //!   generation, gateway methods, HTTP routes) are recorded as `refused`, so
    //!   the operator can see the plugin loaded a subset of what it declares.
    //! - Credentials are **described, never copied**: the manifest's
    //!   `envVars` become a declaration of what the operator must supply through
    //!   Pantheon's own secrets plane.

    use pantheon_extensions::hooks::Hook;
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};

    // ---------------------------------------------------------------------------
    // Foreign manifests
    // ---------------------------------------------------------------------------

    /// `openclaw.plugin.json`. Only the fields the adapter needs; everything else
    /// is ignored so a manifest gaining keys does not break the import.
    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct OpenClawManifest {
        #[serde(default)]
        pub id: Option<String>,
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub description: Option<String>,
        #[serde(default)]
        pub setup: Option<OpenClawSetup>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct OpenClawSetup {
        #[serde(default)]
        pub providers: Vec<OpenClawProvider>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct OpenClawProvider {
        #[serde(default)]
        pub id: Option<String>,
        /// The env var names this provider needs. A *declaration*, not a value.
        #[serde(default, rename = "envVars")]
        pub env_vars: Vec<String>,
    }

    /// OMP keeps its manifest in `package.json` under an `omp` or `pi` field.
    #[derive(Debug, Clone, Default, Deserialize)]
    pub struct OmpManifest {
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub version: Option<String>,
        #[serde(default)]
        pub description: Option<String>,
        #[serde(default)]
        pub tools: Option<String>,
        #[serde(default)]
        pub hooks: Option<String>,
        #[serde(default)]
        pub extensions: Vec<String>,
        #[serde(default)]
        pub commands: Vec<String>,
    }

    #[derive(Debug, Clone, Default, Deserialize)]
    struct PackageJson {
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub version: Option<String>,
        #[serde(default)]
        pub description: Option<String>,
        #[serde(default)]
        pub omp: Option<OmpManifest>,
        #[serde(default)]
        pub pi: Option<OmpManifest>,
    }

    // ---------------------------------------------------------------------------
    // Hook mapping
    // ---------------------------------------------------------------------------

    /// The outcome of mapping one foreign event onto Pantheon.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum HookMap {
        /// Semantics match; the handler will run at this Pantheon hook.
        Mapped(Hook),
        /// Pantheon has no equivalent. Never silently ignored.
        Unsupported { nearest: Option<Hook> },
    }

    /// Map a foreign lifecycle event onto a Pantheon hook.
    ///
    /// The mapping is intentionally conservative. A hook is only reported as
    /// [`HookMap::Mapped`] when it is both semantically equivalent **and**
    /// actually wired (see [`Hook::is_wired`]) - `Mapped` promises the operator
    /// that the handler will run, so it must never be handed out for a hook with
    /// no fire site. `before_tool_call` maps for real (to the
    /// `pre_tool_call` gate), and `pre_gateway_dispatch` maps for real too:
    /// the TUI gateway's surface sink fires it before dispatch.
    pub fn map_hook(event: &str) -> HookMap {
        let norm = event.trim().to_ascii_lowercase().replace(['-', ':'], "_");
        // `Mapped` is a promise; honor it only for wired hooks.
        let mapped = |h: Hook| {
            if h.is_wired() {
                HookMap::Mapped(h)
            } else {
                HookMap::Unsupported { nearest: None }
            }
        };
        match norm.as_str() {
            // Build context/prompt before inference. Direct match.
            "before_prompt_build"
            | "before_llm_call"
            | "pre_llm_call"
            | "prompt_build"
            | "before_prompt" => mapped(Hook::PreLlmCall),
            // Inbound message before the gateway dispatches it. Fired by
            // the TUI gateway's surface sink (observer-class: notified,
            // return value ignored, never blocks dispatch).
            "message_received"
            | "pre_gateway_dispatch"
            | "gateway_dispatch"
            | "before_dispatch" => mapped(Hook::PreGatewayDispatch),
            "before_api_request" | "pre_api_request" | "api_request" => mapped(Hook::PreApiRequest),
            "after_api_request" | "post_api_request" | "api_response" => {
                mapped(Hook::PostApiRequest)
            }
            // Tool lifecycle. `pre_tool_call` is a real gate (can deny) and
            // `transform_tool_result` a real transform, both fired inline by the
            // host at the tool-execution point.
            "before_tool_call" | "pre_tool_call" | "tool_call_before" => mapped(Hook::PreToolCall),
            "after_tool_call" | "post_tool_call" | "tool_call_after" => mapped(Hook::PostToolCall),
            "transform_tool_result" | "transform_terminal_output" | "transform_llm_output" => {
                mapped(Hook::TransformToolResult)
            }
            "message_sending" | "before_send" | "outbound_message" => {
                HookMap::Unsupported { nearest: None }
            }
            "on_session_start" | "session_start" | "session_started" => {
                mapped(Hook::OnSessionStart)
            }
            "on_session_end" | "session_end" | "session_finalize" => mapped(Hook::OnSessionEnd),
            "subagent_start" | "agent_start" => mapped(Hook::SubagentStart),
            "subagent_stop" | "agent_end" => mapped(Hook::SubagentStop),
            "on_stream_start" => mapped(Hook::OnStreamStart),
            "on_stream_delta" => mapped(Hook::OnStreamDelta),
            "on_stream_end" => mapped(Hook::OnStreamEnd),
            // OMP compaction lifecycle. Pantheon fires one post-facto observer
            // (`Hook::OnCompaction`, off the durable ContextTrimmed /
            // ContextCompressed events), so the start/end pair collapses onto it.
            // Documented on the hook; a pre-compaction gate is not promised.
            "auto_compaction_start"
            | "auto_compaction_end"
            | "compaction_start"
            | "compaction_end" => mapped(Hook::OnCompaction),
            // OMP retry lifecycle (`auto_retry_start/end`,
            // `retry_fallback_applied/succeeded`, `ttsr_triggered`). Pantheon's
            // fallback walk lives in the provider plane (`chain.rs`:
            // `ModelEvent::Fallback`), which never reaches the core event fan-out
            // the bridge fires from - so there is no fire site, and per this
            // file's rule these stay Unsupported rather than mapping onto an
            // observer that would fire at the wrong time. Explicit arms (not the
            // catch-all) so they count as reasoned-about in KNOWN_FOREIGN_EVENTS.
            "auto_retry_start"
            | "auto_retry_end"
            | "retry_fallback_applied"
            | "retry_fallback_succeeded"
            | "ttsr_triggered" => HookMap::Unsupported { nearest: None },
            // Unknown: refused rather than guessed.
            _ => HookMap::Unsupported { nearest: None },
        }
    }

    /// Every foreign event name the adapter knows how to reason about. Used to
    /// tell "this event is unsupported on purpose" from "this event is new to us".
    pub const KNOWN_FOREIGN_EVENTS: &[&str] = &[
        "before_prompt_build",
        "before_tool_call",
        "after_tool_call",
        "message_received",
        "message_sending",
        "transform_tool_result",
        "transform_llm_output",
        "on_session_start",
        "on_session_end",
        "subagent_start",
        "subagent_stop",
        "on_stream_start",
        "on_stream_delta",
        "on_stream_end",
        "auto_compaction_start",
        "auto_compaction_end",
        "auto_retry_start",
        "auto_retry_end",
        "retry_fallback_applied",
        "retry_fallback_succeeded",
        "ttsr_triggered",
    ];

    // ---------------------------------------------------------------------------
    // API surface the runner cannot serve
    // ---------------------------------------------------------------------------

    /// `api.*` registration methods observed across the OpenClaw extension set
    /// that Pantheon has no equivalent for. Recorded so a partial load is visible
    /// rather than implied.
    pub const REFUSABLE_API_METHODS: &[&str] = &[
        "registerProvider",
        "registerTool",
        "registerCommand",
        "registerCli",
        "registerGatewayMethod",
        "registerHttpRoute",
        "registerService",
        "registerWebSearchProvider",
        "registerImageGenerationProvider",
        "registerVideoGenerationProvider",
        "registerSpeechProvider",
        "registerMediaUnderstandingProvider",
        "registerMemoryEmbeddingProvider",
        "registerMusicGenerationProvider",
        "registerRealtimeTranscriptionProvider",
        "registerModelCatalogProvider",
    ];

    // ---------------------------------------------------------------------------
    // Report
    // ---------------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CredentialRequirement {
        /// The foreign env var name, e.g. `MODELSTUDIO_API_KEY`.
        pub env_var: String,
        /// The provider or plugin that needs it.
        pub provider: String,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct CompatReport {
        /// Display name of the plugin.
        pub name: String,
        /// Which foreign shape it came from.
        pub origin: String,
        /// Foreign events that mapped cleanly.
        pub mapped: Vec<String>,
        /// Foreign events with no Panthey equivalent. Never empty-and-silent.
        pub unsupported: Vec<String>,
        /// `api.*` methods the plugin wanted that we cannot serve.
        pub refused: Vec<String>,
        /// Env vars the plugin declares it needs. Names only, no values.
        pub credentials: Vec<CredentialRequirement>,
        /// Where the generated `plugin.yaml` was written, if it was.
        pub manifest_written: Option<String>,
    }

    impl CompatReport {
        /// True when the plugin loaded with nothing lost.
        pub fn clean(&self) -> bool {
            self.unsupported.is_empty() && self.refused.is_empty()
        }

        /// One-line operator summary.
        pub fn summary(&self) -> String {
            format!(
                "{} ({}): {} hook(s) mapped, {} unsupported, {} capability(ies) refused",
                self.name,
                self.origin,
                self.mapped.len(),
                self.unsupported.len(),
                self.refused.len()
            )
        }
    }

    // ---------------------------------------------------------------------------
    // Detection
    // ---------------------------------------------------------------------------

    /// What kind of foreign extension a directory holds.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CompatKind {
        OpenClaw,
        Omp,
    }

    impl CompatKind {
        pub fn name(&self) -> &'static str {
            match self {
                CompatKind::OpenClaw => "openclaw",
                CompatKind::Omp => "omp",
            }
        }
    }

    /// Identify a foreign extension directory, or `None` if it is not one.
    pub fn detect_kind(dir: &Path) -> Option<CompatKind> {
        if dir.join("openclaw.plugin.json").is_file() {
            return Some(CompatKind::OpenClaw);
        }
        if let Some(pkg) = read_package(dir) {
            if pkg.omp.is_some() || pkg.pi.is_some() {
                return Some(CompatKind::Omp);
            }
        }
        None
    }

    fn read_package(dir: &Path) -> Option<PackageJson> {
        let body = std::fs::read_to_string(dir.join("package.json")).ok()?;
        serde_json::from_str(&body).ok()
    }

    /// The JS entry file a foreign extension registers from.
    pub fn entry_file(dir: &Path, kind: CompatKind) -> Option<PathBuf> {
        match kind {
            CompatKind::OpenClaw => {
                for c in ["index.js", "index.mjs", "index.cjs"] {
                    if dir.join(c).is_file() {
                        return Some(dir.join(c));
                    }
                }
                None
            }
            CompatKind::Omp => {
                let pkg = read_package(dir)?;
                let m = pkg.omp.as_ref().or(pkg.pi.as_ref())?;
                // `hooks` is the registration entry; fall back to the conventional
                // index when the manifest does not name one.
                if let Some(h) = &m.hooks {
                    if dir.join(h).is_file() {
                        return Some(dir.join(h));
                    }
                }
                for c in ["index.js", "index.mjs", "index.cjs"] {
                    if dir.join(c).is_file() {
                        return Some(dir.join(c));
                    }
                }
                None
            }
        }
    }

    /// Parse a foreign extension's manifest into the fields the adapter needs.
    pub fn read_manifest(
        dir: &Path,
        kind: CompatKind,
    ) -> Option<(String, String, Vec<CredentialRequirement>)> {
        match kind {
            CompatKind::OpenClaw => {
                let body = std::fs::read_to_string(dir.join("openclaw.plugin.json")).ok()?;
                let m: OpenClawManifest = serde_json::from_str(&body).ok()?;
                let name = m
                    .name
                    .clone()
                    .or_else(|| m.id.clone())
                    .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "openclaw-extension".into());
                let creds = m
                    .setup
                    .map(|s| {
                        s.providers
                            .into_iter()
                            .flat_map(|p| {
                                let provider = p.id.clone().unwrap_or_else(|| name.clone());
                                p.env_vars.into_iter().map(move |v| CredentialRequirement {
                                    env_var: v,
                                    provider: provider.clone(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some((name, m.description.unwrap_or_default(), creds))
            }
            CompatKind::Omp => {
                let pkg = read_package(dir)?;
                let m = pkg.omp.clone().or(pkg.pi.clone())?;
                let name = m
                    .name
                    .clone()
                    .or(pkg.name.clone())
                    .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
                    .unwrap_or_else(|| "omp-extension".into());
                let _version = m
                    .version
                    .clone()
                    .or(pkg.version.clone())
                    .unwrap_or_default();
                Some((
                    name,
                    m.description.or(pkg.description).unwrap_or_default(),
                    Vec::new(),
                ))
            }
        }
    }

    /// Scan a foreign extension's JS for the `api.on("<event>"` registrations and
    /// `api.<method>(` calls, so the report can name what it will and will not run.
    ///
    /// This is a text scan, not a parse: the extensions ship minified bundles, and
    /// a regex over the registration call sites is the honest bound. It can miss a
    /// dynamically-computed event name, which is why `CompatReport::unsupported`
    /// is a *lower* bound on what was lost and the runner re-checks at load.
    pub fn scan_registrations(js: &str) -> (Vec<String>, Vec<String>) {
        let mut events = Vec::new();
        let mut methods = Vec::new();

        // api.on('x' | "x" , ...)
        let mut rest = js;
        while let Some(i) = rest.find("api.on(") {
            let after = &rest[i + "api.on(".len()..];
            let quote = match after.chars().next() {
                Some(q @ ('\'' | '"')) => q,
                _ => {
                    rest = &rest[i + 1..];
                    continue;
                }
            };
            let after = &after[1..];
            if let Some(end) = after.find(quote) {
                let ev = &after[..end];
                if !events.iter().any(|e| e == ev) {
                    events.push(ev.to_string());
                }
            }
            rest = &after[after.len().min(1)..];
        }

        for m in REFUSABLE_API_METHODS {
            if js.contains(&format!("api.{m}(")) {
                methods.push((*m).to_string());
            }
        }
        (events, methods)
    }

    // ---------------------------------------------------------------------------
    // Manifest generation
    // ---------------------------------------------------------------------------

    /// Render a scalar as a YAML double-quoted string, escaping every
    /// character that could end the scalar early. Interpolating a name raw
    /// (and the description via Rust `Debug` formatting, which is not YAML
    /// escaping) let a name containing a newline or colon break out of its
    /// field and inject new keys into the generated `plugin.yaml`.
    fn yaml_double_quoted(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('"');
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        }
        out.push('"');
        out
    }

    /// Render a Pantheon `plugin.yaml` for a foreign extension, listing only the
    /// hooks that actually mapped. Written next to the extension so the existing
    /// manager can load it unchanged.
    ///
    /// Every interpolated field is YAML-escaped: hook names are
    /// enum-controlled and safe, but name, version, and description come
    /// from foreign manifests and may contain anything.
    pub fn render_plugin_yaml(
        name: &str,
        version: &str,
        description: &str,
        mapped: &[Hook],
    ) -> String {
        let hooks: Vec<String> = mapped.iter().map(|h| h.name().to_string()).collect();
        format!(
            "# Generated by `pantheon migrate apply` from a foreign extension.\n\
         # Compat adapter: only the hooks below have a Pantheon equivalent.\n\
         # Regenerate rather than hand-editing; unsupported hooks are listed in\n\
         # the migration report, not silently dropped here.\n\
         name: {}\n\
         version: {}\n\
         description: {}\n\
         provides_hooks:\n{}\n",
            yaml_double_quoted(name),
            yaml_double_quoted(version),
            yaml_double_quoted(description),
            hooks
                .iter()
                .map(|h| format!(" - {h}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }

    /// Build the full report for a foreign extension directory without writing
    /// anything. This is what `migrate show` / `migrate plan` use.
    pub fn inspect(dir: &Path) -> Option<CompatReport> {
        let kind = detect_kind(dir)?;
        let (name, _desc, creds) = read_manifest(dir, kind)?;
        let mut report = CompatReport {
            name,
            origin: kind.name().to_string(),
            credentials: creds,
            ..Default::default()
        };

        if let Some(entry) = entry_file(dir, kind) {
            if let Ok(js) = std::fs::read_to_string(&entry) {
                let (events, methods) = scan_registrations(&js);
                for e in events {
                    match map_hook(&e) {
                        HookMap::Mapped(_) => {
                            if !report.mapped.contains(&e) {
                                report.mapped.push(e)
                            }
                        }
                        HookMap::Unsupported { .. } => {
                            if !report.unsupported.contains(&e) {
                                report.unsupported.push(e)
                            }
                        }
                    }
                }
                report.refused = methods;
            }
        }
        Some(report)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A name carrying a newline + colon must round-trip as one scalar,
        /// not inject a second top-level key into the generated YAML.
        #[test]
        fn render_plugin_yaml_escapes_hostile_name() {
            let hostile = "evil:\n injected: true";
            let out = render_plugin_yaml(hostile, "1.0\"x", "desc\nwith \"quotes\"", &[]);
            let v: serde_yaml::Value = serde_yaml::from_str(&out).expect("rendered YAML parses");
            let map = v.as_mapping().expect("top level is a mapping");
            assert_eq!(
                map.get(serde_yaml::Value::String("name".into()))
                    .and_then(|v| v.as_str()),
                Some(hostile),
                "name must round-trip verbatim: {out}"
            );
            assert!(
                map.get(serde_yaml::Value::String("injected".into()))
                    .is_none(),
                "no injected key may appear: {out}"
            );
            assert_eq!(
                map.get(serde_yaml::Value::String("version".into()))
                    .and_then(|v| v.as_str()),
                Some("1.0\"x")
            );
            assert_eq!(
                map.get(serde_yaml::Value::String("description".into()))
                    .and_then(|v| v.as_str()),
                Some("desc\nwith \"quotes\"")
            );
        }
    }
}

pub mod index {
    //! Making the session quarantine live.
    //!
    //! `migrate apply` copies a source's transcripts into
    //! `<data_dir>/imported-sessions/<source>/` and stops there - a file nobody
    //! reads. This module closes that: it turns those quarantined transcripts into
    //! [`SessionChunk`]s and indexes them into the same `session_search` store the
    //! live runtime writes, so a migrated history is actually searchable.
    //!
    //! Two rules keep the import from lying to the operator:
    //!
    //! - **A migrated chunk is namespaced.** `run_id` is
    //!   `migrated:<source>:<session>`, which cannot collide with a real `run_*`
    //!   id, and `seq` is the record's line number. A hit points at the quarantined
    //!   file, not at a ledger event that does not exist.
    //! - **Indexing is idempotent.** `chunk_id` is derived from
    //!   (source, file, line), and `index()` is `INSERT OR REPLACE`, so
    //!   re-indexing converges instead of duplicating rows.
    //!
    //! The chunk text is only what a search would match on. It is never replayed
    //! into a session and never executed.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_storage::search::{SessionChunk, SessionSearch as SessionIndex};
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};

    /// One chunk pulled out of a quarantined transcript.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ImportedChunk {
        pub chunk_id: String,
        pub run_id: String,
        pub seq: i64,
        /// `message`, `title`, or `tool` - the same vocabulary the live path uses.
        pub kind: String,
        pub text: String,
        pub ts_ms: i64,
    }

    fn serr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Storage,
            false,
            cause,
            "check the quarantine dir and that the ledger is not locked",
            "see `pantheon migrate apply` output",
        )
    }

    /// A JSONL transcript record, tolerating the shapes real agents write.
    #[derive(Debug, Deserialize)]
    struct Record {
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        timestamp: Option<serde_json::Value>,
        /// `content` may be a string or an OpenAI-style parts array.
        #[serde(default)]
        content: Option<serde_json::Value>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        message: Option<serde_json::Value>,
    }

    /// Flatten a `content` value to searchable text. Handles a bare string, an
    /// array of `{type, text}` parts, and an object with a `text` field.
    fn content_to_text(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(s);
            }
            serde_json::Value::Array(items) => {
                for it in items {
                    content_to_text(it, out);
                }
            }
            serde_json::Value::Object(map) => {
                if let Some(serde_json::Value::String(t)) = map.get("text") {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(t);
                } else if let Some(inner) = map.get("content") {
                    // A nested envelope: `{"message":{"role":..,"content":..}}`
                    // and `{"content":{"text":..}}` both land here.
                    content_to_text(inner, out);
                }
            }
            _ => {}
        }
    }

    fn timestamp_ms(v: Option<&serde_json::Value>) -> i64 {
        match v {
            Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
            // An ISO-8601 string is left to the caller's ordering; 0 sorts oldest,
            // which is the safe default for a recency-weighted search.
            _ => 0,
        }
    }

    /// The longest text a single chunk will index. A tool result can be enormous
    /// and would otherwise dominate the FTS table.
    const MAX_CHUNK_CHARS: usize = 4000;

    /// One parsed record as a chunk, or `None` when it carries no text.
    /// Shared by the JSONL line path and the whole-file `.json` path so
    /// both shapes index identically.
    fn record_to_chunk(rec: &Record, run_id: &str, stem: &str, seq: i64) -> Option<ImportedChunk> {
        // Text, in the order the shapes appear in practice.
        let mut text = String::new();
        if let Some(s) = &rec.text {
            text.push_str(s);
        }
        if let Some(c) = &rec.content {
            content_to_text(c, &mut text);
        }
        if let Some(m) = &rec.message {
            content_to_text(m, &mut text);
        }
        let text = text.trim().to_string();
        if text.is_empty() {
            return None;
        }
        let text = if text.chars().count() > MAX_CHUNK_CHARS {
            let cut: String = text.chars().take(MAX_CHUNK_CHARS).collect();
            format!("{cut}...")
        } else {
            text
        };

        let kind = match rec.kind.as_deref() {
            Some("tool") | Some("tool_call") | Some("ToolResult") => "tool",
            Some("title") => "title",
            _ => match rec.role.as_deref() {
                Some("title") => "title",
                Some("tool") | Some("tool_call") => "tool",
                _ => "message",
            },
        };

        Some(ImportedChunk {
            // Stable across re-runs, so indexing is idempotent.
            chunk_id: format!("{run_id}#{stem}:{seq}"),
            run_id: run_id.to_string(),
            seq,
            kind: kind.to_string(),
            text,
            ts_ms: timestamp_ms(rec.timestamp.as_ref()),
        })
    }

    /// Parse one quarantined transcript into chunks: JSONL (one record per
    /// line) or whole-file `.json` (an array of records, a `{"messages":
    /// [...]}` object, or a single pretty-printed record object).
    ///
    /// `carry` copies `.json` transcripts into quarantine too, so the
    /// indexer must read them - filtering to `.jsonl` only left every
    /// `.json` transcript unindexed and invisible (not even listed in
    /// `empty_files`). Malformed lines are skipped rather than failing the
    /// import: a truncated final line in a live transcript is normal, not
    /// corruption.
    pub fn parse_transcript(path: &Path, source: &str, session: &str) -> Vec<ImportedChunk> {
        let Ok(body) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        let run_id = format!("migrated:{source}:{session}");
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "t".into());
        let trimmed = body.trim();
        // Whole-file JSON first: a pretty-printed `.json` transcript spans
        // many lines, so line parsing would see only fragments of one
        // record and yield nothing.
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                let records: Vec<serde_json::Value> = match &v {
                    serde_json::Value::Array(items) => items.clone(),
                    serde_json::Value::Object(map) => match map.get("messages") {
                        Some(serde_json::Value::Array(items)) => items.clone(),
                        _ => vec![v.clone()],
                    },
                    _ => Vec::new(),
                };
                let mut out = Vec::new();
                for (i, item) in records.iter().enumerate() {
                    if let Ok(rec) = serde_json::from_value::<Record>(item.clone()) {
                        if let Some(chunk) = record_to_chunk(&rec, &run_id, &stem, i as i64) {
                            out.push(chunk);
                        }
                    }
                }
                if !out.is_empty() {
                    return out;
                }
            }
        }
        let mut out = Vec::new();
        for (i, line) in body.lines().enumerate() {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Record>(t) else {
                continue;
            };
            if let Some(chunk) = record_to_chunk(&rec, &run_id, &stem, i as i64) {
                out.push(chunk);
            }
        }
        out
    }

    impl From<ImportedChunk> for SessionChunk {
        fn from(c: ImportedChunk) -> Self {
            SessionChunk {
                chunk_id: c.chunk_id,
                run_id: c.run_id,
                seq: c.seq,
                kind: c.kind,
                text: c.text,
                ts_ms: c.ts_ms,
            }
        }
    }

    /// What one indexing run did.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct IndexReport {
        pub source: String,
        pub files: usize,
        pub chunks: usize,
        /// Files that produced nothing, named so an unreadable transcript is
        /// visible rather than silently contributing zero rows.
        pub empty_files: Vec<String>,
    }

    /// Parse every transcript under a quarantine dir and index it.
    ///
    /// `index` is the live store, so a migrated history becomes searchable through
    /// the same `session_search` tool the agent already has. Pass a fresh
    /// `SessionIndex::open_in_memory()` to dry-run the parse without writing.
    pub fn index_quarantine(
        quarantine_dir: &Path,
        source: &str,
        index: &SessionIndex,
    ) -> Result<IndexReport, PantheonError> {
        let mut report = IndexReport {
            source: source.to_string(),
            ..Default::default()
        };
        let Ok(rd) = std::fs::read_dir(quarantine_dir) else {
            return Ok(report);
        };
        let mut paths: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .map(|e| e == "jsonl" || e == "json")
                        .unwrap_or(false)
                    && p.file_name().map(|n| n != "manifest.json").unwrap_or(false)
            })
            .collect();
        // Stable order, so a report is comparable run to run.
        paths.sort();

        for p in paths {
            let session = p
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "t".into());
            let chunks = parse_transcript(&p, source, &session);
            if chunks.is_empty() {
                report.empty_files.push(p.to_string_lossy().to_string());
                continue;
            }
            report.files += 1;
            for c in chunks {
                index
                    .index(&c.into())
                    .map_err(|e| serr("MIGRATE_SESS_INDEX", format!("{}: {e}", p.display())))?;
                report.chunks += 1;
            }
        }
        Ok(report)
    }

    /// The quarantine directory for a source.
    pub fn quarantine_dir(data_dir: &Path, source: &str) -> PathBuf {
        data_dir.join("imported-sessions").join(source)
    }

    /// Index the quarantined transcripts for `source` into the live session
    /// search store, treating index failure as import failure.
    ///
    /// A bare [`index_quarantine`] returns `Err` on an index write failure, but
    /// nothing stops a caller from logging it and reporting success anyway
    /// which is what `migrate apply` used to do (stderr line, exit 0). This
    /// function's contract is explicit: an `Err` here means the session import
    /// did not complete and must be reported as failed, never as success.
    /// Callers must propagate the error to their exit status.
    ///
    /// A missing quarantine dir is not a failure: there is simply nothing to
    /// index, and an empty report is returned.
    pub fn ensure_sessions_indexed(
        targets: &crate::Targets,
        source: &str,
        index: &SessionIndex,
    ) -> Result<IndexReport, PantheonError> {
        let q = quarantine_dir(&targets.data_dir, source);
        if !q.is_dir() {
            return Ok(IndexReport {
                source: source.to_string(),
                ..Default::default()
            });
        }
        index_quarantine(&q, source, index).map_err(|e| {
        PantheonError::new(
            "MIGRATE_SESS_INDEX_FAILED",
            Layer::Storage,
            false,
            format!("session indexing failed, import not complete: {e}"),
            "the transcripts are quarantined on disk; fix the index and re-run indexing deliberately",
            "see `pantheon migrate apply` output",
        )
    })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        fn scratch(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-index-test-{}-{}-{tag}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn write_tmp(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            path
        }

        /// A pretty-printed single-object `.json` transcript indexes its
        /// text: before the fix, line parsing saw only fragments of the
        /// object and yielded nothing.
        #[test]
        fn json_single_object_transcript_indexes() {
            let dir = scratch("single");
            let path = write_tmp(
                &dir,
                "chat.json",
                "{\n  \"role\": \"user\",\n  \"content\": \"hello from json\"\n}\n",
            );
            let chunks = parse_transcript(&path, "test", "chat");
            assert_eq!(chunks.len(), 1);
            assert!(chunks[0].text.contains("hello from json"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A `.json` array of records indexes every record.
        #[test]
        fn json_array_transcript_indexes() {
            let dir = scratch("array");
            let path = write_tmp(
            &dir,
            "chat.json",
            "[{\"role\": \"user\", \"content\": \"first\"}, {\"role\": \"assistant\", \"content\": \"second\"}]",
        );
            let chunks = parse_transcript(&path, "test", "chat");
            assert_eq!(chunks.len(), 2);
            assert!(chunks[0].text.contains("first"));
            assert!(chunks[1].text.contains("second"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// JSONL still parses line by line, unchanged.
        #[test]
        fn jsonl_transcript_unchanged() {
            let dir = scratch("jsonl");
            let path = write_tmp(
            &dir,
            "chat.jsonl",
            "{\"role\": \"user\", \"content\": \"one\"}\n{\"role\": \"user\", \"content\": \"two\"}\n",
        );
            let chunks = parse_transcript(&path, "test", "chat");
            assert_eq!(chunks.len(), 2);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// `index_quarantine` picks up `.json` files (not just `.jsonl`)
        /// and does not count a real transcript as empty.
        #[test]
        fn index_quarantine_includes_json_files() {
            let dir = scratch("quarantine");
            std::fs::write(
                dir.join("a.json"),
                "{\"role\": \"user\", \"content\": \"json transcript\"}",
            )
            .unwrap();
            std::fs::write(
                dir.join("b.jsonl"),
                "{\"role\": \"user\", \"content\": \"jsonl transcript\"}",
            )
            .unwrap();
            let index = SessionIndex::open_in_memory().unwrap();
            let report = index_quarantine(&dir, "test", &index).unwrap();
            assert_eq!(report.files, 2);
            assert_eq!(report.chunks, 2);
            assert!(report.empty_files.is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod providers {
    //! Custom provider detection, and reconciliation of carried credentials
    //! against Pantheon's own provider catalog.
    //!
    //! Two separate problems, deliberately kept apart:
    //!
    //! - **Custom providers** (`[providers.<name>]` in a Hermes `config.yaml`)
    //!   describe an endpoint Pantheon has no catalog entry for. They need a
    //!   `[custom_providers.<name>]` section written into `config.toml`.
    //! - **Builtin provider keys** are already covered by the credential carry,
    //!   because Pantheon's catalog names each provider's key env after the same
    //!   `<PROVIDER>_API_KEY` convention the sources use. Nothing needs writing
    //!   but nothing *verifies* it either, so a key that matches no catalog entry
    //!   looks identical to one that does. [`reconcile_keys`] is that report.
    //!
    //! The catalog is read through `pantheon-providers`, so the reconciliation cannot
    //! drift from the catalog the runtime actually uses.

    use pantheon_providers::catalog;
    use serde::{Deserialize, Serialize};
    use std::path::Path;

    // ---------------------------------------------------------------------------
    // Custom providers
    // ---------------------------------------------------------------------------

    /// One custom endpoint, in Pantheon's `[custom_providers.*]` shape.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CustomProvider {
        /// The Pantheon provider id (table name).
        pub id: String,
        /// Human label, from the source when it had one.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        pub label: String,
        /// Full base URL, no trailing slash.
        pub base_url: String,
        /// `openai` or `anthropic`.
        pub api_mode: String,
        /// Env var **name** holding the key. Never the key.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub key_env: Option<String>,
        /// The source's default model for this provider, when it declared one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub default_model: Option<String>,
        /// Models the source advertised, with whatever limits it declared. Most
        /// agent configs list bare names (`glm-5.3-flash: {}`), so limits are
        /// usually absent; where a source does declare them they are carried.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub models: Vec<CustomModelRow>,
    }

    /// One model row from a source `providers:` block.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CustomModelRow {
        pub id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub context_limit: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub max_output_tokens: Option<u32>,
    }

    /// Parse a Hermes `providers:` block.
    ///
    /// Line-scanned rather than YAML-deserialised, for the same reason the MCP
    /// parser is: a full deserialise of the source config would pull unrelated
    /// values - including credentials - into a struct, and this function only ever
    /// needs the endpoint shape. Returns `(id, label, base_url, key_env,
    /// default_model, models)` per entry.
    pub fn parse_hermes_providers(config_yaml: &str) -> Vec<CustomProvider> {
        let lines: Vec<&str> = config_yaml.lines().collect();
        let Some(start) = lines
            .iter()
            .position(|l| l.trim_end() == "providers:" || l.trim_end() == "providers: {}")
        else {
            return Vec::new();
        };
        let base = lines[start].len() - lines[start].trim_start().len();

        let mut out: Vec<CustomProvider> = Vec::new();
        let mut cur: Option<CustomProvider> = None;
        // The indent of the `models:` key currently in scope, or `None`. Tracked by
        // indent rather than a boolean because a `models:` block is *not* the last
        // thing in a provider: the real config puts `api_key:` after it, and a
        // boolean that never resets swallows the key and leaves the provider
        // unusable.
        let mut models_indent: Option<usize> = None;

        macro_rules! flush {
            () => {
                if let Some(p) = cur.take() {
                    if !p.base_url.is_empty() {
                        out.push(p);
                    }
                }
            };
        }

        for line in &lines[start + 1..] {
            let t = line.trim_end();
            let s = t.trim();
            if s.is_empty() || s.starts_with('#') {
                continue;
            }
            let indent = t.len() - t.trim_start().len();
            if indent <= base {
                break; // block ended
            }

            // A new provider at the first level under `providers:`.
            if indent == base + 2 && s.ends_with(':') {
                flush!();
                // A new provider always starts a fresh `models:` scope.
                models_indent = None;
                let id = s.trim_end_matches(':').trim().trim_matches('"').to_string();
                cur = Some(CustomProvider {
                    id,
                    label: String::new(),
                    base_url: String::new(),
                    api_mode: "openai".into(),
                    key_env: None,
                    default_model: None,
                    models: Vec::new(),
                });
                continue;
            }
            let Some(p) = cur.as_mut() else { continue };

            // Inside `models:`, but only while indented deeper than the key. The
            // first line at or above that indent ends the block.
            if let Some(mi) = models_indent {
                if indent > mi {
                    if let Some((k, v)) = s.split_once(':') {
                        let k = k.trim().trim_matches('"').to_string();
                        if !k.is_empty() && !p.models.iter().any(|m| m.id == k) {
                            p.models.push(CustomModelRow {
                                id: k,
                                context_limit: number_field(v, "context_window"),
                                max_output_tokens: number_field(v, "max_tokens"),
                            });
                        }
                    }
                    continue;
                }
                models_indent = None;
            }

            if s == "models:" {
                models_indent = Some(indent);
                continue;
            }
            let Some((k, v)) = s.split_once(':') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim();
            match k {
                "name" => p.label = unquote(v),
                // Hermes spells the base URL `api:` in some entries and
                // `base_url:` in others.
                "api" | "base_url" | "baseUrl" => p.base_url = unquote(v),
                "api_key" | "apiKey" | "key_env" | "keyEnv" => {
                    p.key_env = indirection(v).or_else(|| {
                        // A literal key is reported by presence only; the value
                        // never becomes a key_env.
                        if v.is_empty() {
                            None
                        } else {
                            Some("<literal: not migrated>".into())
                        }
                    })
                }
                // `model:` is the default in the real config; `default_model` is
                // the other spelling seen in the wild.
                "model" | "default_model" | "defaultModel" => {
                    let m = unquote(v);
                    if !m.is_empty() {
                        p.default_model = Some(m);
                    }
                }
                // `api_mode` is Pantheon's spelling; Hermes infers it from `api:`.
                "api_mode" | "apiMode" => p.api_mode = normalise_mode(v),
                _ => {}
            }
        }
        flush!();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// A numeric field from a source model row, if it declared one. Sources write
    /// these as `context_window` / `contextWindow` / `max_tokens` / `maxTokens`.
    fn number_field(v: &str, key: &str) -> Option<u32> {
        let mut v = v.trim();
        if v.is_empty() || v == "{}" {
            return None;
        }
        // Accept `context_window: 500000` inside an inline row too.
        let lower = v.to_ascii_lowercase();
        let want = key.to_ascii_lowercase();
        if let Some(i) = lower.find(&want) {
            v = v[i + want.len()..]
                .trim_start()
                .trim_start_matches(':')
                .trim();
        }
        let digits: String = v.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }

    fn normalise_mode(v: &str) -> String {
        if v.trim().eq_ignore_ascii_case("anthropic") {
            "anthropic".into()
        } else {
            "openai".into()
        }
    }

    fn unquote(s: &str) -> String {
        s.trim().trim_matches('"').trim_matches('\'').to_string()
    }

    /// The variable name inside a `${VAR}` reference.
    fn indirection(v: &str) -> Option<String> {
        let v = v.trim();
        let inner = v.strip_prefix("${")?.strip_suffix('}')?;
        let n = inner.trim();
        if !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            Some(n.to_string())
        } else {
            None
        }
    }

    // ---------------------------------------------------------------------------
    // Catalog reconciliation
    // ---------------------------------------------------------------------------

    /// How one carried credential relates to Pantheon's provider catalog.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum KeyMatch {
        /// The env var name is exactly a catalog provider's `key_env`, so the
        /// carried key is already usable as-is.
        Catalogued,
        /// A catalog provider exists for this key under a *different* env name.
        /// The carried key needs renaming to be picked up.
        NeedsRename { catalog_env: String },
        /// No catalog provider claims this env var. It may be a service token
        /// (channel, MCP) or a provider Pantheon has no entry for.
        Unmatched,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct KeyReport {
        pub env_var: String,
        pub match_kind: KeyMatch,
        /// The catalog provider id, when one applies.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub provider: Option<String>,
    }

    /// The catalog's provider id -> key env, read live from `pantheon-providers`
    /// so it cannot drift from what the runtime resolves.
    pub fn catalog_key_envs() -> Vec<(String, String)> {
        catalog::providers()
            .iter()
            .filter(|p| !p.key_env.trim().is_empty())
            .map(|p| (p.id.clone(), p.key_env.trim().to_string()))
            .collect()
    }

    /// Classify one carried env var against the catalog.
    pub fn reconcile_key(env_var: &str, catalog_envs: &[(String, String)]) -> KeyReport {
        if let Some((id, _)) = catalog_envs.iter().find(|(_, e)| e == env_var) {
            return KeyReport {
                env_var: env_var.to_string(),
                match_kind: KeyMatch::Catalogued,
                provider: Some(id.clone()),
            };
        }
        // A catalog provider whose name matches the variable's stem, but which
        // expects a different variable. `XAI_API_KEY` vs a catalog `key_env` of
        // `XAI_KEY` is the shape this catches.
        let stem = env_var
            .trim_end_matches("_API_KEY")
            .trim_end_matches("_TOKEN")
            .trim_end_matches("_KEY")
            .to_ascii_uppercase();
        if !stem.is_empty() && stem.len() > 2 {
            for (id, kenv) in catalog_envs {
                let kstem = kenv
                    .trim_end_matches("_API_KEY")
                    .trim_end_matches("_TOKEN")
                    .trim_end_matches("_KEY")
                    .to_ascii_uppercase();
                if kstem == stem {
                    return KeyReport {
                        env_var: env_var.to_string(),
                        match_kind: KeyMatch::NeedsRename {
                            catalog_env: kenv.clone(),
                        },
                        provider: Some(id.clone()),
                    };
                }
            }
        }
        KeyReport {
            env_var: env_var.to_string(),
            match_kind: KeyMatch::Unmatched,
            provider: None,
        }
    }

    /// Reconcile a whole set of carried env vars.
    pub fn reconcile_keys(env_vars: &[String]) -> Vec<KeyReport> {
        let cat = catalog_key_envs();
        env_vars.iter().map(|e| reconcile_key(e, &cat)).collect()
    }

    // ---------------------------------------------------------------------------
    // Rendering
    // ---------------------------------------------------------------------------

    /// Render `[custom_providers.<id>]` sections in Pantheon's TOML shape.
    ///
    /// A provider whose key was a literal in the source is emitted with the
    /// `key_env` line **omitted** and a comment saying so, rather than with an
    /// invented env var that would resolve to nothing.
    pub fn render_custom_providers(providers: &[CustomProvider]) -> String {
        let mut s = String::new();
        s.push_str("# Generated by `pantheon migrate apply` from a source agent config.\n");
        s.push_str("# Review before merging into <data_dir>/config.toml.\n");
        s.push_str("# No model list is written: pantheon fetches the live list from the\n");
        s.push_str("# endpoint, and records a model here only once you name one.\n\n");
        for p in providers {
            s.push_str(&format!("[custom_providers.{}]\n", toml_key(&p.id)));
            s.push_str(&format!("base_url = {:?}\n", p.base_url));
            s.push_str(&format!("api_mode = {:?}\n", p.api_mode));
            match &p.key_env {
                Some(k) if !k.starts_with('<') => s.push_str(&format!("key_env = {k:?}\n")),
                _ => {
                    s.push_str(
                        "# key_env: the source stored a literal key, not a variable reference.\n",
                    );
                    s.push_str(&format!(
                        "# Supply it with `pantheon provider add --name {} --key ...`.\n",
                        p.id
                    ));
                }
            }
            if p.default_model.is_some() {
                s.push_str(&format!(
                    "# default model in source: {}\n",
                    p.default_model.as_deref().unwrap_or("")
                ));
            }
            // Deliberately no `models = [...]` here. A source agent config's
            // model list is a snapshot of a third-party endpoint taken whenever
            // that agent last synced - for an aggregator it is stale almost
            // immediately. Pantheon fetches the live list from the endpoint on
            // demand (`pantheon provider models <name>`) and records a model only
            // once the operator has actually named one.
            if !p.models.is_empty() {
                s.push_str(&format!(
                    "# the source advertised {} model id(s); not written down.\n\
                 # Pantheon fetches the live list from the endpoint instead, and\n\
                 # records a model here only when you name one yourself.\n\
                 # See: pantheon provider models {}\n",
                    p.models.len(),
                    p.id
                ));
            }
            s.push('\n');
        }
        s
    }

    /// Bare or quoted TOML key, quoted when it is not a bare key.
    fn toml_key(k: &str) -> String {
        let bare = !k.is_empty()
            && k.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if bare {
            k.to_string()
        } else {
            format!("{k:?}")
        }
    }

    /// A one-line human summary of a provider set.
    pub fn summarise(providers: &[CustomProvider]) -> String {
        if providers.is_empty() {
            return "no custom providers".to_string();
        }
        let with_key = providers
            .iter()
            .filter(|p| p.key_env.as_deref().is_some_and(|k| !k.starts_with('<')))
            .count();
        let models: usize = providers.iter().map(|p| p.models.len()).sum();
        format!(
        "{} custom provider(s), {with_key} with a key variable, {models} model id(s) seen in the source (not written down)",
        providers.len()
    )
    }

    /// Read a provider set from a source config file, or an empty set.
    pub fn read_providers(path: &Path) -> Vec<CustomProvider> {
        std::fs::read_to_string(path)
            .map(|b| parse_hermes_providers(&b))
            .unwrap_or_default()
    }

    // ---------------------------------------------------------------------------
    // Writing
    // ---------------------------------------------------------------------------

    /// Where the reviewable sidecar goes.
    pub fn provider_sidecar_path(data_dir: &Path) -> std::path::PathBuf {
        data_dir.join("providers").join("imported.toml")
    }

    /// Write the sidecar. This is the **default** destination: a provider is
    /// written somewhere reviewable, never straight into the live config.
    pub fn write_provider_sidecar(
        data_dir: &Path,
        source: &str,
        providers: &[CustomProvider],
    ) -> Result<std::path::PathBuf, pantheon_api::error::PantheonError> {
        use pantheon_api::error::{Layer, PantheonError};
        let dir = data_dir.join("providers");
        std::fs::create_dir_all(&dir).map_err(|e| {
            PantheonError::new(
                "MIGRATE_PROV_MKDIR",
                Layer::Runtime,
                false,
                e.to_string(),
                "check the data dir is writable",
                source,
            )
        })?;
        let path = provider_sidecar_path(data_dir);
        let body = format!(
        "# Source: {source}\n# Review, then merge with:\n#   pantheon provider add --name <id> --base-url <url> --key-env <env>\n{}",
        render_custom_providers(providers)
    );
        std::fs::write(&path, body).map_err(|e| {
            PantheonError::new(
                "MIGRATE_PROV_WRITE",
                Layer::Runtime,
                false,
                e.to_string(),
                "check the data dir is writable",
                source,
            )
        })?;
        Ok(path)
    }

    /// Merge provider sections into an existing `config.toml` **body**, without
    /// clobbering anything.
    ///
    /// A text-level merge, not a TOML round-trip: re-serialising a user's
    /// `config.toml` would reorder and reformat every table in it, and a
    /// migration should leave the file it did not write byte-identical outside the
    /// sections it adds. An existing `[custom_providers.<id>]` is **skipped**, not
    /// replaced, and reported.
    pub fn merge_into_config(
        existing: &str,
        providers: &[CustomProvider],
    ) -> (String, Vec<String>, Vec<String>) {
        let mut added = Vec::new();
        let mut skipped = Vec::new();
        let existing_ids: Vec<String> = existing
            .lines()
            .filter_map(|l| {
                l.trim()
                    .strip_prefix("[custom_providers.")
                    .and_then(|r| r.strip_suffix(']'))
                    .map(|k| k.trim().trim_matches('"').to_string())
            })
            .collect();

        let mut out = String::new();
        if !existing.ends_with('\n') && !existing.is_empty() {
            out.push('\n');
        }
        out.push_str(existing);
        if !existing.contains("[custom_providers.") {
            out.push_str("\n# --- added by `pantheon migrate apply` ---\n");
        }

        for p in providers {
            if existing_ids.iter().any(|e| e == &p.id) {
                skipped.push(p.id.clone());
                continue;
            }
            out.push_str(&format!("\n[custom_providers.{}]\n", toml_key(&p.id)));
            out.push_str(&format!("base_url = {:?}\n", p.base_url));
            out.push_str(&format!("api_mode = {:?}\n", p.api_mode));
            if let Some(k) = p.key_env.as_deref().filter(|k| !k.starts_with('<')) {
                out.push_str(&format!("key_env = {k:?}\n"));
            }
            // See `render_custom_providers`: no harvested model list. A model
            // row appears here only after the operator names one, via
            // `pantheon model` (which records it) - never copied from a source
            // config's stale snapshot.
            added.push(p.id.clone());
        }
        (out, added, skipped)
    }

    /// Merge into the live `<data_dir>/config.toml`, backing it up first.
    pub fn merge_into_live_config(
        data_dir: &Path,
        providers: &[CustomProvider],
    ) -> Result<(std::path::PathBuf, Vec<String>, Vec<String>), pantheon_api::error::PantheonError>
    {
        use pantheon_api::error::{Layer, PantheonError};
        if providers.is_empty() {
            return Ok((data_dir.join("config.toml"), Vec::new(), Vec::new()));
        }
        let path = data_dir.join("config.toml");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let (merged, added, skipped) = merge_into_config(&existing, providers);
        if added.is_empty() {
            return Ok((path, added, skipped));
        }
        // Pre-image beside the file, so a bad merge is one `mv` from undone.
        if !existing.is_empty() {
            let bak = path.with_extension("toml.pre-migrate");
            std::fs::write(&bak, &existing).map_err(|e| {
                PantheonError::new(
                    "MIGRATE_PROV_BACKUP",
                    Layer::Runtime,
                    false,
                    e.to_string(),
                    "check the data dir is writable",
                    path.to_string_lossy(),
                )
            })?;
        }
        std::fs::write(&path, &merged).map_err(|e| {
            PantheonError::new(
                "MIGRATE_PROV_MERGE",
                Layer::Runtime,
                false,
                e.to_string(),
                "revert from config.toml.pre-migrate, or use `pantheon provider add`",
                path.to_string_lossy(),
            )
        })?;
        Ok((path, added, skipped))
    }
}

pub mod session_import {
    //! First-class session import: foreign transcripts become real, resumable
    //! ledger runs.
    //!
    //! The quarantine path (`carry::write_session_import_*`) keeps a search-only
    //! copy of a source's transcripts. This module goes further: it replays each
    //! transcript into the ledger as native events, so the session shows up in
    //! `/resume`, reopens, and continues like any Pantheon conversation.
    //!
    //! Design notes:
    //!
    //! - **Deterministic ids.** A run id is
    //!   `imported:<source>:<parent-dir>:<file-stem>`. Re-running an import
    //!   converges: a run that already exists is skipped, never duplicated.
    //! - **Native shapes.** User and assistant records become
    //!   `AssistantMessage` events - the same shape the runtime uses for live
    //!   sessions (user prompts ride `AssistantMessage` with `Role::User`), so
    //!   `rebuild_messages` and resume work unchanged.
    //! - **Reasoning survives.** Thinking traces become `ImportedReasoning`
    //!   events: durable, surfaced in the TUI transcript in order, never sent
    //!   to the model as a conversation message.
    //! - **Tool traffic is dropped.** Pantheon cannot safely re-execute a
    //!   foreign tool call, so tool records are counted and the drop is written
    //!   onto the run as a `RunProgress` note. The transcript is honest about
    //!   what is missing.
    //! - **Provenance.** Source, original session id, and import timestamp are
    //!   written as the run's first `RunProgress` event.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::events::Event;
    use pantheon_api::message::Message;
    use pantheon_storage::Ledger;
    use std::path::{Path, PathBuf};

    fn serr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Storage,
            false,
            cause,
            "check the data dir is writable and the ledger is not locked",
            "see `pantheon migrate apply --categories sessions` output",
        )
    }

    /// One ordered turn extracted from a foreign transcript.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ImportedTurn {
        pub role: TurnRole,
        pub text: String,
    }

    /// The roles a transcript record can play in an imported session.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TurnRole {
        User,
        Assistant,
        Reasoning,
    }

    /// A JSONL transcript record, tolerating the shapes real agents write.
    ///
    /// Beyond the flat `{role, content}` shape this also understands the
    /// Claude-style envelope `{type: "user"|"assistant", message: {role,
    /// content: [...]}}` whose content parts carry `thinking` / `text` /
    /// `tool_use` payloads.
    #[derive(Debug, serde::Deserialize, Default)]
    struct Record {
        #[serde(default)]
        role: Option<String>,
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        r#type: Option<String>,
        #[serde(default)]
        content: Option<serde_json::Value>,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        message: Option<serde_json::Value>,
        /// Anthropic-style thinking blocks also appear flattened on the record.
        #[serde(default)]
        thinking: Option<String>,
        #[serde(default)]
        reasoning_content: Option<String>,
        #[serde(default)]
        reasoning: Option<String>,
    }

    /// Flatten a `content` value to text, collecting reasoning parts separately.
    ///
    /// Returns `(text, reasoning)`: display text first, thinking traces second.
    /// Tool payloads (`tool_use`, `tool_result`, `function_call`, ...) contribute
    /// nothing and are counted by the caller via `dropped`.
    fn content_to_text(
        v: &serde_json::Value,
        text: &mut String,
        reasoning: &mut String,
        dropped: &mut usize,
    ) {
        match v {
            serde_json::Value::String(s) => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(s);
            }
            serde_json::Value::Array(items) => {
                for it in items {
                    content_to_text(it, text, reasoning, dropped);
                }
            }
            serde_json::Value::Object(map) => {
                let t = map.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match t {
                    // Thinking / reasoning parts, in the shapes agents write.
                    "thinking" | "reasoning" | "thought" => {
                        let r = map
                            .get("thinking")
                            .or_else(|| map.get("reasoning"))
                            .or_else(|| map.get("reasoning_content"))
                            .or_else(|| map.get("text"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        if !r.trim().is_empty() {
                            if !reasoning.is_empty() {
                                reasoning.push_str("\n\n");
                            }
                            reasoning.push_str(r.trim());
                        }
                    }
                    // Tool traffic is dropped, not replayed.
                    "tool_use"
                    | "tool_result"
                    | "function_call"
                    | "function_call_output"
                    | "tool_call"
                    | "computer_call" => {
                        *dropped += 1;
                    }
                    _ => {
                        if let Some(s) = map.get("text").and_then(|v| v.as_str()) {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(s);
                        } else if let Some(inner) = map.get("content") {
                            content_to_text(inner, text, reasoning, dropped);
                        } else if let Some(inner) = map.get("message") {
                            content_to_text(inner, text, reasoning, dropped);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Classify one parsed record into ordered turns.
    ///
    /// A record can yield up to two turns (reasoning, then its message), which
    /// keeps a Claude-style `thinking` + `text` pair in its original order.
    /// Returns the turns and the number of dropped non-conversational
    /// records (tool traffic, system/summary metadata, unknown roles).
    fn record_to_turns(rec: &Record) -> (Vec<ImportedTurn>, usize) {
        let mut dropped = 0usize;
        let mut text = String::new();
        let mut reasoning = String::new();

        // Flat reasoning fields some exporters put beside `content`.
        for r in [&rec.thinking, &rec.reasoning_content, &rec.reasoning]
            .into_iter()
            .flatten()
        {
            if !r.trim().is_empty() {
                if !reasoning.is_empty() {
                    reasoning.push_str("\n\n");
                }
                reasoning.push_str(r.trim());
            }
        }
        if let Some(s) = &rec.text {
            text.push_str(s);
        }
        if let Some(c) = &rec.content {
            content_to_text(c, &mut text, &mut reasoning, &mut dropped);
        }
        if let Some(m) = &rec.message {
            // A nested envelope: `{"message":{"role":..,"content":..}}` or the
            // Claude envelope `{"type":"assistant","message":{...}}`.
            if let Ok(nested) = serde_json::from_value::<Record>(m.clone()) {
                let (nested_turns, d) = record_to_turns(&nested);
                dropped += d;
                if nested_role(&nested).is_some() {
                    // Fully-roled envelope: its turns stand as parsed.
                    return (nested_turns, dropped);
                }
                // Role-less envelope: fold its text into this record's role.
                for t in nested_turns {
                    match t.role {
                        TurnRole::Reasoning => {
                            if !reasoning.is_empty() {
                                reasoning.push_str("\n\n");
                            }
                            reasoning.push_str(&t.text);
                        }
                        _ => {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(&t.text);
                        }
                    }
                }
            } else {
                content_to_text(m, &mut text, &mut reasoning, &mut dropped);
            }
        }

        let Some(role) = classify_role(rec) else {
            // Metadata, tool traffic, or an unknown role: dropped, never
            // re-attributed as assistant speech. Counted only when the
            // record actually carried text - an empty husk lost nothing.
            let dropped = if text.trim().is_empty() && reasoning.trim().is_empty() {
                dropped
            } else {
                dropped + 1
            };
            return (Vec::new(), dropped);
        };
        let mut turns = Vec::new();
        match role {
            TurnRole::Reasoning => {
                let body = if reasoning.is_empty() {
                    text
                } else {
                    reasoning
                };
                if !body.trim().is_empty() {
                    turns.push(ImportedTurn {
                        role: TurnRole::Reasoning,
                        text: body.trim().to_string(),
                    });
                }
            }
            TurnRole::User | TurnRole::Assistant => {
                if !reasoning.trim().is_empty() {
                    turns.push(ImportedTurn {
                        role: TurnRole::Reasoning,
                        text: reasoning.trim().to_string(),
                    });
                }
                if !text.trim().is_empty() {
                    turns.push(ImportedTurn {
                        role,
                        text: text.trim().to_string(),
                    });
                }
            }
        }
        (turns, dropped)
    }

    /// The role a nested record declares, if any.
    fn nested_role(rec: &Record) -> Option<TurnRole> {
        let raw = rec
            .role
            .as_deref()
            .or(rec.kind.as_deref())
            .or(rec.r#type.as_deref())?;
        match raw.to_ascii_lowercase().as_str() {
            "user" | "human" => Some(TurnRole::User),
            "assistant" | "ai" | "model" => Some(TurnRole::Assistant),
            "thinking" | "reasoning" | "thought" => Some(TurnRole::Reasoning),
            _ => None,
        }
    }

    /// Roles that are metadata, not speech: a system prompt, a session
    /// summary, or a title must never land in the resumed session as
    /// assistant speech. The target format (ledger user/assistant messages
    /// + reasoning traces) has no system role, so these records are
    ///   dropped and counted, not remapped (see [`classify_role`]).
    fn classify_role(rec: &Record) -> Option<TurnRole> {
        // Only conversational roles become turns. Anything else - system /
        // summary / title metadata, tool traffic, or an unknown role - is
        // `None`: the caller drops it and counts it. Mapping unknown text
        // to Assistant would put words in the assistant's mouth that it
        // never said, and a resumed session would then replay them as its
        // own history.
        let raw = rec
            .role
            .as_deref()
            .or(rec.kind.as_deref())
            .or(rec.r#type.as_deref())
            .unwrap_or("");
        match raw.to_ascii_lowercase().as_str() {
            "user" | "human" => Some(TurnRole::User),
            "assistant" | "ai" | "model" => Some(TurnRole::Assistant),
            "thinking" | "reasoning" | "thought" => Some(TurnRole::Reasoning),
            _ => None,
        }
    }

    /// Whether the record is tool traffic (dropped, counted, never replayed).
    fn is_tool_record(rec: &Record) -> bool {
        let raw = rec
            .role
            .as_deref()
            .or(rec.kind.as_deref())
            .or(rec.r#type.as_deref())
            .unwrap_or("");
        matches!(
            raw.to_ascii_lowercase().as_str(),
            "tool"
                | "tool_use"
                | "tool_result"
                | "tool_call"
                | "function_call"
                | "function_call_output"
                | "computer_call"
        )
    }

    /// Parse one transcript file into ordered turns.
    ///
    /// Accepts JSONL (one record per line) and whole-file JSON: an array of
    /// records, `{"messages": [...]}`, a `{"messages": {...}}` single
    /// message, or a single record object (compact or pretty-printed).
    /// Malformed lines are skipped: a truncated final line in a live
    /// transcript is normal, not corruption. Consecutive same-role turns
    /// merge, so a chatty exporter does not spray one-message-per-line
    /// across the imported session.
    pub fn parse_session_transcript(path: &Path) -> (Vec<ImportedTurn>, usize) {
        let Ok(body) = std::fs::read_to_string(path) else {
            return (Vec::new(), 0);
        };
        let trimmed = body.trim();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                let arr: Option<Vec<serde_json::Value>> = v
                    .as_array()
                    .cloned()
                    .or_else(|| v.get("messages").and_then(|m| m.as_array()).cloned());
                if let Some(arr) = arr {
                    return records_to_turns(arr.iter().map(|v| v.to_string()).collect());
                }
                // A single message object under `messages`, or the whole
                // file as one record: parse it as one record, not as lines
                // (a pretty-printed object spans many lines, each of which
                // fails record parsing, and the file used to yield
                // `SkippedEmpty` for a perfectly good transcript).
                let single: Option<serde_json::Value> = v
                    .get("messages")
                    .filter(|m| m.is_object())
                    .cloned()
                    .or_else(|| {
                        if v.is_object() && looks_like_record(&v) {
                            Some(v.clone())
                        } else {
                            None
                        }
                    });
                if let Some(one) = single {
                    return records_to_turns(vec![one.to_string()]);
                }
            }
        }
        records_to_turns(trimmed.lines().map(|s| s.to_string()).collect())
    }

    /// True when a JSON object carries a record-shaped field, so a
    /// non-record object (e.g. `{"sessions": [...]}`) is not mistaken for
    /// a single-message transcript.
    fn looks_like_record(v: &serde_json::Value) -> bool {
        v.get("role").is_some()
            || v.get("kind").is_some()
            || v.get("type").is_some()
            || v.get("content").is_some()
            || v.get("text").is_some()
            || v.get("message").is_some()
    }

    fn records_to_turns(raw: Vec<String>) -> (Vec<ImportedTurn>, usize) {
        let mut turns: Vec<ImportedTurn> = Vec::new();
        let mut dropped = 0usize;
        for line in raw {
            let t = line.trim();
            if t.is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_str::<Record>(t) else {
                continue;
            };
            if is_tool_record(&rec) {
                dropped += 1;
                continue;
            }
            let (mut ts, d) = record_to_turns(&rec);
            dropped += d;
            for t in ts.drain(..) {
                // Merge consecutive same-role turns.
                if let Some(last) = turns.last_mut() {
                    if last.role == t.role {
                        last.text.push_str("\n\n");
                        last.text.push_str(&t.text);
                        continue;
                    }
                }
                turns.push(t);
            }
        }
        (turns, dropped)
    }

    /// Deterministic run id for one imported transcript.
    ///
    /// `imported:<source>:<parent-dir>:<file-stem>` - stable across re-runs
    /// and across machines, and the parent dir keeps two same-named files in
    /// different session directories from colliding. Residual edge: two
    /// different directories with the *same* name (hermes' `sessions/` vs
    /// `state/sessions/`) holding the same file stem map to one id; the second
    /// import is then skipped as already-imported, which is the safe direction
    /// (no duplication, and same-stem files across those alt dirs would be
    /// copies of one session anyway).
    pub fn import_run_id(source: &str, path: &Path) -> String {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "session".to_string());
        let parent = path
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "sessions".to_string());
        format!(
            "imported:{}:{}:{}",
            sanitize(source),
            sanitize(&parent),
            sanitize(&stem)
        )
    }

    fn sanitize(s: &str) -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    /// What happened when one transcript was offered to the ledger.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ImportStatus {
        /// Written to the ledger as a new run.
        Imported,
        /// The run id already exists; re-import converged, nothing duplicated.
        SkippedExists,
        /// The transcript held no usable turns; nothing was written.
        SkippedEmpty,
    }

    /// The outcome of importing one transcript file.
    #[derive(Debug, Clone)]
    pub struct SessionImportReport {
        pub run_id: String,
        pub status: ImportStatus,
        pub title: String,
        pub user_turns: usize,
        pub assistant_turns: usize,
        pub reasoning_traces: usize,
        pub dropped_tool_records: usize,
    }

    impl SessionImportReport {
        pub fn turns(&self) -> usize {
            self.user_turns + self.assistant_turns
        }
    }

    /// Import one transcript file into the ledger as a resumable run.
    ///
    /// Idempotent: when the deterministic run id already exists the import is
    /// skipped and reported, never duplicated. On success the run is left
    /// `completed` (a finished foreign conversation) so `/resume` can reopen
    /// and continue it.
    pub fn import_session_transcript(
        ledger: &Ledger,
        source: &str,
        path: &Path,
        imported_at_ms: i64,
    ) -> Result<SessionImportReport, PantheonError> {
        let run_id = import_run_id(source, path);
        let (turns, dropped) = parse_session_transcript(path);
        let mut report = SessionImportReport {
            run_id: run_id.clone(),
            status: ImportStatus::SkippedEmpty,
            title: String::new(),
            user_turns: 0,
            assistant_turns: 0,
            reasoning_traces: 0,
            dropped_tool_records: dropped,
        };
        if turns.is_empty() {
            return Ok(report);
        }
        if ledger
            .status(&run_id)
            .map_err(|e| serr("IMPORT_STATUS", e.to_string()))?
            .is_some()
        {
            report.status = ImportStatus::SkippedExists;
            return Ok(report);
        }

        let title = turns
            .iter()
            .find(|t| t.role == TurnRole::User)
            .map(|t| first_line(&t.text, 80))
            .unwrap_or_else(|| {
                path.file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "imported session".to_string())
            });
        report.title = title.clone();

        let original_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        ledger
            .append(&Event::RunStarted {
                run_id: run_id.clone(),
            })
            .map_err(|e| serr("IMPORT_START", e.to_string()))?;
        ledger
            .append(&Event::SessionTitled {
                run_id: run_id.clone(),
                title: title.clone(),
                model: "migration".to_string(),
                source: "import".to_string(),
            })
            .map_err(|e| serr("IMPORT_TITLE", e.to_string()))?;
        ledger
        .append(&Event::RunProgress {
            run_id: run_id.clone(),
            detail: format!(
                "imported from {source} transcript {} original_id={original_id} imported_at_ms={imported_at_ms}",
                path.display()
            ),
        })
        .map_err(|e| serr("IMPORT_PROVENANCE", e.to_string()))?;

        let mut turn_n = 0usize;
        let mut pending_user: Option<String> = None;
        // Buffer user text until the assistant answers so each exchange gets
        // one turn id; a trailing user message without an answer still lands.
        let mut flush = |user: &mut Option<String>,
                         assistant: Option<&ImportedTurn>,
                         reasoning: &[ImportedTurn],
                         ledger: &Ledger|
         -> Result<(), PantheonError> {
            turn_n += 1;
            let turn_id = format!("imported-turn-{turn_n}");
            ledger
                .append(&Event::TurnStarted {
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                })
                .map_err(|e| serr("IMPORT_TURN", e.to_string()))?;
            if let Some(u) = user.take() {
                report.user_turns += 1;
                ledger
                    .append(&Event::AssistantMessage {
                        run_id: run_id.clone(),
                        message: Message::user(u),
                    })
                    .map_err(|e| serr("IMPORT_USER", e.to_string()))?;
            }
            for r in reasoning {
                report.reasoning_traces += 1;
                ledger
                    .append(&Event::ImportedReasoning {
                        run_id: run_id.clone(),
                        turn_id: turn_id.clone(),
                        text: r.text.clone(),
                    })
                    .map_err(|e| serr("IMPORT_REASONING", e.to_string()))?;
            }
            if let Some(a) = assistant {
                report.assistant_turns += 1;
                ledger
                    .append(&Event::AssistantMessage {
                        run_id: run_id.clone(),
                        message: Message::assistant(a.text.clone()),
                    })
                    .map_err(|e| serr("IMPORT_ASSISTANT", e.to_string()))?;
            }
            ledger
                .append(&Event::TurnCompleted {
                    run_id: run_id.clone(),
                    turn_id,
                    outcome: "imported".to_string(),
                })
                .map_err(|e| serr("IMPORT_TURN_DONE", e.to_string()))?;
            Ok(())
        };

        let mut pending_reasoning: Vec<ImportedTurn> = Vec::new();
        for t in &turns {
            match t.role {
                TurnRole::User => {
                    // A new user turn closes any open exchange first.
                    if pending_user.is_some() || !pending_reasoning.is_empty() {
                        flush(
                            &mut pending_user,
                            None,
                            &std::mem::take(&mut pending_reasoning),
                            ledger,
                        )?;
                    }
                    pending_user = Some(match pending_user.take() {
                        Some(u) => format!("{u}\n\n{}", t.text),
                        None => t.text.clone(),
                    });
                }
                TurnRole::Reasoning => pending_reasoning.push(t.clone()),
                TurnRole::Assistant => {
                    flush(
                        &mut pending_user,
                        Some(t),
                        &std::mem::take(&mut pending_reasoning),
                        ledger,
                    )?;
                }
            }
        }
        if pending_user.is_some() || !pending_reasoning.is_empty() {
            flush(
                &mut pending_user,
                None,
                &std::mem::take(&mut pending_reasoning),
                ledger,
            )?;
        }

        if dropped > 0 {
            ledger
            .append(&Event::RunProgress {
                run_id: run_id.clone(),
                detail: format!(
                    "dropped {dropped} non-conversational record(s) at import: foreign tool traffic and system/summary metadata are not replayable as conversation"
                ),
            })
            .map_err(|e| serr("IMPORT_DROPPED", e.to_string()))?;
        }
        ledger
            .append(&Event::RunCompleted {
                run_id: run_id.clone(),
            })
            .map_err(|e| serr("IMPORT_DONE", e.to_string()))?;

        report.status = ImportStatus::Imported;
        Ok(report)
    }

    /// The outcome of importing a batch of session directories.
    pub struct SessionImportBatch {
        pub reports: Vec<SessionImportReport>,
        pub failures: Vec<(PathBuf, String)>,
    }

    /// Import every transcript file found under each session directory.
    ///
    /// `transcript_format` decides what counts as a transcript, mirroring the
    /// quarantine scan so the two views never disagree about the file set.
    /// One bad file does not abort the batch: failures are collected for the
    /// caller to report.
    pub fn import_session_dirs(
        ledger: &Ledger,
        source: &str,
        dirs: &[PathBuf],
        imported_at_ms: i64,
    ) -> SessionImportBatch {
        let mut batch = SessionImportBatch {
            reports: Vec::new(),
            failures: Vec::new(),
        };
        for dir in dirs {
            let Ok(rd) = std::fs::read_dir(dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && crate::carry::transcript_format(p).is_some())
                .collect();
            files.sort();
            for f in files {
                match import_session_transcript(ledger, source, &f, imported_at_ms) {
                    Ok(r) => batch.reports.push(r),
                    Err(e) => batch.failures.push((f, e.to_string())),
                }
            }
        }
        batch
    }

    fn first_line(s: &str, max: usize) -> String {
        let line = s.lines().next().unwrap_or("").trim();
        let cut: String = line.chars().take(max).collect();
        if line.chars().count() > max {
            format!("{cut}...")
        } else {
            cut
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn write_transcript(dir: &Path, name: &str, body: &str) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, body).unwrap();
            path
        }

        fn scratch(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-session-import-test-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// System / summary / title records are dropped, never replayed as
        /// assistant speech; the user turn still imports.
        #[test]
        fn system_summary_title_records_are_dropped_not_assistant() {
            let dir = scratch("roles");
            let path = write_transcript(
            &dir,
            "s.jsonl",
            "{\"role\": \"system\", \"content\": \"You are helpful.\"}\n{\"role\": \"user\", \"content\": \"hello\"}\n{\"role\": \"summary\", \"content\": \"Session summary.\"}\n{\"role\": \"title\", \"content\": \"Chat title\"}\n{\"role\": \"assistant\", \"content\": \"hi there\"}\n",
        );
            let (turns, dropped) = parse_session_transcript(&path);
            assert_eq!(
                turns,
                vec![
                    ImportedTurn {
                        role: TurnRole::User,
                        text: "hello".to_string(),
                    },
                    ImportedTurn {
                        role: TurnRole::Assistant,
                        text: "hi there".to_string(),
                    },
                ]
            );
            assert_eq!(dropped, 3, "system + summary + title must be counted");
            assert!(
                !turns.iter().any(|t| t.text.contains("You are helpful")),
                "system text must not appear as a turn"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// An unknown role is dropped, not silently voiced by the assistant.
        #[test]
        fn unknown_role_is_dropped() {
            let dir = scratch("unknown");
            let path = write_transcript(
            &dir,
            "s.jsonl",
            "{\"role\": \"developer\", \"content\": \"secret instructions\"}\n{\"role\": \"user\", \"content\": \"hi\"}\n",
        );
            let (turns, dropped) = parse_session_transcript(&path);
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].role, TurnRole::User);
            assert_eq!(dropped, 1);
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A pretty-printed single-object JSON transcript yields its turn
        /// instead of `SkippedEmpty`.
        #[test]
        fn pretty_single_object_transcript_parses() {
            let dir = scratch("single");
            let path = write_transcript(
                &dir,
                "s.json",
                "{\n  \"role\": \"user\",\n  \"content\": \"hello single\"\n}\n",
            );
            let (turns, _) = parse_session_transcript(&path);
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].role, TurnRole::User);
            assert_eq!(turns[0].text, "hello single");
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A `{"messages": {...}}` single message object also parses.
        #[test]
        fn messages_single_object_parses() {
            let dir = scratch("messages-obj");
            let path = write_transcript(
                &dir,
                "s.json",
                "{\"messages\": {\"role\": \"assistant\", \"content\": \"wrapped\"}}",
            );
            let (turns, _) = parse_session_transcript(&path);
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].text, "wrapped");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
