//! Tests for `pantheon_gateway::allowlist::tests` — sibling file so sources stay test-free.
use super::*;

fn joe() -> Identity {
    Identity {
        gateway: "telegram".into(),
        user: "joe".into(),
    }
}

#[test]
fn nothing_is_reachable_until_it_is_enabled() {
    let list = Allowlist::new();
    assert_eq!(list.admit(&joe()), Admission::GatewayNotAllowed);

    let mut list = Allowlist::new();
    list.enable_gateway("telegram");
    assert_eq!(
        list.admit(&joe()),
        Admission::NeedsPairing,
        "enabling a surface does not admit strangers"
    );

    list.allow(joe());
    assert_eq!(list.admit(&joe()), Admission::Allowed);
}

#[test]
fn another_gateway_is_not_a_way_in() {
    let mut list = Allowlist::new();
    list.enable_gateway("discord").allow(joe());
    assert_eq!(
        list.admit(&joe()),
        Admission::GatewayNotAllowed,
        "identity is admitted, surface is not"
    );
}

#[test]
fn blocking_beats_membership() {
    let mut list = Allowlist::new();
    list.enable_gateway("telegram").allow(joe()).block(joe());
    assert_eq!(list.admit(&joe()), Admission::Blocked);
    list.allow(joe());
    assert_eq!(list.admit(&joe()), Admission::Allowed);
}

#[test]
fn pairing_code_is_one_shot() {
    let mut pairing = Pairing::new("123456");
    let admitted = pairing.redeem("123456", &joe()).expect("codes match");
    assert_eq!(admitted, joe());
    assert!(
        pairing.redeem("123456", &joe()).is_none(),
        "a used code must not pair a second time"
    );
}

#[test]
fn wrong_code_and_closed_pairing_never_admit() {
    let mut pairing = Pairing::new("123456");
    assert!(pairing.redeem("000000", &joe()).is_none());
    // A wrong attempt does not burn the real code.
    assert!(pairing.redeem("123456", &joe()).is_some());

    let mut closed = Pairing::closed();
    assert!(closed.redeem("", &joe()).is_none());
}
