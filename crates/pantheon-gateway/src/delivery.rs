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

/// Wait before the next attempt when the platform rate-limited us: the
/// server's own `Retry-After` hint wins over our exponential backoff (it
/// knows its window; we are guessing), still capped at [`MAX_BACKOFF_MS`]
/// so one hostile header cannot park the daemon. A missing/zero hint falls
/// back to [`backoff_ms`].
pub fn retry_delay_ms(err: &crate::channel::ChannelError, attempt: u32) -> u64 {
    match err.retry_after_secs {
        Some(hint) if hint > 0 => hint.saturating_mul(1000).min(MAX_BACKOFF_MS),
        _ => backoff_ms(attempt),
    }
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
