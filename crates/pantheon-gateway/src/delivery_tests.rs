//! Tests for `pantheon_gateway::delivery::tests` — sibling file so sources stay test-free.
use super::*;

fn msg(text: &str) -> OutboundMessage {
    OutboundMessage::new("c1", text, "")
}

#[test]
fn retryable_failures_back_off_exponentially_then_stop() {
    assert_eq!(
        plan_delivery(1, 5, true),
        DeliveryOutcome::Retry {
            attempt: 1,
            after_ms: 2_000
        }
    );
    assert_eq!(
        plan_delivery(3, 5, true),
        DeliveryOutcome::Retry {
            attempt: 3,
            after_ms: 8_000
        }
    );
    match plan_delivery(5, 5, true) {
        DeliveryOutcome::Dropped { reason } => assert!(reason.contains("5 attempts")),
        other => panic!("expected Drop, got {other:?}"),
    }
}

#[test]
fn backoff_is_capped_and_never_overflows() {
    assert_eq!(backoff_ms(0), 1_000);
    assert_eq!(backoff_ms(6), 60_000);
    assert_eq!(backoff_ms(64), MAX_BACKOFF_MS, "huge attempts stay capped");
    assert_eq!(backoff_ms(u32::MAX), MAX_BACKOFF_MS);
}

#[test]
fn permanent_failures_are_not_retried() {
    assert!(matches!(
        plan_delivery(1, 5, false),
        DeliveryOutcome::Dropped { .. }
    ));
}

#[test]
fn queued_messages_flush_in_order_on_reconnect() {
    let mut outbox = Outbox::new();
    outbox.enqueue(msg("first"));
    outbox.enqueue(msg("second"));
    assert_eq!(outbox.len(), 2);

    let flushed = outbox.on_reconnect();
    assert_eq!(flushed.len(), 2);
    assert_eq!(flushed[0].text, "first");
    assert_eq!(flushed[1].text, "second");
    assert!(
        outbox.is_empty(),
        "flushing must not replay on the next reconnect"
    );
}

#[test]
fn server_hint_beats_the_guess_but_stays_capped() {
    use super::*;
    use crate::channel::ChannelError;
    let mut hinted = ChannelError::new("X_RATE_LIMITED", "slow down");
    hinted.retry_after_secs = Some(5);
    assert_eq!(retry_delay_ms(&hinted, 0), 5_000);
    // Even attempt 0's 1s guess loses to the server's 5s.
    assert!(retry_delay_ms(&hinted, 0) > backoff_ms(0));
    // Hostile hint still capped.
    hinted.retry_after_secs = Some(999_999);
    assert_eq!(retry_delay_ms(&hinted, 0), MAX_BACKOFF_MS);
    // No hint: pure exponential guess.
    let plain = ChannelError::new("X_RATE_LIMITED", "slow down");
    assert_eq!(retry_delay_ms(&plain, 2), backoff_ms(2));
}
