//! capability policy -> execution boundary (§9 + §10).
//!
//! [`enforce`] is the one bridge between the capability system and the
//! sandbox hierarchy: policy owns *whether*, this crate owns *how isolated*.
//! no "run anyway" escape hatch on purpose - an ungranted capability is
//! denied, not quietly downgraded to a weaker boundary.

use super::level::{profile_for, SandboxProfile};
use super::SandboxLevel;
use pantheon_api::capability::{Capability, Decision, Policy};
use serde::{Deserialize, Serialize};

/// what the runtime may do with one capability request
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Enforcement {
    /// policy allowed it: run inside this profile
    Run(SandboxProfile),
    /// policy didn't grant it (or denied): never executes
    Deny {
        capability: Capability,
        reason: String,
    },
    /// policy gated it: hold at the boundary until a decision arrives
    RequireApproval {
        capability: Capability,
        /// approval scope carried on the `approval.requested` event (§18)
        scope: String,
        profile: SandboxProfile,
    },
}

impl Enforcement {
    /// true when the capability never runs
    pub fn is_denied(&self) -> bool {
        matches!(self, Enforcement::Deny { .. })
    }

    /// profile that would execute, if the request may run at all
    pub fn profile(&self) -> Option<&SandboxProfile> {
        match self {
            Enforcement::Run(profile) | Enforcement::RequireApproval { profile, .. } => {
                Some(profile)
            }
            Enforcement::Deny { .. } => None,
        }
    }

    /// level that would execute, if the request may run at all
    pub fn level(&self) -> Option<SandboxLevel> {
        self.profile().map(|p| p.level)
    }
}

/// decide one capability request against a policy.
///
/// mapping is total and one-to-one with [`Decision`]: `Allow` -> `Run`,
/// `Deny` -> `Deny`, `Approval` -> `RequireApproval`. a capability with no
/// rule is denied, same as `Policy::check` reports (default-deny).
pub fn enforce(policy: &Policy, capability: &Capability) -> Enforcement {
    let profile = profile_for(SandboxLevel::for_capability(capability));
    match policy.check(capability) {
        Decision::Allow => Enforcement::Run(profile),
        Decision::Deny => Enforcement::Deny {
            capability: capability.clone(),
            reason: "not granted by policy (default-deny); add an explicit rule to use it"
                .to_string(),
        },
        Decision::Approval => Enforcement::RequireApproval {
            capability: capability.clone(),
            scope: capability_label(capability),
            profile,
        },
    }
}

/// stable, log-safe label for a capability, e.g. `shell.execute`.
/// delegates to [`Capability::token`] - the single token table lives in core so
/// sandbox labels and MCP parsing can't drift.
///
/// used for approval scopes and events, so it must never carry payload values:
/// a `MessageSend` label is the channel class, not the message.
pub fn capability_label(capability: &Capability) -> String {
    capability.token()
}
