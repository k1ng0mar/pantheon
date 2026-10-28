//! Encrypted local vault (the fallback when no OS keychain is available).
//!
//! Everything is sealed with AES-256-GCM under a 32-byte random key stored
//! in a separate 0600 key file. The payload is written atomically (temp +
//! rename) so a crash can't leave a torn vault, and any bit flip in the file
//! fails decryption loudly instead of returning wrong secrets.
//!
//! Two limitations are by design and documented, not hidden:
//!
//! - **The key sits beside the vault.** The key file lives in the same
//!   data dir as `secrets.json` (that is the only location the vault is
//!   ever told about), so anyone who can read the data dir can read
//!   both the ciphertext and its key. The vault protects secrets at
//!   rest from casual reads, not from a data-dir compromise. Moving the
//!   key elsewhere (OS keychain, separate mount) needs a migration plan
//!   for existing installs and is deliberately not done silently here.
//! - **Permissions are ratcheted, not just set on creation.** An
//!   existing key file that is group/world-readable is tightened to
//!   0600 on load; a file whose mode cannot be tightened fails the open
//!   rather than proceeding with a readable key.
//!
//! OS keychains are the preferred backend; this is the self-hosted fallback.

use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use zeroize::Zeroizing;

#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    v: u32,
    nonce: String,      // hex
    ciphertext: String, // hex
}

#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    secrets: HashMap<String, String>,
}

/// Local-file vault sealed with AES-256-GCM.
#[derive(Debug)]
pub struct EncryptedFileVault {
    data_path: PathBuf,
    key: Zeroizing<[u8; 32]>,
    /// In-memory cache mirroring the decrypted file; persisted on mutation.
    cache: Mutex<HashMap<String, SecretValue>>,
}

const ENVELOPE_VERSION: u32 = 1;

impl EncryptedFileVault {
    /// Open (creating if missing) the vault at `data_path`, using/creating
    /// a 0600 key file at `key_path`.
    pub fn open(
        data_path: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
    ) -> Result<Self, SecretsError> {
        let data_path = data_path.as_ref().to_path_buf();
        let key = load_or_create_key(key_path.as_ref())?;
        let cache = Mutex::new(decrypt_file(&data_path, &key)?);
        Ok(Self {
            data_path,
            key,
            cache,
        })
    }

    fn state(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
        self.cache
            .lock()
            .expect("EncryptedFileVault mutex poisoned")
    }

    fn persist(&self, map: &HashMap<String, SecretValue>) -> Result<(), SecretsError> {
        let payload = Payload {
            secrets: map
                .iter()
                .map(|(k, v)| (k.clone(), v.expose().to_string()))
                .collect(),
        };
        let plaintext = serde_json::to_vec(&payload)
            .map_err(|e| SecretsError::Crypto(format!("serialize: {e}")))?;
        encrypt_and_write(&self.data_path, &self.key, &plaintext)
    }
}

impl SecretVault for EncryptedFileVault {
    fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
        crate::error::validate_name(name)?;
        Ok(self.state().get(name).cloned())
    }

    fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        // Persist first, then swap the cache: the old code mutated the
        // in-memory cache before persisting, so a persist failure
        // returned Err while the process-local cache already reflected
        // the change until restart - a lie to every later `get` in this
        // process. The mutex is not held across the write.
        let new_map = {
            let map = self.state();
            let mut new_map = map.clone();
            new_map.insert(name.to_string(), value);
            new_map
        };
        self.persist(&new_map)?;
        let mut map = self.state();
        *map = new_map;
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        // Same persist-first rule as `set`: a failed delete must leave
        // the cached secret readable, not silently gone.
        let new_map = {
            let map = self.state();
            let mut new_map = map.clone();
            new_map.remove(name);
            new_map
        };
        self.persist(&new_map)?;
        let mut map = self.state();
        *map = new_map;
        Ok(())
    }

    fn names(&self) -> Result<Vec<String>, SecretsError> {
        let mut names: Vec<String> = self.state().keys().cloned().collect();
        names.sort();
        Ok(names)
    }
}

fn load_or_create_key(path: &Path) -> Result<Zeroizing<[u8; 32]>, SecretsError> {
    if path.exists() {
        let bytes = fs::read(path)?;
        if bytes.len() != 32 {
            return Err(SecretsError::Crypto(format!(
                "key file {} has {} bytes, want 32",
                path.display(),
                bytes.len()
            )));
        }
        // 0600 was previously set only on creation: an existing key file
        // left group/world-readable (umask, restore, copy) stayed that
        // way forever. Ratchet it on every load; failing to tighten
        // fails the open rather than proceeding with a readable key.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path)?.permissions().mode() & 0o777;
            if mode != 0o600 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            }
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(Zeroizing::new(key))
    } else {
        let mut key = Zeroizing::new([0u8; 32]);
        rand::rng().fill_bytes(key.as_mut());
        write_key_file(path, &key)?;
        Ok(key)
    }
}

fn write_key_file(path: &Path, key: &[u8; 32]) -> Result<(), SecretsError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    set_private_perms(&file)?;
    file.write_all(key)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn set_private_perms(file: &fs::File) -> Result<(), SecretsError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = file.metadata()?.permissions();
    perms.set_mode(0o600);
    file.set_permissions(perms)?;
    Ok(())
}

#[cfg(not(unix))]
fn set_private_perms(_file: &fs::File) -> Result<(), SecretsError> {
    Ok(())
}

/// Decrypt the vault file; a missing file decrypts to an empty vault.
pub(crate) fn decrypt_file(
    path: &Path,
    key: &[u8; 32],
) -> Result<HashMap<String, SecretValue>, SecretsError> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read(path)?;
    let envelope: Envelope = serde_json::from_slice(&raw)
        .map_err(|e| SecretsError::Crypto(format!("bad envelope: {e}")))?;
    if envelope.v != ENVELOPE_VERSION {
        return Err(SecretsError::Crypto(format!(
            "unsupported vault version {}",
            envelope.v
        )));
    }
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| SecretsError::Crypto(format!("cipher init: {e}")))?;
    let nonce = hex_decode(&envelope.nonce)?;
    let ct = hex_decode(&envelope.ciphertext)?;
    if nonce.len() != 12 {
        return Err(SecretsError::Crypto("bad nonce length".into()));
    }
    let plaintext = cipher
        .decrypt(
            &Nonce::try_from(nonce.as_slice()).expect("nonce length checked above"),
            ct.as_ref(),
        )
        .map_err(|_| SecretsError::Crypto("decryption failed (tampered or wrong key)".into()))?;
    let payload: Payload = serde_json::from_slice(&plaintext)
        .map_err(|e| SecretsError::Crypto(format!("bad payload: {e}")))?;
    Ok(payload
        .secrets
        .into_iter()
        .map(|(k, v)| (k, SecretValue::new(v)))
        .collect())
}

fn encrypt_and_write(path: &Path, key: &[u8; 32], plaintext: &[u8]) -> Result<(), SecretsError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|e| SecretsError::Crypto(format!("cipher init: {e}")))?;
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let ct = cipher
        .encrypt(
            &Nonce::try_from(nonce_bytes.as_slice()).expect("nonce is 12 bytes"),
            plaintext,
        )
        .map_err(|_| SecretsError::Crypto("encryption failed".into()))?;
    let envelope = Envelope {
        v: ENVELOPE_VERSION,
        nonce: hex_encode(&nonce_bytes),
        ciphertext: hex_encode(&ct),
    };
    atomic_write(path, &serde_json::to_vec(&envelope)?)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SecretsError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("tmp");
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp)?;
        set_private_perms(&file)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, SecretsError> {
    if !s.len().is_multiple_of(2) {
        return Err(SecretsError::Crypto("odd-length hex".into()));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|e| SecretsError::Crypto(format!("bad hex: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::SecretVault;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-filevault-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_vault(dir: &Path) -> EncryptedFileVault {
        EncryptedFileVault::open(dir.join("secrets.json"), dir.join(".secrets.key")).unwrap()
    }

    /// A persist failure must not pollute the process-local cache: the
    /// old code inserted into the cache first, so `get` reported the
    /// change even though the call returned `Err`.
    #[test]
    fn failed_set_and_delete_leave_cache_unchanged() {
        let dir = scratch("atomic");
        let vault = open_vault(&dir);
        vault
            .set("KEEP", SecretValue::new("keep-value"))
            .unwrap();
        // Block the atomic write: the tmp file path is a directory, so
        // the persist fails while the cache must stay untouched.
        std::fs::create_dir_all(dir.join("secrets.tmp")).unwrap();
        assert!(vault.set("NEW", SecretValue::new("new-value")).is_err());
        assert!(vault.get("NEW").unwrap().is_none(), "failed set must not cache");
        assert!(
            vault.get("KEEP").unwrap().is_some(),
            "failed set must not disturb other entries"
        );
        assert!(vault.delete("KEEP").is_err());
        assert!(
            vault.get("KEEP").unwrap().is_some(),
            "failed delete must not drop the cached secret"
        );
        // Unblock: the vault is usable again, nothing stuck half-applied.
        std::fs::remove_dir_all(dir.join("secrets.tmp")).unwrap();
        vault
            .set("NEW", SecretValue::new("new-value"))
            .unwrap();
        assert!(vault.get("NEW").unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An existing key file left group/world-readable is tightened to
    /// 0600 on load, not just when first created.
    #[test]
    #[cfg(unix)]
    fn existing_key_file_permissions_ratcheted_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("keyperms");
        let key_path = dir.join(".secrets.key");
        std::fs::write(&key_path, [7u8; 32]).unwrap();
        std::fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();
        let _vault = open_vault(&dir);
        let mode = fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "existing key file must be tightened to 0600");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
