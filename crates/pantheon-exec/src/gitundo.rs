//! Git-based undo: repo-level snapshots for batch workspaces.
//!
//! `safewrite` checkpoints individual file edits. This module snapshots
//! the entire git working tree before a batch of tool calls, creating
//! a lightweight undo point that can restore all changed files at once.
//!
//! Model:
//! - A snapshot commits the *current* working tree to a dedicated ref
//!   `refs/pantheon/undo/<id>` (so it never touches the user's branch
//!   history) and stores the diff between that snapshot and the working
//!   tree as a patch. The patch is what gets reversed on restore.
//! - `restore` reverses the stored patch (`git apply -R`), which moves
//!   the working tree back to the state the snapshot captured. If the
//!   reverse-apply is impossible (files have diverged), it falls back to
//!   checking the snapshot commit's tree straight out over the working dir.
//!
//! This complements `safewrite`: use it when a task touches many files
//! and you want "undo everything since the last checkpoint" without
//! replaying per-file operations.
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Git undo ref namespace, isolated from user refs.
pub const UNDO_REF_PREFIX: &str = "refs/pantheon/undo/";

/// One undo point: a snapshot of the working tree taken before a batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoPoint {
    /// Unique id, e.g. `undo_1730891234567_0001`.
    pub id: String,
    /// Unix ms when the snapshot was taken.
    pub created_ms: i64,
    /// Head commit when the snapshot was taken (for reference).
    pub head_sha: String,
    /// The undo ref this snapshot lives on.
    pub ref_name: String,
    /// Relative path to the stored patch file (inside the state dir).
    pub patch_path: String,
    /// Files that differ between the snapshot and HEAD (for display).
    pub changed_files: Vec<String>,
    /// FNV-1a hash of the patch bytes for integrity.
    pub patch_hash: String,
}

/// Result of a successful undo restore.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoRestore {
    /// The undo point id that was restored.
    pub undo_id: String,
    /// How many files were affected by the snapshot.
    pub restored_files: usize,
    /// True when the patch reversed cleanly; false when the git-checkout
    /// fallback was used instead.
    pub via_patch: bool,
}

/// Manager for git-based undo points. All state lives under `state_dir`
/// and a target git repo.
#[derive(Debug, Clone)]
pub struct GitUndo {
    state_dir: PathBuf,
    repo_dir: PathBuf,
}

fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
}

impl GitUndo {
    pub fn new(state_dir: PathBuf, repo_dir: PathBuf) -> Result<Self, PantheonError> {
        let dir = state_dir.join("git_undo");
        std::fs::create_dir_all(&dir).map_err(|e| {
            err(
                "UNDO_MKDIR",
                format!("create git undo dir {}: {e}", dir.display()),
                "check state dir permissions and disk space",
            )
        })?;
        Ok(Self {
            state_dir: dir,
            repo_dir,
        })
    }

    /// Take a snapshot of the current working tree.
    ///
    /// Commits the full working tree (including untracked files) to a side
    /// commit, points `refs/pantheon/undo/<id>` at it, and stores the
    /// HEAD-to-snapshot diff as a patch. The user's branch is never moved.
    pub fn snapshot(&self, label: &str) -> Result<UndoPoint, PantheonError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let id = format!(
            "undo_{now}_{:04}",
            std::process::id().wrapping_add((now % 10000) as u32) % 10000
        );
        let ref_name = format!("{UNDO_REF_PREFIX}{id}");
        let head_sha = self.git("rev-parse", &["HEAD"])?;

        // Capture the current tree (tracked + untracked) into the index,
        // make a side commit, point the undo ref at it, then restore the
        // index so the working tree looks exactly as it did before.
        let _ = self.git_raw("add", &["-A", "--", "."]);
        let commit = self.git_raw(
            "commit",
            &[
                "--allow-empty",
                "-m",
                &format!("pantheon undo {label}"),
                "--quiet",
            ],
        );
        if !commit.success {
            return Err(err(
                "UNDO_COMMIT",
                format!("git commit failed: {}", commit.stderr),
                "inspect the repo state; the index may be empty or broken",
            ));
        }
        let snap_sha = self.git("rev-parse", &["HEAD"])?;
        let _ = self.git_raw("update-ref", &[ref_name.as_str(), &snap_sha]);
        // Undo the side commit without touching the working tree.
        let _ = self.git_raw("reset", &["--mixed", head_sha.as_str()]);

        // Store the patch that moves the working tree from its current
        // state back to the snapshot state: a forward diff from the base
        // commit to the snapshot commit. On restore, `git apply` moves the
        // tree to match the snapshot: it re-adds deleted files, removes
        // added ones, and reverts modifications.
        let patch_name = format!("{id}.patch");
        let patch_path = self.state_dir.join(&patch_name);
        let diff = self.git("diff", &[head_sha.as_str(), snap_sha.as_str(), "--"])?;
        std::fs::write(&patch_path, diff.as_bytes()).map_err(|e| {
            err(
                "UNDO_WRITE",
                format!("write undo patch: {e}"),
                "check disk space and permissions",
            )
        })?;
        let patch_hash = crate::safewrite::fnv1a_hex(diff.as_bytes());
        let changed = self.git(
            "diff",
            &["--name-only", head_sha.as_str(), snap_sha.as_str(), "--"],
        )?;
        let changed_files: Vec<String> = changed
            .lines()
            .map(|l| l.to_string())
            .filter(|l| !l.is_empty())
            .collect();

        let point = UndoPoint {
            id: id.clone(),
            created_ms: now,
            head_sha,
            ref_name,
            patch_path: patch_name,
            changed_files,
            patch_hash,
        };
        self.write_manifest(&point)?;
        Ok(point)
    }

    /// Restore the working tree to a previous undo point.
    ///
    /// A stored patch only reverts cleanly when the tree still matches
    /// the base it was diffed against; in general a later batch has
    /// diverged, so the reliable restore is a faithful snapshot check:
    /// unstage everything, check the snapshot's tree out over the
    /// working dir (which reverts modifications and re-adds deleted
    /// files), then prune files that exist now but not in the snapshot.
    ///
    /// The stored patch is still applied first, when it can: it is a
    /// cheaper fast path for the common "one edit, one undo" case and it
    /// leaves the index untouched.
    pub fn restore(&self, point: &UndoPoint) -> Result<UndoRestore, PantheonError> {
        let patch_full = self.state_dir.join(&point.patch_path);
        let raw = std::fs::read(&patch_full).map_err(|e| {
            err(
                "UNDO_READ",
                format!("read undo patch {}: {e}", patch_full.display()),
                "patch file missing or unreadable",
            )
        })?;
        // Integrity: a hash mismatch means the patch was touched; refuse it.
        if crate::safewrite::fnv1a_hex(&raw) != point.patch_hash {
            return Err(err(
                "UNDO_CORRUPT",
                format!(
                    "undo patch {} failed integrity check (hash mismatch)",
                    point.id
                ),
                "undo point is corrupt; take a fresh snapshot",
            ));
        }

        // Fast path: the tree is still where the patch was cut, so
        // applying it moves the tree back to the snapshot exactly.
        let apply = self.git_raw(
            "apply",
            &["--ignore-whitespace", patch_full.to_string_lossy().as_ref()],
        );
        if apply.success {
            return Ok(UndoRestore {
                undo_id: point.id.clone(),
                restored_files: point.changed_files.len(),
                via_patch: true,
            });
        }

        // General path: check the snapshot's tree out over the working
        // dir and prune anything not in it. `reset --mixed` first so
        // post-batch additions become untracked (cleanable) rather than
        // staged, then `checkout -f` overwrites tracked content and
        // restores deleted files, then `clean -fd` removes the rest.
        let reset = self.git_raw("reset", &["--mixed", point.head_sha.as_str()]);
        if !reset.success {
            return Err(err(
                "UNDO_FAIL",
                format!("undo restore: git reset failed: {}", reset.stderr),
                "inspect the repo state manually; delete the corrupt undo point",
            ));
        }
        let co = self.git_raw("checkout", &["-f", point.ref_name.as_str(), "--", "."]);
        if !co.success {
            return Err(err(
                "UNDO_FAIL",
                format!(
                    "undo restore failed: patch apply and git checkout both failed: {}",
                    co.stderr
                ),
                "inspect the repo state manually; delete the corrupt undo point",
            ));
        }
        let _ = self.git_raw("clean", &["-fdq", "--", "."]);
        Ok(UndoRestore {
            undo_id: point.id.clone(),
            restored_files: point.changed_files.len(),
            via_patch: false,
        })
    }

    /// List all stored undo points, newest first.
    pub fn list(&self) -> Result<Vec<UndoPoint>, PantheonError> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(&self.state_dir).map_err(|e| {
            err(
                "UNDO_LIST",
                format!("read undo state dir: {e}"),
                "state dir missing; create a fresh one",
            )
        })?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".json") {
                if let Ok(raw) = std::fs::read_to_string(entry.path()) {
                    if let Ok(p) = serde_json::from_str::<UndoPoint>(&raw) {
                        out.push(p);
                    }
                }
            }
        }
        out.sort_by_key(|u| std::cmp::Reverse(u.created_ms));
        Ok(out)
    }

    /// Delete one undo point and its ref.
    pub fn delete(&self, point: &UndoPoint) -> Result<(), PantheonError> {
        let _ = self.git_raw("update-ref", &["-d", point.ref_name.as_str()]);
        let manifest = self.state_dir.join(format!("{}.json", point.id));
        let _ = std::fs::remove_file(&manifest);
        let patch = self.state_dir.join(&point.patch_path);
        let _ = std::fs::remove_file(&patch);
        Ok(())
    }

    // ---- helpers ----

    fn write_manifest(&self, point: &UndoPoint) -> Result<(), PantheonError> {
        let path = self.state_dir.join(format!("{}.json", point.id));
        let raw = serde_json::to_vec(point).map_err(|e| {
            err(
                "UNDO_SER",
                format!("serialize undo manifest: {e}"),
                "retry the snapshot",
            )
        })?;
        std::fs::write(&path, raw).map_err(|e| {
            err(
                "UNDO_SER",
                format!("write undo manifest {}: {e}", path.display()),
                "check disk space",
            )
        })
    }

    /// Run a git subcommand, returning trimmed stdout. Fails hard.
    fn git(&self, head: &str, rest: &[&str]) -> Result<String, PantheonError> {
        let out = Command::new("git")
            .current_dir(&self.repo_dir)
            .arg(head)
            .args(rest)
            .output()
            .map_err(|e| {
                err(
                    "UNDO_GIT",
                    format!("spawn git: {e}"),
                    "git binary missing or path invalid",
                )
            })?;
        if !out.status.success() {
            return Err(err(
                "UNDO_GIT_FAIL",
                format!(
                    "git {head} failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                "inspect the repo; the working tree may be in an unexpected state",
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Run a git subcommand without hard-failing; return success + stderr.
    fn git_raw(&self, head: &str, rest: &[&str]) -> GitResult {
        let Ok(out) = Command::new("git")
            .current_dir(&self.repo_dir)
            .arg(head)
            .args(rest)
            .output()
        else {
            return GitResult {
                success: false,
                stderr: "spawn git failed".to_string(),
            };
        };
        GitResult {
            success: out.status.success(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        }
    }
}

/// Outcome of a best-effort git subcommand.
#[derive(Debug, Clone)]
struct GitResult {
    success: bool,
    stderr: String,
}

/// Convenience: a git repo must exist at `repo_dir` for undo to work.
/// Returns the repo root when one is found, or `None` (undo is a no-op).
pub fn repo_root(repo_dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .current_dir(repo_dir)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if root.is_empty() {
        return None;
    }
    Some(PathBuf::from(root))
}
