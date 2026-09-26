//! Tests for `pantheon_capability::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn coder_push_needs_approval() {
    let p = Policy::coder();
    assert_eq!(check(&p, &Capability::ShellExecute), Verdict::Allow);
    assert!(matches!(
        check(&p, &Capability::GitPush),
        Verdict::NeedsApproval { .. }
    ));
    assert!(matches!(
        check(&p, &Capability::Browser),
        Verdict::Deny { .. }
    ));
    assert!(enforce(&p, &Capability::Browser).is_err());
}
#[test]
fn researcher_is_readonly() {
    let p = Policy::researcher_readonly();
    assert_eq!(check(&p, &Capability::FilesystemRead), Verdict::Allow);
    assert!(matches!(
        check(&p, &Capability::FilesystemWrite),
        Verdict::Deny { .. }
    ));
}
