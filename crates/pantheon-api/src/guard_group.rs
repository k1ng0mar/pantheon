//! guard_group: approval, capability, permission_mode in one file.

pub mod approval {
    //! Third-party plugin approval: the ONE approval store for every plugin kind.
    //!
    //! Security posture, stated plainly: Pantheon does **not** sandbox plugins.
    //! Secrets are scrubbed from a plugin's environment, but a plugin otherwise
    //! runs with the operator's full user privileges - it can read the
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
    //! First-party plugins - those shipped with Pantheon under
    //! `<scope_dir>/bundled/` - do not need approval. Everything else is
    //! third-party.
    //!
    //! Both plugin kinds (tool plugins via `pantheon-exec`, hook plugins via
    //! `pantheon-extensions`) share this store. The only per-kind code is the
    //! manifest loader passed to [`pending_approvals`]/[`record_approval`]:
    //! each kind knows its own manifest filename and shape.
    use crate::error::{Layer, PantheonError};
    use fs2::FileExt as _;
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
    /// is informed consent to everything below - there is no sandbox.
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
    /// (dev+ino) on that same descriptor - never from a path string. There is
    /// no separate resolve-then-read step, so a link retargeted "between"
    /// resolution and read cannot bind stale bytes: the digest always covers
    /// what the loader will actually open, and the identity bound is the file
    /// that was opened, not a path that can be repointed afterwards.
    ///
    /// Identity semantics: dev+ino names the file, not its path. Renaming a
    /// target outside the plugin dir does not lapse the approval; replacing
    /// the target (new inode) does, even when the bytes are identical.
    /// (Non-Unix fallback: size+mtime binding, weaker - a same-size,
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
    /// OUT OF CONTRACT - the hash-then-use gap at the loader: this function
    /// closes the intra-hash TOCTOU, but approval as a whole is still
    /// check-then-use. `verify_plugin` hashes via [`dir_hash`],
    /// `plugin_approval::is_approved` compares against the store, and the
    /// supervisor spawns the returned runner path later - the directory can
    /// change in between those steps. Loaders close the remaining gap with
    /// [`dir_hash_with_override`]: hash the runner's bytes as read through an
    /// already-open fd, compare against the store, and exec a sealed private
    /// copy of those exact bytes - so the approval binds what executes, not
    /// what the path happened to resolve to.
    pub fn dir_hash(dir: &Path) -> Result<String, PantheonError> {
        dir_hash_inner(dir, None)
    }

    /// [`dir_hash`] variant where one file's bytes and identity come from an
    /// already-open handle instead of a fresh path open.
    ///
    /// The loader opens the runner first (pinning the inode), reads its bytes
    /// through that fd, and calls this with the fd's bytes and fstat metadata.
    /// The returned digest therefore binds exactly the bytes the loader holds
    /// an in-place rewrite of the file (same inode, new bytes) between the
    /// read and this call cannot smuggle unapproved bytes past the check,
    /// because the check runs on the bytes already in hand. A passing check
    /// against the approval store proves those bytes were operator-approved.
    ///
    /// `override_rel` is the file's path relative to `dir`. The override skips
    /// the regular-file check - the caller already fstat'd the open handle.
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
    /// size+mtime instead. Weaker than dev+ino - a same-size, same-mtime
    /// replacement would alias - but explicit.
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

    /// Persistent approval records for one plugin scope directory
    /// (`<data_dir>/plugins`, `<data_dir>/extensions`, or
    /// `<project>/.pantheon/plugins`).
    #[derive(Debug, Default)]
    pub struct ApprovalStore {
        path: PathBuf,
        records: HashMap<String, ApprovalRecord>,
        /// True when the store file existed but did not parse. The store
        /// reads as empty (fail-closed: nothing is approved), but it must
        /// never be overwritten by a save - that would silently discard
        /// every approval the corrupt file once held.
        corrupt: bool,
    }

    /// Process-wide write lock for approval stores, the in-process half
    /// of the exclusion [`ApprovalStore::record`]/[`revoke`] hold. Same
    /// shape as the dotenv writer lock: a process mutex plus an exclusive
    /// flock on a sidecar `<store>.lock` file, so writers in *other*
    /// processes cannot interleave a read-modify-write either. The sidecar
    /// is never renamed, so the flock pins a stable inode across the
    /// atomic renames of the store file itself.
    static APPROVAL_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Read one store file: the records, plus whether the file existed
    /// but failed to parse.
    fn load_records(path: &Path) -> (HashMap<String, ApprovalRecord>, bool) {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str::<HashMap<String, ApprovalRecord>>(&text) {
                Ok(records) => (records, false),
                Err(_) => (HashMap::new(), true),
            },
            Err(_) => (HashMap::new(), false),
        }
    }

    /// Held across one [`ApprovalStore::record`]/[`revoke`]: the process
    /// mutex (dropped first) and the sidecar flock file (dropped last,
    /// releasing the flock). Kept as a named type so the two halves
    /// cannot be released independently by accident.
    struct ApprovalWriteGuard {
        _proc: std::sync::MutexGuard<'static, ()>,
        _file: std::fs::File,
    }

    /// Acquire the write exclusion for one store file: the process mutex,
    /// then an exclusive flock on the sidecar `<store>.lock` file (the
    /// dotenv writer pattern). Blocking: a writer waits its turn.
    fn lock_for_write(path: &Path) -> Result<ApprovalWriteGuard, PantheonError> {
        let proc = APPROVAL_WRITE_LOCK
            .lock()
            .map_err(|_| merr("APPROVAL_WRITE", "approval write lock poisoned".to_string()))?;
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock_path = PathBuf::from(lock_path);
        let mut opts = std::fs::OpenOptions::new();
        opts.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = match opts.open(&lock_path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = lock_path.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| merr("APPROVAL_WRITE", format!("create lock dir: {e}")))?;
                }
                opts.open(&lock_path)
                    .map_err(|e| merr("APPROVAL_WRITE", format!("open lock file: {e}")))?
            }
            Err(e) => return Err(merr("APPROVAL_WRITE", format!("open lock file: {e}"))),
        };
        file.lock_exclusive()
            .map_err(|e| merr("APPROVAL_WRITE", format!("flock: {e}")))?;
        Ok(ApprovalWriteGuard {
            _proc: proc,
            _file: file,
        })
    }

    impl ApprovalStore {
        /// Open (or create) the store for a plugin scope directory.
        pub fn open(scope_dir: &Path) -> Self {
            // The store file's parent may not exist yet (e.g. a fresh
            // `<data_dir>/mcp` with only config-declared servers): create it so
            // a later `record` doesn't fail on a missing directory.
            let _ = std::fs::create_dir_all(scope_dir);
            let path = scope_dir.join(APPROVALS_FILE);
            let (records, corrupt) = load_records(&path);
            if corrupt {
                // Loud, not silent: a corrupt store reads as empty
                // (fail-closed), so without this an operator would see
                // every plugin unapproved with no hint that the file they
                // approved into is damaged. The log line reaches the
                // agent log when a sink is set; stderr reaches every CLI
                // caller even when it is not.
                let msg = format!(
                    "WARNING: corrupt approval store {}: approvals read as empty (fail-closed) and the file will not be overwritten; repair or remove it by hand",
                    path.display()
                );
                crate::logging::warn("approval", &msg);
                eprintln!("{msg}");
            }
            Self {
                path,
                records,
                corrupt,
            }
        }

        /// True when the store file existed but failed to parse. Reads are
        /// fail-closed (nothing approved); saves are refused until the file
        /// is repaired or removed by hand, so a corrupt store can never be
        /// silently overwritten with an empty one.
        pub fn is_corrupt(&self) -> bool {
            self.corrupt
        }

        /// True when a live approval exists for this exact name + version +
        /// content hash.
        pub fn is_approved(&self, name: &str, version: &str, dir_hash: &str) -> bool {
            self.records
                .get(name)
                .is_some_and(|r| r.version == version && r.dir_hash == dir_hash)
        }

        /// True when a live approval exists for this exact name + content
        /// hash, regardless of the recorded version string. The hash is
        /// the authoritative gate: a version string is self-reported by
        /// the server and can lie (or bump without any content change),
        /// while the hash binds the approval to the exact content the
        /// operator consented to. The recorded version is kept for
        /// DISPLAY only (approval listings, dashboard responses) and must
        /// never gate execution. MCP's post-handshake gate uses this;
        /// plugin loading keeps the stricter name + version + hash check
        /// in [`Self::is_approved`].
        pub fn is_approved_for_hash(&self, name: &str, dir_hash: &str) -> bool {
            self.records
                .get(name)
                .is_some_and(|r| r.dir_hash == dir_hash)
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
        ///
        /// Holds the write exclusion and re-reads the file under it, so
        /// two processes approving different plugins at once cannot lose
        /// each other's record: the old read-at-open, write-at-record
        /// shape overwrote whatever the other process wrote in between.
        pub fn record(&mut self, rec: ApprovalRecord) -> Result<(), PantheonError> {
            let _guard = lock_for_write(&self.path)?;
            self.refresh_under_lock()?;
            self.records.insert(rec.plugin.clone(), rec);
            self.save()
        }

        /// Revoke an approval. Returns true when one existed.
        ///
        /// Same write exclusion and re-read as [`Self::record`].
        pub fn revoke(&mut self, name: &str) -> Result<bool, PantheonError> {
            let _guard = lock_for_write(&self.path)?;
            self.refresh_under_lock()?;
            let removed = self.records.remove(name).is_some();
            if removed {
                self.save()?;
            }
            Ok(removed)
        }

        /// Re-read the store file under the write lock, failing closed on
        /// a corrupt file (never overwrite it with a partial view).
        fn refresh_under_lock(&mut self) -> Result<(), PantheonError> {
            let (records, corrupt) = load_records(&self.path);
            if corrupt {
                self.corrupt = true;
                return Err(merr(
                    "APPROVAL_CORRUPT",
                    format!(
                        "refusing to overwrite corrupt approval store {}; repair or remove it by hand first",
                        self.path.display()
                    ),
                ));
            }
            self.corrupt = false;
            self.records = records;
            Ok(())
        }

        fn save(&self) -> Result<(), PantheonError> {
            if self.corrupt {
                return Err(merr(
                    "APPROVAL_CORRUPT",
                    format!(
                        "refusing to overwrite corrupt approval store {}; repair or remove it by hand first",
                        self.path.display()
                    ),
                ));
            }
            let text = serde_json::to_string_pretty(&self.records)
                .map_err(|e| merr("APPROVAL_WRITE", format!("serialize: {e}")))?;
            // Atomic: write tmp, fsync the tmp file, then rename, so a crash
            // never leaves half JSON and the renamed file is durable. The
            // directory fsync is best-effort (not all filesystems support it).
            let tmp = self.path.with_extension("json.tmp");
            {
                use std::io::Write;
                let mut f = std::fs::File::create(&tmp)
                    .map_err(|e| merr("APPROVAL_WRITE", format!("write: {e}")))?;
                f.write_all(text.as_bytes())
                    .map_err(|e| merr("APPROVAL_WRITE", format!("write: {e}")))?;
                f.sync_all()
                    .map_err(|e| merr("APPROVAL_WRITE", format!("fsync: {e}")))?;
            }
            std::fs::rename(&tmp, &self.path)
                .map_err(|e| merr("APPROVAL_WRITE", format!("rename: {e}")))?;
            if let Some(parent) = self.path.parent() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
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

    /// Systemic gate state for a plugin scope: `None` when the store is
    /// usable (a missing store reads as empty, the normal first-run
    /// state), else a reason when the store file exists but cannot be
    /// trusted - corrupt JSON or unreadable. Callers use this to
    /// quarantine everything the store gates, instead of treating each
    /// plugin as merely unapproved (which would invite re-approval into a
    /// broken store that can never record it).
    pub fn store_gate_failure(scope_dir: &Path) -> Option<String> {
        let path = scope_dir.join(APPROVALS_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<HashMap<String, ApprovalRecord>>(&text) {
                Ok(_) => None,
                Err(e) => Some(format!(
                    "approval store {} is corrupt ({e}); repair or remove it by hand",
                    path.display()
                )),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => Some(format!(
                "approval store {} is unreadable ({e}); no plugin gated by it can load",
                path.display()
            )),
        }
    }

    /// Record operator approval for a plugin without checking the pending set.
    /// The caller must have shown [`PRIVILEGE_WARNING`] and obtained explicit
    /// consent first - this only persists the record.
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
            approved_at_ms: crate::logging::now_ms() as u64,
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
    /// Fail-closed visibility note: a plugin whose directory cannot be
    /// hashed (dangling symlink, unreadable dir) or whose manifest loader
    /// returns `None` is skipped here, so it appears in neither the
    /// pending list nor the approved set - it cannot be approved through
    /// this path until its directory is readable again.
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
    /// Errors when the plugin is not in the pending set - approval is consent
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
        fn corrupt_store_reads_empty_but_refuses_overwrite() {
            let scope = test_dir("corrupt-store");
            std::fs::write(scope.join(".approvals.json"), "{not json").unwrap();
            let mut store = ApprovalStore::open(&scope);
            assert!(store.is_corrupt(), "corrupt file must be flagged");
            assert!(!store.is_approved("anything", "1", "abc"));
            let rec = ApprovalRecord {
                plugin: "x".to_string(),
                version: "1".to_string(),
                dir_hash: "abc".to_string(),
                approved_at_ms: 0,
            };
            let err = store
                .record(rec)
                .expect_err("save must refuse a corrupt store");
            assert!(err.to_string().contains("corrupt"), "got: {err}");
            // The corrupt file itself is untouched by the refused save.
            assert_eq!(
                std::fs::read_to_string(scope.join(".approvals.json")).unwrap(),
                "{not json"
            );
        }

        #[test]
        fn stale_store_record_does_not_clobber_a_newer_record() {
            // Two handles opened at the same time (two processes, or two
            // call sites): the second record must re-read under the write
            // lock instead of overwriting the first record with its stale
            // at-open view.
            let scope = test_dir("stale-record");
            let mut a = ApprovalStore::open(&scope);
            let mut b = ApprovalStore::open(&scope);
            a.record(ApprovalRecord {
                plugin: "one".to_string(),
                version: "1".to_string(),
                dir_hash: "aaa".to_string(),
                approved_at_ms: 0,
            })
            .unwrap();
            b.record(ApprovalRecord {
                plugin: "two".to_string(),
                version: "1".to_string(),
                dir_hash: "bbb".to_string(),
                approved_at_ms: 0,
            })
            .unwrap();
            let after = ApprovalStore::open(&scope);
            assert!(
                after.is_approved("one", "1", "aaa"),
                "first record must survive the second record: {:?}",
                after.names()
            );
            assert!(after.is_approved("two", "1", "bbb"));
            let _ = std::fs::remove_dir_all(&scope);
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
}

pub mod capability {
    //! Capability system (§9). Security backbone: granular capabilities gated by
    //! policy, never bare `coder = yes`.

    use serde::{Deserialize, Serialize};
    use std::collections::HashSet;

    /// Granular capability tokens.
    #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
    pub enum Capability {
        FilesystemRead,
        FilesystemWrite,
        ShellExecute,
        GitRead,
        GitWrite,
        GitPush,
        NetworkOutbound,
        Browser,
        /// A low-confidence autonomous browser action (`browser_act` in
        /// pantheon-web's `browser` module). Split from Browser because clicking the top
        /// intent candidate is the one browser operation that acts on the
        /// model's behalf without a verified target: default policies mark
        /// this Approval so the run loop parks for a human before it runs.
        /// Mirrors the GitPush split from ShellExecute.
        BrowserAct,
        /// Filling a saved website-login credential into the browser
        /// (`browser_fill_login` in pantheon-web's `browser` module). Split
        /// from Browser because it touches the user's credential vault: the
        /// model never sees the secret (the tool result reports only which
        /// fields were filled), but the *decision* to fill is the user's
        /// default policies mark this Approval so the run loop parks for a
        /// human before it runs. Mirrors the BrowserAct split.
        BrowserFillLogin,
        /// Desktop control through the CUA driver (`cua-driver`). Its own
        /// capability because it drives the user's real desktop: default
        /// policies mark this Approval so the run loop parks for a human
        /// before it acts.
        ComputerUse,
        /// Executing or dynamically tracing a binary under analysis (REA's
        /// `trace_*` / `capture_*` tools). Its own capability because running
        /// the analyzed program is categorically different from reading it:
        /// the target may be untrusted or malicious, and MCP server processes
        /// currently run unsandboxed with the user's privileges. Default
        /// policies mark this Approval so the run loop parks for a human
        /// before it acts. Mirrors the ComputerUse split precedent.
        BinaryTrace,
        MessageSend(String),
        MemoryRead,
        MemoryWrite,
        /// Promoting a record's trust tier (memory_confirm). Split from
        /// MemoryWrite because confirming is the user-vouch path: a model
        /// that may propose records must not be able to confirm its own
        /// into the trusted tier. Default policies mark this Approval so
        /// the run loop parks for a human before the promotion runs.
        MemoryConfirm,
        SecretsUse,
        AgentSpawn,
        /// Leaving a message for another agent profile through the collaboration
        /// mailbox (`agent_message` / `agent_inbox` tools). Split from
        /// [`Capability::AgentSpawn`] because leaving a note for a peer is not
        /// spawning a child: a profile that may coordinate with peers must not
        /// necessarily be able to run a sub-agent in-process. Registered under
        /// the same Delegation tool group as `delegate`, so one toggle governs
        /// agent-to-agent coordination.
        AgentMessage,
        /// Enabling a bundled plugin (`enable_plugin` tool). Split out so the
        /// agent can propose it but never self-authorize: default policies mark
        /// this Approval, so the run parks for a human before any plugin's
        /// code is switched on. Only bundled-catalog plugins are toggleable
        /// this way - there is no agent path to install arbitrary plugins.
        PluginEnable,
        /// Enabling a bundled MCP server (`enable_mcp` tool). The MCP twin
        /// of [`Capability::PluginEnable`]: default policies mark this
        /// Approval, so the run parks for a human before any server's tools
        /// are projected into the registry. Only bundled-catalog servers are
        /// toggleable this way - there is no agent path to put an arbitrary
        /// command on the spawn line (the supply-chain boundary).
        McpEnable,
        /// A tool projected from an MCP server that has no explicit
        /// classification (no per-tool/per-server override, no built-in
        /// inventory entry). Split from [`Capability::NetworkOutbound`]
        /// because the old default let any unclassified third-party tool -
        /// including one that deletes, writes, or sends - ride an allowed
        /// capability under the coder policy, leaving server approval as the
        /// only gate. Default policies mark this Approval, so the run parks
        /// for a human before an unclassified tool runs. Operators classify a
        /// server or tool once via `McpCapabilityOverrides` to allow it.
        McpUnclassified,
        Other(String),
    }

    /// Tool name for the review-verdict tool. Registered only on reviewer
    /// runs in staged review stages (see `pantheon_runtime::swarm_exec`):
    /// the reviewer emits its verdict as a tool call and the orchestrator
    /// extracts the structured args from the ledger, instead of parsing the
    /// reviewer's prose.
    pub const VERDICT_TOOL_NAME: &str = "verdict";

    /// A policy decision for one capability.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub enum Decision {
        Allow,
        Deny,
        Approval,
    }

    impl Capability {
        /// Stable, log-safe token for a capability, e.g. `shell.execute`.
        /// Used for approval scopes and events, so it never carries payload
        /// values: a `MessageSend` token is the channel class, not the message.
        /// Single home for the token table - sandbox labels and MCP parsing
        /// both delegate here so a new variant touches one match.
        pub fn token(&self) -> String {
            match self {
                Capability::FilesystemRead => "filesystem.read".to_string(),
                Capability::FilesystemWrite => "filesystem.write".to_string(),
                Capability::ShellExecute => "shell.execute".to_string(),
                Capability::GitRead => "git.read".to_string(),
                Capability::GitWrite => "git.write".to_string(),
                Capability::GitPush => "git.push".to_string(),
                Capability::NetworkOutbound => "network.outbound".to_string(),
                Capability::Browser => "browser".to_string(),
                Capability::BrowserAct => "browser.act".to_string(),
                Capability::BrowserFillLogin => "browser.fill_login".to_string(),
                Capability::ComputerUse => "computer.use".to_string(),
                Capability::BinaryTrace => "binary.trace".to_string(),
                Capability::MessageSend(_) => "message.send".to_string(),
                Capability::MemoryRead => "memory.read".to_string(),
                Capability::MemoryWrite => "memory.write".to_string(),
                Capability::MemoryConfirm => "memory.confirm".to_string(),
                Capability::SecretsUse => "secrets.use".to_string(),
                Capability::AgentSpawn => "agent.spawn".to_string(),
                Capability::AgentMessage => "agent.message".to_string(),
                Capability::PluginEnable => "plugin.enable".to_string(),
                Capability::McpEnable => "mcp.enable".to_string(),
                Capability::McpUnclassified => "mcp.unclassified".to_string(),
                // Space is percent-encoded, not collapsed to '.': without
                // this `Other("a b")` and `Other("a.b")` produced the same
                // token. '%' is escaped first so a literal "%20" in a name
                // survives the round trip.
                Capability::Other(name) => format!(
                    "other.{}",
                    name.trim().replace('%', "%25").replace(' ', "%20")
                ),
            }
        }

        /// Map a policy token string to a capability. Unknown tokens become
        /// `Other(name)` so policy can still gate them explicitly.
        /// Inverse of [`Capability::token`] for the fixed variants.
        pub fn from_token(token: &str) -> Capability {
            match token.trim() {
                "filesystem.read" => Capability::FilesystemRead,
                "filesystem.write" => Capability::FilesystemWrite,
                "shell.execute" => Capability::ShellExecute,
                "git.read" => Capability::GitRead,
                "git.write" => Capability::GitWrite,
                "git.push" => Capability::GitPush,
                "network.outbound" => Capability::NetworkOutbound,
                "message.send" => Capability::MessageSend(String::new()),
                "browser" => Capability::Browser,
                "browser.act" => Capability::BrowserAct,
                "browser.fill_login" => Capability::BrowserFillLogin,
                "computer.use" => Capability::ComputerUse,
                "binary.trace" => Capability::BinaryTrace,
                "memory.read" => Capability::MemoryRead,
                "memory.write" => Capability::MemoryWrite,
                "memory.confirm" => Capability::MemoryConfirm,
                "secrets.use" => Capability::SecretsUse,
                "agent.spawn" => Capability::AgentSpawn,
                "agent.message" => Capability::AgentMessage,
                "plugin.enable" => Capability::PluginEnable,
                "mcp.enable" => Capability::McpEnable,
                "mcp.unclassified" => Capability::McpUnclassified,
                // Strip the "other." prefix once: `Other("a.b").token()` is
                // "other.a.b", so parsing it must yield the same capability,
                // not `Other("other.a.b")`. Decode the token escaping above.
                other => Capability::Other(
                    other
                        .strip_prefix("other.")
                        .unwrap_or(other)
                        .replace("%20", " ")
                        .replace("%25", "%"),
                ),
            }
        }
    }

    /// Policy: capability -> decision, default-deny.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct Policy {
        rules: std::collections::HashMap<Capability, Decision>,
    }

    impl Policy {
        pub fn allow(mut self, cap: Capability) -> Self {
            self.rules.insert(cap, Decision::Allow);
            self
        }

        pub fn approval(mut self, cap: Capability) -> Self {
            self.rules.insert(cap, Decision::Approval);
            self
        }

        pub fn deny(mut self, cap: Capability) -> Self {
            self.rules.insert(cap, Decision::Deny);
            self
        }

        pub fn check(&self, cap: &Capability) -> Decision {
            self.rules.get(cap).copied().unwrap_or(Decision::Deny)
        }

        /// Coder preset: read/write/exec/git, push needs approval. Browser and
        /// outbound network are allowed (web access is a core assistant
        /// capability); autonomous low-confidence browser acts, filling saved
        /// login credentials, desktop control through the CUA driver,
        /// executing/tracing binaries under analysis, and enabling bundled
        /// plugins need approval.
        /// The `todo` planning tool is allowed: it only mutates the run's own
        /// in-memory task list, persisted to the ledger as a transcript event.
        pub fn coder() -> Self {
            Self::default()
                .allow(Capability::FilesystemRead)
                .allow(Capability::FilesystemWrite)
                .allow(Capability::ShellExecute)
                .allow(Capability::GitRead)
                .allow(Capability::GitWrite)
                .approval(Capability::GitPush)
                .allow(Capability::NetworkOutbound)
                .allow(Capability::Browser)
                .approval(Capability::BrowserAct)
                .approval(Capability::BrowserFillLogin)
                .approval(Capability::ComputerUse)
                .approval(Capability::BinaryTrace)
                .approval(Capability::PluginEnable)
                .approval(Capability::McpEnable)
                // Unclassified MCP tools park: the old NetworkOutbound default
                // let an unknown third-party tool (delete/write/send) run on an
                // allowed capability with only server approval as the gate.
                .approval(Capability::McpUnclassified)
                // Cloudflare CLI: reads ride ShellExecute; anything that
                // creates/modifies infra or can destroy it (DNS, WAF, access
                // policy, tokens) parks for a human. Classification lives in
                // pantheon-exec::cloudflare; the tokens are strings so the
                // classifier can evolve without a Capability variant per verb.
                .approval(Capability::Other("cloudflare.write".into()))
                .approval(Capability::Other("cloudflare.destroy".into()))
                .allow(Capability::MemoryRead)
                .allow(Capability::AgentSpawn)
                .allow(Capability::AgentMessage)
                .allow(Capability::Other(crate::todo::TODO_TOOL_NAME.into()))
                .allow(Capability::Other(VERDICT_TOOL_NAME.into()))
        }

        /// Coder preset plus the memory write capability. Confirming
        /// (promoting a record's trust tier) is a separate capability and
        /// needs human approval: the model may propose, but only a user
        /// vouches a record into the trusted tier.
        pub fn coder_with_memory() -> Self {
            Self::coder()
                .allow(Capability::MemoryWrite)
                .approval(Capability::MemoryConfirm)
        }

        /// Read-only researcher preset.
        pub fn researcher_readonly() -> Self {
            Self::default()
                .allow(Capability::FilesystemRead)
                .allow(Capability::MemoryRead)
        }

        pub fn granted(&self) -> HashSet<Capability> {
            self.rules
                .iter()
                .filter(|(_, d)| **d == Decision::Allow)
                .map(|(c, _)| c.clone())
                .collect()
        }

        /// Capabilities parked on approval. Resume treats scopes recorded in
        /// ApprovalGranted events as one-shot allows for the matching call.
        pub fn approval_caps(&self) -> HashSet<Capability> {
            self.rules
                .iter()
                .filter(|(_, d)| **d == Decision::Approval)
                .map(|(c, _)| c.clone())
                .collect()
        }
    }

    #[cfg(test)]
    mod binary_trace_tests {
        use super::*;

        #[test]
        fn token_round_trip() {
            assert_eq!(Capability::BinaryTrace.token(), "binary.trace");
            assert_eq!(
                Capability::from_token("binary.trace"),
                Capability::BinaryTrace
            );
        }

        #[test]
        fn coder_policy_parks_binary_trace() {
            // Tracing tools must park for human approval under the default
            // coder policy, following the ComputerUse precedent.
            assert_eq!(
                Policy::coder().check(&Capability::BinaryTrace),
                Decision::Approval
            );
            assert!(Policy::coder()
                .approval_caps()
                .contains(&Capability::BinaryTrace));
        }

        #[test]
        fn coder_with_memory_inherits_parking() {
            assert_eq!(
                Policy::coder_with_memory().check(&Capability::BinaryTrace),
                Decision::Approval
            );
        }

        #[test]
        fn default_policy_denies_unlisted() {
            // Fail-closed: a policy that never mentions BinaryTrace denies it.
            assert_eq!(
                Policy::default().check(&Capability::BinaryTrace),
                Decision::Deny
            );
        }

        #[test]
        fn static_analysis_capability_is_allowed() {
            // REA static tools project as FilesystemRead: read-only, no new
            // privilege beyond the agent's own file reads.
            assert_eq!(
                Policy::coder().check(&Capability::FilesystemRead),
                Decision::Allow
            );
        }

        #[test]
        fn message_send_token_round_trip() {
            // `token()` is the channel class, not the message, so the
            // payload is not recoverable; the round trip must at least
            // land back on `MessageSend`, not `Other("message.send")`.
            assert_eq!(Capability::MessageSend("hi".into()).token(), "message.send");
            assert!(matches!(
                Capability::from_token("message.send"),
                Capability::MessageSend(_)
            ));
        }

        #[test]
        fn other_token_round_trip_distinguishes_space_and_dot() {
            assert_ne!(
                Capability::Other("a b".into()).token(),
                Capability::Other("a.b".into()).token()
            );
            for name in ["a b", "a.b", "cloudflare.write", "a%20b"] {
                let cap = Capability::Other(name.into());
                assert_eq!(Capability::from_token(&cap.token()), cap, "for {name}");
            }
            // Legacy tokens without the double prefix also parse.
            assert_eq!(
                Capability::from_token("other.cloudflare.write"),
                Capability::Other("cloudflare.write".into())
            );
        }

        #[test]
        fn agent_message_token_round_trip() {
            assert_eq!(Capability::AgentMessage.token(), "agent.message");
            assert_eq!(
                Capability::from_token("agent.message"),
                Capability::AgentMessage
            );
        }

        #[test]
        fn coder_policy_allows_agent_message() {
            // Messaging a peer is a coordination capability, and the coder
            // preset grants it (as it already grants AgentSpawn for
            // delegation). The runtime still requires an attached profile and
            // the Delegation tool group before registering the tools.
            assert_eq!(
                Policy::coder().check(&Capability::AgentMessage),
                Decision::Allow
            );
        }

        #[test]
        fn default_policy_denies_unlisted_agent_message() {
            // Fail-closed: a policy that never mentions AgentMessage denies it.
            assert_eq!(
                Policy::default().check(&Capability::AgentMessage),
                Decision::Deny
            );
        }
    }
}

pub mod permission_mode {
    //! Permission modes: how much a tool call is trusted before it runs.
    //!
    //! This is a separate axis from [`crate::mode::AgentMode`]. AgentMode asks
    //! *what kind of work* the turn is doing (Plan vs Build) and refuses
    //! mutating tools outright. PermissionMode asks *how much scrutiny* each
    //! allowed call gets, and it can only ever relax or tighten the gate
    //! around calls the policy already permits:
    //!
    //! - [`PermissionMode::Ask`]: never auto-approve. Every capability the
    //!   policy marks `Approval` parks for a human, and the judge is not
    //!   consulted. This is the conservative default: a run parks exactly
    //!   where the deterministic policy says it should.
    //! - [`PermissionMode::Smart`]: the judge decides. When the judge returns
    //!   `Allow` for a call the policy marked `Approval`, the call runs
    //!   without parking. The judge may only escalate, never relax past the
    //!   policy: a `Deny` stays denied and a judge failure falls back to
    //!   parking.
    //! - [`PermissionMode::AllowAll`]: auto-approve every `Approval`-marked
    //!   capability. `Deny` is still `Deny` - this mode cannot grant what the
    //!   policy refuses, so a read-only preset stays read-only.
    //!
    //! The escalation ladder is the safety property: policy is the floor, the
    //! judge may move up from it, and no mode moves below it. AllowAll is the
    //! one mode that removes the human from the loop for approval-gated work,
    //! which is why it is never the default and why `Deny` remains absolute.

    use serde::{Deserialize, Serialize};

    use crate::capability::Decision;
    use crate::model::GateVerdict;

    /// How much scrutiny an approval-gated tool call gets.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub enum PermissionMode {
        /// Park for a human on every approval-gated capability.
        #[default]
        Ask,
        /// Let the judge auto-approve low-risk calls; park the rest.
        Smart,
        /// Auto-approve every approval-gated capability. `Deny` still denies.
        AllowAll,
    }

    impl PermissionMode {
        /// Stable string form (`"ask"` / `"smart"` / `"allow_all"`).
        pub fn as_str(self) -> &'static str {
            match self {
                PermissionMode::Ask => "ask",
                PermissionMode::Smart => "smart",
                PermissionMode::AllowAll => "allow_all",
            }
        }

        /// Parse [`PermissionMode::as_str`] output (case-insensitive), plus
        /// the spellings a user is likely to type for each mode.
        pub fn parse(s: &str) -> Option<Self> {
            match s.trim().to_lowercase().replace('-', "_").as_str() {
                "ask" | "manual" | "default" => Some(PermissionMode::Ask),
                "smart" | "auto" | "judge" => Some(PermissionMode::Smart),
                "allow_all" | "allowall" | "yolo" | "bypass" | "all" => {
                    Some(PermissionMode::AllowAll)
                }
                _ => None,
            }
        }

        /// One-line description for the UI.
        pub fn describe(self) -> &'static str {
            match self {
                PermissionMode::Ask => "ask: park on every approval-gated call",
                PermissionMode::Smart => "smart: the judge auto-approves low-risk calls",
                PermissionMode::AllowAll => "allow_all: auto-approve everything the policy permits",
            }
        }

        /// Resolve one approval-gated capability to a final decision.
        ///
        /// `policy_decision` is what the deterministic host policy said. This
        /// function may only relax an `Approval` into `Allow` (that is what the
        /// modes are for) and may never touch a `Deny`: no mode can grant a
        /// capability the policy refuses, so a read-only preset stays read-only
        /// in every mode.
        ///
        /// `judge_verdict` is the auxiliary judge's opinion, already produced
        /// through `AgentLoop::consult_gate_advisory` (which enforces the
        /// escalate-only ladder and falls back to `None` on any judge failure).
        /// It is only consulted in `Smart`. A `None` there means "the judge
        /// could not help", which must park rather than auto-approve.
        pub fn resolve(
            self,
            policy_decision: Decision,
            judge_verdict: Option<GateVerdict>,
        ) -> Decision {
            match policy_decision {
                // Absolute. Not even AllowAll may widen the policy.
                Decision::Deny => Decision::Deny,
                // Nothing to do: it already runs unattended.
                Decision::Allow => Decision::Allow,
                Decision::Approval => match self {
                    PermissionMode::Ask => Decision::Approval,
                    PermissionMode::AllowAll => Decision::Allow,
                    PermissionMode::Smart => match judge_verdict {
                        // The judge cleared it: run without parking.
                        Some(GateVerdict::Allow) => Decision::Allow,
                        // The judge wants a human, or the judge failed and the
                        // caller passed None. Both park.
                        Some(GateVerdict::NeedsApproval { .. }) | None => Decision::Approval,
                        // The judge escalated past approval to a hard deny.
                        // Honor it: the judge may move up the ladder, and Deny
                        // is up from Approval.
                        Some(GateVerdict::Deny { .. }) => Decision::Deny,
                    },
                },
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn round_trips_through_its_own_string_form() {
            for mode in [
                PermissionMode::Ask,
                PermissionMode::Smart,
                PermissionMode::AllowAll,
            ] {
                assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
            }
        }

        #[test]
        fn accepts_the_spellings_a_user_would_type() {
            assert_eq!(PermissionMode::parse("MANUAL"), Some(PermissionMode::Ask));
            assert_eq!(
                PermissionMode::parse("  Judge "),
                Some(PermissionMode::Smart)
            );
            assert_eq!(
                PermissionMode::parse("allow-all"),
                Some(PermissionMode::AllowAll)
            );
            assert_eq!(
                PermissionMode::parse("yolo"),
                Some(PermissionMode::AllowAll)
            );
        }

        /// An unknown string must not silently become the most permissive
        /// mode. `None` forces the caller to decide, and every caller
        /// defaults to Ask.
        #[test]
        fn unknown_string_is_not_a_mode() {
            assert_eq!(PermissionMode::parse("whatever"), None);
            assert_eq!(PermissionMode::default(), PermissionMode::Ask);
        }

        /// The core safety property: Deny is absolute in every mode, and
        /// Allow is never downgraded. A read-only policy must stay read-only
        /// even under AllowAll.
        #[test]
        fn no_mode_escapes_a_policy_deny() {
            for mode in [
                PermissionMode::Ask,
                PermissionMode::Smart,
                PermissionMode::AllowAll,
            ] {
                assert_eq!(mode.resolve(Decision::Deny, None), Decision::Deny);
                // Even a judge that says "allow" cannot lift a policy deny.
                assert_eq!(
                    mode.resolve(Decision::Deny, Some(GateVerdict::Allow)),
                    Decision::Deny
                );
                // Allow stays Allow: no mode parks something already permitted.
                assert_eq!(mode.resolve(Decision::Allow, None), Decision::Allow);
            }
        }

        /// Ask is the conservative default: it parks on Approval and never
        /// consults the judge, so an unconfigured run behaves exactly as the
        /// deterministic policy alone would.
        #[test]
        fn ask_always_parks_and_ignores_the_judge() {
            assert_eq!(
                PermissionMode::Ask.resolve(Decision::Approval, None),
                Decision::Approval
            );
            assert_eq!(
                PermissionMode::Ask.resolve(Decision::Approval, Some(GateVerdict::Allow)),
                Decision::Approval
            );
        }

        #[test]
        fn allow_all_clears_approval_without_a_judge() {
            assert_eq!(
                PermissionMode::AllowAll.resolve(Decision::Approval, None),
                Decision::Allow
            );
        }

        /// Smart is the judge-backed mode. Only an explicit judge Allow runs
        /// unattended; a missing judge (None) must park, because "the judge
        /// could not be reached" is not the same as "the judge said yes".
        #[test]
        fn smart_runs_only_on_an_explicit_judge_allow() {
            assert_eq!(
                PermissionMode::Smart.resolve(Decision::Approval, Some(GateVerdict::Allow)),
                Decision::Allow
            );
            assert_eq!(
                PermissionMode::Smart.resolve(Decision::Approval, None),
                Decision::Approval,
                "a judge failure must park, never auto-approve"
            );
            assert_eq!(
                PermissionMode::Smart.resolve(
                    Decision::Approval,
                    Some(GateVerdict::NeedsApproval {
                        reason: "looks risky".into()
                    })
                ),
                Decision::Approval
            );
        }

        /// The judge may escalate past Approval to Deny; that direction is
        /// honored rather than folded back into a park.
        #[test]
        fn smart_honors_a_judge_escalation_to_deny() {
            assert_eq!(
                PermissionMode::Smart.resolve(
                    Decision::Approval,
                    Some(GateVerdict::Deny {
                        reason: "exfiltrates credentials".into()
                    })
                ),
                Decision::Deny
            );
        }
    }
}
