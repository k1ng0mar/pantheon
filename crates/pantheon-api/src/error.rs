//! Structured failure information (§20 recovery).
//!
//! Every operation failure carries code + layer + retryability + cause +
//! remediation + evidence.

use serde::{Deserialize, Serialize};

/// Where the failure happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layer {
    Runtime,
    Agent,
    Execution,
    Capability,
    Provider,
    Storage,
    Memory,
    Extension,
    Unknown(String),
}

/// Structured runtime error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PantheonError {
    pub code: String,
    pub layer: Layer,
    pub retryable: bool,
    pub cause: String,
    pub remediation: String,
    pub evidence: String,
}

/// Recovery guidance lives in the `retryable` flag plus `remediation`:
/// retryable errors may be retried, everything else fails. A finer
/// recovery classification (fallback / degrade / pause / resume) is a
/// §20 supervisor concern, not per-error data.

impl PantheonError {
    pub fn new(
        code: impl Into<String>,
        layer: Layer,
        retryable: bool,
        cause: impl Into<String>,
        remediation: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            layer,
            retryable,
            cause: cause.into(),
            remediation: remediation.into(),
            evidence: evidence.into(),
        }
    }
}

impl std::fmt::Display for PantheonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}:{:?}] {} - fix: {}",
            self.code, self.layer, self.cause, self.remediation
        )
    }
}

impl std::error::Error for PantheonError {}
