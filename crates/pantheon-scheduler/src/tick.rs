//! Durable tick driver (§21).
//!
//! One code path owns the fire decision. For each due job it:
//!
//! 1. applies the job's [`OverlapPolicy`](crate::OverlapPolicy) against the
//!    in-flight set (same-process concurrency);
//! 2. wins a durable claim for the occurrence from the
//!    [`DurableClaimLedger`] — an atomic first-wins INSERT, so two ticks
//!    racing the same due job (two threads, two processes, or a restart
//!    replaying a minute) agree on exactly one winner;
//! 3. runs the job with its timeout: a run that outlives
//!    [`Job::effective_timeout_secs`](crate::Job::effective_timeout_secs)
//!    is abandoned — the tick stops waiting. Rust cannot kill a thread,
//!    so the abandoned run keeps going detached until it finishes on its
//!    own.
//!
//! A claim that cannot be persisted fails closed: the run must not start.
//! Execution itself is injected (`Arc<dyn Fn()>`), so the whole driver is
//! testable without a runtime.

use crate::durable::DurableClaimLedger;
use crate::idempotency::occurrence_key;
use crate::{Job, OverlapPolicy};
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    Fired { completion: mpsc::Receiver<RunOutcome> },
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
    /// Job ids that became due while running and asked to queue.
    queued: Mutex<HashSet<String>>,
}

impl TickDriver {
    pub fn new(ledger: DurableClaimLedger) -> Self {
        Self {
            ledger,
            in_flight: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashSet::new()),
        }
    }

    /// Attempt one fire of `job` at `now_ms`.
    ///
    /// `last_fire_ms` is the job's persisted last fire (drives
    /// [`Job::due`](crate::Job::due) and the occurrence stamp).
    /// `execute` runs the job; it is called at most once per won claim,
    /// on a worker thread — this method never blocks on the run itself.
    pub fn tick_job(
        self: &Arc<Self>,
        job: &Job,
        now_ms: i64,
        last_fire_ms: Option<i64>,
        execute: Arc<dyn Fn() + Send + Sync + 'static>,
    ) -> TickDecision {
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
        // occurrence, same key — exactly one wins), and a replacer that
        // loses the claim leaves the in-flight run untouched.
        let (generation, replacing) = {
            let mut in_flight = self.in_flight.lock().expect("tick state lock");
            match in_flight.get(&job.id).copied() {
                None => {
                    in_flight.insert(job.id.clone(), 0);
                    (0, false)
                }
                Some(gen) => match job.overlap {
                    OverlapPolicy::Skip => return TickDecision::SkippedOverlap,
                    OverlapPolicy::Replace => (gen, true),
                    OverlapPolicy::Queue => {
                        self.queued
                            .lock()
                            .expect("tick state lock")
                            .insert(job.id.clone());
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
                    self.in_flight
                        .lock()
                        .expect("tick state lock")
                        .insert(job.id.clone(), next);
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
                }
                return TickDecision::SkippedClaimLost;
            }
            Err(e) => {
                if !replacing {
                    self.release_if_current(&job.id, generation);
                    self.clear_queued(&job.id);
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
        // Queue drains are in-process only (a crash loses them, like the
        // in-flight set); the suffix keeps each drain's claim key unique.
        let mut drain_seq: u64 = 0;
        loop {
            let (done_tx, done_rx) = mpsc::channel();
            let ex = Arc::clone(&execute);
            std::thread::spawn(move || {
                let ok =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ex())).is_ok();
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
                // nothing — the new generation owns the in-flight slot and
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
                let _ = tx.send(RunOutcome::TimedOut);
                self.abandon(&job.id, generation);
                return;
            }
            let _ = tx.send(outcome);
            if self.take_queued(&job.id) {
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
        self.in_flight
            .lock()
            .expect("tick state lock")
            .get(id)
            .copied()
    }

    /// Remove the in-flight entry, but only if it is still ours: a Replace
    /// fire may have installed a newer generation we must not clear.
    fn release_if_current(&self, id: &str, generation: u64) {
        let mut in_flight = self.in_flight.lock().expect("tick state lock");
        if in_flight.get(id).copied() == Some(generation) {
            in_flight.remove(id);
        }
    }

    /// Timeout abandon: give up the slot, and drop any queued duplicate —
    /// the run it would have followed never finished, so the next natural
    /// due fire is the honest retry. Only when we still own the slot; a
    /// Replace fire that disowned us owns the queue now.
    fn abandon(&self, id: &str, generation: u64) {
        if self.generation_of(id) == Some(generation) {
            self.clear_queued(id);
            self.release_if_current(id, generation);
        }
    }

    fn take_queued(&self, id: &str) -> bool {
        self.queued.lock().expect("tick state lock").remove(id)
    }

    fn clear_queued(&self, id: &str) {
        self.queued.lock().expect("tick state lock").remove(id);
    }
}

#[cfg(test)]
#[path = "tick_tests.rs"]
mod tests;
