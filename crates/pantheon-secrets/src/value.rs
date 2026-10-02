//! Zeroizing secret value.
//!
//! `SecretValue` is the only type a secret travels as. It zeroizes its
//! backing buffer on drop, never appears verbatim in its own `Debug`
//! output, and exposes its contents only through [`SecretValue::expose`],
//! which is reserved for the execution boundary.

use std::fmt;
use zeroize::Zeroizing;

/// A secret value that scrubs its contents on drop.
///
/// Cloning is allowed but discouraged; prefer moving. Logs/errors/events
/// must only ever carry `Debug` or [`SecretValue::len`], never raw bytes.
#[derive(Clone, Default)]
pub struct SecretValue(Zeroizing<String>);

impl SecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Borrow the secret for the execution boundary.
    ///
    /// Do not persist, log, or embed this into model context.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretValue(***)")
    }
}

impl PartialEq for SecretValue {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for SecretValue {}

/// Item 5: redaction tests - a secret's value must never surface through
/// formatting or description paths, only through [`SecretValue::expose`]
/// at the execution boundary.
#[cfg(test)]
mod value_redaction_tests {
    use super::*;

    #[test]
    fn debug_never_shows_the_value() {
        let v = SecretValue::new("s3cr3t-value");
        let d = format!("{v:?}");
        assert_eq!(d, "SecretValue(***)");
        assert!(
            !d.contains("s3cr3t"),
            "Debug output must not contain secret material: {d}"
        );
    }

    #[test]
    fn expose_still_returns_the_value_at_the_boundary() {
        // The redaction is a display contract, not data loss: the
        // execution boundary still gets the real bytes via expose().
        assert_eq!(SecretValue::new("s3cr3t-value").expose(), "s3cr3t-value");
    }
}
