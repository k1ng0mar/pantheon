//! Plaintext store migration: backup, migrate, verify, never delete.
//!
//! The gateway and login stores live as plaintext dotenv files today.
//! This migrator copies every entry into the encrypted file vault,
//! verifies each value by reading it back, and records a report.
//! Plaintext originals are left in place. Deleting them is a separate
//! human approved step after a verified restart.

use crate::{EncryptedFileVault, SecretValue, SecretVault};
use std::path::Path;

pub struct MigrationReport {
    pub file: String,
    pub migrated: usize,
    pub verified: bool,
}

/// Migrate one dotenv file into the encrypted vault at `vault_path`
/// sealed by the key at `key_path`. Backup of the source file is written
/// to `<data_dir>/backups/<file>.<timestamp>.bak` when possible.
pub fn migrate_dotenv_to_filevault(
    data_dir: &Path,
    file_name: &str,
    vault_path: &Path,
    key_path: &Path,
) -> Result<MigrationReport, String> {
    let src = data_dir.join(file_name);
    let text = std::fs::read_to_string(&src).map_err(|e| format!("read {file_name}: {e}"))?;
    let entries = pantheon_api::dotenv::parse_dotenv(&text);
    if !entries.is_empty() {
        let backup_dir = data_dir.join("backups");
        std::fs::create_dir_all(&backup_dir).map_err(|e| format!("create backup dir: {e}"))?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let backup = backup_dir.join(format!("{file_name}.{stamp}.bak"));
        std::fs::write(&backup, &text).map_err(|e| format!("backup: {e}"))?;
    }
    let vault =
        EncryptedFileVault::open(vault_path, key_path).map_err(|e| format!("open vault: {e}"))?;
    let mut migrated = 0usize;
    for (name, value) in &entries {
        vault
            .set(name, SecretValue::new(value.clone()))
            .map_err(|e| format!("migrate {name}: {e}"))?;
        migrated += 1;
    }
    // Verify by reopening and reading back every entry.
    let reopened =
        EncryptedFileVault::open(vault_path, key_path).map_err(|e| format!("reopen vault: {e}"))?;
    for (name, value) in &entries {
        let back = reopened
            .get(name)
            .map_err(|e| format!("verify {name}: {e}"))?
            .ok_or_else(|| format!("verify {name}: missing after migration"))?;
        if back.expose() != value {
            return Err(format!("verify {name}: value mismatch"));
        }
    }
    Ok(MigrationReport {
        file: file_name.to_string(),
        migrated,
        verified: true,
    })
}
