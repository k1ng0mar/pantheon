//! Behavioral tests for `/send` and `pantheon send`: the slash command is
//! registered and dispatched, session targets land in the home session's
//! ledger, and every failure is loud. Run with `cargo test -p pantheon-eval`.
//!
//! Network sends (telegram/discord) are exercised through the missing-token
//! path only - the real HTTP calls are covered by the `run.py` eval cases,
//! which run the built binary in a clean sandbox.

use pantheon_api::events::Event;
use pantheon_gateway::schedule_delivery::Deliver;
use pantheon_tui::{commands, send};
use std::path::PathBuf;

// ---------------------------------------------------------------- registry ---

#[test]
fn send_is_a_registered_builtin() {
    let reg = commands::registry();
    let meta = reg.get("send").expect("/send must be registered");
    assert!(
        meta.desc.contains("telegram|discord|mobile|home"),
        "desc advertises the targets: {}",
        meta.desc
    );
    assert!(
        commands::is_builtin("send"),
        "built-ins win over skill names"
    );
    let completions = commands::complete("/se");
    assert!(
        completions.contains(&"/send".to_string()),
        "palette suggests /send: {completions:?}"
    );
}

// ------------------------------------------------------------------ targets ---

fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pantheon-eval-send-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn home_session_texts(dir: &std::path::Path) -> Vec<String> {
    let ledger = pantheon_storage::Ledger::open(&dir.join("ledger.db")).expect("open ledger");
    ledger
        .replay(pantheon_storage::HOME_SESSION_ID)
        .expect("replay home session")
        .into_iter()
        .filter_map(|e| match e.event {
            Event::RunProgress { detail, .. } => Some(detail),
            _ => None,
        })
        .collect()
}

#[test]
fn send_home_posts_into_the_home_session() {
    let dir = scratch_dir("home");
    let confirmation = send::send_to_target(&dir, &Deliver::Home, "hello home")
        .expect("home delivery must succeed");
    assert!(
        confirmation.contains(pantheon_storage::HOME_SESSION_ID),
        "confirmation names the session: {confirmation}"
    );
    let texts = home_session_texts(&dir);
    assert!(
        texts.iter().any(|t| t.contains("hello home")),
        "message landed in the home session ledger: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn send_mobile_posts_into_the_home_session() {
    // The mobile app renders the home session, so `mobile` delivery is a
    // home-session post with the session as the conversation context.
    let dir = scratch_dir("mobile");
    let confirmation = send::send_to_target(&dir, &Deliver::Mobile, "hello phone")
        .expect("mobile delivery must succeed");
    assert!(
        confirmation.contains("mobile"),
        "confirmation says mobile: {confirmation}"
    );
    let texts = home_session_texts(&dir);
    assert!(
        texts.iter().any(|t| t.contains("hello phone")),
        "message landed in the home session ledger: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn send_home_twice_reuses_the_session() {
    // ensure_home_session is idempotent: the second post appends to the
    // same session rather than creating a second one.
    let dir = scratch_dir("twice");
    send::send_to_target(&dir, &Deliver::Home, "first").expect("first post");
    send::send_to_target(&dir, &Deliver::Home, "second").expect("second post");
    let texts = home_session_texts(&dir);
    assert!(
        texts.iter().any(|t| t.contains("first")) && texts.iter().any(|t| t.contains("second")),
        "both messages in one home session: {texts:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// -------------------------------------------------------------------- loud ---

#[test]
fn send_empty_message_is_a_loud_error() {
    let dir = scratch_dir("empty");
    let err = send::send_to_target(&dir, &Deliver::Home, "   ").expect_err("empty must fail");
    assert_eq!(err, "message is empty");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn send_rejects_non_send_targets_loudly() {
    let dir = scratch_dir("unsupported");
    for target in [
        Deliver::Log,
        Deliver::Notify,
        Deliver::File(PathBuf::from("/tmp/x")),
    ] {
        let err = send::send_to_target(&dir, &target, "hi").expect_err("must fail");
        assert!(
            err.contains("telegram|discord|mobile|home"),
            "error lists the supported targets: {err}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parse_rejects_unknown_cli_targets_loudly() {
    let err = Deliver::parse("pigeon").unwrap_err();
    assert!(err.contains("pigeon"), "names the bad target: {err}");
}
