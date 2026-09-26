//! Tests for `pantheon_exec::danger::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn rm_rf_root_is_blocked_in_all_spellings() {
    for cmd in [
        "rm -rf /",
        "rm -fr /",
        "rm -r -f /",
        "sudo rm -rf /",
        "cd /tmp && rm -rf \" / \"",
    ] {
        let a = assess(cmd);
        assert_eq!(a.level, RiskLevel::Critical, "{cmd}");
        assert!(a.matches.iter().any(|m| m.rule == "rm_rf_root"), "{cmd}");
    }
}

#[test]
fn fork_bomb_is_blocked() {
    assert_eq!(assess(":(){ :|:& };:").level, RiskLevel::Critical);
}

#[test]
fn dd_to_block_device_is_blocked() {
    assert_eq!(
        assess("dd if=/dev/zero of=/dev/sda bs=1M").level,
        RiskLevel::Critical
    );
}

#[test]
fn mkfs_on_device_is_blocked() {
    assert_eq!(assess("mkfs.ext4 /dev/sdb1").level, RiskLevel::Critical);
}

#[test]
fn ordinary_commands_pass() {
    for cmd in [
        "ls -la",
        "rm -rf ./build",         // project-local delete is allowed
        "rm -rf /tmp/pantheon-x", // tmp paths are fine
        "git push origin main",
        "dd if=a of=b", // dd but not to a block device
        "chmod +x script.sh",
        "cargo test --workspace",
        "echo \"rm -rf /\"", // quoted mention in an argument
    ] {
        let a = assess(cmd);
        assert_eq!(a.level, RiskLevel::Low, "{cmd} must pass: {a:?}");
    }
}

#[test]
fn gate_returns_structured_error_with_rule_names() {
    let err = gate("rm -rf /").unwrap_err();
    assert_eq!(err.code, "DANGER_BLOCKED");
    assert!(err.cause.contains("rm_rf_root"));
}

#[test]
fn normalization_collapses_quotes_and_case() {
    let a = assess("RM   -R -F   \"/\" ");
    assert_eq!(a.level, RiskLevel::Critical);
}

#[test]
fn is_git_push_catches_every_spelling_the_model_would_try() {
    for cmd in [
        "git push",
        "git push origin main",
        "git -C /tmp/repo push origin main",
        "git --git-dir=/tmp/repo/.git push",
        "git -c user.email=a@b push",
        "cd /tmp && git push",
        "sudo git push",
        "FOO=1 git push",
        "echo hi && git push",
        "git push --force",
    ] {
        assert!(is_git_push(cmd), "must gate: {cmd}");
    }
}

#[test]
fn is_git_push_ignores_non_push_git_and_non_git_commands() {
    for cmd in [
        "git status",
        "git log --oneline",
        "git commit -m x",
        "ls -la",
        "echo git push",
        "git",
        "pushd /tmp/repo",
    ] {
        assert!(!is_git_push(cmd), "must not gate: {cmd}");
    }
}
