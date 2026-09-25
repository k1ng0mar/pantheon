//! Secrets (§13). Keys never in prompts or logs.
//!
//! The broker owns resolution and injection:
//! - `SecretValue` is a zeroizing wrapper — dropped bytes are scrubbed, and
//!   its `Debug` output never contains the value.
//! - Vaults are pluggable: OS keychain ([`keychain::KeychainVault`] —
//!   macOS Keychain, Windows CredMan, Linux Secret Service), encrypted
//!   local vault, env compat.
//! - `SecretsBroker::inject` is the execution-boundary API. Callers get a
//!   value or `None`; they never see a secret rendered into context or logs.
//!
//! Resolution order (documented, deterministic): configured durable vaults
//! in insertion order, then the environment (`PANTHEON_SECRET_*`).

pub mod broker;
pub mod env;
pub mod error;
pub mod filevault;
pub mod keychain;
pub mod value;
pub mod vault;

pub use broker::SecretsBroker;
pub use env::EnvVault;
pub use error::SecretsError;
pub use filevault::EncryptedFileVault;
pub use keychain::KeychainVault;
pub use value::SecretValue;
pub use vault::{MemoryVault, SecretVault};
