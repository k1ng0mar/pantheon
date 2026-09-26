//! Tests for `pantheon_cli::session_cli::tests` — sibling file so sources stay test-free.
use super::*;

fn repl(tag: &str) -> (Repl, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("pantheon-name-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let model_policy = pantheon_core::model::ModelPolicy {
        default: pantheon_core::model::DefaultModel {
            provider: "local".into(),
            model: "m".into(),
        },
        fallbacks: pantheon_core::model::FallbackChain::default(),
        auxiliaries: Vec::new(),
    };
    let session = Session::new(
        dir.clone(),
        pantheon_core::capability::Policy::coder(),
        model_policy,
        pantheon_secrets::SecretsBroker::from_system_env(),
    )
    .unwrap();
    (
        Repl {
            session,
            run_id: "run_name_t".into(),
            policy_preset: "coder".into(),
            model: None,
            namespace: "nyx".into(),
        },
        dir,
    )
}

fn title_of(r: &Repl) -> Option<String> {
    r.session
        .supervisor
        .ledger_title(&r.run_id)
        .unwrap()
        .filter(|t| !t.is_empty())
}

/// /name renames the conversation, survives restarts (durable event),
/// normalizes like a model reply, and last write wins.
#[test]
fn name_renames_the_conversation_durably() {
    let (mut r, dir) = repl("rename");
    // Rename a run that has never chatted: the row is created for it.
    assert!(command(&mut r, "/name Fix the login bug"));
    assert_eq!(title_of(&r).as_deref(), Some("Fix the login bug"));
    // Recorded as a manual title event (source/model for /explain).
    let entries = r.session.supervisor.replay(&r.run_id).unwrap();
    assert!(entries.iter().any(|e| matches!(
        &e.event,
        pantheon_core::events::Event::SessionTitled { source, model, .. }
            if source == "manual" && model == "user"
    )));
    // Quotes are stripped, exactly like the bounded model path.
    assert!(command(&mut r, "/name \"Second title\""));
    assert_eq!(title_of(&r).as_deref(), Some("Second title"));
    // Bare /name reports without changing anything.
    assert!(command(&mut r, "/name"));
    assert_eq!(title_of(&r).as_deref(), Some("Second title"));
    // Empty input is rejected, not stored.
    assert!(command(&mut r, "/name    "));
    assert_eq!(title_of(&r).as_deref(), Some("Second title"));
    let _ = std::fs::remove_dir_all(&dir);
}
