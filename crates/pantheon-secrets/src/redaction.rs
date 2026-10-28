//! Exact-value redaction registry.
//!
//! Complements pattern-based redaction (see `pantheon-api` logging): instead
//! of guessing what a secret looks like from its shape, the registry records
//! the exact bytes the broker handed out and scrubs those substrings from
//! model-facing text. Registration happens at fill time, before the value can
//! appear in any tool result, so a later echo can never reach the model.
//!
//! Registries are keyed by scope (a data dir or profile home) and shared
//! process-wide per scope: a broker that registers and a session that scrubs
//! stay in sync without plumbing handles between them. Scopes never share
//! entries - scrubbing scope B's output with scope A's values would confirm
//! to B that those bytes exist.
//!
//! Fail-open on a poisoned lock is a deliberate tradeoff, documented
//! here rather than changed: `register` skips and `scrub` returns text
//! unchanged when the per-scope lock is poisoned (a thread panicked
//! while holding it). Failing the caller instead would break unrelated
//! model-facing flows over a best-effort defence-in-depth layer -
//! pattern-based redaction in `pantheon-api` logging still applies -
//! and there is no safe state to recover: the guarded deque may be
//! half-mutated. Poisoning is rare (it needs a panic mid-register /
//! mid-scrub) and loud in logs via the panic itself.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

/// Upper bound on registered values per scope. A fill-heavy session evicts
/// its oldest entries instead of growing without limit. Eviction is
/// loud (stderr): silently losing scrubbing for an early secret would
/// leave that secret unscrubbed with no trace.
const MAX_VALUES_PER_SCOPE: usize = 64;

/// Minimum length of a registered secret. A 1-char "secret" would scrub
/// benign text everywhere it appears; below this length the value is
/// not distinctive enough to scrub safely.
const MIN_SECRET_LEN: usize = 4;

/// Placeholder substituted for a registered secret value.
const REDACTED: &str = "[redacted-secret]";

/// Process-wide scope-keyed registries. Insertions into the map are
/// infallible, so a poisoned map lock is recovered rather than treated as
/// fatal; the per-scope value lock below is where fail-open applies.
static REGISTRIES: LazyLock<Mutex<HashMap<String, Arc<RedactionRegistry>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Thread-safe registry of exact secret values for one scope.
#[derive(Debug)]
pub struct RedactionRegistry {
    scope: String,
    values: Mutex<VecDeque<String>>,
}

impl RedactionRegistry {
    /// Registry for `scope`, shared process-wide: every caller asking for
    /// the same scope sees the same entries.
    pub fn for_scope(scope: &str) -> Arc<Self> {
        let mut map = REGISTRIES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(scope.to_string())
            .or_insert_with(|| {
                Arc::new(Self {
                    scope: scope.to_string(),
                    values: Mutex::new(VecDeque::new()),
                })
            })
            .clone()
    }

    /// The scope this registry was created for.
    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Record an exact secret value for later scrubbing. Empty values
    /// are ignored: matching the empty string would hit everywhere.
    /// Values shorter than [`MIN_SECRET_LEN`] are ignored for the same
    /// reason at a smaller scale: a 1-char secret would scrub benign
    /// text wherever that character appears. Re-registering a value
    /// refreshes its recency so live secrets are evicted last. Fails
    /// open: a poisoned lock skips registration instead of failing the
    /// caller (see the module docs for the tradeoff).
    pub fn register(&self, value: &str) {
        if value.len() < MIN_SECRET_LEN {
            return;
        }
        let Ok(mut values) = self.values.lock() else {
            return; // poisoned: fail open, skip registration
        };
        if let Some(pos) = values.iter().position(|v| v == value) {
            values.remove(pos);
        }
        values.push_back(value.to_string());
        while values.len() > MAX_VALUES_PER_SCOPE {
            values.pop_front();
            // Loud, but never with the value: eviction means an early
            // secret is no longer scrubbed, and the operator should know.
            eprintln!(
                "redaction: scope {} evicted its oldest registered secret (cap {MAX_VALUES_PER_SCOPE})",
                self.scope
            );
        }
    }

    /// Replace every registered value in `text` with the placeholder.
    /// Longest values go first so a registered substring never shadows its
    /// registered superstring. Fails open: a poisoned lock returns `text`
    /// unchanged rather than failing the caller.
    pub fn scrub(&self, text: &str) -> String {
        let Ok(values) = self.values.lock() else {
            return text.to_string(); // poisoned: fail open, no scrub
        };
        if values.is_empty() {
            return text.to_string();
        }
        let mut ordered: Vec<&str> = values.iter().map(String::as_str).collect();
        ordered.sort_by_key(|a| std::cmp::Reverse(a.len()));
        let mut out = text.to_string();
        for value in ordered {
            if out.contains(value) {
                out = out.replace(value, REDACTED);
            }
        }
        out
    }

    /// Number of values currently registered.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.values.lock().map(|v| v.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_value_is_scrubbed() {
        let reg = RedactionRegistry::for_scope("test-redact-exact");
        reg.register("alpha-bravo-123");
        assert_eq!(
            reg.scrub("leaked: alpha-bravo-123 end"),
            "leaked: [redacted-secret] end"
        );
        assert_eq!(
            reg.scrub("nothing sensitive here"),
            "nothing sensitive here"
        );
    }

    #[test]
    fn longest_value_wins_over_substring() {
        let reg = RedactionRegistry::for_scope("test-redact-longest");
        reg.register("abcd");
        reg.register("abcdef");
        // One placeholder for the whole superstring, not a nested mess.
        assert_eq!(reg.scrub("x abcdef y"), "x [redacted-secret] y");
        // The short value still scrubs on its own.
        assert_eq!(reg.scrub("x abcd y"), "x [redacted-secret] y");
    }

    /// Values below the minimum length are not registered at all: a
    /// 1-char secret would otherwise scrub benign text everywhere.
    #[test]
    fn short_values_are_not_registered() {
        let reg = RedactionRegistry::for_scope("test-redact-minlen");
        reg.register("x");
        reg.register("ab");
        reg.register("abc");
        assert_eq!(reg.len(), 0);
        assert_eq!(reg.scrub("x ab abc"), "x ab abc");
        reg.register("abcd");
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.scrub("x abcd y"), "x [redacted-secret] y");
    }

    #[test]
    fn oldest_entries_evict_past_bound() {
        let reg = RedactionRegistry::for_scope("test-redact-evict");
        for i in 0..70 {
            reg.register(&format!("token-{i:03}"));
        }
        assert_eq!(reg.len(), MAX_VALUES_PER_SCOPE);
        // The first six registrations fell off the front.
        assert!(
            !reg.scrub("token-000").contains(REDACTED),
            "evicted value must not scrub"
        );
        assert!(
            reg.scrub("token-069").contains(REDACTED),
            "newest value must scrub"
        );
    }

    #[test]
    fn scopes_do_not_cross_scrub() {
        let a = RedactionRegistry::for_scope("test-redact-scope-a");
        let b = RedactionRegistry::for_scope("test-redact-scope-b");
        a.register("scope-a-only-secret");
        assert_eq!(
            b.scrub("scope-a-only-secret"),
            "scope-a-only-secret",
            "scope B must not scrub scope A's values"
        );
        assert!(
            a.scrub("scope-a-only-secret").contains(REDACTED),
            "scope A still scrubs its own values"
        );
    }

    #[test]
    fn empty_value_is_ignored() {
        let reg = RedactionRegistry::for_scope("test-redact-empty");
        reg.register("");
        assert_eq!(reg.len(), 0);
        assert_eq!(reg.scrub("anything"), "anything");
    }

    #[test]
    fn poisoned_lock_fails_open() {
        let reg = RedactionRegistry::for_scope("test-redact-poison");
        reg.register("s3cr3t-value");
        // Poison the value lock.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = reg.values.lock().unwrap();
            panic!("intentional poison for test");
        }));
        // Scrub degrades to a no-op instead of failing or panicking.
        assert_eq!(
            reg.scrub("has s3cr3t-value inside"),
            "has s3cr3t-value inside"
        );
        // Registration is a silent no-op too.
        reg.register("another-value");
    }
}
