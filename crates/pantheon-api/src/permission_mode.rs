//! Permission modes: how much a tool call is trusted before it runs.
//!
//! This is a separate axis from [`crate::mode::AgentMode`]. AgentMode asks
//! *what kind of work* the turn is doing (Plan vs Build) and refuses
//! mutating tools outright. PermissionMode asks *how much scrutiny* each
//! allowed call gets, and it can only ever relax or tighten the gate
//! around calls the policy already permits:
//!
//! - [`PermissionMode::Ask`]: never auto-approve. Every capability the
//!   policy marks `Approval` parks for a human, and the judge is not
//!   consulted. This is the conservative default: a run parks exactly
//!   where the deterministic policy says it should.
//! - [`PermissionMode::Smart`]: the judge decides. When the judge returns
//!   `Allow` for a call the policy marked `Approval`, the call runs
//!   without parking. The judge may only escalate, never relax past the
//!   policy: a `Deny` stays denied and a judge failure falls back to
//!   parking.
//! - [`PermissionMode::AllowAll`]: auto-approve every `Approval`-marked
//!   capability. `Deny` is still `Deny` - this mode cannot grant what the
//!   policy refuses, so a read-only preset stays read-only.
//!
//! The escalation ladder is the safety property: policy is the floor, the
//! judge may move up from it, and no mode moves below it. AllowAll is the
//! one mode that removes the human from the loop for approval-gated work,
//! which is why it is never the default and why `Deny` remains absolute.

use serde::{Deserialize, Serialize};

use crate::capability::Decision;
use crate::model::GateVerdict;

/// How much scrutiny an approval-gated tool call gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PermissionMode {
    /// Park for a human on every approval-gated capability.
    #[default]
    Ask,
    /// Let the judge auto-approve low-risk calls; park the rest.
    Smart,
    /// Auto-approve every approval-gated capability. `Deny` still denies.
    AllowAll,
}

impl PermissionMode {
    /// Stable string form (`"ask"` / `"smart"` / `"allow_all"`).
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask",
            PermissionMode::Smart => "smart",
            PermissionMode::AllowAll => "allow_all",
        }
    }

    /// Parse [`PermissionMode::as_str`] output (case-insensitive), plus
    /// the spellings a user is likely to type for each mode.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace('-', "_").as_str() {
            "ask" | "manual" | "default" => Some(PermissionMode::Ask),
            "smart" | "auto" | "judge" => Some(PermissionMode::Smart),
            "allow_all" | "allowall" | "yolo" | "bypass" | "all" => Some(PermissionMode::AllowAll),
            _ => None,
        }
    }

    /// One-line description for the UI.
    pub fn describe(self) -> &'static str {
        match self {
            PermissionMode::Ask => "ask: park on every approval-gated call",
            PermissionMode::Smart => "smart: the judge auto-approves low-risk calls",
            PermissionMode::AllowAll => "allow_all: auto-approve everything the policy permits",
        }
    }

    /// Resolve one approval-gated capability to a final decision.
    ///
    /// `policy_decision` is what the deterministic host policy said. This
    /// function may only relax an `Approval` into `Allow` (that is what the
    /// modes are for) and may never touch a `Deny`: no mode can grant a
    /// capability the policy refuses, so a read-only preset stays read-only
    /// in every mode.
    ///
    /// `judge_verdict` is the auxiliary judge's opinion, already produced
    /// through `AgentLoop::consult_gate_advisory` (which enforces the
    /// escalate-only ladder and falls back to `None` on any judge failure).
    /// It is only consulted in `Smart`. A `None` there means "the judge
    /// could not help", which must park rather than auto-approve.
    pub fn resolve(
        self,
        policy_decision: Decision,
        judge_verdict: Option<GateVerdict>,
    ) -> Decision {
        match policy_decision {
            // Absolute. Not even AllowAll may widen the policy.
            Decision::Deny => Decision::Deny,
            // Nothing to do: it already runs unattended.
            Decision::Allow => Decision::Allow,
            Decision::Approval => match self {
                PermissionMode::Ask => Decision::Approval,
                PermissionMode::AllowAll => Decision::Allow,
                PermissionMode::Smart => match judge_verdict {
                    // The judge cleared it: run without parking.
                    Some(GateVerdict::Allow) => Decision::Allow,
                    // The judge wants a human, or the judge failed and the
                    // caller passed None. Both park.
                    Some(GateVerdict::NeedsApproval { .. }) | None => Decision::Approval,
                    // The judge escalated past approval to a hard deny.
                    // Honor it: the judge may move up the ladder, and Deny
                    // is up from Approval.
                    Some(GateVerdict::Deny { .. }) => Decision::Deny,
                },
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_its_own_string_form() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::Smart,
            PermissionMode::AllowAll,
        ] {
            assert_eq!(PermissionMode::parse(mode.as_str()), Some(mode));
        }
    }

    #[test]
    fn accepts_the_spellings_a_user_would_type() {
        assert_eq!(PermissionMode::parse("MANUAL"), Some(PermissionMode::Ask));
        assert_eq!(
            PermissionMode::parse("  Judge "),
            Some(PermissionMode::Smart)
        );
        assert_eq!(
            PermissionMode::parse("allow-all"),
            Some(PermissionMode::AllowAll)
        );
        assert_eq!(
            PermissionMode::parse("yolo"),
            Some(PermissionMode::AllowAll)
        );
    }

    /// An unknown string must not silently become the most permissive
    /// mode. `None` forces the caller to decide, and every caller
    /// defaults to Ask.
    #[test]
    fn unknown_string_is_not_a_mode() {
        assert_eq!(PermissionMode::parse("whatever"), None);
        assert_eq!(PermissionMode::default(), PermissionMode::Ask);
    }

    /// The core safety property: Deny is absolute in every mode, and
    /// Allow is never downgraded. A read-only policy must stay read-only
    /// even under AllowAll.
    #[test]
    fn no_mode_escapes_a_policy_deny() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::Smart,
            PermissionMode::AllowAll,
        ] {
            assert_eq!(mode.resolve(Decision::Deny, None), Decision::Deny);
            // Even a judge that says "allow" cannot lift a policy deny.
            assert_eq!(
                mode.resolve(Decision::Deny, Some(GateVerdict::Allow)),
                Decision::Deny
            );
            // Allow stays Allow: no mode parks something already permitted.
            assert_eq!(mode.resolve(Decision::Allow, None), Decision::Allow);
        }
    }

    /// Ask is the conservative default: it parks on Approval and never
    /// consults the judge, so an unconfigured run behaves exactly as the
    /// deterministic policy alone would.
    #[test]
    fn ask_always_parks_and_ignores_the_judge() {
        assert_eq!(
            PermissionMode::Ask.resolve(Decision::Approval, None),
            Decision::Approval
        );
        assert_eq!(
            PermissionMode::Ask.resolve(Decision::Approval, Some(GateVerdict::Allow)),
            Decision::Approval
        );
    }

    #[test]
    fn allow_all_clears_approval_without_a_judge() {
        assert_eq!(
            PermissionMode::AllowAll.resolve(Decision::Approval, None),
            Decision::Allow
        );
    }

    /// Smart is the judge-backed mode. Only an explicit judge Allow runs
    /// unattended; a missing judge (None) must park, because "the judge
    /// could not be reached" is not the same as "the judge said yes".
    #[test]
    fn smart_runs_only_on_an_explicit_judge_allow() {
        assert_eq!(
            PermissionMode::Smart.resolve(Decision::Approval, Some(GateVerdict::Allow)),
            Decision::Allow
        );
        assert_eq!(
            PermissionMode::Smart.resolve(Decision::Approval, None),
            Decision::Approval,
            "a judge failure must park, never auto-approve"
        );
        assert_eq!(
            PermissionMode::Smart.resolve(
                Decision::Approval,
                Some(GateVerdict::NeedsApproval {
                    reason: "looks risky".into()
                })
            ),
            Decision::Approval
        );
    }

    /// The judge may escalate past Approval to Deny; that direction is
    /// honored rather than folded back into a park.
    #[test]
    fn smart_honors_a_judge_escalation_to_deny() {
        assert_eq!(
            PermissionMode::Smart.resolve(
                Decision::Approval,
                Some(GateVerdict::Deny {
                    reason: "exfiltrates credentials".into()
                })
            ),
            Decision::Deny
        );
    }
}
