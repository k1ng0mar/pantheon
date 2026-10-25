//! Bot Screen takeover leases: cross-process human-control state for the
//! CUA-driven desktop.
//!
//! The dashboard (human viewer) and the agent loop (tool calls) may run in
//! different processes, so takeover state lives in
//! `<data_dir>/desktop-takeover.json` — never in memory. The agent loop
//! consults it on every `mcp_cua-driver_*` tool call (the `PreToolCall`
//! sites in `session.rs`); the dashboard owns the read-modify-write cycle
//! for takeover/heartbeat/input/release.
//!
//! Failure policy is deliberately asymmetric:
//! - a missing file means no takeover (free);
//! - an unreadable or corrupt file PARKS the agent (fail closed — a human
//!   may hold the cursor, and a corrupt file must never be read as "free");
//! - the release summary is best-effort (fail open — a missing note must
//!   never block a tool call).
//!
//! Writes are atomic (temp file + rename), so lock-free readers never see
//! torn JSON. Mutations that read-modify-write go through
//! [`with_takeover_locked`] (fs2 exclusive lock on a sidecar file).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Lease state file name inside the data dir.
pub const TAKEOVER_FILE: &str = "desktop-takeover.json";
/// Sidecar lock file for read-modify-write cycles.
const TAKEOVER_LOCK_FILE: &str = "desktop-takeover.lock";
/// How long a takeover lease lasts without a heartbeat.
pub const LEASE_TTL_MS: i64 = 5 * 60 * 1000;
/// A lease whose heartbeat lapsed longer than this may be claimed by a
/// new takeover. The agent stays parked until explicit release, a fresh
/// takeover, or hard expiry — never auto-resumed into a half-done
/// human flow.
pub const CLAIM_AFTER_MS: i64 = 30_000;
/// How long a release summary survives for injection into the next
/// computer-use tool result.
pub const RELEASE_NOTE_TTL_MS: i64 = 10 * 60 * 1000;

/// Wall-clock milliseconds since the Unix epoch.
///
/// Public re-export of the canonical `pantheon_api::logging::now_ms`; this
/// is part of the crate's public surface (the dashboard reads leases through
/// it), so it stays a named re-export rather than a private copy.
pub use pantheon_api::logging::now_ms;

/// A fresh lease id: 128-bit randomness, hex. Single-purpose bearer for
/// the takeover holder; never logged. (The dashboard token is the real
/// authentication boundary; the lease id distinguishes holders.)
pub fn new_lease_id() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// One human input action during a takeover, for the audit trail and the
/// release summary. `detail` is display-safe by construction — never typed
/// text (it may contain credentials); see the dashboard's input path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TakeoverAction {
    pub action: String,
    pub detail: String,
    pub at_ms: i64,
}

/// A live human takeover of the desktop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TakeoverLease {
    pub lease_id: String,
    pub holder: String,
    pub acquired_at_ms: i64,
    pub expires_at_ms: i64,
    pub last_heartbeat_ms: i64,
    #[serde(default)]
    pub actions: Vec<TakeoverAction>,
}

/// What the human did, kept briefly after release so the next
/// computer-use tool result can carry it into the run.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReleaseRecord {
    pub summary: String,
    pub at_ms: i64,
    #[serde(default)]
    pub consumed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TakeoverFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<TakeoverLease>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_release: Option<ReleaseRecord>,
}

/// What the agent loop should do about computer-use tools right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// No takeover: proceed.
    Free,
    /// A human holds (or may hold) the desktop: park computer-use tools.
    /// `since_ms` is the takeover start, or 0 when unknown (corrupt file).
    Parked { since_ms: i64 },
}

pub fn lease_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TAKEOVER_FILE)
}

/// True for a desktop driver's projected MCP tools
/// (`mcp_cua-driver_*`, `mcp_tapp_*`). Used by the agent-loop gate; the
/// dashboard drives the same tools through its own manager and is NOT
/// gated here.
pub fn is_computer_use_tool(name: &str) -> bool {
    name.starts_with(&format!("mcp_{}_", crate::computer::CUA_DRIVER_SERVER))
        || name.starts_with(&format!("mcp_{}_", crate::computer::TAPP_DRIVER_ID))
}

/// Agent-loop gate: consult the lease file. Lock-free (reads only);
/// atomic renames mean readers never see torn JSON.
pub fn check_takeover(data_dir: &Path) -> Gate {
    let raw = match std::fs::read_to_string(lease_path(data_dir)) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Gate::Free,
        // Unreadable (permissions, IO): fail closed — a human may hold
        // the cursor.
        Err(_) => return Gate::Parked { since_ms: 0 },
    };
    let file: TakeoverFile = match serde_json::from_str(&raw) {
        Ok(f) => f,
        // Corrupt: fail closed, same reasoning as unreadable.
        Err(_) => return Gate::Parked { since_ms: 0 },
    };
    match file.lease {
        Some(lease) if now_ms() < lease.expires_at_ms => Gate::Parked {
            since_ms: lease.acquired_at_ms,
        },
        _ => Gate::Free,
    }
}

/// Take (and consume) a pending release summary for injection into the
/// next computer-use tool result. Best-effort and fail-open: any problem
/// yields None, never an error, and a missing summary must never block
/// the tool call it would have annotated.
pub fn take_release_note(data_dir: &Path) -> Option<String> {
    with_takeover_locked(data_dir, |file| {
        let rec = file.last_release.as_mut()?;
        if rec.consumed || rec.summary.trim().is_empty() {
            return None;
        }
        if now_ms() - rec.at_ms > RELEASE_NOTE_TTL_MS {
            return None;
        }
        rec.consumed = true;
        Some(rec.summary.clone())
    })
    .ok()
    .flatten()
}

/// True when a new takeover may replace the current lease: no lease, the
/// lease hard-expired, or the heartbeat lapsed longer than CLAIM_AFTER_MS.
pub fn claimable(file: &TakeoverFile) -> bool {
    match &file.lease {
        None => true,
        Some(l) => {
            let now = now_ms();
            now >= l.expires_at_ms || now - l.last_heartbeat_ms > CLAIM_AFTER_MS
        }
    }
}

/// Read-modify-write under an exclusive cross-process lock. A missing or
/// corrupt file reads as default — replacement is the recovery path (the
/// agent gate independently fails closed on corrupt files, so a corrupt
/// file parks the agent until a human takes over fresh).
pub fn with_takeover_locked<T>(
    data_dir: &Path,
    f: impl FnOnce(&mut TakeoverFile) -> T,
) -> std::io::Result<T> {
    let lock_path = data_dir.join(TAKEOVER_LOCK_FILE);
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        // Lock-only file: never read, so no truncation needed; the
        // explicit `false` documents the intent (clippy
        // suspicious_open_options).
        .truncate(false)
        .open(&lock_path)?;
    fs2::FileExt::lock_exclusive(&lock_file)?;
    let result = (|| {
        let mut file: TakeoverFile = std::fs::read_to_string(lease_path(data_dir))
            .ok()
            .and_then(|r| serde_json::from_str(&r).ok())
            .unwrap_or_default();
        let out = f(&mut file);
        write_atomic(data_dir, &file)?;
        Ok(out)
    })();
    let _ = fs2::FileExt::unlock(&lock_file);
    result
}

fn write_atomic(data_dir: &Path, file: &TakeoverFile) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(file).map_err(std::io::Error::other)?;
    let tmp = data_dir.join(format!(".{TAKEOVER_FILE}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, lease_path(data_dir))?;
    Ok(())
}

/// Short human duration: "45s", "2m14s", "1h3m". For refusal messages.
pub fn fmt_duration(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{}s", s / 60, s % 60)
    } else {
        format!("{}h{}m", s / 3600, (s % 3600) / 60)
    }
}

/// Human-readable release summary from a lease's action history.
pub fn release_summary(lease: &TakeoverLease) -> String {
    let dur = fmt_duration(now_ms() - lease.acquired_at_ms);
    if lease.actions.is_empty() {
        return format!("controlled the desktop for {dur} (no input actions recorded)");
    }
    let parts: Vec<&str> = lease.actions.iter().map(|a| a.detail.as_str()).collect();
    // Cap the summary: the audit trail keeps the full history in the
    // file; the summary is for humans and tool results.
    let mut joined = parts.join(", ");
    const MAX: usize = 600;
    if joined.len() > MAX {
        joined.truncate(MAX);
        joined.push_str("...");
    }
    format!("controlled the desktop for {dur}: {joined}")
}

/// Tool-shaped refusal for a parked computer-use call. Tells the model
/// WHY it may not proceed so it can do other work or wait instead of
/// retrying in a tight loop.
pub fn parked_refusal(since_ms: i64) -> String {
    if since_ms > 0 {
        format!(
            "[desktop takeover] a human has control of the desktop (for the last {}); computer-use tools are parked until they release — do other work or wait instead of retrying",
            fmt_duration(now_ms() - since_ms)
        )
    } else {
        "[desktop takeover] a human may have control of the desktop (takeover state unreadable); computer-use tools are parked until the state clears — do other work or wait instead of retrying".to_string()
    }
}

/// Suffix appended to the next successful computer-use tool result after
/// a human released the desktop, so the run resumes with context.
pub fn release_note_suffix(summary: &str) -> String {
    format!("\n[desktop takeover] a human took over the desktop and released it: {summary}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-takeover-test-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    fn live_lease() -> TakeoverLease {
        let now = now_ms();
        TakeoverLease {
            lease_id: new_lease_id(),
            holder: "dashboard".to_string(),
            acquired_at_ms: now - 10_000,
            expires_at_ms: now + LEASE_TTL_MS,
            last_heartbeat_ms: now,
            actions: vec![],
        }
    }

    fn write(file_dir: &Path, file: &TakeoverFile) {
        with_takeover_locked(file_dir, |f| *f = file.clone()).expect("write");
    }

    #[test]
    fn missing_file_is_free() {
        let dir = scratch("missing");
        assert_eq!(check_takeover(&dir), Gate::Free);
        assert_eq!(take_release_note(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_lease_parks() {
        let dir = scratch("live");
        let lease = live_lease();
        let since = lease.acquired_at_ms;
        write(
            &dir,
            &TakeoverFile {
                lease: Some(lease),
                last_release: None,
            },
        );
        assert_eq!(check_takeover(&dir), Gate::Parked { since_ms: since });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn expired_lease_is_free() {
        let dir = scratch("expired");
        let now = now_ms();
        let lease = TakeoverLease {
            expires_at_ms: now - 1,
            last_heartbeat_ms: now - LEASE_TTL_MS - 1,
            ..live_lease()
        };
        write(
            &dir,
            &TakeoverFile {
                lease: Some(lease),
                last_release: None,
            },
        );
        assert_eq!(check_takeover(&dir), Gate::Free);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_parks_fail_closed() {
        let dir = scratch("corrupt");
        std::fs::write(lease_path(&dir), b"{not json").expect("write");
        assert_eq!(check_takeover(&dir), Gate::Parked { since_ms: 0 });
        // ... but the dashboard can still recover by taking over fresh:
        // with_takeover_locked reads corrupt as default.
        with_takeover_locked(&dir, |f| {
            assert!(f.lease.is_none(), "corrupt reads as default under lock");
        })
        .expect("locked read");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_heartbeat_is_claimable_but_still_parked() {
        let dir = scratch("stale");
        let now = now_ms();
        let lease = TakeoverLease {
            last_heartbeat_ms: now - CLAIM_AFTER_MS - 1,
            ..live_lease()
        };
        let file = TakeoverFile {
            lease: Some(lease),
            last_release: None,
        };
        // Claimable by a new takeover ...
        assert!(claimable(&file));
        // ... yet the agent stays parked until that happens (never
        // auto-resumed into a half-done human flow).
        write(&dir, &file);
        assert!(matches!(check_takeover(&dir), Gate::Parked { .. }));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn release_note_taken_once_then_consumed() {
        let dir = scratch("note");
        write(
            &dir,
            &TakeoverFile {
                lease: None,
                last_release: Some(ReleaseRecord {
                    summary: "did the thing".to_string(),
                    at_ms: now_ms(),
                    consumed: false,
                }),
            },
        );
        assert_eq!(take_release_note(&dir).as_deref(), Some("did the thing"));
        assert_eq!(
            take_release_note(&dir),
            None,
            "second take must be empty (consumed)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_release_note_ignored() {
        let dir = scratch("stale-note");
        write(
            &dir,
            &TakeoverFile {
                lease: None,
                last_release: Some(ReleaseRecord {
                    summary: "old".to_string(),
                    at_ms: now_ms() - RELEASE_NOTE_TTL_MS - 1,
                    consumed: false,
                }),
            },
        );
        assert_eq!(take_release_note(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn computer_use_tool_names_match() {
        assert!(is_computer_use_tool("mcp_cua-driver_click"));
        assert!(is_computer_use_tool("mcp_cua-driver_screenshot"));
        assert!(!is_computer_use_tool("mcp_other-server_click"));
        assert!(!is_computer_use_tool("shell.execute"));
        assert!(!is_computer_use_tool("cua-driver_click"));
    }

    #[test]
    fn lease_ids_unique_and_hex() {
        let a = new_lease_id();
        let b = new_lease_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn release_summary_mentions_duration_and_actions() {
        let mut lease = live_lease();
        lease.actions.push(TakeoverAction {
            action: "click".to_string(),
            detail: "click at (120, 340)".to_string(),
            at_ms: now_ms(),
        });
        lease.actions.push(TakeoverAction {
            action: "type".to_string(),
            detail: "typing into a field".to_string(),
            at_ms: now_ms(),
        });
        let s = release_summary(&lease);
        assert!(s.contains("click at (120, 340)"), "{s}");
        assert!(s.contains("typing into a field"), "{s}");
        assert!(!s.contains("password"), "typed text must never leak: {s}");
    }
}
