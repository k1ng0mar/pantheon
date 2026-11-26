//! Secrets. Durable vaults in insertion order, then env under
//! `PANTHEON_SECRET_*`; first hit wins. `inject` gives `None` for an
//! unconfigured name instead of an error.

mod auth_group;
mod broker_group;
pub use auth_group::dotenv;
pub use auth_group::dotenv::DotenvVault;
pub use auth_group::env;
pub use auth_group::env::EnvVault;
pub use auth_group::gateway_tokens;
pub use auth_group::gateway_tokens::{
    delete_gateway_token, gateway_token, gateway_token_broker, mask_token, set_gateway_token,
    DISCORD_TOKEN_NAME, GATEWAY_SECRETS_FILE, TELEGRAM_TOKEN_NAME,
};
pub use auth_group::logins;
pub use auth_group::logins::{LoginCredential, LoginStore, LOGINS_FILE, LOGINS_META_FILE, MASKED};
pub use auth_group::migration;
pub use auth_group::oauth;
pub use auth_group::secure_forms;
pub use auth_group::secure_forms::{
    default_verifier, BotTokenVerifier, LocalVerifier, ProviderModelsVerifier, SaveOutcome,
    SecretCaller, SecretEntry, SecretScope, SecureFormStore, Verifier, VerifyOutcome,
};
pub use broker_group::broker;
pub use broker_group::broker::SecretsBroker;
pub use broker_group::error;
pub use broker_group::error::SecretsError;
pub use broker_group::filevault;
pub use broker_group::filevault::EncryptedFileVault;
pub use broker_group::keychain;
pub use broker_group::keychain::KeychainVault;
pub use broker_group::redaction;
pub use broker_group::redaction::RedactionRegistry;
pub use broker_group::value;
pub use broker_group::value::SecretValue;
pub use broker_group::vault;
pub use broker_group::vault::{MemoryVault, ReadOnlyVault, SecretVault};
