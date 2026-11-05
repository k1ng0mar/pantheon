//! Per-run credential grants for delegated child sessions.
//!
//! A `CredentialGrant` names exactly which secrets a child run may
//! resolve and for which purpose. The scoped broker checks the grant
//! before resolving and refuses ungranted names instead of falling
//! through to unrestricted environment lookup. Grants are computed as
//! intersections at each delegation level, never unions, and expire
//! with the child run. Broker scoping alone does not stop a child with
//! shell access from reading secret files directly; filesystem
//! confinement is the separate half of the boundary.

use crate::{SecretVault, SecretsBroker, SecretsError};

/// Why a child run needs a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialPurpose {
    Provider,
    Mcp,
    BrowserFill,
    Gateway,
}

/// One explicit grant for one child run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialGrant {
    pub run_id: String,
    pub name: String,
    pub purpose: CredentialPurpose,
}

impl CredentialGrant {
    pub fn new(
        run_id: impl Into<String>,
        name: impl Into<String>,
        purpose: CredentialPurpose,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            name: name.into(),
            purpose,
        }
    }
}

/// Intersection of parent effective grants and child requested grants.
/// Grandchildren therefore can only narrow, never widen.
pub fn intersect_grants(
    parent: &[CredentialGrant],
    requested: &[CredentialGrant],
) -> Vec<CredentialGrant> {
    let mut out = Vec::new();
    for r in requested {
        if parent
            .iter()
            .any(|p| p.name == r.name && p.purpose == r.purpose)
            && !out
                .iter()
                .any(|g: &CredentialGrant| g.name == r.name && g.purpose == r.purpose)
        {
            out.push(r.clone());
        }
    }
    out
}

/// Broker wrapper that enforces a grant set. Un-granted names return
/// `None` from `inject` and `SECRET_NOT_GRANTED` from `resolve`, never
/// a fall through to the inner broker's environment.
#[derive(Debug)]
pub struct ScopedBroker {
    inner: SecretsBroker,
    grants: Vec<CredentialGrant>,
    run_id: String,
}

impl ScopedBroker {
    pub fn new(
        inner: SecretsBroker,
        run_id: impl Into<String>,
        grants: Vec<CredentialGrant>,
    ) -> Self {
        Self {
            inner,
            run_id: run_id.into(),
            grants,
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    pub fn grants(&self) -> &[CredentialGrant] {
        &self.grants
    }

    fn granted(&self, name: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.name == name && g.run_id == self.run_id)
    }

    pub fn inject(&self, name: &str) -> Result<Option<crate::SecretValue>, SecretsError> {
        if !self.granted(name) {
            return Ok(None);
        }
        self.inner.inject(name)
    }

    pub fn resolve(&self, name: &str) -> Result<Option<crate::SecretValue>, SecretsError> {
        if !self.granted(name) {
            return Err(SecretsError::Unsupported(format!(
                "SECRET_NOT_GRANTED: {name} is not granted to run {}",
                self.run_id
            )));
        }
        self.inner.resolve(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SecretValue;

    fn broker_with(name: &str, value: &str) -> SecretsBroker {
        let mut b = SecretsBroker::new();
        let mem = crate::MemoryVault::new();
        let _ = mem.set(name, SecretValue::new(value));
        b = b.with_vault(Box::new(mem));
        b
    }

    #[test]
    fn scoped_broker_refuses_ungranted_name() {
        let inner = broker_with("PROVIDER_KEY", "secret");
        let scoped = ScopedBroker::new(inner, "run-1", vec![]);
        assert!(scoped.inject("PROVIDER_KEY").unwrap().is_none());
        let err = scoped.resolve("PROVIDER_KEY").unwrap_err();
        assert!(err.to_string().contains("SECRET_NOT_GRANTED"));
    }

    #[test]
    fn scoped_broker_allows_granted_name() {
        let inner = broker_with("PROVIDER_KEY", "secret");
        let scoped = ScopedBroker::new(
            inner,
            "run-1",
            vec![CredentialGrant::new(
                "run-1",
                "PROVIDER_KEY",
                CredentialPurpose::Provider,
            )],
        );
        assert_eq!(
            scoped.inject("PROVIDER_KEY").unwrap().unwrap().expose(),
            "secret"
        );
    }

    #[test]
    fn intersect_narrows_not_widens() {
        let parent = vec![
            CredentialGrant::new("p", "A", CredentialPurpose::Provider),
            CredentialGrant::new("p", "B", CredentialPurpose::Provider),
        ];
        let requested = vec![
            CredentialGrant::new("c", "B", CredentialPurpose::Provider),
            CredentialGrant::new("c", "C", CredentialPurpose::Provider),
        ];
        let out = intersect_grants(&parent, &requested);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "B");
    }
}
