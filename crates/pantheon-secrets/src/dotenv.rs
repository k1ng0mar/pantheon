//! Dotenv-file vault: `<data_dir>/.env` as a [`SecretVault`].
//!
//! The dashboard's `.env` key manager used to read and write the file
//! directly, bypassing the secrets crate. Routing it through this vault
//! means dotenv keys get the same name validation, value hygiene, and
//! broker contracts as every other backend. Reads and writes reuse the
//! atomic helpers in [`pantheon_api::dotenv`] (line-preserving, owner-only
//! permissions), so behavior matches the rest of the codebase.

use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;
use std::path::{Path, PathBuf};

/// `SecretVault` over `<data_dir>/.env`.
#[derive(Debug)]
pub struct DotenvVault {
    data_dir: PathBuf,
    file_name: String,
}

impl DotenvVault {
    /// Vault over the `.env` file inside `data_dir`.
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self::new_file(data_dir, ".env")
    }

    /// Vault over a different dotenv file inside `data_dir` (same
    /// atomic/permission semantics). Used for secret namespaces that
    /// must not appear in the `.env` key manager — e.g. website-login
    /// passwords live in `logins.env`, never in `.env`.
    pub fn new_file(data_dir: impl AsRef<Path>, file_name: &str) -> Self {
        Self {
            data_dir: data_dir.as_ref().to_path_buf(),
            file_name: file_name.to_string(),
        }
    }

    fn path(&self) -> PathBuf {
        self.data_dir.join(&self.file_name)
    }
}

impl SecretVault for DotenvVault {
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        Ok(
            pantheon_api::dotenv::read_dotenv_file_value(&self.data_dir, &self.file_name, name)
                .map(SecretValue::new),
        )
    }

    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        if !pantheon_api::dotenv::valid_key(name) {
            return Err(SecretsError::Invalid(format!(
                "invalid dotenv key name {name:?}"
            )));
        }
        if value.expose().contains('\n') || value.expose().contains('\r') {
            return Err(SecretsError::Invalid(format!(
                "value for {name:?} must be single-line"
            )));
        }
        pantheon_api::dotenv::upsert_dotenv_file(
            &self.data_dir,
            &self.file_name,
            name,
            value.expose(),
        )
        .map_err(SecretsError::Io)
    }

    fn delete(&self, name: &str) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        // Removing an unknown name is a no-op, per the trait contract.
        pantheon_api::dotenv::delete_dotenv_file_key(&self.data_dir, &self.file_name, name)
            .map_err(SecretsError::Io)?;
        Ok(())
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        let text = std::fs::read_to_string(self.path()).unwrap_or_default();
        let mut names: Vec<String> = pantheon_api::dotenv::parse_dotenv(&text)
            .into_iter()
            .map(|(k, _)| k)
            .collect();
        names.sort();
        names.dedup();
        Ok(names)
    }
}

#[cfg(test)]
mod dotenv_vault_concurrency_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    /// Item 2b: 8 threads x 25 keys through the real write path
    /// (`DotenvVault::set`, the same path `PUT /api/env` uses). Without
    /// an inter-request lock around read-modify-write, concurrent
    /// writers lose updates (the audit empirically saw 100/200 survive).
    #[test]
    fn concurrent_sets_all_survive() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-secrets-conc-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let vault = DotenvVault::new(&dir);
        std::thread::scope(|s| {
            for t in 0..8 {
                let vault = &vault;
                s.spawn(move || {
                    for k in 0..25 {
                        let name = format!("CONC_{t}_{k}");
                        vault
                            .set(&name, SecretValue::new(format!("v{t}-{k}")))
                            .expect("concurrent set");
                    }
                });
            }
        });
        let mut missing = Vec::new();
        for t in 0..8 {
            for k in 0..25 {
                let name = format!("CONC_{t}_{k}");
                match vault.get(&name).expect("get") {
                    Some(v) if v.expose() == format!("v{t}-{k}") => {}
                    other => missing.push((name, other.map(|v| v.expose().to_string()))),
                }
            }
        }
        assert!(
            missing.is_empty(),
            "lost {} of 200 concurrent writes, e.g. {:?}",
            missing.len(),
            &missing[..missing.len().min(3)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
