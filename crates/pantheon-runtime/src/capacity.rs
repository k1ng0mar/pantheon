//! Process-wide admission control for turns.
//!
//! Concurrency in Pantheon used to be bounded only per session: ten sessions
//! (or a swarm plus a gateway plus the TUI) could all reach the provider at
//! once with nothing pushing back. This is the gate OpenClaw calls global
//! capacity admission: a turn acquires a slot before streaming and releases it
//! when the turn ends, and a full gate makes the turn *wait* (bounded) instead
//! of piling on.
//!
//! Scope is the process, matching the model provider connections it protects.
//! The limit is configured once per process (`configure`) from
//! `[runtime] max_concurrent_turns` / `PANTHEON_MAX_CONCURRENT_TURNS`; a
//! second `configure` with a different value takes effect for the next
//! acquisition.

use pantheon_api::error::{Layer, PantheonError};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default ceiling: enough for a TUI turn, a gateway turn and a small swarm on
/// one host without hammering a single provider account.
pub const DEFAULT_MAX_CONCURRENT_TURNS: usize = 4;

struct Gate {
    limit: AtomicUsize,
    state: Mutex<GateState>,
    cv: Condvar,
}

#[derive(Default)]
struct GateState {
    active: usize,
    waiting: usize,
}

fn gate() -> &'static Gate {
    static GATE: OnceLock<Gate> = OnceLock::new();
    GATE.get_or_init(|| Gate {
        limit: AtomicUsize::new(
            std::env::var("PANTHEON_MAX_CONCURRENT_TURNS")
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(DEFAULT_MAX_CONCURRENT_TURNS),
        ),
        state: Mutex::new(GateState::default()),
        cv: Condvar::new(),
    })
}

/// Set this process's turn ceiling. `0` restores the default.
pub fn configure(max_concurrent_turns: usize) {
    let n = if max_concurrent_turns == 0 {
        DEFAULT_MAX_CONCURRENT_TURNS
    } else {
        max_concurrent_turns
    };
    gate().limit.store(n, Ordering::Relaxed);
    gate().cv.notify_all();
}

/// Current ceiling, for `/status` and tests.
pub fn limit() -> usize {
    gate().limit.load(Ordering::Relaxed)
}

/// Turns currently admitted, for diagnostics.
pub fn active() -> usize {
    gate().state.lock().map(|s| s.active).unwrap_or(0)
}

/// A held admission slot. Dropping it releases the slot and wakes a waiter.
#[derive(Debug)]
pub struct CapacityGuard {
    released: bool,
}

impl Drop for CapacityGuard {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let g = gate();
        if let Ok(mut st) = g.state.lock() {
            st.active = st.active.saturating_sub(1);
            g.cv.notify_one();
        }
    }
}

/// Admit one turn, waiting up to `wait` for a free slot.
///
/// Returns `RT_CAPACITY_TIMEOUT` when the wait expires: a queued turn that
/// never starts is better reported than starting late with a stale budget.
pub fn acquire(wait: Duration) -> Result<CapacityGuard, PantheonError> {
    let g = gate();
    let started = Instant::now();
    let mut st = g
        .state
        .lock()
        .map_err(|_| cap_err("capacity lock is poisoned", "restart pantheon"))?;
    let limit = g.limit.load(Ordering::Relaxed).max(1);
    if st.active >= limit {
        st.waiting += 1;
        loop {
            let left = wait.saturating_sub(started.elapsed());
            if left.is_zero() {
                st.waiting = st.waiting.saturating_sub(1);
                return Err(cap_err(
                    &format!(
                        "no turn slot after {}ms (limit {limit}, {} waiting)",
                        wait.as_millis(),
                        st.waiting
                    ),
                    "let the running turns finish, or raise [runtime] max_concurrent_turns",
                ));
            }
            let (next, _timeout) = g
                .cv
                .wait_timeout(st, left)
                .map_err(|_| cap_err("capacity wait lock is poisoned", "restart pantheon"))?;
            st = next;
            let limit = g.limit.load(Ordering::Relaxed).max(1);
            if st.active < limit {
                break;
            }
        }
        st.waiting = st.waiting.saturating_sub(1);
    }
    st.active += 1;
    Ok(CapacityGuard { released: false })
}

fn cap_err(cause: &str, fix: &str) -> PantheonError {
    PantheonError::new(
        "RT_CAPACITY_TIMEOUT",
        Layer::Runtime,
        true,
        cause.to_string(),
        fix.to_string(),
        "",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// These tests mutate one process-wide gate; without a lock they race.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_full_gate_queues_then_admits_on_release() {
        let _g = serial();
        configure(1);
        let first = acquire(Duration::from_secs(1)).expect("first slot");
        assert_eq!(active(), 1);
        let waiter = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = waiter.clone();
        let handle = std::thread::spawn(move || {
            let g = acquire(Duration::from_secs(5)).expect("second slot after release");
            flag.store(true, Ordering::SeqCst);
            drop(g);
        });
        std::thread::sleep(Duration::from_millis(80));
        assert!(!waiter.load(Ordering::SeqCst), "second turn must wait");
        drop(first);
        handle.join().unwrap();
        assert!(waiter.load(Ordering::SeqCst));
        assert_eq!(active(), 0);
        configure(0);
    }

    #[test]
    fn a_wait_that_expires_reports_a_timeout() {
        let _g = serial();
        configure(1);
        let _held = acquire(Duration::from_millis(50)).expect("slot");
        let err = acquire(Duration::from_millis(50)).expect_err("timeout");
        assert_eq!(err.code, "RT_CAPACITY_TIMEOUT");
        configure(0);
    }
}
