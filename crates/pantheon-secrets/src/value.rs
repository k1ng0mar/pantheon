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
#[derive(Clone)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_reveals_value() {
        let s = SecretValue::new("hunter2-dont-log-me");
        let rendered = format!("{s:?}");
        assert!(!rendered.contains("hunter2"));
        assert_eq!(rendered, "SecretValue(***)");
        // Length is safe to surface.
        assert_eq!(s.len(), "hunter2-dont-log-me".len());
    }

    #[test]
    fn round_trips_value() {
        let s = SecretValue::new("sk-live-123");
        assert_eq!(s.expose(), "sk-live-123");
        assert_eq!(s, SecretValue::new("sk-live-123"));
        assert_ne!(s, SecretValue::new("sk-live-456"));
    }
}
