//! Eval-gating: the no-regression check.
//!
//! Skill and persona proposals must pass their relevant `pantheon-eval`
//! targets before they can be approved. Bounded: at most `max_evals`
//! targets, each with a timeout. A regression rejects the proposal and
//! the rejection is audited.
//!
//! This is the *no-regression* half of validation. The *improvement*
//! half — proving the proposal makes a held-out task better — lives in
//! [`crate::replay`]. A proposal ships only when both pass.

use crate::NightlyConfig;
use std::time::{Duration, Instant};

/// Outcome of one eval target run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalOutcome {
    Pass,
    Fail(String),
    Timeout,
    Error(String),
}

/// Runs eval targets. Implementations must be bounded (timeout).
pub trait EvalRunner {
    fn run_eval(&self, target: &str, timeout: Duration) -> EvalOutcome;
}

/// Verdict over the whole gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalVerdict {
    /// All tagged evals passed. Carries a human-readable summary for the
    /// audit log and the approval prompt.
    Pass(String),
    /// The gate ran no evals: the proposal had no eval tags. This is NOT
    /// a pass — an untested proposal must not look validated. The caller
    /// decides whether the proposal's kind is allowlisted to skip
    /// eval-gating; otherwise it escalates.
    Skipped(String),
    /// At least one eval failed/timed out/errored. The proposal is
    /// rejected; the reason names the failing target.
    Reject(String),
}

/// Real runner: `cargo test -p pantheon-eval --test <target>`.
/// Each target runs in its own process with a deadline; an expired
/// deadline kills the child.
pub struct SubprocessEvalRunner {
    pub cargo: String,
    /// Default per-target timeout, used when callers don't pass one.
    pub eval_timeout: Duration,
    /// Default cap on targets per gate decision.
    pub max_evals: usize,
}

impl SubprocessEvalRunner {
    /// Build a runner with the timeout/cap from [`NightlyConfig`].
    pub fn new(eval_timeout: Duration, max_evals: usize) -> Self {
        Self {
            cargo: "cargo".to_string(),
            eval_timeout,
            max_evals,
        }
    }
}

impl Default for SubprocessEvalRunner {
    fn default() -> Self {
        Self::new(Duration::from_secs(120), 3)
    }
}

impl EvalRunner for SubprocessEvalRunner {
    fn run_eval(&self, target: &str, timeout: Duration) -> EvalOutcome {
        let mut child = match std::process::Command::new(&self.cargo)
            .args(["test", "-p", "pantheon-eval", "--test", target])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return EvalOutcome::Error(format!("spawn cargo: {e}")),
        };
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return if status.success() {
                        EvalOutcome::Pass
                    } else {
                        EvalOutcome::Fail(format!(
                            "cargo test -p pantheon-eval --test {target} exited {}",
                            status.code().unwrap_or(-1)
                        ))
                    };
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return EvalOutcome::Timeout;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return EvalOutcome::Error(format!("wait on cargo: {e}")),
            }
        }
    }
}

/// Gate a proposal: run up to `config.max_evals` of its tagged evals.
/// All must pass; the first failure rejects the proposal.
///
/// An empty tag list yields [`EvalVerdict::Skipped`], never a vacuous
/// `Pass`: a proposal that ran zero evals is not a proposal that passed
/// its evals. The caller decides whether the proposal's kind is
/// allowlisted to skip eval-gating.
pub fn gate(
    proposal: &crate::Proposal,
    tags: &[&str],
    runner: &dyn EvalRunner,
    config: &NightlyConfig,
) -> EvalVerdict {
    let tags: Vec<&str> = tags.iter().take(config.max_evals).copied().collect();
    if tags.is_empty() {
        // No evals ran: report that explicitly. Callers must treat
        // `Skipped` as *not validated* unless the proposal kind is
        // allowlisted to skip eval-gating — a vacuous pass here let
        // broken drafts validate green.
        return EvalVerdict::Skipped("no evals tagged".into());
    }
    let mut passed = Vec::new();
    for tag in tags {
        match runner.run_eval(tag, config.eval_timeout) {
            EvalOutcome::Pass => passed.push(tag.to_string()),
            EvalOutcome::Fail(detail) => {
                return EvalVerdict::Reject(format!(
                    "proposal '{}' rejected: eval '{tag}' failed: {detail}",
                    proposal.id
                ));
            }
            EvalOutcome::Timeout => {
                return EvalVerdict::Reject(format!(
                    "proposal '{}' rejected: eval '{tag}' timed out after {:?}",
                    proposal.id, config.eval_timeout
                ));
            }
            EvalOutcome::Error(detail) => {
                return EvalVerdict::Reject(format!(
                    "proposal '{}' rejected: eval '{tag}' errored: {detail}",
                    proposal.id
                ));
            }
        }
    }
    EvalVerdict::Pass(format!("evals passed: {}", passed.join(", ")))
}

// Small deterministic invariant tests only.
