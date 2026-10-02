//! `<data_dir>/.env`: the pantheon folder's own key store.
//!
//! `pantheon model` writes API keys here (`PANTHEON_KEY_X=...`, comma
//! separated when stacked). Every CLI invocation loads this file first:
//! entries fill in process env vars that are not already set, so an
//! exported variable always beats the file and raw keys never appear in
//! `config.toml` (which stores only env-var *names*).
//!
//! Format: `KEY=VALUE` per line, `#` comments and blank lines ignored,
//! optional `export ` prefix, single/double quotes stripped.

use std::path::Path;
use std::path::PathBuf;

use fs2::FileExt as _;

/// Parse dotenv text into `(KEY, value)` pairs. Later lines win.
pub fn parse_dotenv(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim).unwrap_or(line);
        let Some(eq) = line.find('=') else { continue };
        let key = line[..eq].trim();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let mut value = line[eq + 1..].trim().to_string();
        // Strip an inline comment only when the value is unquoted.
        if !(value.starts_with('\'') || value.starts_with('"')) {
            if let Some(hash) = value.find(" #") {
                value.truncate(hash);
                value = value.trim_end().to_string();
            }
        }
        // Strip one layer of matching quotes. Double-quoted values honor
        // the two escape sequences the writer emits (`\"` and `\\`);
        // any other backslash sequence stays literal so hand-written
        // files (e.g. `"C:\path"`) keep reading exactly as before.
        // Single-quoted values are literal, shell-style.
        if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
            value = unescape_dotenv_value(&value[1..value.len() - 1]);
        } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
            value = value[1..value.len() - 1].to_string();
        }
        out.push((key.to_string(), value));
    }
    out
}

/// Undo the writer's escaping inside a double-quoted value: `\"` → `"`,
/// `\\` → `\`. Any other `\x` is kept verbatim (backslash included) so
/// values the writer never produced still parse the old way.
fn unescape_dotenv_value(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Render a value for the file: always double-quoted, with `\` and `"`
/// escaped. Quoting makes the round-trip lossless - `parse_dotenv`
/// strips ` #` comments and edge whitespace only on *unquoted* values,
/// so a raw `abc # def` used to read back as `abc`. Values must still
/// be single-line; `\n`/`\r` are rejected by the callers.
fn quote_dotenv_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn dotenv_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(".env")
}

/// Path of a named dotenv-style file inside `data_dir` (e.g. the
/// website-login vault `logins.env`). Same directory, same atomic and
/// permission semantics as `.env`.
pub fn dotenv_file_path(data_dir: &Path, file_name: &str) -> std::path::PathBuf {
    data_dir.join(file_name)
}

/// The key a raw line assigns, if any (`export ` prefix tolerated).
/// Used by upsert/delete so all three agree on what "the K1 line" means.
fn dotenv_line_key(raw_line: &str) -> Option<&str> {
    let trimmed = raw_line.trim();
    let probe = trimmed
        .strip_prefix("export ")
        .map(str::trim)
        .unwrap_or(trimmed);
    let eq = probe.find('=')?;
    let key = probe[..eq].trim();
    if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    Some(key)
}

/// Process-wide write lock for dotenv files, the in-process half of the
/// write exclusion. Every read-modify-write below runs inside
/// [`with_dotenv_write_lock`], which holds this mutex for the whole
/// read → transform → write sequence (so concurrent `set` calls in this
/// process cannot interleave and lose updates - fresh-broker-per-request
/// means the lock cannot live on the broker) AND an exclusive flock on a
/// sidecar `<target>.lock` file (so writers in *other* processes cannot
/// interleave either). Reads stay unlocked: every write below is an atomic
/// rename, so a concurrent reader only ever sees the old file or the new one.
static DOTENV_WRITE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sidecar lock file for a dotenv target: `<target>.lock`
/// (`<data_dir>/.env` → `<data_dir>/.env.lock`). It is never renamed, so
/// the inode the flock pins stays stable across the atomic renames of the
/// target itself - locking the target directly would be racy, because the
/// rename swaps the locked inode out from under the locker.
fn dotenv_lock_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

fn open_dotenv_lock_file(lock_path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(lock_path)
}

/// Whether `pid` currently names a live process. Used to tell a temp file
/// leaked by a dead writer apart from one owned by a writer that is still
/// going (e.g. an older binary writing without the flock).
#[cfg(unix)]
fn pid_is_alive(pid: u32) -> bool {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s,
        Err(_) => return false, // no /proc entry: not alive
    };
    // A zombie still has a /proc entry but holds no fds, so it cannot own
    // a temp either: treat it as dead. Field 3 of /proc/<pid>/stat is the
    // state code; the comm field may itself contain ')', hence `rfind`.
    // Anything unparseable-but-present is treated as alive (conservative:
    // a live writer's temp must never be reaped).
    let Some(close) = stat.rfind(')') else {
        return true;
    };
    !matches!(stat[close + 1..].trim_start().chars().next(), Some('Z'))
}

/// Where liveness cannot be checked, never sweep: a live writer's temp
/// must not be removed.
#[cfg(not(unix))]
fn pid_is_alive(_pid: u32) -> bool {
    true
}

/// Remove `<target>.tmp.<pid>` temp files leaked by writers that died
/// mid-write (kill -9 between temp creation and rename leaves them; dozens
/// were observed across repeated kill rounds). Only temps whose pid is dead
/// are removed - a live writer's temp is never touched - so this is safe to
/// call at startup ([`load_dotenv`]) and on every write.
///
/// Matches the [`atomic_write_dotenv_file`] naming (`<target>.tmp.<pid>`,
/// e.g. `.env.tmp.123`; note a target that already has an extension keeps
/// only its stem: `logins.env` → `logins.tmp.<pid>`).
pub fn sweep_stale_dotenv_temps(data_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    let me = std::process::id();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some((prefix, pid_s)) = name.rsplit_once(".tmp.") else {
            continue;
        };
        if prefix.is_empty() || pid_s.is_empty() || !pid_s.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        if pid == me || pid_is_alive(pid) {
            continue;
        }
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Run `f` with the full dotenv write exclusion held, in a fixed order:
/// the process-wide mutex first, then an exclusive flock on the sidecar
/// `<target>.lock` file. The flock is blocking - a writer waits its turn
/// rather than failing - and the OS releases it if the holder dies, so a
/// kill -9'd writer can never wedge later writers; it can only leak its
/// temp file, which is reaped here by [`sweep_stale_dotenv_temps`]
/// (we hold the exclusive flock, so no live flock-aware writer can own a
/// temp for this target right now; the pid check additionally protects
/// temps from writers that predate the flock).
fn with_dotenv_write_lock<T>(
    path: &Path,
    f: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let _proc = DOTENV_WRITE_LOCK.lock().unwrap();
    let lock_path = dotenv_lock_path(path);
    let lock_file = match open_dotenv_lock_file(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // `delete_*` on a missing data dir: make room for the lock
            // file rather than failing the whole operation.
            if let Some(parent) = lock_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            open_dotenv_lock_file(&lock_path)?
        }
        Err(e) => return Err(e),
    };
    lock_file.lock_exclusive()?;
    struct Unlock<'a>(&'a std::fs::File);
    impl Drop for Unlock<'_> {
        fn drop(&mut self) {
            let _ = self.0.unlock();
        }
    }
    let _flock = Unlock(&lock_file);
    if let Some(parent) = lock_path.parent() {
        sweep_stale_dotenv_temps(parent);
    }
    f()
}

/// Write `text` to `path` atomically: bytes go to a temp file created
/// with owner-only permissions *at creation* (no umask window - the
/// file is never world-readable, not even briefly), fsynced, then
/// renamed over the target. A crash at any point leaves the old file
/// or the complete new file, never a torn one. The temp name carries
/// the pid so two processes writing at once cannot share it.
fn atomic_write_dotenv_file(path: &Path, text: &str) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        use std::io::Write;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // Belt and braces: the mode was set at creation, but a pre-existing
    // temp file (stale pid reuse) could carry wider permissions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Load `<data_dir>/.env` into the process environment. Never overrides
/// an already-set variable. Missing/unreadable file = no-op. Like every
/// dotenv loader, the last occurrence of a key wins.
///
/// Also reaps stale `<target>.tmp.<pid>` files left by writers that died
/// mid-write (see [`sweep_stale_dotenv_temps`]): this runs at every CLI
/// startup, so leaked temps do not accumulate across kill -9 rounds.
pub fn load_dotenv(data_dir: &Path) {
    sweep_stale_dotenv_temps(data_dir);
    let Ok(text) = std::fs::read_to_string(dotenv_path(data_dir)) else {
        return;
    };
    // Reversed + set-if-absent = last occurrence wins, exports still win.
    for (key, value) in parse_dotenv(&text).into_iter().rev() {
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, &value);
        }
    }
}

/// Insert or replace `KEY=value` in `<data_dir>/.env`, preserving every
/// other line (comments, order). A hand-edited file with duplicate KEY
/// lines collapses to one (last-wins, matching `load_dotenv`).
/// Creates the file (and dir) if missing.
///
/// Atomic: the whole read-modify-write holds the cross-process write
/// exclusion (process mutex + exclusive flock) and the result is written
/// via temp-file + rename, so a crash mid-write never tears the file.
/// Values are quoted on write so they round-trip losslessly through
/// [`parse_dotenv`].
pub fn upsert_dotenv(data_dir: &Path, key: &str, value: &str) -> std::io::Result<()> {
    upsert_dotenv_file(data_dir, ".env", key, value)
}

/// [`upsert_dotenv`] against a named dotenv-style file inside
/// `data_dir` instead of `.env`.
///
/// Holds the full cross-process write exclusion (process mutex + exclusive
/// flock) for the whole read-modify-write, so concurrent writers in this
/// or any other process serialize instead of losing updates.
pub fn upsert_dotenv_file(
    data_dir: &Path,
    file_name: &str,
    key: &str,
    value: &str,
) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = dotenv_file_path(data_dir, file_name);
    with_dotenv_write_lock(&path, || {
        let rendered = format!("{key}={}", quote_dotenv_value(value));
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut replaced = false;
        let mut lines: Vec<String> = Vec::new();
        for raw_line in existing.lines() {
            if !replaced && dotenv_line_key(raw_line) == Some(key) {
                lines.push(rendered.clone());
                replaced = true;
            } else if dotenv_line_key(raw_line) == Some(key) {
                continue; // stale duplicate
            } else {
                lines.push(raw_line.to_string());
            }
        }
        if !replaced {
            lines.push(rendered);
        }
        let mut text = lines.join("\n");
        text.push('\n');
        atomic_write_dotenv_file(&path, &text)
    })
}

/// Write `KEY=value` to `<data_dir>/.env` AND the current process env,
/// so the value is usable immediately (model listing, same-process
/// resolution) as well as by future invocations.
pub fn persist_dotenv_value(data_dir: &Path, key: &str, value: &str) -> std::io::Result<()> {
    upsert_dotenv(data_dir, key, value)?;
    std::env::set_var(key, value);
    Ok(())
}

/// Read one raw value back out of `<data_dir>/.env` (for "keep existing"
/// prompts). `None` = absent or unreadable.
pub fn read_dotenv_value(data_dir: &Path, key: &str) -> Option<String> {
    read_dotenv_file_value(data_dir, ".env", key)
}

/// [`read_dotenv_value`] against a named dotenv-style file inside
/// `data_dir` instead of `.env`.
pub fn read_dotenv_file_value(data_dir: &Path, file_name: &str, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(dotenv_file_path(data_dir, file_name)).ok()?;
    parse_dotenv(&text)
        .into_iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v)
        .next_back()
}

/// Delete `KEY` from `<data_dir>/.env`, preserving every other line.
/// Drops every occurrence (hand-edited duplicates included).
/// Returns whether a line was removed. Missing file = `Ok(false)`.
///
/// Note: permissions are left untouched (the file already exists with
/// owner-only mode when we created it; don't widen here).
pub fn delete_dotenv_key(data_dir: &Path, key: &str) -> std::io::Result<bool> {
    delete_dotenv_file_key(data_dir, ".env", key)
}

/// [`delete_dotenv_key`] against a named dotenv-style file inside
/// `data_dir` instead of `.env`.
pub fn delete_dotenv_file_key(
    data_dir: &Path,
    file_name: &str,
    key: &str,
) -> std::io::Result<bool> {
    let path = dotenv_file_path(data_dir, file_name);
    // Same cross-process exclusion as the upserts: delete is a
    // read-modify-write too.
    with_dotenv_write_lock(&path, || {
        let existing = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut removed = false;
        let mut lines: Vec<String> = Vec::new();
        for raw_line in existing.lines() {
            if dotenv_line_key(raw_line) == Some(key) {
                removed = true;
            } else {
                lines.push(raw_line.to_string());
            }
        }
        if removed {
            let mut text = lines.join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            atomic_write_dotenv_file(&path, &text)?;
        }
        Ok(removed)
    })
}

/// Validate a dotenv key name (`KEY=...`). Mirrors the parser's rule so a
/// rejected key is rejected the same way everywhere.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Apply many upserts and deletes to `<data_dir>/.env` in one atomic step:
/// a single read, a single temp-file + rename write, owner-only permissions.
///
/// Line-preserving like [`upsert_dotenv`]: comments, order, and unrelated
/// keys survive; hand-edited duplicates collapse (last-wins). Values are
/// quoted on write so they round-trip losslessly through [`parse_dotenv`];
/// they must be single-line - a value containing `\n` or `\r` is rejected
/// rather than written in a form the parser would read back differently.
///
/// Holds the full cross-process write exclusion (process-wide dotenv
/// mutex + exclusive flock on the sidecar lock file) for the whole
/// read-modify-write, so concurrent batches (or single upserts) in this
/// process or any other serialize instead of losing updates. This is the
/// write path behind multi-key `PUT /api/env` - one batch, one atomic commit.
pub fn apply_dotenv_batch(
    data_dir: &Path,
    upserts: &[(String, String)],
    deletes: &[String],
) -> std::io::Result<()> {
    for (key, value) in upserts {
        if !valid_key(key) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid env key {key:?}"),
            ));
        }
        if value.contains('\n') || value.contains('\r') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("value for {key} must be single-line"),
            ));
        }
    }
    for key in deletes {
        if !valid_key(key) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid env key {key:?}"),
            ));
        }
    }
    std::fs::create_dir_all(data_dir)?;
    let path = dotenv_path(data_dir);
    // Cross-process exclusion for the whole read-modify-write, like the
    // single upserts: concurrent batches (or single upserts) in this or
    // any other process serialize instead of losing updates.
    with_dotenv_write_lock(&path, || {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut lines: Vec<String> = Vec::new();
        let mut done: Vec<bool> = vec![false; upserts.len()];
        for raw_line in existing.lines() {
            match dotenv_line_key(raw_line) {
                Some(k) if deletes.iter().any(|d| d == k) => continue,
                Some(k) => match upserts.iter().position(|(uk, _)| uk == k) {
                    Some(i) => {
                        // First occurrence is replaced, later duplicates dropped:
                        // the file collapses to one line per key (last-wins).
                        if done[i] {
                            continue;
                        }
                        done[i] = true;
                        lines.push(format!(
                            "{}={}",
                            upserts[i].0,
                            quote_dotenv_value(&upserts[i].1)
                        ));
                    }
                    None => lines.push(raw_line.to_string()),
                },
                None => lines.push(raw_line.to_string()),
            }
        }
        for (i, (key, value)) in upserts.iter().enumerate() {
            if !done[i] {
                lines.push(format!("{key}={}", quote_dotenv_value(value)));
            }
        }
        let mut text = lines.join("\n");
        text.push('\n');
        // Temp file (owner-only at creation) + rename: a crash mid-write
        // never leaves half a key file.
        atomic_write_dotenv_file(&path, &text)
    })
}

#[cfg(test)]
mod dotenv_durability_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dotenv-durability-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Item 2c: values with comment markers, quotes, and edge whitespace
    /// must round-trip EXACTLY. Before the quoting fix, `parse_dotenv`
    /// stripped ` #` and quotes on read, so `"abc # def"` came back as
    /// `"abc"` - silent round-trip corruption.
    #[test]
    fn quoted_values_round_trip_losslessly() {
        let dir = scratch_dir("roundtrip");
        let cases = [
            "abc # def",
            "a\"b",
            "c:\\path\\to",
            "  spaced  ",
            "trailing ",
            "semicolon;here",
            "hash#inside",
            "single'quote",
            "dollar$var",
            "backtick`tick",
            "unicode-•••-✓",
            "",
            "#leading",
            "a=b",
            "=leading-eq",
            "\"",
            "\\",
            "quote\"and\\back",
        ];
        for (i, v) in cases.iter().enumerate() {
            let key = format!("ROUNDTRIP_{i}");
            upsert_dotenv_file(&dir, ".env", &key, v).expect("upsert");
            let back = read_dotenv_file_value(&dir, ".env", &key).expect("read back");
            assert_eq!(&back, v, "value {v:?} must round-trip exactly");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 2a: a crash mid-write must leave the old file or the new
    /// file - never a torn one. Readers racing a writer must only ever
    /// observe complete, well-formed snapshots. (With the old direct
    /// `fs::write` path this test observes torn lines.)
    #[test]
    fn concurrent_readers_never_see_torn_file() {
        let dir = scratch_dir("torn");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let torn = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = Vec::new();
        // One writer rewriting the whole file in a tight loop.
        {
            let dir = dir.clone();
            let stop = stop.clone();
            handles.push(std::thread::spawn(move || {
                let mut gen = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    gen += 1;
                    let batch: Vec<(String, String)> = (0..40)
                        .map(|i| {
                            (
                                format!("TORN_{i}"),
                                format!("gen-{gen}-pad-xxxxxxxxxxxx-{i}"),
                            )
                        })
                        .collect();
                    let _ = apply_dotenv_batch(&dir, &batch, &[]);
                }
            }));
        }
        // Readers asserting every snapshot is well-formed.
        for _ in 0..2 {
            let dir = dir.clone();
            let stop = stop.clone();
            let torn = torn.clone();
            handles.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let path = dotenv_file_path(&dir, ".env");
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    for line in text.lines() {
                        let t = line.trim();
                        if t.is_empty() || t.starts_with('#') {
                            continue;
                        }
                        // Every non-blank line must be a complete KEY= line
                        // from one generation - a torn write shows up as a
                        // truncated line or mixed generations.
                        let Some(eq) = t.find('=') else {
                            torn.store(true, Ordering::Relaxed);
                            return;
                        };
                        if !t[..eq]
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_')
                        {
                            torn.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                    // Generation consistency: all values in one snapshot
                    // share a generation marker.
                    let vals: Vec<String> =
                        parse_dotenv(&text).into_iter().map(|(_, v)| v).collect();
                    if !vals.is_empty() {
                        let gen0 = vals[0].split('-').nth(1).unwrap_or("").to_string();
                        if vals
                            .iter()
                            .any(|v| v.split('-').nth(1).unwrap_or("") != gen0)
                        {
                            torn.store(true, Ordering::Relaxed);
                            return;
                        }
                    }
                }
            }));
        }
        std::thread::sleep(std::time::Duration::from_millis(1500));
        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        assert!(
            !torn.load(Ordering::Relaxed),
            "readers observed a torn or generation-mixed snapshot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 2a: a multi-key batch applies atomically - after the call
    /// every key is present with its exact value.
    #[test]
    fn batch_upserts_apply_atomically() {
        let dir = scratch_dir("batch");
        let batch: Vec<(String, String)> = (0..200)
            .map(|i| (format!("BATCH_{i}"), format!("v{i} # not-a-comment")))
            .collect();
        apply_dotenv_batch(&dir, &batch, &[]).expect("batch apply");
        for (k, v) in &batch {
            assert_eq!(
                read_dotenv_file_value(&dir, ".env", k).as_deref(),
                Some(v.as_str())
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 2 (umask): the secret file is owner-only from creation.
    #[test]
    #[cfg(unix)]
    fn secret_file_created_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("perms");
        upsert_dotenv_file(&dir, ".env", "K", "v").expect("upsert");
        let mode = std::fs::metadata(dotenv_file_path(&dir, ".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "secret file must be owner-only, got {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Stale-temp sweep: `<target>.tmp.<pid>` files whose pid is dead are
    /// reaped on write; a temp owned by a live pid (or a file that merely
    /// looks like a temp) is never touched.
    #[test]
    fn stale_temps_are_swept_on_write() {
        let dir = scratch_dir("sweep");
        // Dead pids: u32::MAX can never name a live process.
        let dead: Vec<String> = [u32::MAX, u32::MAX - 1]
            .iter()
            .map(|p| format!(".env.tmp.{p}"))
            .collect();
        for name in &dead {
            std::fs::write(dir.join(name), "STALE=x\n").unwrap();
        }
        // Live pid: PID 1 (init) is always alive on Linux, and unlike our
        // own pid it can never collide with this writer's own temp name.
        // Must survive the sweep.
        let live = dir.join(".env.tmp.1");
        std::fs::write(&live, "STALE=y\n").unwrap();
        // Looks like a temp but the suffix is not a pid. Must survive.
        let not_a_pid = dir.join(".env.tmp.notes");
        std::fs::write(&not_a_pid, "x").unwrap();

        upsert_dotenv_file(&dir, ".env", "SWEEP_K", "v").expect("upsert");

        for name in &dead {
            assert!(
                !dir.join(name).exists(),
                "dead-pid temp {name} must be reaped"
            );
        }
        assert!(live.exists(), "live-pid temp must survive the sweep");
        assert!(not_a_pid.exists(), "non-pid suffix must survive the sweep");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cross-process exclusion: N child *processes* x M sets against the
    /// same file must lose nothing. The in-process mutex cannot serialize
    /// across processes; the exclusive flock on the sidecar lock file
    /// does. Without it, racing read-modify-writes lost over half the
    /// keys (265/800 survived in the pre-fix stress run).
    #[test]
    fn cross_process_concurrent_sets_lose_nothing() {
        const PROCS: u64 = 4;
        const SETS: u64 = 50;
        // Child branch: spawned below via `current_exe`; does its share of
        // sets through the real write path and exits.
        if std::env::var("PANTHEON_DOTENV_STRESS_CHILD").is_ok() {
            let dir = std::path::PathBuf::from(
                std::env::var("PANTHEON_DOTENV_STRESS_DIR").expect("stress dir"),
            );
            let worker: u64 = std::env::var("PANTHEON_DOTENV_STRESS_WORKER")
                .expect("stress worker")
                .parse()
                .expect("worker number");
            for k in 0..SETS {
                upsert_dotenv_file(
                    &dir,
                    ".env",
                    &format!("XPROC_{worker}_{k}"),
                    &format!("w{worker}v{k}"),
                )
                .expect("child set");
            }
            return;
        }
        let dir = scratch_dir("xproc");
        let exe = std::env::current_exe().expect("test exe");
        let test_name =
            "dotenv::dotenv_durability_tests::cross_process_concurrent_sets_lose_nothing";
        let mut children = Vec::new();
        for w in 0..PROCS {
            children.push(
                std::process::Command::new(&exe)
                    .args(["--exact", test_name, "--nocapture"])
                    .env("PANTHEON_DOTENV_STRESS_CHILD", "1")
                    .env("PANTHEON_DOTENV_STRESS_DIR", &dir)
                    .env("PANTHEON_DOTENV_STRESS_WORKER", w.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .expect("spawn stress child"),
            );
        }
        for mut child in children {
            let status = child.wait().expect("wait for stress child");
            assert!(status.success(), "stress child exited unsuccessfully");
        }
        let mut missing = 0u64;
        for w in 0..PROCS {
            for k in 0..SETS {
                let key = format!("XPROC_{w}_{k}");
                let want = format!("w{w}v{k}");
                if read_dotenv_file_value(&dir, ".env", &key).as_deref() != Some(&want) {
                    missing += 1;
                }
            }
        }
        assert_eq!(
            missing,
            0,
            "lost {missing} of {} cross-process writes",
            PROCS * SETS
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
