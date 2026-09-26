//! Inbound dedup (§16).
//!
//! Gateways redeliver after a reconnect or a dropped ack, and the agent must
//! nevertheless see each message exactly once. Message ids are only unique
//! within a surface, so keys are namespaced by gateway and user.
//!
//! The window is bounded: a redelivery older than `capacity` messages is
//! indistinguishable from a new message. Keep the capacity comfortably above
//! the largest reconnect backlog a surface can replay.

use crate::Identity;
use std::collections::{HashSet, VecDeque};

/// Dedup key for one inbound message.
pub fn dedup_key(from: &Identity, message_id: &str) -> String {
    format!("{}:{}:{}", from.gateway, from.user, message_id)
}

/// Bounded set of recently seen inbound messages, oldest evicted first.
#[derive(Debug)]
pub struct DedupWindow {
    capacity: usize,
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl DedupWindow {
    /// A window that remembers the last `capacity` messages. Zero capacity
    /// dedups nothing, which is only correct for a non-redelivering surface.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    /// Record a message. `true` means it is new and should be processed;
    /// `false` means it is a redelivery that must be dropped.
    pub fn observe(&mut self, from: &Identity, message_id: &str) -> bool {
        let key = dedup_key(from, message_id);
        if self.seen.contains(&key) {
            return false;
        }
        if self.capacity == 0 {
            return true;
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        while self.order.len() > self.capacity {
            if let Some(evicted) = self.order.pop_front() {
                self.seen.remove(&evicted);
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
#[path = "dedup_tests.rs"]
mod tests;
