//! Root-owned atomic delegation budget (Umar's Decision B).
//!
//! One [`RunBudget`] lives on the ROOT run of a delegation tree:
//! `RunBudget { max_delegations: 8, delegations_used: 0 }`. Every
//! `delegate()` resolves the root for the calling run and atomically
//! increments `delegations_used`; when `used >= max` the delegation is
//! rejected. The budget is never reset when a child run is created
//! children share the root's counter.
//!
//! A "delegation" counts only on **successful descendant session
//! creation**: failed or rejected attempts consume nothing. The
//! [`DelegateBudgetStore::try_consume_delegation`] contract is therefore:
//!
//! 1. Call it AFTER the child session is successfully created
//!    (`build_delegate_session` returned `Ok`).
//! 2. It returns a [`DelegationGuard`]. Keep the guard alive while the
//!    remaining spawn wiring runs.
//! 3. When the child is observable/running, call
//!    [`DelegationGuard::commit`] - the budget stays consumed.
//! 4. If anything between creation and commit fails, just let the guard
//!    drop (or call [`DelegateBudgetStore::release_delegation`]): the
//!    increment is rolled back and the child→root link is removed.
//!
//! Per-run stats are tracked separately for observability: every
//! successfully created child run is linked to its root
//! (`parent_of[child_run_id] = root_run_id`), readable via
//! [`DelegateBudgetStore::stats`]. Enforcement always reads the root's
//! [`RunBudget`], never the per-run links.
//!
//! ## Storage choice
//!
//! The budget lives in process memory (this store), NOT in the ledger's
//! `runs` table. Rationale:
//!
//! * The budget is a live-process atomic-enforcement concern. A
//!   delegation tree cannot survive a process restart - child sessions
//!   run on live threads - so a persisted counter has no coherent
//!   cross-restart semantics; the first run after a restart would read a
//!   stale `delegations_used` for a tree that no longer exists.
//! * Atomicity here means a CAS loop on an [`AtomicU64`]: concurrent
//!   delegates can never overshoot `max`. Doing the same through SQLite
//!   would mean `BEGIN IMMEDIATE` transactions serializing every delegate
//!   call on the ledger lock, plus a forward-only migration for columns
//!   that are dead weight the moment the process exits.
//! * This is a like-for-like replacement of the existing accounting
//!   location: the current cap (`delegations_used:
//!   Arc<Mutex<HashMap<String, u32>>>` on the session) is already
//!   process-local memory; this module just moves it to root ownership
//!   with atomic increments.
//! * Durable observability already exists: the ledger records
//!   `AgentSpawned` / `AgentCompleted` per run. If a durable
//!   parent→root link is ever wanted, a `parent_run_id` ledger column
//!   could mirror [`DelegateBudgetStore`]'s in-memory linkage
//!   enforcement would still read the root budget here.
//!
//! ## Wiring (done by the parent orchestrator, NOT this module)
//!
//! * Add `pub mod delegate_budget;` to `crates/pantheon-runtime/src/lib.rs`.
//! * In the blocking `delegate` path (`run_delegate_child` in
//!   `session.rs`): immediately after
//!   `let child_session = build_delegate_session(...)?;` succeeds, call
//!   `store.try_consume_delegation(&driver.run_id, &child_run)`; keep the
//!   guard; call `guard.commit()` after the `AgentSpawned` emit succeeds.
//!   Map [`BudgetExceeded`] to a `PantheonError` with code
//!   `DELEGATE_CAP_EXCEEDED` (mirroring the existing cap error).
//! * In the threaded path (`SessionSpawner::spawn_handle` in
//!   `session.rs`): same, right after `build_delegate_session(...)?;`,
//!   and `guard.commit()` after `self.registry.spawn_child(...)` returns
//!   `Ok` - a registry rejection rolls the budget back via `Drop`.
//! * Ensure the root budget exists before the first delegation of a run:
//!   `store.ensure_budget(&run_id, max_delegations)` at turn/driver
//!   setup. Re-ensuring never resets an existing budget.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// The delegation budget owned by one root run.
///
/// `max_delegations` is set once (from `[budget].max_delegations`,
/// default [`pantheon_api::config::DEFAULT_MAX_DELEGATIONS`]) and never
/// changed afterwards; `delegations_used` is incremented atomically, one
/// per successfully created descendant session, across the whole tree.
pub struct RunBudget {
    max_delegations: u32,
    delegations_used: AtomicU64,
}

impl RunBudget {
    fn new(max_delegations: u32) -> Self {
        Self {
            max_delegations,
            delegations_used: AtomicU64::new(0),
        }
    }

    /// Current `(used, max)` snapshot for observability.
    pub fn snapshot(&self) -> (u64, u32) {
        (
            self.delegations_used.load(Ordering::Acquire),
            self.max_delegations,
        )
    }
}

/// Rejection returned when the root budget is exhausted: `used >= max`.
///
/// The wiring maps this to a `PantheonError` with code
/// `DELEGATE_CAP_EXCEEDED`, mirroring the existing per-run cap error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExceeded {
    /// The root run whose budget was exhausted.
    pub root_run_id: String,
    /// Delegations consumed at rejection time.
    pub used: u64,
    /// The configured cap.
    pub max: u32,
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "run {} already used {} of {} delegate calls",
            self.root_run_id, self.used, self.max
        )
    }
}

/// Per-run observability snapshot: which root a run belongs to and how
/// much of that root's budget is consumed. Enforcement never reads this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunBudgetStats {
    /// The run this snapshot was taken for.
    pub run_id: String,
    /// The root of the delegation tree this run belongs to. A run that
    /// has never delegated (and was never created as a child) is its
    /// own root.
    pub root_run_id: String,
    /// The recorded parent link (`Some(root)` for a child created by
    /// [`DelegateBudgetStore::try_consume_delegation`], `None` for roots
    /// and unknown runs).
    pub parent_run_id: Option<String>,
    /// Delegations consumed against the root budget.
    pub delegations_used: u64,
    /// The root budget's cap.
    pub max_delegations: u32,
}

/// RAII rollback token from
/// [`DelegateBudgetStore::try_consume_delegation`].
///
/// Dropping the guard without calling [`DelegationGuard::commit`]
/// releases the consumed slot and removes the child→root link - the
/// "child creation failed partway" path. Call [`DelegationGuard::commit`]
/// once the child session is running/observable; the budget then stays
/// consumed even if the child later fails its task (a failed *task* is
/// not a failed *creation*).
pub struct DelegationGuard<'a> {
    store: &'a DelegateBudgetStore,
    child_run_id: String,
    committed: bool,
}

impl fmt::Debug for DelegationGuard<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DelegationGuard")
            .field("child_run_id", &self.child_run_id)
            .field("committed", &self.committed)
            .finish()
    }
}

impl DelegationGuard<'_> {
    /// Commit the delegation: the descendant session was successfully
    /// created and is now running, so the budget slot stays consumed.
    pub fn commit(mut self) {
        self.committed = true;
        // `self` drops here with `committed == true`: no rollback.
    }

    /// The child run id this guard holds a slot for.
    pub fn child_run_id(&self) -> &str {
        &self.child_run_id
    }
}

impl Drop for DelegationGuard<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.store.release_delegation(&self.child_run_id);
        }
    }
}

/// Root-owned delegation budgets for every live delegation tree.
///
/// Cheap to share: clone the [`std::sync::Arc`] around one instance (or
/// use [`DelegateBudgetStore::global`]) and hand it to every
/// [`DelegateDriver`](crate::session::DelegateDriver)-equivalent that
/// needs to enforce the cap.
pub struct DelegateBudgetStore {
    /// Root run id → its budget. Insert-if-absent only; entries are
    /// never replaced or reset, so a child's driver cannot clobber the
    /// root's counter.
    budgets: Mutex<HashMap<String, RunBudget>>,
    /// Child run id → root run id. Written once per successful
    /// consumption, removed on rollback. Observability only;
    /// enforcement walks this to find the root, then reads the budget.
    parent_of: Mutex<HashMap<String, String>>,
}

impl DelegateBudgetStore {
    /// An empty store. Tests use fresh instances; production wiring uses
    /// [`DelegateBudgetStore::global`].
    pub fn new() -> Self {
        Self {
            budgets: Mutex::new(HashMap::new()),
            parent_of: Mutex::new(HashMap::new()),
        }
    }

    /// The process-wide store the `delegate()` paths share.
    pub fn global() -> &'static Self {
        static GLOBAL: OnceLock<DelegateBudgetStore> = OnceLock::new();
        GLOBAL.get_or_init(DelegateBudgetStore::new)
    }

    /// Test-only escape hatch: drop every budget and link.
    #[cfg(test)]
    fn clear(&self) {
        self.budgets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.parent_of
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Ensure a budget exists for the root of `run_id`, creating it with
    /// `max_delegations` when absent. Returns `true` when a budget was
    /// created, `false` when one already existed.
    ///
    /// Never resets: an existing budget keeps its `max_delegations` and
    /// its `delegations_used`, even if this is called again with a
    /// different max. Because the root is resolved first, a nested
    /// child's driver setup (`ensure_budget(child_id, ...)`) lands on
    /// the same root budget instead of forking a new one. The wiring
    /// calls this once per turn/driver setup with the run's own id.
    pub fn ensure_budget(&self, run_id: &str, max_delegations: u32) -> bool {
        let root_run_id = self.resolve_root(run_id);
        let mut budgets = self.budgets.lock().unwrap_or_else(|e| e.into_inner());
        if budgets.contains_key(&root_run_id) {
            return false;
        }
        budgets.insert(root_run_id, RunBudget::new(max_delegations));
        true
    }

    /// Resolve the root run for any run id by walking the child→root
    /// links. A run with no recorded parent - a root, or a run the
    /// store has never seen - resolves to itself. Cycle-safe.
    pub fn resolve_root(&self, run_id: &str) -> String {
        let links = self.parent_of.lock().unwrap_or_else(|e| e.into_inner());
        let mut current = run_id.to_string();
        // Links are only ever written child → root (depth 1), but walk
        // the chain anyway so a future multi-hop linkage still resolves.
        let mut hops = 0usize;
        while let Some(parent) = links.get(&current) {
            if parent == &current || hops > 1024 {
                break; // self-link or absurd chain: fail closed on the id itself
            }
            current = parent.clone();
            hops += 1;
        }
        current
    }

    /// Atomically consume one delegation slot against the root budget
    /// for `caller_run_id`, recording `child_run_id` as a descendant of
    /// that root.
    ///
    /// Call this AFTER the descendant session is successfully created.
    /// The increment is a CAS loop on the root's counter: concurrent
    /// delegates can never overshoot `max`. When `used >= max` at claim
    /// time, nothing is consumed and [`BudgetExceeded`] is returned
    /// failed/rejected attempts never burn budget.
    ///
    /// On success the child→root link is recorded (per-run observability;
    /// nested delegates resolve the same root) and a
    /// [`DelegationGuard`] is returned: [`DelegationGuard::commit`] it
    /// once the child is running, or let it drop to roll back.
    ///
    /// If no budget exists for the resolved root (wiring has not called
    /// [`DelegateBudgetStore::ensure_budget`] on this path), one is
    /// created lazily with
    /// [`pantheon_api::config::DEFAULT_MAX_DELEGATIONS`] so delegation
    /// keeps working instead of failing closed on bookkeeping.
    pub fn try_consume_delegation(
        &self,
        caller_run_id: &str,
        child_run_id: &str,
    ) -> Result<DelegationGuard<'_>, BudgetExceeded> {
        let root_run_id = self.resolve_root(caller_run_id);
        // Fetch-or-create the root budget. The entry is never replaced
        // afterwards, so the reference-free CAS below always hits the
        // root's one true counter.
        let mut budgets = self.budgets.lock().unwrap_or_else(|e| e.into_inner());
        let budget = budgets
            .entry(root_run_id.clone())
            .or_insert_with(|| RunBudget::new(pantheon_api::config::DEFAULT_MAX_DELEGATIONS));
        // Atomic check-and-increment: exactly one winner per slot, no
        // overshoot under contention.
        loop {
            let used = budget.delegations_used.load(Ordering::Acquire);
            if used >= budget.max_delegations as u64 {
                return Err(BudgetExceeded {
                    root_run_id: root_run_id.clone(),
                    used,
                    max: budget.max_delegations,
                });
            }
            match budget.delegations_used.compare_exchange_weak(
                used,
                used + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(_) => continue,
            }
        }
        drop(budgets);
        // Record the child → root link for observability (nested
        // delegates resolve this root) and for rollback symmetry.
        self.parent_of
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(child_run_id.to_string(), root_run_id);
        Ok(DelegationGuard {
            store: self,
            child_run_id: child_run_id.to_string(),
            committed: false,
        })
    }

    /// Roll back one consumed slot: decrement the root's
    /// `delegations_used` (saturating at zero) and remove the
    /// child→root link. Called automatically when a
    /// [`DelegationGuard`] drops uncommitted - "child creation failed
    /// partway" - and available for manual use.
    ///
    /// A no-op for unknown child run ids.
    pub fn release_delegation(&self, child_run_id: &str) {
        let root = self
            .parent_of
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(child_run_id);
        let Some(root_run_id) = root else { return };
        let budgets = self.budgets.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(budget) = budgets.get(&root_run_id) {
            let used = &budget.delegations_used;
            let mut current = used.load(Ordering::Acquire);
            loop {
                if current == 0 {
                    break;
                }
                match used.compare_exchange_weak(
                    current,
                    current - 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => break,
                    Err(actual) => current = actual,
                }
            }
        }
    }

    /// Observability snapshot for one run: its root, the recorded
    /// parent link, and the root budget's `(used, max)`. Returns `None`
    /// only when the run is unknown AND no budget row exists for it
    /// i.e. the store has never seen this run id at all.
    pub fn stats(&self, run_id: &str) -> Option<RunBudgetStats> {
        let parent_run_id = self
            .parent_of
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned();
        let root_run_id = self.resolve_root(run_id);
        let budgets = self.budgets.lock().unwrap_or_else(|e| e.into_inner());
        let budget = budgets.get(&root_run_id)?;
        let (delegations_used, max_delegations) = budget.snapshot();
        Some(RunBudgetStats {
            run_id: run_id.to_string(),
            root_run_id,
            parent_run_id,
            delegations_used,
            max_delegations,
        })
    }

    /// Current `delegations_used` for the root of `run_id`, or `None`
    /// when the store has no budget for it.
    pub fn delegations_used(&self, run_id: &str) -> Option<u64> {
        let root = self.resolve_root(run_id);
        self.budgets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&root)
            .map(|b| b.delegations_used.load(Ordering::Acquire))
    }
}

impl Default for DelegateBudgetStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::{Arc, Barrier};

    /// Budget of N allows exactly N successful consumptions, counting
    /// across nested children that all resolve the same root.
    #[test]
    fn root_budget_allows_exactly_n_across_nested_children() {
        let store = DelegateBudgetStore::new();
        assert!(store.ensure_budget("root", 3));

        // root -> c1
        store
            .try_consume_delegation("root", "c1")
            .expect("1st delegation")
            .commit();
        // c1 -> c2 (caller is a child: must resolve root, not reset)
        store
            .try_consume_delegation("c1", "c2")
            .expect("2nd delegation (nested)")
            .commit();
        // c2 -> c3 (two levels deep, still the root's budget)
        store
            .try_consume_delegation("c2", "c3")
            .expect("3rd delegation (nested x2)")
            .commit();

        assert_eq!(store.delegations_used("root"), Some(3));
        assert_eq!(store.delegations_used("c3"), Some(3)); // resolves root
        assert_eq!(store.resolve_root("c3"), "root");
        assert_eq!(store.resolve_root("c1"), "root");
    }

    /// The (N+1)th delegation is rejected and consumes nothing.
    #[test]
    fn n_plus_one_is_rejected_without_consuming() {
        let store = DelegateBudgetStore::new();
        store.ensure_budget("root", 2);
        store.try_consume_delegation("root", "c1").unwrap().commit();
        store.try_consume_delegation("c1", "c2").unwrap().commit();

        let err = store
            .try_consume_delegation("c2", "c3")
            .expect_err("3rd of max 2 must be rejected");
        assert_eq!(
            err,
            BudgetExceeded {
                root_run_id: "root".to_string(),
                used: 2,
                max: 2,
            }
        );
        // Failed attempt burned nothing.
        assert_eq!(store.delegations_used("root"), Some(2));
        // The rejected child was never linked to the root.
        assert_eq!(store.resolve_root("c3"), "c3");
        assert!(store.stats("c3").is_none());
    }

    /// A guard dropped without commit rolls the slot back and unlinks
    /// the child: "child creation failed partway".
    #[test]
    fn uncommitted_guard_rolls_back() {
        let store = DelegateBudgetStore::new();
        store.ensure_budget("root", 2);
        store.try_consume_delegation("root", "c1").unwrap().commit();
        {
            let _guard = store.try_consume_delegation("root", "c2").unwrap();
            assert_eq!(store.delegations_used("root"), Some(2));
            // Guard drops here uncommitted.
        }
        assert_eq!(store.delegations_used("root"), Some(1));
        assert_eq!(store.resolve_root("c2"), "c2"); // link removed
    }

    /// Manual rollback path is a no-op for unknown children and
    /// saturates at zero instead of underflowing.
    #[test]
    fn release_is_safe_on_unknown_children() {
        let store = DelegateBudgetStore::new();
        store.ensure_budget("root", 1);
        store.release_delegation("never-existed"); // no panic, no-op
        assert_eq!(store.delegations_used("root"), Some(0));
        // Double release of the same child cannot drive the counter negative.
        store.try_consume_delegation("root", "c1").unwrap().commit();
        store.release_delegation("c1");
        store.release_delegation("c1");
        assert_eq!(store.delegations_used("root"), Some(0));
    }

    /// Re-ensuring a budget never resets it: children (and repeated
    /// driver setups) share the root's counter and cap.
    #[test]
    fn budget_is_never_reset_by_reensure() {
        let store = DelegateBudgetStore::new();
        assert!(store.ensure_budget("root", 3));
        store.try_consume_delegation("root", "c1").unwrap().commit();
        // A nested child's driver setup must not reset or raise the cap.
        assert!(!store.ensure_budget("root", 99));
        assert!(!store.ensure_budget("c1", 99));
        let stats = store.stats("root").unwrap();
        assert_eq!(stats.max_delegations, 3);
        assert_eq!(stats.delegations_used, 1);
    }

    /// Per-run observability: each child records parent = root, while
    /// enforcement reads the root budget.
    #[test]
    fn stats_track_per_run_parent_linkage() {
        let store = DelegateBudgetStore::new();
        store.ensure_budget("root", 8);
        store.try_consume_delegation("root", "c1").unwrap().commit();

        let child = store.stats("c1").expect("child stats");
        assert_eq!(child.run_id, "c1");
        assert_eq!(child.root_run_id, "root");
        assert_eq!(child.parent_run_id.as_deref(), Some("root"));
        assert_eq!(child.delegations_used, 1);
        assert_eq!(child.max_delegations, 8);

        let root = store.stats("root").expect("root stats");
        assert_eq!(root.root_run_id, "root");
        assert_eq!(root.parent_run_id, None);
        assert_eq!(root.delegations_used, 1);
    }

    /// Unknown roots are treated as their own root with the default
    /// cap, so delegation keeps working on paths the wiring has not
    /// explicitly seeded yet.
    #[test]
    fn unknown_run_lazily_gets_default_budget() {
        let store = DelegateBudgetStore::new();
        store
            .try_consume_delegation("lonely", "kid")
            .unwrap()
            .commit();
        let stats = store.stats("lonely").unwrap();
        assert_eq!(stats.root_run_id, "lonely");
        assert_eq!(
            stats.max_delegations,
            pantheon_api::config::DEFAULT_MAX_DELEGATIONS
        );
        assert_eq!(stats.delegations_used, 1);
    }

    /// Concurrent delegates never overshoot max: with max = 50 and 200
    /// racing attempts, exactly 50 succeed and `used` ends at 50.
    #[test]
    fn concurrent_consumes_never_exceed_max() {
        const MAX: u32 = 50;
        const THREADS: usize = 8;
        const PER_THREAD: usize = 25; // 200 attempts total
        let store = Arc::new(DelegateBudgetStore::new());
        store.ensure_budget("root", MAX);
        let barrier = Arc::new(Barrier::new(THREADS));
        let ok_count = Arc::new(AtomicU64::new(0));
        let err_count = Arc::new(AtomicU64::new(0));

        std::thread::scope(|s| {
            for t in 0..THREADS {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let ok_count = Arc::clone(&ok_count);
                let err_count = Arc::clone(&err_count);
                s.spawn(move || {
                    barrier.wait(); // release all threads at once: maximum contention
                    for i in 0..PER_THREAD {
                        let child = format!("t{t}_c{i}");
                        match store.try_consume_delegation("root", &child) {
                            Ok(guard) => {
                                guard.commit();
                                ok_count.fetch_add(1, AtomicOrdering::Relaxed);
                            }
                            Err(_) => {
                                err_count.fetch_add(1, AtomicOrdering::Relaxed);
                            }
                        }
                    }
                });
            }
        });

        assert_eq!(ok_count.load(AtomicOrdering::Relaxed), u64::from(MAX));
        assert_eq!(
            err_count.load(AtomicOrdering::Relaxed),
            (THREADS * PER_THREAD) as u64 - u64::from(MAX)
        );
        assert_eq!(store.delegations_used("root"), Some(u64::from(MAX)));
    }

    /// The global store is a single shared instance.
    #[test]
    fn global_store_is_shared() {
        let a = DelegateBudgetStore::global() as *const _;
        let b = DelegateBudgetStore::global() as *const _;
        assert_eq!(a, b);
        // Leave it clean for other tests in this binary.
        DelegateBudgetStore::global().clear();
    }
}
