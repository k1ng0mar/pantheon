//! Secrets (§13). Keys never in prompts or logs.
//!
//! The broker owns resolution and injection:
//! - `SecretValue` is a zeroizing wrapper - dropped bytes are scrubbed, and
//!   its `Debug` output never contains the value.
//! - Vaults are pluggable: OS keychain ([`keychain::KeychainVault`]
//!   macOS Keychain, Windows CredMan, Linux Secret Service), encrypted
//!   local vault, env compat.
//! - `SecretsBroker::inject` is the execution-boundary API. Callers get a
//!   value or `None`; they never see a secret rendered into context or logs.
//!
//! Resolution order (documented, deterministic): configured durable vaults
//! in insertion order, then the environment (`PANTHEON_SECRET_*`).

pub mod broker;
pub mod dotenv;
pub mod env;
pub mod error;
pub mod filevault;
pub mod gateway_tokens;
pub mod keychain;
pub mod logins;
pub mod value;
pub mod vault;

pub use broker::SecretsBroker;
pub use dotenv::DotenvVault;
pub use env::EnvVault;
pub use error::SecretsError;
pub use filevault::EncryptedFileVault;
pub use gateway_tokens::{
    delete_gateway_token, gateway_token, gateway_token_broker, mask_token, set_gateway_token,
    DISCORD_TOKEN_NAME, GATEWAY_SECRETS_FILE, TELEGRAM_TOKEN_NAME,
};
pub use keychain::KeychainVault;
pub use logins::{LoginCredential, LoginStore, LOGINS_FILE, LOGINS_META_FILE, MASKED};
pub use value::SecretValue;
pub use vault::{MemoryVault, ReadOnlyVault, SecretVault};
