//! Tests for `pantheon_sandbox::enforce::tests` — sibling file so sources stay test-free.
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
