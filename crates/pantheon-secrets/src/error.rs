//! Structured secrets failure information.

use std::fmt;

/// Errors produced by vaults and the broker.
#[derive(Debug)]
pub enum SecretsError {
    /// Filesystem-level failure (key file, encrypted vault file, perms).
    Io(std::io::Error),
    /// Encryption/decryption failure (tampered file, wrong key, corrupt data).
    Crypto(String),
    /// The operation is not available for this vault/backend.
    Unsupported(String),
    /// The request itself was invalid (empty name, bad reference).
    Invalid(String),
    /// The platform backend failed (OS keychain locked, D-Bus error,
    /// credential store missing). The secret itself may well exist
    /// the store just could not answer.
    Backend(String),
}

impl fmt::Display for SecretsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretsError::Io(e) => write!(f, "secrets I/O error: {e}"),
            SecretsError::Crypto(m) => write!(f, "secrets crypto failure: {m}"),
            SecretsError::Unsupported(m) => write!(f, "secrets unsupported: {m}"),
            SecretsError::Invalid(m) => write!(f, "invalid secrets request: {m}"),
            SecretsError::Backend(m) => write!(f, "secrets backend failure: {m}"),
        }
    }
}

impl std::error::Error for SecretsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SecretsError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SecretsError {
    fn from(e: std::io::Error) -> Self {
        SecretsError::Io(e)
    }
}

impl From<serde_json::Error> for SecretsError {
    fn from(e: serde_json::Error) -> Self {
        SecretsError::Crypto(format!("json: {e}"))
    }
}

/// Name validation shared by vaults and the broker.
pub(crate) fn validate_name(name: &str) -> Result<(), SecretsError> {
    if name.is_empty() {
        return Err(SecretsError::Invalid(
            "secret name must not be empty".into(),
        ));
    }
    if name.len() > 256 {
        return Err(SecretsError::Invalid(
            "secret name exceeds 256 chars".into(),
        ));
    }
    Ok(())
}
