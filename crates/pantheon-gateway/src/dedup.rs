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
        Self { capacity, order: VecDeque::new(), seen: HashSet::new() }
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
mod tests {
    use super::*;

    fn joe() -> Identity {
        Identity { gateway: "discord".into(), user: "joe".into() }
    }

    #[test]
    fn a_redelivery_is_dropped() {
        let mut window = DedupWindow::new(16);
        assert!(window.observe(&joe(), "m1"), "first delivery is processed");
        assert!(!window.observe(&joe(), "m1"), "redelivery is dropped");
        assert!(window.observe(&joe(), "m2"), "a new message is processed");
        assert_eq!(window.len(), 2);
    }

    #[test]
    fn the_same_id_on_a_different_surface_is_a_different_message() {
        let telegram = Identity { gateway: "telegram".into(), user: "joe".into() };
        let discord = Identity { gateway: "discord".into(), user: "joe".into() };
        let mut window = DedupWindow::new(16);
        assert!(window.observe(&telegram, "42"));
        assert!(
            window.observe(&discord, "42"),
            "ids are only unique within a surface"
        );
    }

    #[test]
    fn the_same_id_from_a_different_user_is_not_collapsed() {
        let a = Identity { gateway: "discord".into(), user: "a".into() };
        let b = Identity { gateway: "discord".into(), user: "b".into() };
        let mut window = DedupWindow::new(16);
        assert!(window.observe(&a, "1"));
        assert!(window.observe(&b, "1"));
    }

    #[test]
    fn the_window_is_bounded() {
        let mut window = DedupWindow::new(2);
        for id in ["m1", "m2", "m3"] {
            assert!(window.observe(&joe(), id));
        }
        assert_eq!(window.len(), 2, "oldest entry is evicted");
        // m1 has aged out, so a very late redelivery of it looks new again.
        assert!(window.observe(&joe(), "m1"));
        assert!(!window.observe(&joe(), "m3"), "recent entries are still held");
    }

    #[test]
    fn a_zero_capacity_window_dedups_nothing() {
        let mut window = DedupWindow::new(0);
        assert!(window.observe(&joe(), "m1"));
        assert!(window.observe(&joe(), "m1"));
        assert!(window.is_empty());
    }
}
