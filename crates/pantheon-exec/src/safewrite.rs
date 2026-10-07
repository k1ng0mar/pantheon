//! Safe file mutation: preview, staged writes, checkpoints, atomic apply,
//! stale-edit detection, rollback by checkpoint or ledger sequence.
//!
//! Recovery ideas (original code): checkpoint-before-mutation, hash-chained
//! journal, atomic publish (tmp+fsync+rename), startup replay of torn
//! applies. No model calls here; capability gating happens before tools run.
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::confine::confine;
/// Serialize a tool result to pretty JSON, mapping failure to a
/// PantheonError. A report that cannot serialize is still a failure worth
/// reporting, not a reason to panic over whatever it was describing.
pub fn json_out<T: serde::Serialize>(v: &T) -> Result<String, PantheonError> {
    serde_json::to_string_pretty(v).map_err(|e| serr("SAFE_WRITE_JSON", e.to_string()))
}

/// Structured error for the safe-write tool surface. `pub` because the
/// tool registration half of this module lives in `pantheon-tools`.
pub fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "re-run preview, re-stage after re-reading the file, or roll back to a checkpoint",
        "",
    )
}
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
pub fn fnv1a_hex(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}
fn uniq() -> String {
    // Millisecond timestamps alone collide under rapid successive calls
    // (two checkpoints created in the same ms would share an ID and
    // overwrite each other's manifests), so mix in a process-wide counter.
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let ms = now_ms() as u64;
    let n = CTR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{ms}_{:04}_{n}",
        (ms.wrapping_add(std::process::id() as u64) >> 7) % 10000
    )
}
/// Content fingerprint: hash + length + existence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub hash: String,
    pub len: u64,
    pub existed: bool,
}
/// Fingerprint whatever is on disk. Missing file => existed=false.
pub fn fingerprint_of(path: &Path) -> Result<Fingerprint, PantheonError> {
    match std::fs::read(path) {
        Ok(b) => Ok(Fingerprint {
            hash: fnv1a_hex(&b),
            len: b.len() as u64,
            existed: true,
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Fingerprint {
            hash: fnv1a_hex(&[]),
            len: 0,
            existed: false,
        }),
        Err(e) => Err(serr(
            "SAFE_READ",
            format!("fingerprint {}: {e}", path.display()),
        )),
    }
}
/// One pending file mutation with an optional staleness guard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEdit {
    pub path: PathBuf,
    pub new_content: Vec<u8>,
    pub expected_hash: Option<String>,
}
/// Read-only preview of what an edit would do. Never touches disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Preview {
    pub path: PathBuf,
    pub existed: bool,
    pub before_hash: String,
    pub after_hash: String,
    pub before_lines: usize,
    pub after_lines: usize,
    pub changed: bool,
    pub excerpt: String,
}
/// Build a preview for one edit: current bytes vs proposed bytes.
pub fn preview_edit(path: &Path, new_content: &[u8]) -> Result<Preview, PantheonError> {
    let before = fingerprint_of(path)?;
    let after_hash = fnv1a_hex(new_content);
    let before_lines = if before.existed {
        std::fs::read_to_string(path)
            .map(|t| t.lines().count())
            .unwrap_or(0)
    } else {
        0
    };
    let proposed = String::from_utf8_lossy(new_content);
    Ok(Preview {
        path: path.to_path_buf(),
        existed: before.existed,
        changed: before.hash != after_hash || !before.existed,
        before_hash: before.hash,
        after_hash,
        before_lines,
        after_lines: proposed.lines().count(),
        excerpt: proposed.lines().take(40).collect::<Vec<_>>().join("\n"),
    })
}
/// Write bytes atomically: tmp file in target dir + fsync + rename.
///
/// The tmp file is created with `O_NOFOLLOW|O_CREAT|O_EXCL` (unix): a
/// pre-planted symlink at the tmp name fails the open instead of redirecting
/// the write elsewhere. Rename-over replaces a destination symlink rather
/// than following it, so a swapped final component cannot redirect the
/// publish either. Callers confine `path` first (see [`SafeWriter`]).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), PantheonError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| serr("SAFE_MKDIR", format!("mkdir {}: {e}", parent.display())))?;
        }
    }
    let mut tmp_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    tmp_name.push_str(&format!(".tmp.{}.{}", std::process::id(), uniq()));
    let tmp = path.with_file_name(tmp_name);
    let mut f = open_tmp_nofollow(&tmp)
        .map_err(|e| serr("SAFE_WRITE", format!("tmp create {}: {e}", tmp.display())))?;
    {
        use std::io::Write;
        f.write_all(bytes)
            .map_err(|e| serr("SAFE_WRITE", format!("tmp write {}: {e}", tmp.display())))?;
    }
    let _ = f.sync_all();
    std::fs::rename(&tmp, path)
        .map_err(|e| serr("SAFE_RENAME", format!("publish {}: {e}", path.display())))?;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            if let Ok(d) = std::fs::File::open(parent) {
                let _ = d.sync_all();
            }
        }
    }
    Ok(())
}

/// Open a fresh file for writing without following a final-component
/// symlink. With `O_CREAT|O_EXCL|O_NOFOLLOW` a symlink at `path` fails with
/// `EEXIST` instead of being traversed (Linux); the non-unix fallback keeps
/// `O_EXCL` so a planted name still fails rather than being clobbered.
#[cfg(unix)]
fn open_tmp_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}
#[cfg(not(unix))]
fn open_tmp_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}
/// One file captured inside a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointFile {
    pub path: PathBuf,
    pub existed: bool,
    pub hash: String,
    pub blob: String,
}
/// Checkpoint manifest: pre-images + ledger anchor + hash-chain link.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: String,
    pub created_ms: i64,
    pub ledger_seq: i64,
    pub prev: Option<String>,
    pub chain_hash: String,
    pub files: Vec<CheckpointFile>,
}
/// Staged batch manifest: proposed blobs + staleness baselines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedBatch {
    pub id: String,
    pub created_ms: i64,
    pub files: Vec<StagedFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedFile {
    pub path: PathBuf,
    pub expected_hash: Option<String>,
    pub staged_hash: String,
    pub blob: String,
    pub len: u64,
}
/// Receipt for one atomic apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyReceipt {
    pub stage_id: Option<String>,
    pub checkpoint_id: String,
    pub ledger_seq: i64,
    pub files: Vec<AppliedFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppliedFile {
    pub path: PathBuf,
    pub before_hash: String,
    pub after_hash: String,
}
/// Journal entry: every mutation step is appended before it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JournalEntry {
    kind: String,
    id: String,
    checkpoint: Option<String>,
    ledger_seq: i64,
    paths: Vec<PathBuf>,
    prev_hash: String,
    hash: String,
}
/// Workspace-scoped safe writer. Layout under state_dir:
/// checkpoints/<id>/manifest.json + blobs/, staging/<id>/, safewrite.jsonl.
///
/// When a workspace root is set (via [`SafeWriter::with_workspace_root`]),
/// every target path is confined with `crate::confine` before any read or
/// write - deny globs first, then containment. The tool layer always sets
/// it; direct engine users should too.
#[derive(Debug, Clone)]
pub struct SafeWriter {
    state_dir: PathBuf,
    checkpoints_dir: PathBuf,
    staging_dir: PathBuf,
    journal: PathBuf,
    workspace_root: Option<PathBuf>,
}
impl SafeWriter {
    pub fn new(state_dir: PathBuf) -> Result<Self, PantheonError> {
        let w = Self {
            checkpoints_dir: state_dir.join("checkpoints"),
            staging_dir: state_dir.join("staging"),
            journal: state_dir.join("safewrite.jsonl"),
            state_dir,
            workspace_root: None,
        };
        std::fs::create_dir_all(&w.checkpoints_dir)
            .map_err(|e| serr("SAFE_MKDIR", format!("checkpoints dir: {e}")))?;
        std::fs::create_dir_all(&w.staging_dir)
            .map_err(|e| serr("SAFE_MKDIR", format!("staging dir: {e}")))?;
        w.recover()?;
        Ok(w)
    }
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }
    /// Confine every target path through `crate::confine`. Returns the
    /// canonical paths, which the caller must use for IO instead of the
    /// originals so the check and the use agree.
    pub fn with_workspace_root(mut self, root: PathBuf) -> Self {
        self.workspace_root = Some(root);
        self
    }
    fn confine_all(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>, PantheonError> {
        match &self.workspace_root {
            Some(root) => paths.iter().map(|p| confine(p, root)).collect(),
            None => Ok(paths.to_vec()),
        }
    }
    /// Stage/checkpoint ids are generated (`ckpt_`/`stage_` + digits); ids
    /// arriving from tool args must match the same charset so `..` cannot
    /// escape the checkpoints/staging dirs via path joins.
    fn valid_id(id: &str) -> Result<(), PantheonError> {
        let ok = !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if ok {
            Ok(())
        } else {
            Err(serr(
                "SAFE_BAD_ID",
                format!("invalid stage/checkpoint id {id:?}"),
            ))
        }
    }
    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.checkpoints_dir.join(id).join("manifest.json")
    }
    fn read_manifest(&self, id: &str) -> Result<Checkpoint, PantheonError> {
        Self::valid_id(id)?;
        let raw = std::fs::read_to_string(self.checkpoint_path(id))
            .map_err(|e| serr("SAFE_NO_CHECKPOINT", format!("checkpoint {id}: {e}")))?;
        serde_json::from_str(&raw)
            .map_err(|e| serr("SAFE_MANIFEST", format!("checkpoint {id}: {e}")))
    }
    fn list_checkpoint_ids(&self) -> Result<Vec<String>, PantheonError> {
        let mut ids: Vec<String> = vec![];
        let rd = std::fs::read_dir(&self.checkpoints_dir)
            .map_err(|e| serr("SAFE_READ", format!("list checkpoints: {e}")))?;
        for e in rd.flatten() {
            if e.path().join("manifest.json").exists() {
                if let Some(n) = e.file_name().to_str() {
                    ids.push(n.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
    pub fn list_checkpoints(&self) -> Result<Vec<Checkpoint>, PantheonError> {
        let mut out = vec![];
        for id in self.list_checkpoint_ids()? {
            out.push(self.read_manifest(&id)?);
        }
        Ok(out)
    }
    fn journal_tail_hash(&self) -> String {
        let Ok(raw) = std::fs::read_to_string(&self.journal) else {
            return "genesis".into();
        };
        let mut last = "genesis".to_string();
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_str::<JournalEntry>(line) {
                last = e.hash.clone();
            }
        }
        last
    }
    fn journal_append(
        &self,
        kind: &str,
        id: &str,
        checkpoint: Option<String>,
        ledger_seq: i64,
        paths: Vec<PathBuf>,
    ) -> Result<(), PantheonError> {
        let prev_hash = self.journal_tail_hash();
        let mut acc = prev_hash.as_bytes().to_vec();
        acc.extend_from_slice(kind.as_bytes());
        acc.extend_from_slice(id.as_bytes());
        acc.extend_from_slice(ledger_seq.to_string().as_bytes());
        for p in &paths {
            acc.extend_from_slice(p.to_string_lossy().as_bytes());
        }
        let hash = fnv1a_hex(&acc);
        let e = JournalEntry {
            kind: kind.into(),
            id: id.into(),
            checkpoint,
            ledger_seq,
            paths,
            prev_hash,
            hash,
        };
        let line = serde_json::to_string(&e).map_err(|e| serr("SAFE_JOURNAL", e.to_string()))?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.journal)
            .map_err(|e| serr("SAFE_JOURNAL", format!("open journal: {e}")))?;
        writeln!(f, "{line}").map_err(|e| serr("SAFE_JOURNAL", format!("write journal: {e}")))?;
        let _ = f.sync_all();
        Ok(())
    }
    /// Snapshot pre-images for paths, anchoring ledger_seq in the manifest.
    pub fn checkpoint(
        &self,
        paths: &[PathBuf],
        ledger_seq: i64,
    ) -> Result<Checkpoint, PantheonError> {
        let paths = self.confine_all(paths)?;
        let id = format!("ckpt_{}", uniq());
        let dir = self.checkpoints_dir.join(&id);
        std::fs::create_dir_all(dir.join("blobs"))
            .map_err(|e| serr("SAFE_MKDIR", format!("checkpoint dir: {e}")))?;
        let mut files = vec![];
        for (i, p) in paths.iter().enumerate() {
            let fp = fingerprint_of(p)?;
            let blob = if fp.existed {
                std::fs::read(p).unwrap_or_default()
            } else {
                vec![]
            };
            let name = format!("blob_{i}");
            std::fs::write(dir.join("blobs").join(&name), &blob)
                .map_err(|e| serr("SAFE_WRITE", format!("checkpoint blob: {e}")))?;
            files.push(CheckpointFile {
                path: p.clone(),
                existed: fp.existed,
                hash: fp.hash,
                blob: name,
            });
        }
        let prev = self.list_checkpoint_ids()?.pop();
        let mut acc = prev.clone().unwrap_or_default().as_bytes().to_vec();
        acc.extend_from_slice(id.as_bytes());
        acc.extend_from_slice(ledger_seq.to_string().as_bytes());
        for f in &files {
            acc.extend_from_slice(f.hash.as_bytes());
        }
        let cp = Checkpoint {
            id: id.clone(),
            created_ms: now_ms(),
            ledger_seq,
            prev,
            chain_hash: fnv1a_hex(&acc),
            files,
        };
        let raw =
            serde_json::to_string_pretty(&cp).map_err(|e| serr("SAFE_MANIFEST", e.to_string()))?;
        atomic_write(&self.checkpoint_path(&id), raw.as_bytes())?;
        let paths_vec: Vec<PathBuf> = cp.files.iter().map(|f| f.path.clone()).collect();
        self.journal_append("checkpoint", &id, None, ledger_seq, paths_vec)?;
        Ok(cp)
    }
    /// Stage a batch without touching targets. Captures expected_hash baselines.
    pub fn stage_edits(&self, edits: Vec<FileEdit>) -> Result<StagedBatch, PantheonError> {
        if edits.is_empty() {
            return Err(serr("SAFE_EMPTY", "no edits to stage".into()));
        }
        let mut edits = edits;
        let confined =
            self.confine_all(&edits.iter().map(|e| e.path.clone()).collect::<Vec<_>>())?;
        for (e, p) in edits.iter_mut().zip(confined) {
            e.path = p;
        }
        let id = format!("stage_{}", uniq());
        let dir = self.staging_dir.join(&id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| serr("SAFE_MKDIR", format!("staging dir: {e}")))?;
        let mut files = vec![];
        for (i, e) in edits.iter().enumerate() {
            let name = format!("file_{i}");
            atomic_write(&dir.join(&name), &e.new_content)?;
            files.push(StagedFile {
                path: e.path.clone(),
                expected_hash: e.expected_hash.clone(),
                staged_hash: fnv1a_hex(&e.new_content),
                blob: name,
                len: e.new_content.len() as u64,
            });
        }
        let batch = StagedBatch {
            id: id.clone(),
            created_ms: now_ms(),
            files,
        };
        let raw = serde_json::to_string_pretty(&batch)
            .map_err(|e| serr("SAFE_MANIFEST", e.to_string()))?;
        atomic_write(&dir.join("manifest.json"), raw.as_bytes())?;
        let paths_vec: Vec<PathBuf> = batch.files.iter().map(|f| f.path.clone()).collect();
        self.journal_append("stage", &id, None, -1, paths_vec)?;
        Ok(batch)
    }
    fn read_staged(&self, id: &str) -> Result<(StagedBatch, PathBuf), PantheonError> {
        Self::valid_id(id)?;
        let dir = self.staging_dir.join(id);
        let raw = std::fs::read_to_string(dir.join("manifest.json"))
            .map_err(|e| serr("SAFE_NO_STAGE", format!("stage {id}: {e}")))?;
        let b: StagedBatch = serde_json::from_str(&raw)
            .map_err(|e| serr("SAFE_MANIFEST", format!("stage {id}: {e}")))?;
        Ok((b, dir))
    }
    /// Atomically apply staged blobs. Validates stale edits first, snapshots
    /// an auto-checkpoint, journals begin, publishes all files, journals
    /// commit. On any failure the auto-checkpoint is restored.
    pub fn apply_staged(
        &self,
        stage_id: &str,
        ledger_seq: i64,
    ) -> Result<ApplyReceipt, PantheonError> {
        let (mut batch, dir) = self.read_staged(stage_id)?;
        // Staged paths come back off disk; re-confine defensively.
        let confined = self.confine_all(
            &batch
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>(),
        )?;
        for (f, p) in batch.files.iter_mut().zip(confined) {
            f.path = p;
        }
        let paths: Vec<PathBuf> = batch.files.iter().map(|f| f.path.clone()).collect();
        for f in &batch.files {
            if let Some(exp) = &f.expected_hash {
                let cur = fingerprint_of(&f.path)?;
                if &cur.hash != exp {
                    return Err(PantheonError::new(
                        "SAFE_STALE",
                        Layer::Execution,
                        false,
                        format!(
                            "stale edit on {}: expected {exp}, on-disk {}",
                            f.path.display(),
                            cur.hash
                        ),
                        "preview again to refresh expected_hash, then re-stage",
                        "",
                    ));
                }
            }
        }
        let cp = self.checkpoint(&paths, ledger_seq)?;
        self.journal_append(
            "begin",
            stage_id,
            Some(cp.id.clone()),
            ledger_seq,
            paths.clone(),
        )?;
        let mut applied = vec![];
        for f in &batch.files {
            let before = fingerprint_of(&f.path)?;
            let blob = std::fs::read(dir.join(&f.blob))
                .map_err(|e| serr("SAFE_READ", format!("staged blob {}: {e}", f.blob)))?;
            if let Err(e) = atomic_write(&f.path, &blob) {
                let _ = self.restore_checkpoint(&cp.id);
                return Err(e);
            }
            let after = fingerprint_of(&f.path)?;
            applied.push(AppliedFile {
                path: f.path.clone(),
                before_hash: before.hash,
                after_hash: after.hash,
            });
        }
        self.journal_append("commit", stage_id, Some(cp.id.clone()), ledger_seq, paths)?;
        Ok(ApplyReceipt {
            stage_id: Some(stage_id.into()),
            checkpoint_id: cp.id,
            ledger_seq,
            files: applied,
        })
    }
    /// Direct atomic apply without a named stage (validates, checkpoints, publishes).
    pub fn apply_edits(
        &self,
        edits: Vec<FileEdit>,
        ledger_seq: i64,
    ) -> Result<ApplyReceipt, PantheonError> {
        if edits.is_empty() {
            return Err(serr("SAFE_EMPTY", "no edits to apply".into()));
        }
        let mut edits = edits;
        let confined =
            self.confine_all(&edits.iter().map(|e| e.path.clone()).collect::<Vec<_>>())?;
        for (e, p) in edits.iter_mut().zip(confined) {
            e.path = p;
        }
        for e in &edits {
            if let Some(exp) = &e.expected_hash {
                let cur = fingerprint_of(&e.path)?;
                if &cur.hash != exp {
                    return Err(PantheonError::new(
                        "SAFE_STALE",
                        Layer::Execution,
                        false,
                        format!(
                            "stale edit on {}: expected {exp}, on-disk {}",
                            e.path.display(),
                            cur.hash
                        ),
                        "preview again to refresh expected_hash, then retry",
                        "",
                    ));
                }
            }
        }
        let paths: Vec<PathBuf> = edits.iter().map(|e| e.path.clone()).collect();
        let cp = self.checkpoint(&paths, ledger_seq)?;
        self.journal_append(
            "begin",
            "direct",
            Some(cp.id.clone()),
            ledger_seq,
            paths.clone(),
        )?;
        let mut applied = vec![];
        for e in &edits {
            let before = fingerprint_of(&e.path)?;
            if let Err(err) = atomic_write(&e.path, &e.new_content) {
                let _ = self.restore_checkpoint(&cp.id);
                return Err(err);
            }
            let after = fingerprint_of(&e.path)?;
            applied.push(AppliedFile {
                path: e.path.clone(),
                before_hash: before.hash,
                after_hash: after.hash,
            });
        }
        self.journal_append("commit", "direct", Some(cp.id.clone()), ledger_seq, paths)?;
        Ok(ApplyReceipt {
            stage_id: None,
            checkpoint_id: cp.id,
            ledger_seq,
            files: applied,
        })
    }
    /// Restore one checkpoint's pre-images (atomic per file).
    pub fn restore_checkpoint(&self, id: &str) -> Result<Vec<PathBuf>, PantheonError> {
        let cp = self.read_manifest(id)?;
        let dir = self.checkpoints_dir.join(id);
        let mut restored = vec![];
        let confined =
            self.confine_all(&cp.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>())?;
        for (f, target) in cp.files.iter().zip(confined) {
            let blob = std::fs::read(dir.join("blobs").join(&f.blob))
                .map_err(|e| serr("SAFE_READ", format!("checkpoint blob {}: {e}", f.blob)))?;
            if f.existed {
                atomic_write(&target, &blob)?;
            } else if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|e| serr("SAFE_WRITE", format!("remove {}: {e}", target.display())))?;
            }
            restored.push(target);
        }
        self.journal_append(
            "rollback",
            id,
            Some(id.into()),
            cp.ledger_seq,
            restored.clone(),
        )?;
        Ok(restored)
    }
    /// Roll back to the latest checkpoint with ledger_seq <= target.
    pub fn rollback_to_seq(&self, target: i64) -> Result<(String, Vec<PathBuf>), PantheonError> {
        let mut best: Option<Checkpoint> = None;
        for cp in self.list_checkpoints()? {
            if cp.ledger_seq <= target
                && best
                    .as_ref()
                    .map(|b| cp.ledger_seq > b.ledger_seq)
                    .unwrap_or(true)
            {
                best = Some(cp);
            }
        }
        let cp = best.ok_or_else(|| {
            serr(
                "SAFE_NO_CHECKPOINT",
                format!("no checkpoint at or before ledger seq {target}"),
            )
        })?;
        let id = cp.id.clone();
        let restored = self.restore_checkpoint(&id)?;
        Ok((id, restored))
    }
    /// Startup recovery: a begin with no matching commit means a torn apply;
    /// restore its checkpoint and journal the recovery. Hash-chain breaks in
    /// the journal are reported, never silently ignored.
    pub fn recover(&self) -> Result<Vec<String>, PantheonError> {
        let Ok(raw) = std::fs::read_to_string(&self.journal) else {
            return Ok(vec![]);
        };
        let mut entries: Vec<JournalEntry> = vec![];
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let e: JournalEntry = serde_json::from_str(line)
                .map_err(|e| serr("SAFE_JOURNAL", format!("corrupt journal line: {e}")))?;
            entries.push(e);
        }
        let mut expect_prev = "genesis".to_string();
        for e in &entries {
            if e.prev_hash != expect_prev {
                return Err(serr(
                    "SAFE_JOURNAL",
                    format!("journal chain break at {} {}", e.kind, e.id),
                ));
            }
            expect_prev = e.hash.clone();
        }
        let mut begins: Vec<&JournalEntry> = vec![];
        let mut commits: Vec<(&str, &Option<String>)> = vec![];
        for e in &entries {
            if e.kind == "begin" {
                begins.push(e);
            }
            if e.kind == "commit" || e.kind == "rollback" {
                commits.push((e.id.as_str(), &e.checkpoint));
            }
        }
        let mut recovered = vec![];
        for b in begins {
            let done = commits.iter().any(|(cid, _)| *cid == b.id.as_str());
            if done {
                continue;
            }
            if let Some(cp) = &b.checkpoint {
                if self.checkpoint_path(cp).exists() {
                    let dir = self.checkpoints_dir.join(cp);
                    if let Ok(raw) = std::fs::read_to_string(dir.join("manifest.json")) {
                        if let Ok(cpdata) = serde_json::from_str::<Checkpoint>(&raw) {
                            for f in &cpdata.files {
                                if let Ok(blob) = std::fs::read(dir.join("blobs").join(&f.blob)) {
                                    if f.existed {
                                        let _ = atomic_write(&f.path, &blob);
                                    } else if f.path.exists() {
                                        let _ = std::fs::remove_file(&f.path);
                                    }
                                }
                            }
                            recovered.push(cp.clone());
                        }
                    }
                }
            }
        }
        for id in &recovered {
            self.journal_append("recover", id, Some(id.clone()), -1, vec![])?;
        }
        Ok(recovered)
    }
}
