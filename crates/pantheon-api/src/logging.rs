//! Structured file logging, std-only.
//!
//! The runtime had no logging at all: no `tracing`, no `log`, no files. Every
//! failure was a line on stderr that vanished when the process exited, so
//! "why did that turn die" was unanswerable after the fact and `pantheon logs`
//! had nothing real to read.
//!
//! This is deliberately dependency-free. The workspace has no logging crate and
//! adding one means a global subscriber, per-crate span instrumentation, and a
//! subscriber that has to be initialised before anything can log - all of it
//! invisible in review. A writer behind one function, called where the
//! interesting thing happens, is greppable and cannot silently no-op.
//!
//! Two files, split by severity, because they answer different questions:
//!
//! - `agent.log` (DEBUG+) - what the runtime did, in order.
//! - `errors.log` (WARNING+) - what went wrong, for grepping.
//!
//! Each line is `TIMESTAMP LEVEL [component] message`, which is the shape
//! `hermes logs` parses back out with a regex. Keeping the two in agreement is
//! what makes the reader possible at all.
//!
//! Every line passes through `redact()` before it is written, so API keys
//! and bearer tokens never reach disk even when a caller logs a raw request
//! dump. Files rotate at 10 MiB, keeping 5 generations (`agent.log.1`..=
//! `agent.log.5`), enforced on the write path so no call site can forget.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warning = 2,
    Error = 3,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warning => "WARNING",
            Level::Error => "ERROR",
        }
    }

    /// Parse a level name, case-insensitively. `None` for anything else, so a
    /// bad `--level` is rejected rather than silently meaning DEBUG.
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "debug" => Some(Level::Debug),
            "info" => Some(Level::Info),
            "warn" | "warning" => Some(Level::Warning),
            "error" => Some(Level::Error),
            _ => None,
        }
    }
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The log files a reader can address by name.
pub const AGENT_LOG: &str = "agent.log";
pub const ERRORS_LOG: &str = "errors.log";
pub const GATEWAY_LOG: &str = "gateway.log";

/// All known files, for `logs list`. This is a closed set because a reader
/// cannot enumerate a data dir for `*.log` and stay correct: the backup copies
/// `repair` writes (`ledger.db.<stamp>.bak`) and any editor swap file would
/// show up as a log.
pub const KNOWN_LOGS: &[&str] = &[AGENT_LOG, ERRORS_LOG, GATEWAY_LOG];

/// Size at which a log file is rotated. 10 MiB is small enough that a
/// `logs tail` never chokes and large enough that rotation is rare: a
/// busy turn writes kilobytes, so one generation covers weeks.
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Rotated generations kept per log file: `agent.log.1` ..= `agent.log.5`.
/// `.1` is always the newest. Oldest is deleted on rotation, never archived,
/// so a forgotten debug session cannot fill a disk.
pub const LOG_ROTATE_GENERATIONS: u32 = 5;

/// Where the logs live, and the threshold. Set once from the CLI entry point
/// at startup; until then logging is a no-op, so a library test or an embedded
/// use never writes files it did not ask for.
static SINK: OnceLock<Sink> = OnceLock::new();

struct Sink {
    dir: PathBuf,
    min: Level,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn stamp(ms: i64) -> String {
    // UTC, `YYYY-MM-DD HH:MM:SS.mmm`. Hand-rolled rather than pulling a date
    // crate for one format, and it matches the prefix `hermes logs` expects.
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // Civil-from-days (Howard Hinnant's algorithm), valid for any i64 count.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{millis:03}")
}

/// Point the logger at `dir/logs` with a minimum level. Call once, early.
///
/// A second call is ignored rather than an error: the gateway and a CLI command
/// can both reach this, and whichever runs second must not silently retarget
/// the first one's files.
pub fn init(dir: &Path, min: Level) {
    let _ = SINK.set(Sink {
        dir: dir.join("logs"),
        min,
    });
}

/// The configured log directory, if logging was initialised.
pub fn log_dir() -> Option<PathBuf> {
    SINK.get().map(|s| s.dir.clone())
}

/// Append one line to `file`, and to `errors.log` too when the level is a
/// warning or worse. Failures are swallowed on purpose: a log write must never
/// take down the operation being logged, and a missing log line is far less
/// damaging than a failed turn.
///
/// The message is redacted before writing, so an API key or bearer token in
/// a request dump never reaches the file even when the caller forgot to
/// sanitize. Rotation is enforced on the way in, so the files cannot grow
/// forever no matter who writes to them.
///
/// Takes a full path and reads no global, so the formatting and the
/// warn+ mirroring are testable without racing for the process-wide `SINK`.
pub(crate) fn append(file: &Path, level: Level, component: &str, msg: &str) {
    // Redact first: this is the single choke point every log line passes
    // through, so secrets cannot reach disk via a forgotten call site.
    let redacted = redact(msg);
    // Escape newlines before writing. A caller passing a multi-line tool
    // result is the ordinary case, and an unescaped one would let the message
    // forge additional log lines - including ones that look like a different
    // component's ERROR. The reader is line-oriented, so this is what keeps a
    // record one record.
    let flat = redacted.replace('\r', "\\r").replace('\n', "\\n");
    let line = format!("{} {level} [{component}] {flat}\n", stamp(now_ms()));
    if let Ok(mut f) = open_append(file) {
        let _ = f.write_all(line.as_bytes());
    }
    if level >= Level::Warning {
        // Derived from `file`, not from the sink, so a gateway-logged warning
        // lands in the same errors file instead of a second one.
        let errors = file.with_file_name(ERRORS_LOG);
        if let Ok(mut f) = open_append(&errors) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

fn open_append(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Enforce the size cap before every write. Best-effort: a failed rotation
    // degrades to the old unbounded behaviour rather than losing the line.
    rotate_if_needed(path);
    OpenOptions::new().create(true).append(true).open(path)
}

/// Rotate `path` when it has reached `LOG_ROTATE_BYTES`: `path` becomes
/// `path.1`, the old `path.1` becomes `path.2`, and so on up to
/// `LOG_ROTATE_GENERATIONS`, at which point the oldest generation is deleted.
/// Called before every append, so all three log files rotate no matter which
/// entry point wrote them.
///
/// Best-effort by design. Two processes can race a rename, and a full disk
/// fails every op inside; either way logging must never take down the turn
/// being logged. Every failure here is swallowed.
fn rotate_if_needed(path: &Path) {
    rotate_sized(path, LOG_ROTATE_BYTES, LOG_ROTATE_GENERATIONS);
}

fn rotate_sized(path: &Path, limit_bytes: u64, generations: u32) {
    let too_big = std::fs::metadata(path)
        .map(|m| m.len() >= limit_bytes)
        .unwrap_or(false);
    if !too_big {
        return;
    }
    rotate_log(path, generations);
}

/// Unconditionally rotate `path` through `generations` generations. Public
/// so a CLI `logs rotate` or a startup sweep can rotate files the append
/// path has not seen yet; the append path calls it only when the size cap
/// is hit.
pub fn rotate_log(path: &Path, generations: u32) {
    let name = path.to_string_lossy();
    // Drop the oldest generation first so the chain of renames has a free
    // slot; a missing file anywhere in the chain is fine (first rotation).
    let _ = std::fs::remove_file(format!("{name}.{generations}"));
    for i in (1..generations).rev() {
        let _ = std::fs::rename(format!("{name}.{i}"), format!("{name}.{}", i + 1));
    }
    let _ = std::fs::rename(path, format!("{name}.1"));
}

/// The one place level filtering and file routing happen, so no call site can
/// forget the threshold or write to a file the reader does not know about.
fn emit(file: &str, level: Level, component: &str, msg: &str) {
    let Some(sink) = SINK.get() else { return };
    if level < sink.min {
        return;
    }
    append(&sink.dir.join(file), level, component, msg);
}

/// Record something in `agent.log` at INFO.
pub fn info(component: &str, msg: impl AsRef<str>) {
    emit(AGENT_LOG, Level::Info, component, msg.as_ref());
}

pub fn debug(component: &str, msg: impl AsRef<str>) {
    emit(AGENT_LOG, Level::Debug, component, msg.as_ref());
}

pub fn warn(component: &str, msg: impl AsRef<str>) {
    emit(AGENT_LOG, Level::Warning, component, msg.as_ref());
}

pub fn error(component: &str, msg: impl AsRef<str>) {
    let Some(sink) = SINK.get() else { return };
    append(
        &sink.dir.join(AGENT_LOG),
        Level::Error,
        component,
        msg.as_ref(),
    );
}

// --- redaction -----------------------------------------------------------

/// Redact secrets from a message before it reaches a log file.
///
/// The redaction pipeline is fail-closed: when a secret pattern is detected,
/// the entire match is replaced with `[REDACTED]`. This is deliberately
/// aggressive - a partial leak is still a leak.
///
/// Patterns redacted:
/// - `sk-or-v1-...` (OpenRouter keys)
/// - `sk-...` (OpenAI-style keys)
/// - `Bearer <value>` (Authorization headers)
/// - `api-key: <value>` (API key headers)
/// - `PANTHEON_SECRET_<name>=<value>` (env-style secrets)
/// - values under sensitive keys (`password`, `token`, `api_key`,
///   `secret`, `authorization`, and close variants) when the input
///   parses as JSON - a logged tool-args dump like
///   `{"password": "..."}` carries no known prefix for the scan above.
pub fn redact(msg: &str) -> String {
    let mut out = msg.to_string();
    out = redact_prefix(&out, "sk-or-v1-");
    out = redact_prefix(&out, "sk-");
    out = redact_prefix(&out, "Bearer ");
    out = redact_prefix(&out, "api-key: ");
    out = redact_prefix(&out, "api-key=");
    out = redact_prefix(&out, "PANTHEON_SECRET_");
    out = redact_json_values(&out);
    out
}

/// True when a JSON object key names a credential. Matching is on the
/// lowercased key with `-`/space folded to `_`: the known credential
/// names, plus any `*_token` / `*_secret` / `*_password` compound
/// (`access_token`, `client_secret`, `db_password`, ...). Deliberately
/// narrower than a substring scan - `token_count` is telemetry, not a
/// secret, and redacting it would gut usage logs.
fn is_sensitive_key(key: &str) -> bool {
    let k = key.to_lowercase().replace(['-', ' '], "_");
    matches!(
        k.as_str(),
        "password"
            | "passwd"
            | "pwd"
            | "token"
            | "api_key"
            | "apikey"
            | "secret"
            | "authorization"
            | "auth"
            | "bearer"
            | "private_key"
            | "client_secret"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "auth_token"
            | "session_token"
            | "webhook_secret"
            | "signing_secret"
    ) || k.ends_with("_token")
        || k.ends_with("_secret")
        || k.ends_with("_password")
}

/// Replace every value under a sensitive key, at any depth. Returns
/// true when anything changed.
fn redact_json_node(v: &mut serde_json::Value) -> bool {
    let mut changed = false;
    match v {
        serde_json::Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if is_sensitive_key(key) {
                    if val.as_str() != Some("[REDACTED]") {
                        *val = serde_json::Value::String("[REDACTED]".to_string());
                        changed = true;
                    }
                } else {
                    changed |= redact_json_node(val);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items.iter_mut() {
                changed |= redact_json_node(item);
            }
        }
        _ => {}
    }
    changed
}

/// JSON-aware pass over [`redact`]: when the whole input parses as
/// JSON, values under sensitive keys are replaced wholesale. Input
/// that does not parse, or parses with nothing sensitive in it, is
/// returned byte-for-byte - clean JSON keeps its original formatting,
/// and only a message that actually carried a secret is re-serialized.
fn redact_json_values(input: &str) -> String {
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(input) else {
        return input.to_string();
    };
    if !redact_json_node(&mut v) {
        return input.to_string();
    }
    serde_json::to_string(&v).unwrap_or_else(|_| input.to_string())
}

/// Redact a secret that starts with a known prefix. The secret runs until
/// the next whitespace or end of string.
fn redact_prefix(input: &str, prefix: &str) -> String {
    if !input.contains(prefix) {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(pos) = rest.find(prefix) {
        out.push_str(&rest[..pos]);
        out.push_str("[REDACTED]");
        rest = &rest[pos + prefix.len()..];
        // Consume the secret value: everything up to the next whitespace
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Gateway-scoped records go to their own file so `logs gateway` is not mostly
/// agent turns.
pub fn gateway(level: Level, component: &str, msg: impl AsRef<str>) {
    emit(GATEWAY_LOG, level, component, msg.as_ref());
}

#[cfg(test)]
mod redact_tests {
    use super::redact;

    #[test]
    fn prefix_scan_still_redacts_non_json_text() {
        let out = redact("key sk-or-v1-abcdef and Bearer xyz");
        assert!(!out.contains("abcdef"), "prefix secret leaked: {out}");
        assert!(!out.contains("xyz"), "bearer value leaked: {out}");
        assert_eq!(redact("nothing secret here"), "nothing secret here");
    }

    #[test]
    fn json_values_under_sensitive_keys_are_redacted() {
        let out = redact(r#"{"user": "umar", "password": "hunter2"}"#);
        let v: serde_json::Value = serde_json::from_str(&out).expect("still valid JSON");
        assert_eq!(v["password"], "[REDACTED]");
        assert_eq!(v["user"], "umar", "non-sensitive values pass through");
        assert!(!out.contains("hunter2"));
    }

    #[test]
    fn json_redaction_covers_variants_nesting_and_non_string_values() {
        let out = redact(
            r#"{"access_token": "tok-1", "attempts": 2, "nested": {"api-key": "k", "token": 4242}, "list": [{"client_secret": "s"}]}"#,
        );
        let v: serde_json::Value = serde_json::from_str(&out).expect("still valid JSON");
        assert_eq!(v["access_token"], "[REDACTED]");
        assert_eq!(v["nested"]["api-key"], "[REDACTED]");
        assert_eq!(v["nested"]["token"], "[REDACTED]");
        assert_eq!(v["list"][0]["client_secret"], "[REDACTED]");
        assert_eq!(v["attempts"], 2, "non-sensitive values pass through");
        for leaked in ["tok-1", "4242", "\"k\"", "client_secret\": \"s"] {
            assert!(!out.contains(leaked), "leaked {leaked}: {out}");
        }
    }

    #[test]
    fn clean_json_and_telemetry_keys_pass_through_byte_for_byte() {
        let clean = "{ \"command\": \"ls -la\", \"count\": 3 }";
        assert_eq!(redact(clean), clean, "clean JSON is not re-serialized");
        let telemetry = r#"{"token_count": 1200, "model": "gpt"}"#;
        assert_eq!(redact(telemetry), telemetry);
    }
}
