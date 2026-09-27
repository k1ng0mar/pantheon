//! Tests for `pantheon_runtime::session::cancel_tests` — sibling file so sources stay test-free.
use super::*;

fn test_session(tag: &str) -> (Session, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("pantheon-cancel-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let policy = pantheon_api::capability::Policy::coder();
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "test".into(),
            model: "test".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: Vec::new(),
    };
    let secrets = pantheon_secrets::SecretsBroker::from_system_env();
    let s = Session::new(dir.clone(), policy, model_policy, secrets).unwrap();
    (s, dir)
}

/// The cancel token starts clear, and reset_cancel clears it again.
#[test]
fn cancel_token_lifecycle() {
    let (s, _dir) = test_session("tok");
    assert!(!s.is_canceled(), "fresh session is not canceled");
    s.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(s.is_canceled(), "token observed");
    s.reset_cancel();
    assert!(!s.is_canceled(), "reset clears the token");
}
