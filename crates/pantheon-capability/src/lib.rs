//! Capability plane: Policy lives in pantheon-api; this crate enforces it
//! at the execution boundary (check-then-act, default-deny).
use pantheon_api::capability::{Capability, Decision, Policy};
use pantheon_api::error::{Layer, PantheonError};

fn cerr(code: &str, cause: String, retryable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Capability,
        retryable,
        cause,
        "request approval or narrow the capability grant",
        "",
    )
}

/// Enforcement outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    NeedsApproval { capability: Capability },
    Deny { capability: Capability },
}

/// Check a capability against a policy. Never panics, never defaults to allow.
pub fn check(policy: &Policy, cap: &Capability) -> Verdict {
    match policy.check(cap) {
        Decision::Allow => Verdict::Allow,
        Decision::Approval => Verdict::NeedsApproval {
            capability: cap.clone(),
        },
        Decision::Deny => Verdict::Deny {
            capability: cap.clone(),
        },
    }
}

/// Enforce: Allow passes, anything else becomes a structured error.
pub fn enforce(policy: &Policy, cap: &Capability) -> Result<(), PantheonError> {
    match check(policy, cap) {
        Verdict::Allow => Ok(()),
        Verdict::NeedsApproval { capability } => Err(cerr(
            "CAP_APPROVAL_REQUIRED",
            format!("capability {capability:?} needs approval"),
            false,
        )),
        Verdict::Deny { capability } => Err(cerr(
            "CAP_DENIED",
            format!("capability {capability:?} denied by policy"),
            false,
        )),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
