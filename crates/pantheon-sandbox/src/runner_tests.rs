//! Tests for `pantheon_sandbox::runner::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn in_process_boundary_uses_direct_command() {
    let profile = SandboxProfile {
        level: crate::SandboxLevel::Low,
        boundary: crate::ExecutionBoundary::InProcess,
        drop_capabilities: false,
        no_new_privs: false,
        network: false,
        max_memory_mb: None,
        max_pids: None,
        wall_clock_ms: 30_000,
        allow_direct_fallback: false,
    };
    let cmd = build_sandboxed(&profile, "echo", &["hello"], "/tmp");
    assert_eq!(cmd.get_program(), "echo");
}

#[test]
fn container_boundary_uses_bwrap_if_available() {
    let profile = crate::SandboxLevel::High.profile();
    let cmd = build_sandboxed(&profile, "echo", &["hello"], "/tmp");
    let prog = cmd.get_program().to_string_lossy().to_string();
    // Should be bwrap if available, or echo as fallback
    assert!(prog == "bwrap" || prog == "echo", "got: {}", prog);
}

#[test]
fn high_level_attempts_userns() {
    let profile = crate::SandboxLevel::High.profile();
    let cmd = build_sandboxed(&profile, "ls", &[], "/tmp");
    let prog = cmd.get_program().to_string_lossy().to_string();
    if prog == "bwrap" {
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // High should attempt user namespace isolation
        assert!(
            args.iter().any(|a| a == "--unshare-user-try"),
            "high should attempt user namespace: {:?}",
            args
        );
    }
    // If bwrap is not available, the command falls back to direct exec
}
