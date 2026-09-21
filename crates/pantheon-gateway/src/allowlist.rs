//! Auth, allowlist, and pairing (§16).
//!
//! Default-deny, like every other boundary in the runtime: nothing is
//! reachable until a gateway is enabled and an identity is admitted, and a
//! stranger must pair before the agent will hear it.

use crate::Identity;
use std::collections::HashSet;

/// Why an inbound identity may or may not reach the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// Known identity on an enabled surface.
    Allowed,
    /// Identity is admitted, but its surface is not enabled.
    GatewayNotAllowed,
    /// Explicitly blocked; never re-admitted by pairing.
    Blocked,
    /// Unknown identity: pair first.
    NeedsPairing,
}

/// Which surfaces and identities may reach the runtime.
#[derive(Debug, Default)]
pub struct Allowlist {
    gateways: HashSet<String>,
    users: HashSet<Identity>,
    blocked: HashSet<Identity>,
}

impl Allowlist {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable a surface. Until this is called the gateway is closed.
    pub fn enable_gateway(&mut self, gateway: &str) -> &mut Self {
        self.gateways.insert(gateway.to_string());
        self
    }

    /// Admit a known identity and clear any block on it.
    pub fn allow(&mut self, id: Identity) -> &mut Self {
        self.blocked.remove(&id);
        self.users.insert(id);
        self
    }

    /// Block an identity, overriding membership.
    pub fn block(&mut self, id: Identity) -> &mut Self {
        self.users.remove(&id);
        self.blocked.insert(id);
        self
    }

    /// Decide one inbound identity.
    pub fn admit(&self, id: &Identity) -> Admission {
        if self.blocked.contains(id) {
            return Admission::Blocked;
        }
        if !self.gateways.contains(&id.gateway) {
            return Admission::GatewayNotAllowed;
        }
        if !self.users.contains(id) {
            return Admission::NeedsPairing;
        }
        Admission::Allowed
    }
}

/// One-shot pairing: an operator mints a code, an unknown identity redeems
/// it, and the code burns so a shared secret cannot be replayed.
#[derive(Debug)]
pub struct Pairing {
    code: Option<String>,
    used: bool,
}

impl Pairing {
    pub fn new(code: &str) -> Self {
        Self {
            code: Some(code.to_string()),
            used: false,
        }
    }

    /// A pairing that accepts nobody (the default posture).
    pub fn closed() -> Self {
        Self {
            code: None,
            used: false,
        }
    }

    /// Redeem a presented code. `None` means wrong, already used, or closed.
    /// On success the identity is returned for the caller to `allow`.
    pub fn redeem(&mut self, presented: &str, id: &Identity) -> Option<Identity> {
        if self.used {
            return None;
        }
        match self.code.as_deref() {
            Some(code) if code == presented => {
                self.used = true;
                self.code = None;
                Some(id.clone())
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
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
}
