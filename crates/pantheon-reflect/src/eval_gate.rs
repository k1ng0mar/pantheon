//! Eval-gating: skill and persona proposals must pass relevant
//! `pantheon-eval` targets before they can be approved. Bounded: at most
//! `max_evals` targets, each with a timeout. A regression rejects the
//! proposal and the rejection is audited.
//!
//! The [`EvalRunner`] trait keeps this testable: the real implementation
//! shells out to `cargo test -p pantheon-eval --test <target>`; eval tests
//! inject a fake.

use crate::ReflectConfig;
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
    /// Build a runner with the timeout/cap from [`ReflectConfig`].
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
pub fn gate(
    proposal: &crate::Proposal,
    tags: &[&str],
    runner: &dyn EvalRunner,
    config: &ReflectConfig,
) -> EvalVerdict {
    let tags: Vec<&str> = tags.iter().take(config.max_evals).copied().collect();
    if tags.is_empty() {
        // No relevant evals tagged: nothing to gate on. This only happens
        // for kinds that skip eval-gating (memory lessons); callers should
        // not gate those, but a vacuous pass is safer than a block.
        return EvalVerdict::Pass("no evals tagged".into());
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::propose::{ProposalKind, ProposalStatus};
    use crate::Proposal;

    struct FakeRunner(EvalOutcome);
    impl EvalRunner for FakeRunner {
        fn run_eval(&self, _t: &str, _d: Duration) -> EvalOutcome {
            self.0.clone()
        }
    }

    fn dummy() -> Proposal {
        Proposal {
            id: "rfl_x".into(),
            kind: ProposalKind::Skill {
                name: "s".into(),
                update: false,
            },
            title: "t".into(),
            body: "b".into(),
            provenance_runs: vec![],
            provenance_turns: vec![],
            eval_tags: vec!["tools_skills".into()],
            status: ProposalStatus::Proposed,
        }
    }

    #[test]
    fn failing_eval_rejects() {
        let v = gate(
            &dummy(),
            &["tools_skills"],
            &FakeRunner(EvalOutcome::Fail("boom".into())),
            &ReflectConfig::default(),
        );
        assert!(matches!(v, EvalVerdict::Reject(_)));
    }

    #[test]
    fn timeout_rejects() {
        let v = gate(
            &dummy(),
            &["tools_skills"],
            &FakeRunner(EvalOutcome::Timeout),
            &ReflectConfig::default(),
        );
        assert!(matches!(v, EvalVerdict::Reject(r) if r.contains("timed out")));
    }

    #[test]
    fn passing_eval_passes_with_summary() {
        let v = gate(
            &dummy(),
            &["tools_skills"],
            &FakeRunner(EvalOutcome::Pass),
            &ReflectConfig::default(),
        );
        assert!(matches!(v, EvalVerdict::Pass(s) if s.contains("tools_skills")));
    }
}
