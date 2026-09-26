//! Tests for `pantheon_gateway::dedup::tests` — sibling file so sources stay test-free.
use super::*;

fn joe() -> Identity {
    Identity {
        gateway: "discord".into(),
        user: "joe".into(),
    }
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
    let telegram = Identity {
        gateway: "telegram".into(),
        user: "joe".into(),
    };
    let discord = Identity {
        gateway: "discord".into(),
        user: "joe".into(),
    };
    let mut window = DedupWindow::new(16);
    assert!(window.observe(&telegram, "42"));
    assert!(
        window.observe(&discord, "42"),
        "ids are only unique within a surface"
    );
}

#[test]
fn the_same_id_from_a_different_user_is_not_collapsed() {
    let a = Identity {
        gateway: "discord".into(),
        user: "a".into(),
    };
    let b = Identity {
        gateway: "discord".into(),
        user: "b".into(),
    };
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
    assert!(
        !window.observe(&joe(), "m3"),
        "recent entries are still held"
    );
}

#[test]
fn a_zero_capacity_window_dedups_nothing() {
    let mut window = DedupWindow::new(0);
    assert!(window.observe(&joe(), "m1"));
    assert!(window.observe(&joe(), "m1"));
    assert!(window.is_empty());
}
