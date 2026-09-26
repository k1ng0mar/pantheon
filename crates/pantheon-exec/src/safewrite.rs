//! Safe file mutation: preview, staged writes, checkpoints, atomic apply,
//! stale-edit detection, rollback by checkpoint or ledger sequence.
//!
//! Recovery ideas (original code): checkpoint-before-mutation, hash-chained
//! journal, atomic publish (tmp+fsync+rename), startup replay of torn
//! applies. No model calls here; capability gating happens before tools run.
use crate::tools::{parse_args, ToolRegistry};
use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
fn serr(code: &str, cause: String) -> PantheonError {
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
fn fnv1a_hex(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}
fn uniq() -> String {
    let ms = now_ms() as u64;
    format!(
        "{ms}_{:04}",
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
    std::fs::write(&tmp, bytes)
        .map_err(|e| serr("SAFE_WRITE", format!("tmp write {}: {e}", tmp.display())))?;
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
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
#[derive(Debug, Clone)]
pub struct SafeWriter {
    state_dir: PathBuf,
    checkpoints_dir: PathBuf,
    staging_dir: PathBuf,
    journal: PathBuf,
}
impl SafeWriter {
    pub fn new(state_dir: PathBuf) -> Result<Self, PantheonError> {
        let w = Self {
            checkpoints_dir: state_dir.join("checkpoints"),
            staging_dir: state_dir.join("staging"),
            journal: state_dir.join("safewrite.jsonl"),
            state_dir,
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
    fn checkpoint_path(&self, id: &str) -> PathBuf {
        self.checkpoints_dir.join(id).join("manifest.json")
    }
    fn read_manifest(&self, id: &str) -> Result<Checkpoint, PantheonError> {
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
    /// Test-only seam: same as `journal_append` but callable from outside the
    /// module so torn-apply recovery tests can simulate a missing commit.
    #[cfg(test)]
    pub fn journal_append_for_test(
        &self,
        kind: &str,
        id: &str,
        checkpoint: Option<String>,
        ledger_seq: i64,
        paths: Vec<PathBuf>,
    ) -> Result<(), PantheonError> {
        self.journal_append(kind, id, checkpoint, ledger_seq, paths)
    }
    /// Snapshot pre-images for paths, anchoring ledger_seq in the manifest.
    pub fn checkpoint(
        &self,
        paths: &[PathBuf],
        ledger_seq: i64,
    ) -> Result<Checkpoint, PantheonError> {
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
        let (batch, dir) = self.read_staged(stage_id)?;
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
        for f in &cp.files {
            let blob = std::fs::read(dir.join("blobs").join(&f.blob))
                .map_err(|e| serr("SAFE_READ", format!("checkpoint blob {}: {e}", f.blob)))?;
            if f.existed {
                atomic_write(&f.path, &blob)?;
            } else if f.path.exists() {
                std::fs::remove_file(&f.path)
                    .map_err(|e| serr("SAFE_WRITE", format!("remove {}: {e}", f.path.display())))?;
            }
            restored.push(f.path.clone());
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

#[cfg(test)]
#[path = "safewrite_tests.rs"]
mod tests;
fn jstr(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}
fn tool_err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check tool arguments",
        "",
    )
}
fn parse_edit_list(v: &serde_json::Value) -> Result<Vec<FileEdit>, PantheonError> {
    let arr = v
        .get("edits")
        .and_then(|x| x.as_array())
        .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing array 'edits'".into()))?;
    let mut out = vec![];
    for item in arr {
        let path = item
            .get("path")
            .and_then(|x| x.as_str())
            .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "each edit needs string 'path'".into()))?;
        let content = item
            .get("content")
            .and_then(|x| x.as_str())
            .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "each edit needs string 'content'".into()))?;
        out.push(FileEdit {
            path: PathBuf::from(path),
            new_content: content.as_bytes().to_vec(),
            expected_hash: item
                .get("expected_hash")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        });
    }
    Ok(out)
}
fn state_dir_from(args: &serde_json::Value) -> Result<PathBuf, PantheonError> {
    if let Some(d) = jstr(args, "state_dir") {
        return Ok(PathBuf::from(d));
    }
    if let Ok(d) = std::env::var("PANTHEON_DATA_DIR") {
        return Ok(PathBuf::from(d).join("safewrite"));
    }
    Err(tool_err(
        "TOOL_BAD_ARGS",
        "missing 'state_dir' (or set PANTHEON_DATA_DIR)".into(),
    ))
}
/// Register the safe-write toolset on a registry. All mutating tools
/// snapshot a checkpoint first; preview/checkpoints/rollback never mutate targets.
/// Tools: preview_file, stage_files, apply_files, apply_staged,
/// checkpoint_files, list_checkpoints, rollback_checkpoint, rollback_seq.
pub fn register_safewrite(reg: &mut ToolRegistry, default_state_dir: PathBuf) {
    let dir: std::sync::Arc<PathBuf> = std::sync::Arc::new(default_state_dir);
    let mk = |name: &str, desc: &str, props: serde_json::Value, required: Vec<&str>| ToolSchema {
        name: name.into(),
        description: desc.into(),
        parameters: serde_json::json!({"type":"object","properties":props,"required":required}),
    };
    let d0 = dir.clone();
    reg.register(
        mk(
            "preview_file",
            "Preview a file write: hashes, line counts, 40-line excerpt. Read-only.",
            serde_json::json!({"path":{"type":"string"},"content":{"type":"string"}}),
            vec!["path", "content"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let path = jstr(&v, "path")
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'path'".into()))?;
            let content = jstr(&v, "content")
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'content'".into()))?;
            let _ = &d0;
            let pv = preview_edit(Path::new(&path), content.as_bytes())?;
            Ok(serde_json::to_string_pretty(&pv).unwrap())
        },
    );
    let d1 = dir.clone();
    reg.register(
        mk(
            "stage_files",
            "Stage file edits without touching targets. Returns stage id + baselines.",
            serde_json::json!({"edits":{"type":"array"},"state_dir":{"type":"string"}}),
            vec!["edits"],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d1).clone());
            let w = SafeWriter::new(sd)?;
            let batch = w.stage_edits(parse_edit_list(&v)?)?;
            Ok(serde_json::to_string_pretty(&batch).unwrap())
        },
    );
    let d2 = dir.clone();
    reg.register(mk("apply_files", "Validate + checkpoint + atomically apply edits. Fails on stale expected_hash.",
        serde_json::json!({"edits":{"type":"array"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["edits"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d2).clone());
            let w = SafeWriter::new(sd)?;
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let r = w.apply_edits(parse_edit_list(&v)?, seq)?;
            Ok(serde_json::to_string_pretty(&r).unwrap())
        });
    let d3 = dir.clone();
    reg.register(mk("apply_staged", "Atomically apply a staged batch by id.",
        serde_json::json!({"stage_id":{"type":"string"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["stage_id"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d3).clone());
            let w = SafeWriter::new(sd)?;
            let sid = jstr(&v, "stage_id").ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'stage_id'".into()))?;
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let r = w.apply_staged(&sid, seq)?;
            Ok(serde_json::to_string_pretty(&r).unwrap())
        });
    let d4 = dir.clone();
    reg.register(mk("checkpoint_files", "Snapshot pre-images for paths, anchored to a ledger seq.",
        serde_json::json!({"paths":{"type":"array"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["paths"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d4).clone());
            let w = SafeWriter::new(sd)?;
            let paths: Vec<PathBuf> = v.get("paths").and_then(|x| x.as_array()).map(|a| a.iter()
                .filter_map(|x| x.as_str()).map(PathBuf::from).collect()).unwrap_or_default();
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let cp = w.checkpoint(&paths, seq)?;
            Ok(serde_json::to_string_pretty(&cp).unwrap())
        });
    let d5 = dir.clone();
    reg.register(
        mk(
            "list_checkpoints",
            "List checkpoints with ledger anchors.",
            serde_json::json!({"state_dir":{"type":"string"}}),
            vec![],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d5).clone());
            let w = SafeWriter::new(sd)?;
            let list = w.list_checkpoints()?;
            Ok(serde_json::to_string_pretty(&list).unwrap())
        },
    );
    let d6 = dir.clone();
    reg.register(
        mk(
            "rollback_checkpoint",
            "Restore a checkpoint by id (atomic per file).",
            serde_json::json!({"checkpoint_id":{"type":"string"},"state_dir":{"type":"string"}}),
            vec!["checkpoint_id"],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d6).clone());
            let w = SafeWriter::new(sd)?;
            let id = jstr(&v, "checkpoint_id")
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'checkpoint_id'".into()))?;
            let restored = w.restore_checkpoint(&id)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({"checkpoint": id, "restored": restored}),
            )
            .unwrap())
        },
    );
    let d7 = dir;
    reg.register(
        mk(
            "rollback_seq",
            "Restore the latest checkpoint at or before a ledger seq.",
            serde_json::json!({"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}),
            vec!["ledger_seq"],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d7).clone());
            let w = SafeWriter::new(sd)?;
            let seq = v
                .get("ledger_seq")
                .and_then(|x| x.as_i64())
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing integer 'ledger_seq'".into()))?;
            let (id, restored) = w.rollback_to_seq(seq)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({"checkpoint": id, "restored": restored}),
            )
            .unwrap())
        },
    );
}
