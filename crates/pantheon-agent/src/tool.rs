//! Tool execution boundary: capability gate, then runner. The gate runs
//! before the runner, always — there is no path that executes an ungranted
//! capability.
use crate::capability::{enforce, Verdict};
use pantheon_api::capability::{Capability, Policy};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::events::Event;

/// Where events go. Implemented by the runtime supervisor; tests use a Vec.
pub trait EventSink {
    fn emit(&self, event: Event);
}

/// Executes one tool. Implementations decide how (process, http, in-proc).
pub trait ToolRunner {
    fn run(&self, name: &str, args: &str) -> Result<String, PantheonError>;
}

/// What the gate decided for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// May run.
    Allow,
    /// Policy gated it: caller must park and ask.
    NeedsApproval { capability: Capability },
}

fn aerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Agent,
        false,
        cause,
        "adjust the policy or grant the capability explicitly",
        "",
    )
}

/// Check one capability against a policy, mapping a denial to a structured
/// error the loop can surface as `RunFailed`.
pub fn gate(policy: &Policy, cap: &Capability) -> Result<GateOutcome, PantheonError> {
    match enforce(policy, cap) {
        Ok(()) => Ok(GateOutcome::Allow),
        Err(_e) => {
            // enforce() folds both Deny and Approval into errors; re-check to
            // distinguish so approval does not look like a hard denial.
            match crate::capability::check(policy, cap) {
                Verdict::NeedsApproval { capability } => {
                    Ok(GateOutcome::NeedsApproval { capability })
                }
                Verdict::Deny { capability } => Err(aerr(
                    "CAP_DENIED",
                    format!("capability {capability:?} denied by policy"),
                )),
                Verdict::Allow => Ok(GateOutcome::Allow),
            }
        }
    }
}
