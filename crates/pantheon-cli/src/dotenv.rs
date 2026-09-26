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
        // Strip one layer of matching quotes.
        if value.len() >= 2
            && ((value.starts_with('\'') && value.ends_with('\''))
                || (value.starts_with('"') && value.ends_with('"')))
        {
            value = value[1..value.len() - 1].to_string();
        }
        out.push((key.to_string(), value));
    }
    out
}

fn dotenv_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join(".env")
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

/// Restrict a key file to owner-only (unix).
///
/// Reports failure instead of swallowing it. This file holds live API keys,
/// so a chmod that does not take effect leaves a world-readable credential on
/// disk; the user needs to hear about that rather than be told the key was
/// saved. A non-unix host has no equivalent to enforce, so it is a no-op.
fn restrict_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Load `<data_dir>/.env` into the process environment. Never overrides
/// an already-set variable. Missing/unreadable file = no-op. Like every
/// dotenv loader, the last occurrence of a key wins.
pub fn load_dotenv(data_dir: &Path) {
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
pub fn upsert_dotenv(data_dir: &Path, key: &str, value: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = dotenv_path(data_dir);
    let rendered = format!("{key}={value}");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.is_empty() {
        std::fs::write(&path, format!("{rendered}\n"))?;
        restrict_permissions(&path)?;
    }
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
    std::fs::write(&path, text)?;
    restrict_permissions(&path)?;
    Ok(())
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
    let text = std::fs::read_to_string(dotenv_path(data_dir)).ok()?;
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
    let path = dotenv_path(data_dir);
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
        std::fs::write(&path, text)?;
    }
    Ok(removed)
}

/// Process-global env/data-dir mutations must serialize: parallel test
/// threads share one environment. Every test that sets `PANTHEON_*` or
/// `SYSTEMD_*` holds this across the test. (Test-only; invisible in normal
/// builds.)
///
/// There is exactly one lock, in one place, on purpose. A test file that
/// declares its own `OnceLock<Mutex<()>>` is not safer, it is invisible to
/// every other file: two locks provide no mutual exclusion, so a
/// `cmd_model` and a `cmd_fallback` will happily race on one
/// `PANTHEON_DATA_DIR`, and whichever verb calls `std::process::exit` first
/// takes the whole test binary down without a panic line to find.
///
/// Lock with `TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())` rather
/// than `.unwrap()`: one panicking test should not cascade into every other
/// env-touching test in the crate.
#[cfg(test)]
pub mod test_support {
    use std::sync::Mutex;
    pub static TEST_ENV_LOCK: Mutex<()> = Mutex::new(());
}
