//! The session-scoped workspace root.
//!
//! The workspace root is the boundary every file path is confined to and
//! the directory relative paths resolve against. Historically it was the
//! *process* working directory, captured once when the tool registry was
//! built. That made the root a property of the process rather than of
//! the session: moving a session to another checkout (a git worktree, a
//! sibling project) meant starting a new process, and every tool that
//! had captured the old root kept using it.
//!
//! [`WorkspaceRoot`] is the swappable form. One handle is shared by every
//! tool in a session; the current value is read *at call time*, so a move
//! takes effect on the next tool call without re-registering anything.
//!
//! Two properties matter:
//!
//! - **Cheap to read.** A tool call reads the root and clones one
//!   `PathBuf`. No lock is held across a tool call, and no tool can
//!   observe a torn value: the path is swapped atomically under the
//!   lock, and readers see either the old or the new root, never a
//!   mixture.
//! - **Explicit about the move.** [`WorkspaceRoot::set`] returns the
//!   previous root so the caller can report it (and record it in the
//!   ledger). A root never changes itself.
//!
//! The handle is `Send + Sync` and cheap to clone, so it can be captured
//! by tool closures and shared with the host that owns the session.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

/// A shared, swappable workspace root.
///
/// Cloning is cheap (the inner `Arc` is shared) and every clone observes
/// the same root, so a move is visible to all of them at once.
#[derive(Clone, Debug)]
pub struct WorkspaceRoot {
    path: Arc<RwLock<PathBuf>>,
}

impl WorkspaceRoot {
    /// A root pinned at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(RwLock::new(path.into())),
        }
    }

    /// A root at the process working directory.
    ///
    /// The fallback when the working directory cannot be read is `.`,
    /// which is what the rest of the workspace does in that case: the
    /// root is still confined, it is just not canonical.
    pub fn from_cwd() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    /// The current root.
    ///
    /// A poisoned lock (a panic while the root was being written) is
    /// recovered rather than propagated: the value inside is still a
    /// valid path, and refusing to serve it would turn a panic in an
    /// unrelated thread into a dead session.
    pub fn path(&self) -> PathBuf {
        self.path
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Move the root. Returns the previous root.
    ///
    /// The caller decides whether the new path is acceptable (that it
    /// exists, that it is a directory); this only performs the swap.
    pub fn set(&self, path: impl Into<PathBuf>) -> PathBuf {
        let mut guard = self.path.write().unwrap_or_else(|poisoned| poisoned.into_inner());
        std::mem::replace(&mut *guard, path.into())
    }

    /// True when `path` is the root itself or lives under it.
    ///
    /// Lexical: the path is *not* canonicalized here, so this is a fast
    /// pre-check, not the containment guarantee. The authoritative check
    /// is `pantheon_exec::confine::confine`, which canonicalizes and
    /// applies the deny globs.
    pub fn contains(&self, path: &Path) -> bool {
        let root = self.path();
        path == root || path.starts_with(&root)
    }
}

#[cfg(test)]
mod tests {
    use super::WorkspaceRoot;
    use std::path::{Path, PathBuf};

    #[test]
    fn clones_share_one_root() {
        let root = WorkspaceRoot::new("/tmp/a");
        let clone = root.clone();
        assert_eq!(root.path(), PathBuf::from("/tmp/a"));
        assert_eq!(clone.path(), PathBuf::from("/tmp/a"));
    }

    /// A move is visible to every clone at once: that is the whole
    /// point of the handle over a captured `PathBuf`.
    #[test]
    fn move_is_visible_to_clones() {
        let root = WorkspaceRoot::new("/tmp/a");
        let clone = root.clone();
        let previous = root.set("/tmp/b");
        assert_eq!(previous, PathBuf::from("/tmp/a"));
        assert_eq!(clone.path(), PathBuf::from("/tmp/b"));
        assert_eq!(root.path(), PathBuf::from("/tmp/b"));
    }

    #[test]
    fn contains_covers_the_root_and_children() {
        let root = WorkspaceRoot::new("/tmp/a");
        assert!(root.contains(Path::new("/tmp/a")));
        assert!(root.contains(Path::new("/tmp/a/b/c.rs")));
        assert!(!root.contains(Path::new("/tmp/ab")));
        assert!(!root.contains(Path::new("/tmp")));
    }

    /// A panic while the root was being written must not brick the
    /// session: the poisoned lock still holds a usable path.
    #[test]
    fn poisoned_lock_still_serves_the_root() {
        let root = WorkspaceRoot::new("/tmp/a");
        let clone = root.clone();
        let _ = std::thread::spawn(move || {
            let _ = clone.set("/tmp/b");
            panic!("poison the lock");
        })
        .join();
        // The write happened before the panic, so the root moved and
        // the lock is poisoned.
        assert_eq!(root.path(), PathBuf::from("/tmp/b"));
    }
}
