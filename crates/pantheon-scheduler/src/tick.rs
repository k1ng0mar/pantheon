//! Durable tick driver (§21).
//!
//! One code path owns the fire decision. For each due job it:
//!
//! 1. applies the job's [`OverlapPolicy`](crate::OverlapPolicy) against the
//!    in-flight set (same-process concurrency);
//! 2. wins a durable claim for the occurrence from the
//!    [`DurableClaimLedger`] - an atomic first-wins INSERT, so two ticks
//!    racing the same due job (two threads, two processes, or a restart
//!    replaying a minute) agree on exactly one winner;
//! 3. runs the job with its timeout: a run that outlives
//!    [`Job::effective_timeout_secs`](crate::Job::effective_timeout_secs)
//!    is abandoned - the tick stops waiting. Rust cannot kill a thread,
//!    so the abandoned run keeps going detached until it finishes on its
//!    own.
//!
//! A claim that cannot be persisted fails closed: the run must not start.
//! Execution itself is injected (`Arc<dyn Fn()>`), so the whole driver is
//! testable without a runtime.

use crate::durable::DurableClaimLedger;
use crate::{Job, OverlapPolicy};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Process-wide drain-occurrence sequence: recovery drain claim keys
/// must be unique per take, so two crashes in the same millisecond (or
/// two racing recoveries) cannot share a key.
static DRAIN_SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Key for one occurrence of a job.
///
/// `occurrence` must identify the fire instant (a millis stamp for cron and
/// interval jobs) so that two fires inside the same occurrence collapse to
/// one key.
pub fn occurrence_key(job: &Job, occurrence: &str) -> String {
    format!("{}{}", job.idempotency_key, occurrence)
}

/// Which occurrences have already been claimed.
///
/// In-memory on purpose: the caller persists keys in the ledger. A
/// recovered process rebuilds this from the ledger before the first tick.
#[derive(Debug, Default)]
pub struct ClaimLedger {
    claimed: HashSet<String>,
}

impl ClaimLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim an occurrence. `true` means this is the first claim and the run
    /// should start; `false` means it is a replay and must not run twice.
    pub fn claim(&mut self, key: &str) -> bool {
        self.claimed.insert(key.to_string())
    }

    /// Has this occurrence already had a run started?
    pub fn is_claimed(&self, key: &str) -> bool {
        self.claimed.contains(key)
    }

    /// Release a claim after a run ends, so the key is not held forever by
    /// long-lived processes.
    pub fn release(&mut self, key: &str) -> bool {
        self.claimed.remove(key)
    }

    pub fn len(&self) -> usize {
        self.claimed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claimed.is_empty()
    }
}

/// How one fired run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The executor returned.
    Completed,
    /// The run outlived the job's timeout and was abandoned: the tick
    /// stopped waiting. The thread keeps running detached until it
    /// finishes on its own.
    TimedOut,
    /// The executor panicked.
    Panicked,
    /// The executor returned normally but the task itself failed
    /// (app-level failure, e.g. the agent run errored). Never produced
    /// by the tick driver - the app records it when its executor
    /// reports a task error. Counts as a failure for run history.
    Failed,
    /// The run was superseded by an [`OverlapPolicy::Replace`] fire while
    /// it was still in flight.
    Replaced,
}

/// What one tick decided for one job.
#[derive(Debug)]
pub enum TickDecision {
    /// The claim was won and the run started. The receiver yields one
    /// [`RunOutcome`] per run this fire produces (a queue drain can
    /// produce a second one).
    Fired {
        completion: mpsc::Receiver<RunOutcome>,
    },
    /// The fire was deferred: the job was still running and its overlap
    /// policy is [`OverlapPolicy::Queue`]. It runs once more after the
    /// in-flight run finishes.
    Queued,
    /// Not due (or paused).
    NotDue,
    /// Another tick won the durable claim for this occurrence: a replay,
    /// and it must not run twice.
    SkippedClaimLost,
    /// Still running from an earlier fire and the overlap policy is
    /// [`OverlapPolicy::Skip`]. The caller logs this.
    SkippedOverlap,
    /// The claim could not be persisted. Fail closed: the run must not
    /// start.
    ClaimFailed(String),
}

/// Fresh paused-state predicate for queue drains: `Fn(job_id) -> bool`.
/// Installed by the app crate, which owns the job store - the driver
/// itself never touches the store, it just asks before draining.
pub type PausedCheck = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Owns the fire decision for every tick.
///
/// Share with `Arc`: [`TickDriver::tick_job`] takes `&Arc<Self>` so worker
/// threads can report back to the same driver.
pub struct TickDriver {
    ledger: DurableClaimLedger,
    /// Job ids with a run in flight in this process. The generation
    /// counter lets an [`OverlapPolicy::Replace`] fire disown the run it
    /// supersedes without racing its cleanup.
    in_flight: Mutex<HashMap<String, u64>>,
    /// Job ids that became due while running and asked to queue. Durable
    /// twin: the [`DurableClaimLedger`] drain queue - the in-memory set is
    /// the fast path, the store is what survives a crash.
    queued: Mutex<HashSet<String>>,
    paused_check: std::sync::RwLock<Option<PausedCheck>>,
}

impl TickDriver {
    pub fn new(ledger: DurableClaimLedger) -> Self {
        Self {
            ledger,
            in_flight: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashSet::new()),
            paused_check: std::sync::RwLock::new(None),
        }
    }

    /// Install the paused-state predicate consulted before draining a
    /// queued fire. Without one, drains assume "not paused" (tests, and
    /// the tick's own paused filter).
    pub fn set_paused_check(&self, check: PausedCheck) {
        if let Ok(mut g) = self.paused_check.write() {
            *g = Some(check);
        }
    }

    /// Lock the in-flight map. A poisoned mutex means a worker thread
    /// panicked while holding it, so the guarded state is untrustworthy:
    /// callers fail closed instead of panicking the tick path.
    fn lock_in_flight(&self) -> Option<std::sync::MutexGuard<'_, HashMap<String, u64>>> {
        self.in_flight.lock().ok()
    }

    /// Lock the queued set; see [`TickDriver::lock_in_flight`].
    fn lock_queued(&self) -> Option<std::sync::MutexGuard<'_, HashSet<String>>> {
        self.queued.lock().ok()
    }

    /// Fresh paused state for `id`, for queue drains. Fails closed: a
    /// poisoned check lock, or a check that reports paused, drops the
    /// drain instead of firing blind.
    fn is_paused(&self, id: &str) -> bool {
        match self.paused_check.read() {
            Ok(g) => g.as_ref().is_some_and(|check| check(id)),
            Err(_) => true,
        }
    }

    /// Attempt one fire of `job` at `now_ms`.
    ///
    /// `last_fire_ms` is the job's persisted last fire (drives
    /// [`Job::due`](crate::Job::due) and the occurrence stamp).
    /// `execute` runs the job; it is called at most once per won claim,
    /// on a worker thread - this method never blocks on the run itself.
    pub fn tick_job(
        self: &Arc<Self>,
        job: &Job,
        now_ms: i64,
        last_fire_ms: Option<i64>,
        execute: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> TickDecision {
        // Crash recovery for the durable drain queue: a previous process
        // may have died owing this job a queued fire. Atomic take - this
        // process now owns the owed fire.
        let recovered = match self.ledger.take_pending_drain(&job.id) {
            Ok(taken) => taken,
            Err(e) => {
                eprintln!("tick: drain-queue take failed for {}: {e}", job.id);
                false
            }
        };
        if recovered {
            if self.is_paused(&job.id) {
                eprintln!(
                    "tick: job {} owed a queued fire at shutdown; paused now, dropping it",
                    job.id
                );
            } else if !job.due(now_ms, last_fire_ms) && self.generation_of(&job.id).is_none() {
                // Not due and nothing running: nothing would drain a
                // seeded queue entry, so fire the owed run directly as a
                // drain-claimed fire, exactly like a worker drain does.
                return self.fire_drain(job, now_ms, execute);
            } else if let Some(mut queued) = self.lock_queued() {
                // Also due (or busy): seed the in-memory queue and let
                // the normal overlap path below drain the owed fire after
                // the natural fire, or behind the in-flight run.
                queued.insert(job.id.clone());
            }
        }
        if !job.due(now_ms, last_fire_ms) {
            return TickDecision::NotDue;
        }
        let Some(stamp) = job.occurrence_stamp(now_ms, last_fire_ms) else {
            return TickDecision::NotDue;
        };

        // Overlap gate first: cheaper than a claim write, and a skipped
        // fire must not consume an occurrence claim.
        //
        // Replace deliberately bumps the generation only AFTER winning the
        // claim below: the claim serializes racing replacers (same
        // occurrence, same key - exactly one wins), and a replacer that
        // loses the claim leaves the in-flight run untouched.
        let (generation, replacing) = {
            let mut in_flight = match self.lock_in_flight() {
                Some(g) => g,
                // Poisoned: the in-flight set is untrustworthy, so the
                // overlap decision can't be made - fail closed, don't fire.
                None => return TickDecision::ClaimFailed("tick state lock poisoned".to_string()),
            };
            match in_flight.get(&job.id).copied() {
                None => {
                    in_flight.insert(job.id.clone(), 0);
                    (0, false)
                }
                Some(gen) => match job.overlap {
                    OverlapPolicy::Skip => return TickDecision::SkippedOverlap,
                    OverlapPolicy::Replace => (gen, true),
                    OverlapPolicy::Queue => {
                        match self.lock_queued() {
                            Some(mut queued) => {
                                queued.insert(job.id.clone());
                            }
                            None => {
                                return TickDecision::ClaimFailed(
                                    "tick state lock poisoned".to_string(),
                                )
                            }
                        }
                        // Durable twin of the in-memory insert: a crash
                        // must not lose the owed fire. Best-effort - if
                        // the write fails the entry still queues for this
                        // process's lifetime; the failure is loud.
                        if let Err(e) = self.ledger.enqueue_drain(&job.id) {
                            eprintln!(
                                "tick: durable drain-queue write failed for {}: {e}; queued in-process only",
                                job.id
                            );
                        }
                        return TickDecision::Queued;
                    }
                },
            }
        };

        let key = occurrence_key(job, &stamp.to_string());
        let generation = match self.ledger.claim(&key) {
            Ok(true) => {
                if replacing {
                    let next = generation + 1;
                    match self.lock_in_flight() {
                        Some(mut in_flight) => {
                            in_flight.insert(job.id.clone(), next);
                        }
                        // Won the claim but can't record the generation:
                        // fail closed - don't start a run we can't track.
                        None => {
                            self.release_if_current(&job.id, generation);
                            return TickDecision::ClaimFailed(
                                "tick state lock poisoned".to_string(),
                            );
                        }
                    }
                    next
                } else {
                    generation
                }
            }
            Ok(false) => {
                // A replay: another tick won this occurrence. Only release
                // the slot we installed ourselves; a replacer that lost the
                // claim must not disturb the in-flight run.
                if !replacing {
                    self.release_if_current(&job.id, generation);
                    self.clear_queued(&job.id);
                    let _ = self.ledger.dequeue_drain(&job.id);
                }
                return TickDecision::SkippedClaimLost;
            }
            Err(e) => {
                if !replacing {
                    self.release_if_current(&job.id, generation);
                    self.clear_queued(&job.id);
                    let _ = self.ledger.dequeue_drain(&job.id);
                }
                return TickDecision::ClaimFailed(e.to_string());
            }
        };

        let (tx, rx) = mpsc::channel();
        let driver = Arc::clone(self);
        let job = job.clone();
        std::thread::spawn(move || driver.run_worker(job, generation, execute, tx));
        TickDecision::Fired { completion: rx }
    }

    /// Fire a drain-claimed run immediately: the crash-recovery path for
    /// a durable drain-queue leftover when the job is not otherwise due.
    /// Claims a unique drain occurrence (idempotent across restarts),
    /// installs the in-flight slot, and spawns the worker - the same
    /// shape as a natural fire, minus the occurrence claim.
    fn fire_drain(
        self: &Arc<Self>,
        job: &Job,
        now_ms: i64,
        execute: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> TickDecision {
        let seq = DRAIN_SEQ.fetch_add(1, Ordering::SeqCst);
        let key = occurrence_key(
            job,
            &format!("recovery:{now_ms}:{}:q{seq}", std::process::id()),
        );
        match self.ledger.claim(&key) {
            Ok(true) => {}
            Ok(false) => return TickDecision::SkippedClaimLost,
            Err(e) => return TickDecision::ClaimFailed(e.to_string()),
        }
        {
            let mut in_flight = match self.lock_in_flight() {
                Some(g) => g,
                // Won the claim but can't record the generation:
                // fail closed - don't start a run we can't track. (The
                // claim stays won; like the natural path, claims are
                // never released.)
                None => {
                    return TickDecision::ClaimFailed("tick state lock poisoned".to_string());
                }
            };
            if in_flight.contains_key(&job.id) {
                // Raced with a natural fire between the recovery check
                // and this lock: seed the queue and let the in-flight
                // run's worker drain the owed fire.
                drop(in_flight);
                if let Some(mut queued) = self.lock_queued() {
                    queued.insert(job.id.clone());
                }
                let _ = self.ledger.enqueue_drain(&job.id);
                return TickDecision::Queued;
            }
            in_flight.insert(job.id.clone(), 0);
        }
        let (tx, rx) = mpsc::channel();
        let driver = Arc::clone(self);
        let job = job.clone();
        std::thread::spawn(move || driver.run_worker(job, 0, execute, tx));
        TickDecision::Fired { completion: rx }
    }

    /// The worker loop for one fired generation: run, enforce the timeout,
    /// report the outcome, drain one queued fire, then release the slot.
    fn run_worker(
        self: &Arc<Self>,
        job: Job,
        generation: u64,
        execute: Arc<dyn Fn() + Send + Sync + 'static>,
        tx: mpsc::Sender<RunOutcome>,
    ) {
        let timeout = Duration::from_secs(job.effective_timeout_secs());
        // The suffix keeps each drain's claim key unique; the owed fire
        // itself is durable (the ledger's drain queue) and only deleted
        // once taken - a crash before the take is recovered at the next
        // `tick_job` entry.
        let mut drain_seq: u64 = 0;
        loop {
            let (done_tx, done_rx) = mpsc::channel();
            let ex = Arc::clone(&execute);
            std::thread::spawn(move || {
                let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ex())).is_ok();
                let _ = done_tx.send(ok);
            });
            let outcome = match done_rx.recv_timeout(timeout) {
                Ok(true) => RunOutcome::Completed,
                Ok(false) => RunOutcome::Panicked,
                Err(mpsc::RecvTimeoutError::Timeout) => RunOutcome::TimedOut,
                // Unreachable in practice: panics are caught above, so the
                // sender only drops if spawning the child failed.
                Err(mpsc::RecvTimeoutError::Disconnected) => RunOutcome::Panicked,
            };
            if self.generation_of(&job.id) != Some(generation) {
                // A Replace fire disowned us while we ran: report the
                // supersession instead of the run's own outcome, and touch
                // nothing - the new generation owns the in-flight slot and
                // the queue now. Exactly one outcome per run.
                let _ = tx.send(RunOutcome::Replaced);
                return;
            }
            if outcome == RunOutcome::TimedOut {
                // Abandon: stop waiting. The thread cannot be killed; it
                // stays detached until it finishes on its own.
                eprintln!(
                    "tick: job {} ran past its {}s timeout; abandoning the run",
                    job.id,
                    job.effective_timeout_secs()
                );
                // Release the slot BEFORE reporting: an observer that
                // sees TimedOut must already be able to fire again.
                // (Reporting first let a fast re-tick observe the stale
                // slot and skip a fire that should have run.)
                self.abandon(&job.id, generation);
                let _ = tx.send(RunOutcome::TimedOut);
                return;
            }
            let _ = tx.send(outcome);
            if self.take_queued(&job.id) {
                // The durable row is deleted when the drain is TAKEN,
                // before the drain run completes (take-before-complete):
                // a crash in between loses the owed run, but deleting
                // only after completion would re-fire a drain whose
                // pre-crash run may already have executed the job.
                if let Err(e) = self.ledger.dequeue_drain(&job.id) {
                    eprintln!("tick: drain-queue delete failed for {}: {e}", job.id);
                }
                // The job may have been paused while the run was in
                // flight: re-check paused state fresh from the store
                // instead of trusting the snapshot taken at fire time.
                // A paused job's queued fire is dropped, not executed.
                if self.is_paused(&job.id) {
                    eprintln!(
                        "tick: job {} paused while queued; dropping the queued fire",
                        job.id
                    );
                    break;
                }
                drain_seq += 1;
                let key = occurrence_key(&job, &format!("{}:q{drain_seq}", now_ms()));
                match self.ledger.claim(&key) {
                    Ok(_) => continue,
                    Err(e) => {
                        eprintln!(
                            "tick: queue-drain claim failed for {}: {e}; dropping the queued fire",
                            job.id
                        );
                        break;
                    }
                }
            }
            break;
        }
        self.release_if_current(&job.id, generation);
    }

    fn generation_of(&self, id: &str) -> Option<u64> {
        self.lock_in_flight()?.get(id).copied()
    }

    /// Remove the in-flight entry, but only if it is still ours: a Replace
    /// fire may have installed a newer generation we must not clear.
    fn release_if_current(&self, id: &str, generation: u64) {
        let Some(mut in_flight) = self.lock_in_flight() else {
            // Poisoned: leave the entry rather than touch untrustworthy
            // state; the tick fails closed (future fires skip/queue).
            eprintln!("tick: in-flight state lock poisoned; leaving entry for {id}");
            return;
        };
        if in_flight.get(id).copied() == Some(generation) {
            in_flight.remove(id);
        }
    }

    /// Timeout abandon: give up the slot, and drop any queued duplicate
    /// the run it would have followed never finished, so the next natural
    /// due fire is the honest retry. Only when we still own the slot; a
    /// Replace fire that disowned us owns the queue now.
    fn abandon(&self, id: &str, generation: u64) {
        if self.generation_of(id) == Some(generation) {
            self.clear_queued(id);
            let _ = self.ledger.dequeue_drain(id);
            self.release_if_current(id, generation);
        }
    }

    fn take_queued(&self, id: &str) -> bool {
        // Poisoned: don't drain - fail closed.
        self.lock_queued().is_some_and(|mut q| q.remove(id))
    }

    fn clear_queued(&self, id: &str) {
        if let Some(mut q) = self.lock_queued() {
            q.remove(id);
        }
    }
}

#[cfg(test)]
mod tick_drain_queue_tests {
    use super::*;
    use crate::ScheduleKind;
    use std::sync::atomic::AtomicUsize;

    fn reopen_driver(dir: &tempfile::TempDir) -> Arc<TickDriver> {
        Arc::new(TickDriver::new(
            DurableClaimLedger::open(&dir.path().join("claims.db")).unwrap(),
        ))
    }

    fn counter_execute(ran: &Arc<AtomicUsize>) -> Arc<dyn Fn() + Send + Sync + 'static> {
        let ran_w = Arc::clone(ran);
        Arc::new(move || {
            ran_w.fetch_add(1, Ordering::SeqCst);
        })
    }

    /// A crashed process queued a fire and died before draining it: the
    /// recovered process fires the owed run directly when the job is
    /// idle and not otherwise due.
    #[test]
    fn recovery_fires_owed_run_when_idle_and_not_due() {
        let dir = tempfile::tempdir().unwrap();
        DurableClaimLedger::open(&dir.path().join("claims.db"))
            .unwrap()
            .enqueue_drain("rj1")
            .unwrap();

        let driver = reopen_driver(&dir);
        let mut job = Job::new(
            "rj1",
            ScheduleKind::OneShot {
                at_ms: now_ms() + 3_600_000,
            },
            "agent",
        );
        job.overlap = OverlapPolicy::Queue;
        let ran = Arc::new(AtomicUsize::new(0));

        match driver.tick_job(&job, now_ms(), None, counter_execute(&ran)) {
            TickDecision::Fired { completion } => {
                assert_eq!(
                    completion.recv_timeout(Duration::from_secs(10)),
                    Ok(RunOutcome::Completed)
                );
            }
            d => panic!("expected Fired, got {d:?}"),
        }
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        // The leftover was consumed: a second tick sees nothing to recover.
        match driver.tick_job(&job, now_ms(), None, Arc::new(|| {})) {
            TickDecision::NotDue => {}
            d => panic!("expected NotDue, got {d:?}"),
        }
    }

    /// When the job is also due, the recovered fire seeds the in-memory
    /// queue and drains after the natural fire - no duplicate, no loss.
    #[test]
    fn recovery_seeds_queue_when_also_due() {
        let dir = tempfile::tempdir().unwrap();
        DurableClaimLedger::open(&dir.path().join("claims.db"))
            .unwrap()
            .enqueue_drain("rj2")
            .unwrap();

        let driver = reopen_driver(&dir);
        let mut job = Job::new(
            "rj2",
            ScheduleKind::OneShot {
                at_ms: now_ms() - 1_000,
            },
            "agent",
        );
        job.overlap = OverlapPolicy::Queue;
        let ran = Arc::new(AtomicUsize::new(0));

        match driver.tick_job(&job, now_ms(), None, counter_execute(&ran)) {
            TickDecision::Fired { completion } => {
                // Natural fire + the recovered drain, back to back.
                assert_eq!(
                    completion.recv_timeout(Duration::from_secs(10)),
                    Ok(RunOutcome::Completed)
                );
                assert_eq!(
                    completion.recv_timeout(Duration::from_secs(10)),
                    Ok(RunOutcome::Completed)
                );
            }
            d => panic!("expected Fired, got {d:?}"),
        }
        assert_eq!(ran.load(Ordering::SeqCst), 2);
    }

    /// The Queue overlap path writes the durable row: a fresh handle on
    /// the same DB (a stand-in for the post-crash process) sees the owed
    /// fire even though the in-memory set died with the old process.
    #[test]
    fn queue_path_persists_drain_durably() {
        let dir = tempfile::tempdir().unwrap();
        let driver = reopen_driver(&dir);
        let mut job = Job::new(
            "qj",
            ScheduleKind::OneShot {
                at_ms: now_ms() - 1_000,
            },
            "agent",
        );
        job.overlap = OverlapPolicy::Queue;

        // The first fire blocks until the test releases it, so the
        // second tick observes the overlap.
        let (block_tx, block_rx) = mpsc::channel::<()>();
        let block_rx = Arc::new(Mutex::new(block_rx));
        let execute: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
            block_rx.lock().unwrap().recv().unwrap();
        });

        let first = driver.tick_job(&job, now_ms(), None, Arc::clone(&execute));
        assert!(matches!(first, TickDecision::Fired { .. }));
        let second = driver.tick_job(&job, now_ms(), None, Arc::clone(&execute));
        assert!(matches!(second, TickDecision::Queued));
        // The queue is durable: a fresh handle sees the owed fire.
        let fresh = DurableClaimLedger::open(&dir.path().join("claims.db")).unwrap();
        assert!(fresh.take_pending_drain("qj").unwrap());

        // Release both runs: the natural fire, then its drain.
        block_tx.send(()).unwrap();
        block_tx.send(()).unwrap();
        if let TickDecision::Fired { completion } = first {
            assert_eq!(
                completion.recv_timeout(Duration::from_secs(10)),
                Ok(RunOutcome::Completed)
            );
            assert_eq!(
                completion.recv_timeout(Duration::from_secs(10)),
                Ok(RunOutcome::Completed)
            );
        }
    }
}
