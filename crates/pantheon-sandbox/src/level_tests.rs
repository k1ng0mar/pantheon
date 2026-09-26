//! Tests for `pantheon_sandbox::level::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn levels_order_from_weakest_to_strongest() {
    assert!(SandboxLevel::Low < SandboxLevel::Medium);
    assert!(SandboxLevel::Medium < SandboxLevel::High);
    assert!(SandboxLevel::High < SandboxLevel::VeryHigh);
}

#[test]
fn each_level_maps_to_its_own_boundary() {
    assert_eq!(SandboxLevel::Low.boundary(), ExecutionBoundary::InProcess);
    assert_eq!(
        SandboxLevel::Medium.boundary(),
        ExecutionBoundary::IsolatedProcess
    );
    assert_eq!(SandboxLevel::High.boundary(), ExecutionBoundary::Container);
    assert_eq!(SandboxLevel::VeryHigh.boundary(), ExecutionBoundary::Vm);
}

#[test]
fn strong_boundaries_are_hardened() {
    for level in [SandboxLevel::High, SandboxLevel::VeryHigh] {
        let profile = level.profile();
        assert!(
            profile.drop_capabilities,
            "{level:?} must drop capabilities"
        );
        assert!(profile.no_new_privs, "{level:?} must set no-new-privs");
        assert!(
            profile.max_memory_mb.is_some(),
            "{level:?} needs a memory cap"
        );
        assert!(profile.max_pids.is_some(), "{level:?} needs a pids cap");
    }
}

#[test]
fn in_process_and_isolated_steps_are_offline() {
    assert!(!SandboxLevel::Low.profile().network);
    assert!(!SandboxLevel::Medium.profile().network);
    // The isolated process step is the one that gets no-new-privs.
    assert!(SandboxLevel::Medium.profile().no_new_privs);
    assert!(SandboxLevel::Low.profile().max_pids.is_none());
}
