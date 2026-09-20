//! Capability policy -> execution boundary (§9 + §10).
//!
//! [`enforce`] is the single bridge between the capability system and the
//! sandbox hierarchy: policy owns *whether*, this crate owns *how isolated*.
//! There is deliberately no "run anyway" escape hatch — an ungranted
//! capability is denied, not quietly downgraded to a weaker boundary.

use crate::level::{profile_for, SandboxProfile};
use crate::SandboxLevel;
use pantheon_core::capability::{Capability, Decision, Policy};
use serde::{Deserialize, Serialize};

/// What the runtime may do with one capability request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Enforcement {
    /// Policy allowed it: execute inside this profile.
    Run(SandboxProfile),
    /// Policy did not grant it (or denied it). Never executes.
    Deny {
        capability: Capability,
        reason: String,
    },
    /// Policy gated it: hold at the boundary until a decision arrives.
    RequireApproval {
        capability: Capability,
        /// Approval scope carried on the `approval.requested` event (§18).
        scope: String,
        profile: SandboxProfile,
    },
}

impl Enforcement {
    /// True when the capability never runs.
    pub fn is_denied(&self) -> bool {
        matches!(self, Enforcement::Deny { .. })
    }

    /// The profile that would execute, if the request may run at all.
    pub fn profile(&self) -> Option<&SandboxProfile> {
        match self {
            Enforcement::Run(profile) | Enforcement::RequireApproval { profile, .. } => {
                Some(profile)
            }
            Enforcement::Deny { .. } => None,
        }
    }

    /// The level that would execute, if the request may run at all.
    pub fn level(&self) -> Option<SandboxLevel> {
        self.profile().map(|p| p.level)
    }
}

/// Decide one capability request against a policy.
///
/// Mapping is total and one-to-one with [`Decision`]: `Allow` -> `Run`,
/// `Deny` -> `Deny`, `Approval` -> `RequireApproval`. A capability with no
/// rule is denied, exactly as `Policy::check` reports (default-deny).
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

/// Stable, log-safe label for a capability, e.g. `shell.execute`.
///
/// Used for approval scopes and events, so it must never carry payload
/// values: a `MessageSend` label is the channel class, not the message.
pub fn capability_label(capability: &Capability) -> String {
    match capability {
        Capability::FilesystemRead => "filesystem.read".to_string(),
        Capability::FilesystemWrite => "filesystem.write".to_string(),
        Capability::ShellExecute => "shell.execute".to_string(),
        Capability::GitRead => "git.read".to_string(),
        Capability::GitWrite => "git.write".to_string(),
        Capability::GitPush => "git.push".to_string(),
        Capability::NetworkOutbound => "network.outbound".to_string(),
        Capability::Browser => "browser".to_string(),
        Capability::MessageSend(_) => "message.send".to_string(),
        Capability::MemoryRead => "memory.read".to_string(),
        Capability::MemoryWrite => "memory.write".to_string(),
        Capability::SecretsUse => "secrets.use".to_string(),
        Capability::AgentSpawn => "agent.spawn".to_string(),
        Capability::Other(name) => format!("other.{}", name.trim().replace(' ', ".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ExecutionBoundary;

    #[test]
    fn allow_runs_at_the_level_the_capability_demands() {
        let policy = Policy::coder();
        match enforce(&policy, &Capability::FilesystemRead) {
            Enforcement::Run(profile) => {
                assert_eq!(profile.level, SandboxLevel::Low);
                assert_eq!(profile.boundary, ExecutionBoundary::InProcess);
            }
            other => panic!("expected Run, got {other:?}"),
        }
        match enforce(&policy, &Capability::ShellExecute) {
            Enforcement::Run(profile) => {
                assert_eq!(profile.level, SandboxLevel::High);
                assert_eq!(profile.boundary, ExecutionBoundary::Container);
                assert!(profile.drop_capabilities);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn deny_never_produces_a_profile() {
        // The researcher preset has no rule for shell.execute.
        let policy = Policy::researcher_readonly();
        let decision = enforce(&policy, &Capability::ShellExecute);
        assert!(decision.is_denied());
        assert!(decision.profile().is_none(), "denied runs have no boundary");
        match decision {
            Enforcement::Deny { capability, reason } => {
                assert_eq!(capability, Capability::ShellExecute);
                assert!(reason.contains("default-deny"), "reason was: {reason}");
            }
            other => panic!("expected Deny, got {other:?}"),
        }
    }

    #[test]
    fn approval_holds_at_the_boundary_with_a_scope() {
        let policy = Policy::coder();
        match enforce(&policy, &Capability::GitPush) {
            Enforcement::RequireApproval {
                capability,
                scope,
                profile,
            } => {
                assert_eq!(capability, Capability::GitPush);
                assert_eq!(scope, "git.push");
                assert_eq!(profile.level, SandboxLevel::VeryHigh);
                assert_eq!(profile.boundary, ExecutionBoundary::Vm);
            }
            other => panic!("expected RequireApproval, got {other:?}"),
        }
    }

    #[test]
    fn ungranted_secrets_never_reach_a_boundary() {
        // Default policy grants nothing: secrets.use is denied outright.
        let policy = Policy::default();
        assert!(enforce(&policy, &Capability::SecretsUse).is_denied());

        // Grant it and it runs at the strongest level we have.
        let policy = Policy::default().allow(Capability::SecretsUse);
        assert_eq!(
            enforce(&policy, &Capability::SecretsUse).level(),
            Some(SandboxLevel::VeryHigh)
        );
    }

    #[test]
    fn labels_are_log_safe() {
        assert_eq!(capability_label(&Capability::ShellExecute), "shell.execute");
        // The channel and message must not leak into the label.
        assert_eq!(
            capability_label(&Capability::MessageSend("discord".to_string())),
            "message.send"
        );
    }
}
