//! Session lifecycle: one gsd-browser session per Pantheon run, with
//! idle garbage collection.
//!
//! gsd-browser isolates state per `--session <name>` (its own daemon and
//! browser). Pantheon runs are identified by run ids that may contain
//! characters unsafe for a session name, so [`sanitize_session_name`]
//! maps run id → session name. [`SessionManager`] records last-use time
//! per session and, on every touch, stops the daemons of sessions idle
//! longer than the configured timeout.
//!
//! ## `daemon stop` (verified live)
//!
//! GC calls [`BrowserBackend::stop_daemon`], which this crate implements
//! as `gsd-browser --session <s> daemon stop`. Verified against
//! gsd-browser 0.1.24 on 2026-09-29: the subcommand exists, prints
//! `Daemon stopped.`, and exits 0. If a future CLI ever drops it, the
//! stop fails, the failure is logged to stderr, the session entry is
//! *kept* (so a later touch can retry), and nothing else breaks - GC
//! degrades to bookkeeping, never to an error.

use super::backend::BrowserBackend;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default idle timeout (seconds) after which an unused session's daemon
/// is stopped: 15 minutes.
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 900;

/// Maximum gsd session-name length. gsd-browser accepts arbitrary names;
/// 64 keeps them readable in `ps` output and safely under any OS-level
/// socket-path limits the daemon may impose.
const MAX_SESSION_LEN: usize = 64;

/// Map a Pantheon run id to a safe gsd session name: keep
/// `[A-Za-z0-9_-]`, replace everything else with `-`, truncate to 64
/// chars. Empty results become `"session"`.
pub fn sanitize_session_name(run_id: &str) -> String {
    let mut s: String = run_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.len() > MAX_SESSION_LEN {
        s.truncate(MAX_SESSION_LEN);
    }
    if s.is_empty() {
        return "session".to_string();
    }
    s
}

/// Tracks per-session last-use times and GCs idle daemons.
///
/// `Send + Sync` so one manager can be shared across all tool closures
/// registered for a process.
pub struct SessionManager {
    last_use: Mutex<HashMap<String, Instant>>,
    idle_timeout: Duration,
}

impl SessionManager {
    pub fn new(idle_timeout_secs: u64) -> Self {
        Self {
            last_use: Mutex::new(HashMap::new()),
            idle_timeout: Duration::from_secs(idle_timeout_secs.max(1)),
        }
    }

    /// Record use of `session` now, and stop the daemons of any *other*
    /// sessions idle longer than the timeout. Daemon stops are
    /// best-effort: failures are logged to stderr and the entry is kept
    /// so a later touch retries. Never panics, never propagates.
    pub fn touch(&self, session: &str, backend: &dyn BrowserBackend) {
        let mut map = self.last_use.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        map.insert(session.to_string(), now);
        let idle: Vec<String> = map
            .iter()
            .filter(|(name, last)| {
                name.as_str() != session && now.duration_since(**last) > self.idle_timeout
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in idle {
            match backend.stop_daemon(&name) {
                Ok(()) => {
                    map.remove(&name);
                }
                Err(e) => {
                    // Best-effort: keep the entry so the next touch
                    // retries, and say so loudly enough to debug.
                    eprintln!(
                        "[pantheon-web] idle GC: failed to stop daemon for session \
                         '{name}' (will retry on next touch): {e}"
                    );
                }
            }
        }
    }
}
