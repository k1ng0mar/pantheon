//! First-class git worktrees: create, list, and remove isolated working
//! trees without cloning and without disturbing the current checkout.
//!
//! A worktree is the natural workspace unit for parallel agent work: each
//! one is its own `HEAD`, index, and working checkout, while sharing the
//! repository's object store. Pantheon keeps managed worktrees under
//! `<state_dir>/worktrees/<repo-name>-<hash>/<name>` so they never nest
//! inside the repository or collide with another repo's namespace.
//!
//! The engine is deliberately thin: every operation shells out to `git
//! worktree` and parses the porcelain output. Branches created here live
//! in the `pantheon/<name>` namespace, so `worktree remove` can identify
//! its own branches and never touches the user's branch naming.

use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::gitundo::repo_root;
use crate::safewrite::fnv1a_hex;

fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
}

/// The branch prefix Pantheon uses for worktrees it creates.
pub const WORKTREE_BRANCH_PREFIX: &str = "pantheon/";

/// One worktree as reported by `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    pub path: PathBuf,
    pub head: String,
    /// Short branch name (e.g. `main`), when attached to one.
    pub branch: Option<String>,
    pub bare: bool,
    pub detached: bool,
    /// The repository's primary working tree.
    pub main: bool,
    pub locked: bool,
    pub prunable: bool,
}

impl Worktree {
    /// The branch Pantheon created for this worktree, if it is one of
    /// ours. Useful for cleanup and for display.
    pub fn pantheon_branch(&self) -> Option<&str> {
        self.branch
            .as_deref()
            .filter(|b| b.starts_with(WORKTREE_BRANCH_PREFIX))
    }
}

/// Manage worktrees for one git repository.
#[derive(Debug, Clone)]
pub struct WorktreeManager {
    repo: PathBuf,
    root: PathBuf,
}

impl WorktreeManager {
    /// Bind to the repository containing `repo_dir` and keep managed
    /// worktrees under `<state_dir>/worktrees/<repo>-<hash>`.
    pub fn new(
        repo_dir: impl AsRef<Path>,
        state_dir: impl AsRef<Path>,
    ) -> Result<Self, PantheonError> {
        let repo = repo_root(repo_dir.as_ref()).ok_or_else(|| {
            err(
                "WORKTREE_NO_REPO",
                format!("no git repository found at {}", repo_dir.as_ref().display()),
                "run the command inside a git working tree or pass a repo path",
            )
        })?;
        // A linked worktree's `--show-toplevel` is that worktree, not the
        // repository's primary tree. Bind the namespace to the primary
        // tree so `pantheon worktree list` run from either location sees
        // the same managed root.
        let main = main_worktree(&repo).unwrap_or_else(|| repo.clone());
        let canonical = std::fs::canonicalize(&main).unwrap_or(main);
        let name = canonical
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        let slug: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let hash = fnv1a_hex(canonical.to_string_lossy().as_bytes());
        let namespace = format!("{}-{}", slug.trim_matches('-'), &hash[..8]);
        let mut root = state_dir.as_ref().join("worktrees").join(&namespace);
        // If the data dir happens to be inside the repository (single-dir
        // installs), nesting a worktree inside the checkout it was created
        // from is confusing and can trip recursive tooling. Put managed
        // worktrees beside the repository instead.
        if root.starts_with(&canonical) {
            let parent = canonical.parent().unwrap_or(&canonical);
            root = parent.join(".pantheon-worktrees").join(&namespace);
        }
        Ok(Self {
            repo: canonical,
            root,
        })
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Validate a worktree name and return the managed path.
    pub fn path_for(&self, name: &str) -> Result<PathBuf, PantheonError> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }

    /// Find a linked worktree by managed name or by the
    /// `pantheon/<name>` branch. Unlike [`Self::path_for`], this finds a
    /// worktree created with an explicit `--path`.
    pub fn find(&self, name: &str) -> Result<Option<Worktree>, PantheonError> {
        let managed = self.path_for(name)?;
        let branch = format!("{WORKTREE_BRANCH_PREFIX}{name}");
        Ok(self
            .list()?
            .into_iter()
            .find(|w| w.path == managed || w.branch.as_deref() == Some(branch.as_str())))
    }

    /// `git worktree list --porcelain`, parsed. The first entry is the
    /// repository's main working tree.
    pub fn list(&self) -> Result<Vec<Worktree>, PantheonError> {
        let out = self.git(&["worktree", "list", "--porcelain"])?;
        Ok(parse_porcelain(&out))
    }

    /// Create a worktree from `base` (default `HEAD`) on branch
    /// `pantheon/<name>`. A branch that already exists is attached instead
    /// of recreated (useful when retrying after the directory was removed);
    /// if it is checked out somewhere else, git refuses and the error is
    /// returned unchanged. `base` is ignored when the branch already exists.
    ///
    /// `path` overrides the managed location. The caller is responsible
    /// for picking a location outside the repository.
    pub fn create(
        &self,
        name: &str,
        base: Option<&str>,
        path: Option<&Path>,
    ) -> Result<Worktree, PantheonError> {
        validate_name(name)?;
        let managed = self.path_for(name)?;
        let target = match path {
            Some(p) if p.is_absolute() => p.to_path_buf(),
            Some(p) => std::env::current_dir()
                .map(|cwd| cwd.join(p))
                .unwrap_or_else(|_| p.to_path_buf()),
            None => managed,
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                err(
                    "WORKTREE_MKDIR",
                    format!("create worktree parent {}: {e}", parent.display()),
                    "check permissions and disk space",
                )
            })?;
        }
        if target.exists() && !target.is_dir() {
            return Err(err(
                "WORKTREE_EXISTS",
                format!("{} exists and is not a directory", target.display()),
                "choose a different --path or remove the file",
            ));
        }

        let branch = format!("{WORKTREE_BRANCH_PREFIX}{name}");
        let branch_exists = self
            .git_status(&[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ])
            .is_some_and(|s| s.status.success());
        let target_s = target.to_string_lossy().to_string();
        let base = base.unwrap_or("HEAD");
        let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
        if !branch_exists {
            args.push("--no-track".into());
            args.push("-b".into());
            args.push(branch.clone());
        }
        args.push(target_s.clone());
        if branch_exists {
            args.push(branch.clone());
        } else {
            args.push(base.to_string());
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.git(&refs)?;

        // Return the canonical entry from git, not the path we asked for
        // (git may normalize it).
        let wanted = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        self.list()?
            .into_iter()
            .find(|w| w.path == wanted || w.path == target)
            .ok_or_else(|| {
                err(
                    "WORKTREE_NOT_FOUND",
                    format!(
                        "git reported success but {} is not in `worktree list`",
                        target.display()
                    ),
                    "run `git worktree list` to inspect the repository state",
                )
            })
    }

    /// Remove a Pantheon-managed worktree by name or path. A custom path is
    /// recognized by the `pantheon/` branch namespace. The primary working
    /// tree and unrelated linked worktrees are never removable. The branch
    /// is left in place: deleting it is a separate, deliberate act.
    pub fn remove(&self, target: &str, force: bool) -> Result<Worktree, PantheonError> {
        let path_target = target.contains('/') || target.contains('\\');
        let wanted = if path_target {
            let p = PathBuf::from(target);
            std::fs::canonicalize(&p).unwrap_or(p)
        } else {
            self.path_for(target)?
        };
        let entry = if path_target {
            self.list()?
                .into_iter()
                .find(|w| w.path == wanted || w.path == Path::new(target))
        } else {
            self.find(target)?
        }
        .ok_or_else(|| {
            err(
                "WORKTREE_NOT_FOUND",
                format!("no worktree at {}", wanted.display()),
                "run `pantheon worktree list` to see the managed worktrees",
            )
        })?;
        if entry.main {
            return Err(err(
                "WORKTREE_MAIN",
                "the repository's primary working tree cannot be removed".to_string(),
                "remove a linked worktree, or delete the repository manually",
            ));
        }
        let managed_root = std::fs::canonicalize(&self.root).unwrap_or_else(|_| self.root.clone());
        if !entry.path.starts_with(&managed_root) && entry.pantheon_branch().is_none() {
            return Err(err(
                "WORKTREE_NOT_MANAGED",
                format!(
                    "{} is not a Pantheon-managed worktree",
                    entry.path.display()
                ),
                "remove it with `git worktree remove`, or use a Pantheon worktree",
            ));
        }
        let path_s = entry.path.to_string_lossy().to_string();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(&path_s);
        self.git(&args)?;
        // Prune metadata for any worktree whose directory vanished.
        let _ = self.git(&["worktree", "prune"]);
        Ok(entry)
    }

    fn git(&self, args: &[&str]) -> Result<String, PantheonError> {
        let out = self.git_status(args).ok_or_else(|| {
            err(
                "WORKTREE_GIT",
                "failed to run git".to_string(),
                "ensure git is installed and on PATH",
            )
        })?;
        if !out.status.success() {
            return Err(err(
                "WORKTREE_GIT",
                format!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
                "fix the git error and retry",
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn git_status(&self, args: &[&str]) -> Option<std::process::Output> {
        Command::new("git")
            .current_dir(&self.repo)
            .args(args)
            .output()
            .ok()
    }
}

/// The first `worktree` entry in `git worktree list --porcelain` is the
/// repository's primary working tree. Returns `None` when git cannot
/// report one (then the caller falls back to the current root).
fn main_worktree(repo: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    raw.lines()
        .find_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
}

fn validate_name(name: &str) -> Result<(), PantheonError> {
    if name.is_empty() || name.len() > 64 {
        return Err(err(
            "WORKTREE_BAD_NAME",
            "worktree name must be 1..=64 characters".to_string(),
            "use a short name like 'review' or 'feature-x'",
        ));
    }
    if name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(err(
            "WORKTREE_BAD_NAME",
            format!("invalid worktree name '{name}'"),
            "names may not contain path separators",
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(err(
            "WORKTREE_BAD_NAME",
            format!("invalid worktree name '{name}'"),
            "use letters, digits, dashes, underscores, and dots",
        ));
    }
    Ok(())
}

/// Parse `git worktree list --porcelain`. Blocks are separated by blank
/// lines; the first block is the main worktree.
fn parse_porcelain(raw: &str) -> Vec<Worktree> {
    let mut out: Vec<Worktree> = Vec::new();
    let mut cur: Option<Worktree> = None;
    for line in raw.lines() {
        if line.trim().is_empty() {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(w) = cur.take() {
                out.push(w);
            }
            cur = Some(Worktree {
                path: PathBuf::from(path),
                head: String::new(),
                branch: None,
                bare: false,
                detached: false,
                main: out.is_empty() && cur.is_none(),
                locked: false,
                prunable: false,
            });
            continue;
        }
        let Some(w) = cur.as_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            w.head = head.to_string();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            w.branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
            );
        } else if line == "bare" {
            w.bare = true;
        } else if line == "detached" {
            w.detached = true;
        } else if line.starts_with("locked") {
            w.locked = true;
        } else if line.starts_with("prunable") {
            w.prunable = true;
        }
    }
    if let Some(w) = cur.take() {
        out.push(w);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsafe_names() {
        assert!(validate_name("").is_err());
        assert!(validate_name("../escape").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a\\b").is_err());
        assert!(validate_name("with space").is_err());
        assert!(validate_name("ok-name_1.2").is_ok());
    }

    #[test]
    fn parses_porcelain_with_main_and_linked() {
        let raw = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /state/worktrees/repo-abcd/review
HEAD 2222222222222222222222222222222222222222
branch refs/heads/pantheon/review
locked reason

";
        let list = parse_porcelain(raw);
        assert_eq!(list.len(), 2);
        assert!(list[0].main);
        assert!(!list[0].detached);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert!(!list[1].main);
        assert!(list[1].locked);
        assert_eq!(list[1].pantheon_branch(), Some("pantheon/review"));
    }

    #[test]
    fn create_list_remove_round_trip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .current_dir(&repo)
                .args(args)
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "init"]);

        let state = tmp.path().join("state");
        let m = WorktreeManager::new(&repo, &state).expect("manager");
        let wt = m.create("review", None, None).expect("create");
        assert!(wt.path.exists());
        assert_eq!(wt.branch.as_deref(), Some("pantheon/review"));
        assert!(!wt.main);

        let all = m.list().expect("list");
        assert_eq!(all.len(), 2);
        assert!(all[0].main);

        // Running from inside the linked worktree must still resolve the
        // repository's primary tree, so managed paths share one namespace.
        let from_linked = WorktreeManager::new(&wt.path, &state).expect("manager from linked");
        assert_eq!(from_linked.root(), m.root());
        assert_eq!(from_linked.list().expect("list").len(), 2);

        // While the directory exists, a second create with the same name
        // fails with git's path-exists error.
        assert!(m.create("review", None, None).is_err());

        let removed = m.remove("review", false).expect("remove");
        assert_eq!(removed.branch.as_deref(), Some("pantheon/review"));
        assert!(!m.list().expect("list").iter().any(|w| !w.main));

        // The branch outlives the worktree; recreating attaches it instead
        // of failing on an existing ref.
        let again = m.create("review", None, None).expect("recreate");
        assert_eq!(again.branch.as_deref(), Some("pantheon/review"));
        m.remove("review", false).expect("remove recreated");

        // An explicit --path is still addressable by name (the branch is
        // in Pantheon's namespace) and removable safely.
        let custom = tmp.path().join("elsewhere");
        let made = m
            .create("custom", None, Some(&custom))
            .expect("create custom");
        assert_eq!(made.path, custom);
        let found = m.find("custom").expect("find").expect("custom worktree");
        assert_eq!(found.path, custom);
        m.remove("custom", false).expect("remove custom");
        assert!(!custom.exists());
    }
}
