//! Delivery planning and reconnect handling (§16).
//!
//! Surfacing failures must never lose agent output or silently duplicate it:
//! a retryable failure is retried with backoff, a permanent failure is
//! handed back with a reason, and messages queued while a surface was down
//! are flushed in order on reconnect.

use crate::OutboundMessage;

/// What to do with one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Delivered,
    /// Try again in `after_ms`.
    Retry {
        attempt: u32,
        after_ms: u64,
    },
    /// Do not retry; report why (bad target, blocked, message too large).
    Dropped {
        reason: String,
    },
}

/// Cap on backoff so a long outage does not push retries hours out.
pub const MAX_BACKOFF_MS: u64 = 60_000;

/// Exponential backoff: 1s, 2s, 4s, ... capped at [`MAX_BACKOFF_MS`].
pub fn backoff_ms(attempt: u32) -> u64 {
    // Shift is capped before use so a large attempt cannot overflow.
    1_000u64
        .checked_shl(attempt.min(6))
        .unwrap_or(MAX_BACKOFF_MS)
        .min(MAX_BACKOFF_MS)
}

/// Plan the next step for a delivery attempt (`attempt` is 1-based).
pub fn plan_delivery(attempt: u32, max_attempts: u32, retryable: bool) -> DeliveryOutcome {
    if !retryable {
        return DeliveryOutcome::Dropped {
            reason: "surface rejected the message permanently".to_string(),
        };
    }
    if attempt >= max_attempts {
        return DeliveryOutcome::Dropped {
            reason: format!("gave up after {attempt} attempts"),
        };
    }
    DeliveryOutcome::Retry {
        attempt,
        after_ms: backoff_ms(attempt),
    }
}

/// Messages waiting for a surface to come back.
///
/// Kept in order: a conversation must not see replies out of sequence after
/// a reconnect.
#[derive(Debug, Default)]
pub struct Outbox {
    pending: Vec<OutboundMessage>,
}

impl Outbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a message for a surface that is currently down.
    pub fn enqueue(&mut self, message: OutboundMessage) {
        self.pending.push(message);
    }

    /// Surface is back: hand over everything queued, in order.
    pub fn on_reconnect(&mut self) -> Vec<OutboundMessage> {
        std::mem::take(&mut self.pending)
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> OutboundMessage {
        OutboundMessage {
            to_conversation: "c1".into(),
            text: text.into(),
        }
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
}
