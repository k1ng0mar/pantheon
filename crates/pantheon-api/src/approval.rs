//! Third-party plugin approval: the ONE approval store for every plugin kind.
//!
//! Security posture, stated plainly: Pantheon does **not** sandbox plugins.
//! Secrets are scrubbed from a plugin's environment, but a plugin otherwise
//! runs with the operator's full user privileges — it can read the
//! operator's files, access the network, and execute commands. A crashing
//! or timed-out plugin fails safely (its action is denied or errors), but
//! that is a fail-safe, not isolation.
//!
//! Because of that, a third-party plugin is never loaded or spawned until
//! the operator has explicitly approved it. Approval is recorded (name,
//! version, content hash, timestamp) in `<scope_dir>/.approvals.json` so
//! consent is traceable. The approval is bound to the plugin's content
//! hash: if the plugin's code changes after approval, the approval lapses
//! and the warning is shown again.
//!
//! First-party plugins — those shipped with Pantheon under
//! `<scope_dir>/bundled/` — do not need approval. Everything else is
//! third-party.
//!
//! Both plugin kinds (tool plugins via `pantheon-exec`, hook plugins via
//! `pantheon-extensions`) share this store. The only per-kind code is the
//! manifest loader passed to [`pending_approvals`]/[`record_approval`]:
//! each kind knows its own manifest filename and shape.
use crate::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Extension,
        false,
        cause,
        "check the plugin directory and its approvals",
        "",
    )
}

/// The warning shown before a third-party plugin can be approved. Approval
/// is informed consent to everything below — there is no sandbox.
pub const PRIVILEGE_WARNING: &str = "\
WARNING: '{name}' is third-party code, and Pantheon does not sandbox plugins.\n\
\n\
If you approve this plugin, it will run with your FULL user privileges:\n\
  - read and write your files\n\
  - access the network\n\
  - execute commands as you\n\
\n\
Secrets (API keys, tokens) are scrubbed from the plugin's environment, but\n\
everything else your user account can do, the plugin can do. A crashing or\n\
timed-out plugin fails safely (its action is denied), but that is a\n\
fail-safe, not isolation.\n\
\n\
Only approve plugins from sources you trust. Approving means you consent to\n\
the plugin running with your full privileges.";

const APPROVALS_FILE: &str = ".approvals.json";

/// One recorded operator approval. Bound to the plugin's content hash so a
/// code change after approval lapses the consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub plugin: String,
    pub version: String,
    /// sha256 over the plugin directory (relative paths + contents).
    pub dir_hash: String,
    /// Unix millis when the operator approved.
    pub approved_at_ms: u64,
}

/// A discovered plugin that is not approved yet.
#[derive(Debug, Clone)]
pub struct PendingPlugin {
    pub name: String,
    pub version: String,
    pub dir: PathBuf,
}

/// Compute a stable sha256 over a plugin directory: sorted relative paths
/// plus file contents.
///
/// Each entry is opened exactly ONCE: a single `open()` resolves a symlink
/// and opens its target atomically, the bytes are read from the resulting
/// file descriptor, and the entry's identity is bound from `fstat`
/// (dev+ino) on that same descriptor — never from a path string. There is
/// no separate resolve-then-read step, so a link retargeted "between"
/// resolution and read cannot bind stale bytes: the digest always covers
/// what the loader will actually open, and the identity bound is the file
/// that was opened, not a path that can be repointed afterwards.
///
/// Identity semantics: dev+ino names the file, not its path. Renaming a
/// target outside the plugin dir does not lapse the approval; replacing
/// the target (new inode) does, even when the bytes are identical.
/// (Non-Unix fallback: size+mtime binding, weaker — a same-size,
/// same-mtime replacement would alias. Documented here, not hidden.)
///
/// Symlinks are therefore bound by their live target's bytes + identity.
/// A link escaping the plugin dir is allowed and bound the same way.
///
/// Fail-closed cases (the hash errors, so no approval can bind): dangling
/// links, unreadable targets, links to non-regular files (e.g.
/// directories), and link loops.
///
/// `.approvals.json` is NOT skipped: a file by that name planted inside
/// the plugin dir is hashed like any other file. (The real store lives in
/// the scope dir, outside any plugin dir, and loaders only execute
/// manifest-declared entrypoints, so excluding the name bought nothing
/// and hid tampering.)
///
/// OUT OF CONTRACT — the hash-then-use gap at the loader: this function
/// closes the intra-hash TOCTOU, but approval as a whole is still
/// check-then-use. `verify_plugin` hashes via [`dir_hash`],
/// `plugin_approval::is_approved` compares against the store, and the
/// supervisor spawns the returned runner path later — the directory can
/// change in between those steps. Loaders close the remaining gap with
/// [`dir_hash_with_override`]: hash the runner's bytes as read through an
/// already-open fd, compare against the store, and exec a sealed private
/// copy of those exact bytes — so the approval binds what executes, not
/// what the path happened to resolve to.
pub fn dir_hash(dir: &Path) -> Result<String, PantheonError> {
    dir_hash_inner(dir, None)
}

/// [`dir_hash`] variant where one file's bytes and identity come from an
/// already-open handle instead of a fresh path open.
///
/// The loader opens the runner first (pinning the inode), reads its bytes
/// through that fd, and calls this with the fd's bytes and fstat metadata.
/// The returned digest therefore binds exactly the bytes the loader holds —
/// an in-place rewrite of the file (same inode, new bytes) between the
/// read and this call cannot smuggle unapproved bytes past the check,
/// because the check runs on the bytes already in hand. A passing check
/// against the approval store proves those bytes were operator-approved.
///
/// `override_rel` is the file's path relative to `dir`. The override skips
/// the regular-file check — the caller already fstat'd the open handle.
pub fn dir_hash_with_override(
    dir: &Path,
    override_rel: &Path,
    override_bytes: &[u8],
    override_md: &std::fs::Metadata,
) -> Result<String, PantheonError> {
    dir_hash_inner(dir, Some((override_rel, override_bytes, override_md)))
}

fn dir_hash_inner(
    dir: &Path,
    ovr: Option<(&Path, &[u8], &std::fs::Metadata)>,
) -> Result<String, PantheonError> {
    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(dir, &mut files)
        .map_err(|e| merr("APPROVAL_HASH", format!("walk {}: {e}", dir.display())))?;
    files.sort();
    let mut h = Sha256::new();
    for f in &files {
        let rel = f.strip_prefix(dir).unwrap_or(f);
        h.update(rel.to_string_lossy().as_bytes());
        h.update([0u8]);
        // Pinned override: these bytes were read through an already-open
        // fd by the caller, so they are exactly what will execute.
        if let Some((orel, obytes, omd)) = ovr {
            if rel == orel {
                hash_file_bytes(&mut h, obytes, omd);
                continue;
            }
        }
        // Bind what the plugin will actually execute, opened once.
        let mut file = std::fs::File::open(f)
            .map_err(|e| merr("APPROVAL_HASH", format!("open {}: {e}", f.display())))?;
        let md = file
            .metadata()
            .map_err(|e| merr("APPROVAL_HASH", format!("fstat {}: {e}", f.display())))?;
        if !md.is_file() {
            return Err(merr(
                "APPROVAL_HASH",
                format!("not a regular file: {}", f.display()),
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| merr("APPROVAL_HASH", format!("read {}: {e}", f.display())))?;
        hash_file_bytes(&mut h, &bytes, &md);
    }
    Ok(hex_of(h.finalize()))
}

/// Mix one file's bytes and opened-file identity into the digest.
fn hash_file_bytes(h: &mut Sha256, bytes: &[u8], md: &std::fs::Metadata) {
    h.update(bytes);
    // Identity of the file actually opened (fstat), not a path string.
    file_identity(md, h);
    h.update([1u8]);
}

/// Mix the opened file's identity into the digest: dev+ino from fstat on
/// Unix. This binds the digest to the file that was opened, so a path
/// retarget after the open cannot alias a recorded approval.
#[cfg(unix)]
fn file_identity(md: &std::fs::Metadata, h: &mut Sha256) {
    use std::os::unix::fs::MetadataExt;
    h.update(md.dev().to_le_bytes());
    h.update(md.ino().to_le_bytes());
}

/// Non-Unix fallback: std has no stable file-id API here, so bind
/// size+mtime instead. Weaker than dev+ino — a same-size, same-mtime
/// replacement would alias — but explicit.
#[cfg(not(unix))]
fn file_identity(md: &std::fs::Metadata, h: &mut Sha256) {
    h.update(md.len().to_le_bytes());
    if let Ok(m) = md.modified() {
        if let Ok(d) = m.duration_since(std::time::UNIX_EPOCH) {
            h.update(d.as_nanos().to_le_bytes());
        }
    }
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            collect_files(&p, out)?;
        } else if ft.is_file() || ft.is_symlink() {
            out.push(p);
        }
    }
    Ok(())
}

fn hex_of(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 of raw bytes, hex-encoded. Used by non-directory approval
/// subjects (MCP server binaries, endpoint URLs) so they can bind an
/// approval to content the same way [`dir_hash`] binds plugin dirs.
pub fn bytes_hash(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex_of(h.finalize())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Persistent approval records for one plugin scope directory
/// (`<data_dir>/plugins`, `<data_dir>/extensions`, or
/// `<project>/.pantheon/plugins`).
#[derive(Debug, Default)]
pub struct ApprovalStore {
    path: PathBuf,
    records: HashMap<String, ApprovalRecord>,
}

impl ApprovalStore {
    /// Open (or create) the store for a plugin scope directory.
    pub fn open(scope_dir: &Path) -> Self {
        // The store file's parent may not exist yet (e.g. a fresh
        // `<data_dir>/mcp` with only config-declared servers): create it so
        // a later `record` doesn't fail on a missing directory.
        let _ = std::fs::create_dir_all(scope_dir);
        let path = scope_dir.join(APPROVALS_FILE);
        let records = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<HashMap<String, ApprovalRecord>>(&t).ok())
            .unwrap_or_default();
        Self { path, records }
    }

    /// True when a live approval exists for this exact name + version +
    /// content hash.
    pub fn is_approved(&self, name: &str, version: &str, dir_hash: &str) -> bool {
        self.records
            .get(name)
            .is_some_and(|r| r.version == version && r.dir_hash == dir_hash)
    }

    /// The recorded approval for `name`, if any. Lets a caller compare
    /// against a partially-known identity (e.g. an MCP server whose
    /// self-reported version is only known after the handshake) without
    /// opening the store file twice.
    pub fn get(&self, name: &str) -> Option<&ApprovalRecord> {
        self.records.get(name)
    }

    /// Every approved name in the store, sorted.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.records.keys().cloned().collect();
        out.sort();
        out
    }

    /// Record an approval (insert or overwrite).
    pub fn record(&mut self, rec: ApprovalRecord) -> Result<(), PantheonError> {
        self.records.insert(rec.plugin.clone(), rec);
        self.save()
    }

    /// Revoke an approval. Returns true when one existed.
    pub fn revoke(&mut self, name: &str) -> Result<bool, PantheonError> {
        let removed = self.records.remove(name).is_some();
        if removed {
            self.save()?;
        }
        Ok(removed)
    }

    fn save(&self) -> Result<(), PantheonError> {
        let text = serde_json::to_string_pretty(&self.records)
            .map_err(|e| merr("APPROVAL_WRITE", format!("serialize: {e}")))?;
        // Atomic: write tmp then rename, so a crash never leaves half JSON.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| merr("APPROVAL_WRITE", format!("write: {e}")))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| merr("APPROVAL_WRITE", format!("rename: {e}")))?;
        Ok(())
    }
}

/// True for plugins shipped with Pantheon: `<scope_dir>/bundled/<name>`.
/// Only these skip approval; everything else is third-party.
pub fn is_bundled(scope_dir: &Path, plugin_dir: &Path) -> bool {
    plugin_dir.starts_with(scope_dir.join("bundled"))
}

/// Convenience: is this plugin currently approved in `scope_dir`?
pub fn is_approved(scope_dir: &Path, name: &str, version: &str, hash: &str) -> bool {
    ApprovalStore::open(scope_dir).is_approved(name, version, hash)
}

/// Record operator approval for a plugin without checking the pending set.
/// The caller must have shown [`PRIVILEGE_WARNING`] and obtained explicit
/// consent first — this only persists the record.
pub fn record_approval_for(
    scope_dir: &Path,
    name: &str,
    version: &str,
    plugin_dir: &Path,
) -> Result<ApprovalRecord, PantheonError> {
    let rec = ApprovalRecord {
        plugin: name.to_string(),
        version: version.to_string(),
        dir_hash: dir_hash(plugin_dir)?,
        approved_at_ms: now_ms(),
    };
    ApprovalStore::open(scope_dir).record(rec.clone())?;
    Ok(rec)
}

/// Revoke a recorded approval. The plugin stays installed but will no
/// longer load until re-approved.
pub fn revoke_approval_for(scope_dir: &Path, name: &str) -> Result<bool, PantheonError> {
    ApprovalStore::open(scope_dir).revoke(name)
}

/// List third-party plugins in `scope_dir` that are installed but not
/// approved (or whose approval lapsed after a code change).
///
/// `load_name_version` maps a plugin directory to its `(name, version)`;
/// each plugin kind supplies its own manifest loader, so this store never
/// learns manifest formats.
pub fn pending_approvals(
    scope_dir: &Path,
    load_name_version: &dyn Fn(&Path) -> Option<(String, String)>,
) -> Vec<PendingPlugin> {
    let store = ApprovalStore::open(scope_dir);
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(scope_dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        if is_bundled(scope_dir, &p) {
            continue;
        }
        let Some((name, version)) = load_name_version(&p) else {
            continue;
        };
        let hash = match dir_hash(&p) {
            Ok(h) => h,
            Err(_) => continue,
        };
        if !store.is_approved(&name, &version, &hash) {
            out.push(PendingPlugin {
                name,
                version,
                dir: p,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Record operator approval for the plugin named `name` in `scope_dir`.
/// Errors when the plugin is not in the pending set — approval is consent
/// for a specific discovered plugin, never a blank check.
/// The caller must have shown [`PRIVILEGE_WARNING`] and obtained explicit
/// consent first.
pub fn record_approval(
    scope_dir: &Path,
    name: &str,
    load_name_version: &dyn Fn(&Path) -> Option<(String, String)>,
) -> Result<ApprovalRecord, PantheonError> {
    let pending = pending_approvals(scope_dir, load_name_version);
    let p = pending.iter().find(|p| p.name == name).ok_or_else(|| {
        merr(
            "APPROVAL_NOT_PENDING",
            format!("no pending third-party plugin named '{name}'"),
        )
    })?;
    record_approval_for(scope_dir, &p.name, &p.version, &p.dir)
}

/// Render the privilege warning for a plugin.
pub fn warning_text(name: &str) -> String {
    PRIVILEGE_WARNING.replace("{name}", name)
}

// Small deterministic invariant tests only. Filesystem behavior
// (hash stability, lapse on code change, store round-trip) is covered in
// `pantheon-eval` (`eval/tests/approval_store.rs`).

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Unique scratch dir under the system temp dir (std-only: this crate
    /// has no tempfile dev-dependency).
    fn test_dir(tag: &str) -> PathBuf {
        let n = TEST_DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let d = std::env::temp_dir().join(format!(
            "pantheon-approval-test-{}-{tag}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn plugin_with_link(scratch: &Path, target: &Path) -> PathBuf {
        let p = scratch.join("plugin");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("run.sh"), "echo one").unwrap();
        symlink(target, p.join("helper.py")).unwrap();
        p
    }

    #[test]
    fn dir_hash_detects_symlink_target_content_change() {
        let scratch = test_dir("target-content");
        let outside = scratch.join("evil.py");
        std::fs::write(&outside, "version one").unwrap();
        let p = plugin_with_link(&scratch, &outside);
        let h1 = dir_hash(&p).unwrap();
        assert_eq!(h1, dir_hash(&p).unwrap(), "stable while untouched");
        // Rewrite the target after the hash was taken: the approval binding
        // must lapse even though the link itself did not change.
        std::fs::write(&outside, "version two").unwrap();
        let h2 = dir_hash(&p).unwrap();
        assert_ne!(h1, h2, "rewriting a symlink target must change the digest");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_detects_change_through_symlink_chain() {
        let scratch = test_dir("chain");
        let real = scratch.join("real.py");
        std::fs::write(&real, "v1").unwrap();
        let mid = scratch.join("mid");
        symlink(&real, &mid).unwrap();
        let p = plugin_with_link(&scratch, &mid);
        let h1 = dir_hash(&p).unwrap();
        std::fs::write(&real, "v2").unwrap();
        assert_ne!(
            h1,
            dir_hash(&p).unwrap(),
            "rewriting the end of a symlink chain must change the digest"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_dangling_symlink_fails_closed() {
        let scratch = test_dir("dangling");
        let p = plugin_with_link(&scratch, &scratch.join("no-such-file.py"));
        assert!(
            dir_hash(&p).is_err(),
            "a dangling symlink must fail closed, not hash"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_symlink_to_directory_fails_closed() {
        let scratch = test_dir("dirlink");
        let sub = scratch.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let p = plugin_with_link(&scratch, &sub);
        assert!(
            dir_hash(&p).is_err(),
            "a symlink to a directory must fail closed, not hash"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_symlink_retarget_still_changes_hash() {
        let scratch = test_dir("retarget");
        let a = scratch.join("a.py");
        let b = scratch.join("b.py");
        std::fs::write(&a, "same bytes").unwrap();
        std::fs::write(&b, "same bytes").unwrap();
        let p = plugin_with_link(&scratch, &a);
        let h1 = dir_hash(&p).unwrap();
        std::fs::remove_file(p.join("helper.py")).unwrap();
        symlink(&b, p.join("helper.py")).unwrap();
        assert_ne!(
            h1,
            dir_hash(&p).unwrap(),
            "retargeting a link must change the digest even when the bytes match"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_retarget_between_resolve_and_read_lapses_approval() {
        // Deterministic replay of the intra-hash TOCTOU the pre-fix code
        // was vulnerable to: it resolved each entry with three separate
        // path-based syscalls (canonicalize -> is_file -> read), so a
        // link retargeted between the resolve and the read bound the OLD
        // target's bytes to the approval while the loader followed the
        // NEW target.
        let scratch = test_dir("toctou-interleave");
        let a = scratch.join("a.py");
        let b = scratch.join("b.py");
        std::fs::write(&a, "payload A").unwrap();
        std::fs::write(&b, "payload B").unwrap();
        let p = scratch.join("plugin");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("run.sh"), "echo one").unwrap();
        let link = p.join("helper.py");
        symlink(&a, &link).unwrap();

        // Approval recorded while the link points at A.
        let h_approval = dir_hash(&p).unwrap();

        // The old three-syscall sequence, with the attacker's retarget
        // injected exactly where the window was:
        let resolved = std::fs::canonicalize(&link).unwrap(); // resolve -> A
        assert!(resolved.is_file());
        // Attacker retargets the link here, inside the old code's window:
        std::fs::remove_file(&link).unwrap();
        symlink(&b, &link).unwrap();
        // The old code then read the RESOLVED path, i.e. the stale target.
        let stale_bytes = std::fs::read(&resolved).unwrap();
        assert_eq!(stale_bytes, b"payload A", "old sequence binds stale bytes");
        // ...while the loader follows the (retargeted) link and gets B:
        assert_eq!(std::fs::read(&link).unwrap(), b"payload B");

        // Post-fix, dir_hash opens each entry once and reads via the fd, so
        // the digest follows the live target: it must NOT reproduce the
        // pre-retarget approval.
        let h_after = dir_hash(&p).unwrap();
        assert_ne!(
            h_after, h_approval,
            "resolve -> retarget -> read must not match the pre-retarget approval"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_replaced_target_changes_digest_despite_identical_bytes() {
        // Same path, same bytes, NEW inode (delete + recreate): the old
        // code bound the canonical path string, so this was invisible.
        let scratch = test_dir("identity-replace");
        let a = scratch.join("a.py");
        std::fs::write(&a, "same bytes").unwrap();
        let p = plugin_with_link(&scratch, &a);
        let h1 = dir_hash(&p).unwrap();
        let b = scratch.join("b.py");
        std::fs::write(&b, "same bytes").unwrap();
        std::fs::rename(&b, &a).unwrap(); // a now has b's inode
        let h2 = dir_hash(&p).unwrap();
        assert_ne!(
            h1, h2,
            "replacing a symlink target (new inode, identical bytes and path) must change the digest"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_renamed_target_keeps_digest_for_same_inode() {
        // Same inode, different path: identity is the file, not the path.
        let scratch = test_dir("identity-rename");
        let a = scratch.join("a.py");
        std::fs::write(&a, "payload").unwrap();
        let p = scratch.join("plugin");
        std::fs::create_dir_all(&p).unwrap();
        symlink(&a, p.join("helper.py")).unwrap();
        let h1 = dir_hash(&p).unwrap();
        let a2 = scratch.join("a2.py");
        std::fs::rename(&a, &a2).unwrap();
        std::fs::remove_file(p.join("helper.py")).unwrap();
        symlink(&a2, p.join("helper.py")).unwrap();
        let h2 = dir_hash(&p).unwrap();
        assert_eq!(
            h1, h2,
            "renaming a target (same inode) must not lapse the approval"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[test]
    fn dir_hash_includes_planted_approvals_json() {
        // A planted `.approvals.json` inside the plugin dir must be bound
        // into the digest like any other file.
        let scratch = test_dir("approvals-included");
        let p = scratch.join("plugin");
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("run.sh"), "echo one").unwrap();
        std::fs::write(p.join(".approvals.json"), r#"{"planted":1}"#).unwrap();
        let h1 = dir_hash(&p).unwrap();
        std::fs::write(p.join(".approvals.json"), r#"{"planted":2}"#).unwrap();
        let h2 = dir_hash(&p).unwrap();
        assert_ne!(
            h1, h2,
            "a planted .approvals.json must be covered by the digest"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }
}
