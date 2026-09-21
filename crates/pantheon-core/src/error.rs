//! Structured failure information (§20 recovery).
//!
//! Every operation failure carries code + layer + retryability + cause +
//! remediation + evidence. Recovery classes: retry / fallback / degrade /
//! pause / resume / fail.

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
    Secrets,
    Scheduler,
    Gateway,
    Extension,
    Unknown(String),
}

/// What the supervisor may do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryClass {
    Retry,
    Fallback,
    Degrade,
    Pause,
    Resume,
    Fail,
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
    pub recovery: RecoveryClass,
}

impl PantheonError {
    pub fn new(
        code: impl Into<String>,
        layer: Layer,
        retryable: bool,
        cause: impl Into<String>,
        remediation: impl Into<String>,
        evidence: impl Into<String>,
    ) -> Self {
        let code = code.into();
        let recovery = if retryable {
            RecoveryClass::Retry
        } else {
            RecoveryClass::Fail
        };
        Self {
            code,
            layer,
            retryable,
            cause: cause.into(),
            remediation: remediation.into(),
            evidence: evidence.into(),
            recovery,
        }
    }
}

impl std::fmt::Display for PantheonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{}:{:?}] {} — fix: {}",
            self.code, self.layer, self.cause, self.remediation
        )
    }
}

impl std::error::Error for PantheonError {}
