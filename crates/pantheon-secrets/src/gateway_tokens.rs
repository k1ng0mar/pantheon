//! Gateway chat-surface tokens: the Telegram and Discord bot tokens that
//! `gateway run` needs.
//!
//! Collected by the Full Setup wizard's Gateway screen. The tokens live in
//! `<data_dir>/gateway.env` - a [`DotenvVault`] namespace deliberately
//! separate from `.env`, so bot tokens never surface in the `.env` key
//! manager - and a process env var always wins over the file when both are
//! set (an exported rotation must beat a stale stored token).
//!
//! Tokens travel only as [`SecretValue`]. The one display helper,
//! [`mask_token`], shows the last four characters; nothing here ever logs
//! or prints a full value.

use crate::{
    vault::SecretVault, DotenvVault, EnvVault, MemoryVault, ReadOnlyVault, SecretValue,
    SecretsBroker, SecretsError,
};
use std::path::Path;

/// Secrets-file namespace for gateway tokens (`<data_dir>/gateway.env`).
pub const GATEWAY_SECRETS_FILE: &str = "gateway.env";
/// Secret name (and env var) for the Telegram bot token.
pub const TELEGRAM_TOKEN_NAME: &str = "PANTHEON_TELEGRAM_BOT_TOKEN";
/// Secret name (and env var) for the Discord bot token.
pub const DISCORD_TOKEN_NAME: &str = "PANTHEON_DISCORD_TOKEN";

/// Broker resolving the two channel tokens: process env first, then
/// `<data_dir>/gateway.env`. The env values are mirrored into a read-only
/// front vault - the same trick as
/// [`SecretsBroker::from_system_env_with_api_key`] - so an exported var
/// beats the file on read, while `set`/`delete` fall through to the file
/// vault and can never touch the process environment.
pub fn gateway_token_broker(data_dir: &Path) -> SecretsBroker {
    let mirror = MemoryVault::new();
    for name in [TELEGRAM_TOKEN_NAME, DISCORD_TOKEN_NAME] {
        if let Ok(v) = std::env::var(name) {
            if !v.trim().is_empty() {
                let _ = mirror.set(name, SecretValue::new(v));
            }
        }
    }
    SecretsBroker::new()
        .with_vault(Box::new(DotenvVault::new_file(
            data_dir,
            GATEWAY_SECRETS_FILE,
        )))
        .with_vault_front(Box::new(ReadOnlyVault::new(mirror)))
        .with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()))
}

/// Read one channel token. `None` = not configured anywhere; blank values
/// are treated as missing. The value is never logged.
pub fn gateway_token(data_dir: &Path, name: &str) -> Option<SecretValue> {
    gateway_token_broker(data_dir)
        .resolve(name)
        .ok()
        .flatten()
        .filter(|v| !v.expose().trim().is_empty())
}

/// Store one channel token in `<data_dir>/gateway.env`.
pub fn set_gateway_token(
    data_dir: &Path,
    name: &str,
    token: SecretValue,
) -> Result<(), SecretsError> {
    file_broker(data_dir).set(name, token)
}

/// Remove one channel token from `<data_dir>/gateway.env`. A name no
/// vault knows is a no-op.
pub fn delete_gateway_token(data_dir: &Path, name: &str) -> Result<(), SecretsError> {
    file_broker(data_dir).delete(name)
}

/// Broker over the secrets file only - no env mirror, so writes and
/// deletes land in the file and nothing else.
fn file_broker(data_dir: &Path) -> SecretsBroker {
    SecretsBroker::new()
        .with_vault(Box::new(DotenvVault::new_file(
            data_dir,
            GATEWAY_SECRETS_FILE,
        )))
        .with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()))
}

/// Masked display for a token: `••••1234`. The last four characters let
/// the user tell tokens apart; never the full value. Tokens of four
/// characters or fewer are fully masked.
pub fn mask_token(value: &SecretValue) -> String {
    let chars: Vec<char> = value.expose().chars().collect();
    if chars.len() <= 4 {
        return "••••".to_string();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}{tail}", "•".repeat(chars.len() - 4))
}
