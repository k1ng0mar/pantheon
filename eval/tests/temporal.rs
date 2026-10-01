//! Tacit temporal awareness: the model notices when a conversation has
//! meaningfully aged, without timestamping every message.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval --test temporal`.
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use pantheon_api::events::Event;
use pantheon_api::message::Message;
use pantheon_api::temporal::{resolve_tz, temporal_hint, TemporalConfig};
use pantheon_runtime::session::{outgoing_user_message, Session};
use pantheon_storage::LedgerEntry;

fn cfg() -> TemporalConfig {
    TemporalConfig::default()
}

fn lagos() -> Tz {
    Tz::Africa__Lagos
}

/// `2026-09-27 23:55` Lagos time, as epoch millis — deterministic.
fn lagos_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
    lagos()
        .with_ymd_and_hms(y, mo, d, h, mi, 0)
        .unwrap()
        .timestamp_millis()
}

fn entry(ts_ms: i64, event: Event) -> LedgerEntry {
    LedgerEntry {
        id: 1,
        run_id: "run-1".to_string(),
        seq: 1,
        ts_ms,
        event,
    }
}

fn assistant_entry(ts_ms: i64, text: &str) -> LedgerEntry {
    entry(
        ts_ms,
        Event::AssistantMessage {
            run_id: "run-1".into(),
            message: Message::assistant(text),
        },
    )
}

#[test]
fn gap_below_threshold_stays_silent() {
    // Fixed noon UTC: a live `Utc::now()` breaks this test within an hour
    // after a UTC midnight, when the date-rollover trigger ("yesterday")
    // legitimately fires for a 1h gap. Deterministic by construction.
    let now = Utc
        .with_ymd_and_hms(2026, 9, 29, 12, 0, 0)
        .unwrap()
        .timestamp_millis();
    let one_hour_ago = now - 3_600_000;
    assert_eq!(
        temporal_hint(Some(one_hour_ago), now, &Tz::UTC, &cfg()),
        None
    );
}

#[test]
fn gap_grades_are_coarse_and_human() {
    let now = Utc::now().timestamp_millis();
    let c = TemporalConfig {
        min_gap_secs: 600, // 10 minutes, so the minute grade is reachable
        ..cfg()
    };
    let cases: &[(i64, &str)] = &[
        (
            40 * 60,
            "[temporal: about 40 minutes have passed since the previous exchange]",
        ),
        (
            5 * 3600,
            "[temporal: about 5 hours have passed since the previous exchange]",
        ),
        (
            30 * 3600,
            "[temporal: about a day has passed since the previous exchange]",
        ),
        (
            3 * 86400,
            "[temporal: about 3 days have passed since the previous exchange]",
        ),
        (
            10 * 86400,
            "[temporal: about a week has passed since the previous exchange]",
        ),
        (
            20 * 86400,
            "[temporal: about 3 weeks have passed since the previous exchange]",
        ),
        (
            45 * 86400,
            "[temporal: about a month has passed since the previous exchange]",
        ),
        (
            100 * 86400,
            "[temporal: about 3 months have passed since the previous exchange]",
        ),
    ];
    for (gap_secs, want) in cases {
        let last = now - gap_secs * 1000;
        assert_eq!(
            temporal_hint(Some(last), now, &Tz::UTC, &c).as_deref(),
            Some(*want),
            "gap {gap_secs}s"
        );
    }
}

#[test]
fn midnight_rollover_triggers_on_a_short_gap() {
    // 23:55 -> 00:05 Lagos: only 10 minutes, but the date changed.
    let last = lagos_ms(2026, 9, 27, 23, 55);
    let now = lagos_ms(2026, 9, 28, 0, 5);
    assert_eq!(
        temporal_hint(Some(last), now, &lagos(), &cfg()).as_deref(),
        Some("[temporal: the previous exchange was yesterday]")
    );
}

#[test]
fn same_day_short_gap_stays_silent() {
    let last = lagos_ms(2026, 9, 28, 10, 0);
    let now = lagos_ms(2026, 9, 28, 10, 50);
    assert_eq!(temporal_hint(Some(last), now, &lagos(), &cfg()), None);
}

#[test]
fn new_session_stays_silent() {
    let now = Utc::now().timestamp_millis();
    assert_eq!(temporal_hint(None, now, &Tz::UTC, &cfg()), None);
}

#[test]
fn disabled_stays_silent() {
    let now = Utc::now().timestamp_millis();
    let c = TemporalConfig {
        enabled: false,
        ..cfg()
    };
    let last = now - 30 * 86_400_000; // a month — would otherwise fire
    assert_eq!(temporal_hint(Some(last), now, &Tz::UTC, &c), None);
}

#[test]
fn clock_skew_stays_silent() {
    let now = Utc::now().timestamp_millis();
    // Last turn timestamped in the future, or the same millisecond.
    assert_eq!(temporal_hint(Some(now + 1000), now, &Tz::UTC, &cfg()), None);
    assert_eq!(temporal_hint(Some(now), now, &Tz::UTC, &cfg()), None);
}

#[test]
fn min_gap_zero_disables_elapsed_but_keeps_rollover() {
    let c = TemporalConfig {
        min_gap_secs: 0,
        ..cfg()
    };
    // Five hours, same day: no elapsed trigger, no rollover -> silent.
    let last = lagos_ms(2026, 9, 28, 10, 0);
    let now = lagos_ms(2026, 9, 28, 15, 0);
    assert_eq!(temporal_hint(Some(last), now, &lagos(), &c), None);
    // But a midnight rollover still fires.
    let last = lagos_ms(2026, 9, 27, 23, 55);
    let now = lagos_ms(2026, 9, 28, 0, 5);
    assert_eq!(
        temporal_hint(Some(last), now, &lagos(), &c).as_deref(),
        Some("[temporal: the previous exchange was yesterday]")
    );
}

#[test]
fn rollover_opt_out_stays_silent() {
    let c = TemporalConfig {
        notify_date_change: false,
        ..cfg()
    };
    let last = lagos_ms(2026, 9, 27, 23, 55);
    let now = lagos_ms(2026, 9, 28, 0, 5);
    assert_eq!(temporal_hint(Some(last), now, &lagos(), &c), None);
}

#[test]
fn multi_day_gap_does_not_stack_wordings() {
    // Three days implies the date changed; only the elapsed wording fires.
    let now = Utc::now().timestamp_millis();
    let last = now - 3 * 86_400_000;
    let hint = temporal_hint(Some(last), now, &Tz::UTC, &cfg()).unwrap();
    assert_eq!(
        hint,
        "[temporal: about 3 days have passed since the previous exchange]"
    );
    assert!(!hint.contains("yesterday"));
}

#[test]
fn resolve_tz_honors_explicit_config_then_fails_open() {
    let c = TemporalConfig {
        timezone: Some("Africa/Lagos".to_string()),
        ..cfg()
    };
    assert_eq!(resolve_tz(&c), Tz::Africa__Lagos);
    // Garbage never panics; falls back to system local, then UTC.
    let c = TemporalConfig {
        timezone: Some("not/a-zone".to_string()),
        ..cfg()
    };
    let _ = resolve_tz(&c);
}

#[test]
fn last_assistant_ts_ms_reads_the_latest_assistant_activity() {
    use pantheon_runtime::temporal::last_assistant_ts_ms;
    let entries = vec![
        assistant_entry(1000, "first"),
        entry(
            2000,
            Event::AssistantMessage {
                run_id: "run-1".into(),
                message: Message::user("user prompts ride the same event"),
            },
        ),
        assistant_entry(3000, "second"),
    ];
    // The user-role row at 2000 is not assistant activity; the latest
    // assistant message is at 3000.
    assert_eq!(last_assistant_ts_ms(&entries), Some(3000));

    // Tool traffic counts as assistant-side activity.
    let entries = vec![entry(
        4000,
        Event::ToolMessage {
            run_id: "run-1".into(),
            message: Message::assistant("tool result"),
        },
    )];
    assert_eq!(last_assistant_ts_ms(&entries), Some(4000));

    // No assistant activity at all -> silent (new session).
    let entries = vec![entry(
        1000,
        Event::AssistantMessage {
            run_id: "run-1".into(),
            message: Message::user("hello"),
        },
    )];
    assert_eq!(last_assistant_ts_ms(&entries), None);
    assert_eq!(last_assistant_ts_ms(&[]), None);
}

#[test]
fn hint_is_ephemeral_never_persisted() {
    // The contract: the turn driver hands the *augmented* message to the
    // model (`assemble_turn`) while the ledger row keeps the raw prompt.
    // Here both sides are exercised — the augmented text carries the
    // hint, the raw text (what `chat_turn` wraps in `Message::user` for
    // the ledger emit) does not.
    let raw = "what did we decide about the deploy?";
    let hint = Some("[temporal: about 3 days have passed since the previous exchange]".to_string());
    let outgoing = outgoing_user_message(&hint, raw);
    assert!(outgoing.contains("[temporal:"));
    assert!(outgoing.starts_with(raw));

    let ledger_row = Message::user(raw);
    assert!(!ledger_row.content.contains("[temporal:"));

    // No hint -> the outgoing message is byte-identical to the prompt.
    assert_eq!(outgoing_user_message(&None, raw), raw);
}

#[test]
fn session_seam_computes_the_hint_from_replayed_entries() {
    use pantheon_api::capability::Policy;
    use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy, ReasoningLevel};
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(
        dir.path().to_path_buf(),
        Policy::coder(),
        ModelPolicy {
            reasoning_budget: None,
            reasoning: ReasoningLevel::default(),
            default: DefaultModel {
                provider: "local".into(),
                model: "default".into(),
            },
            fallbacks: FallbackChain {
                fallbacks: Vec::new(),
            },
            auxiliaries: Vec::new(),
        },
        pantheon_secrets::SecretsBroker::from_system_env(),
    )
    .unwrap();

    // An assistant turn three days ago -> a hint fires through the seam.
    let now = Utc::now().timestamp_millis();
    let entries = vec![assistant_entry(now - 3 * 86_400_000, "done")];
    let hint = session.temporal_hint_for_entries(&entries).unwrap();
    assert_eq!(
        hint,
        "[temporal: about 3 days have passed since the previous exchange]"
    );

    // Fresh activity -> silent.
    let entries = vec![assistant_entry(now - 60_000, "just now")];
    assert_eq!(session.temporal_hint_for_entries(&entries), None);

    // Toggling the config off silences the seam without touching turns.
    session.set_temporal_config(TemporalConfig {
        enabled: false,
        ..TemporalConfig::default()
    });
    let entries = vec![assistant_entry(now - 3 * 86_400_000, "done")];
    assert_eq!(session.temporal_hint_for_entries(&entries), None);
}
