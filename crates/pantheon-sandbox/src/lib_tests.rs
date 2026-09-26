//! Tests for `pantheon_sandbox::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn levels_escalate_with_risk() {
    assert_eq!(
        SandboxLevel::for_capability(&Capability::FilesystemRead),
        SandboxLevel::Low
    );
    assert_eq!(
        SandboxLevel::for_capability(&Capability::ShellExecute),
        SandboxLevel::High
    );
    assert_eq!(
        SandboxLevel::for_capability(&Capability::SecretsUse),
        SandboxLevel::VeryHigh
    );
    assert_eq!(
        SandboxLevel::for_capability(&Capability::GitPush),
        SandboxLevel::VeryHigh
    );
}
