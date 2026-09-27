//! Encrypted local vault (the fallback when no OS keychain is available).
//!
//! Everything is sealed with AES-256-GCM under a 32-byte random key stored
//! in a separate 0600 key file. The payload is written atomically (temp +
//! rename) so a crash can't leave a torn vault, and any bit flip in the file
//! fails decryption loudly instead of returning wrong secrets.
//!
//! OS keychains are the preferred backend; this is the self-hosted fallback.

use crate::error::SecretsError;
use crate::value::SecretValue;
use crate::vault::SecretVault;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::rngs::OsRng;
use rand::RngCore;
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
        let mut map = self.state();
        map.insert(name.to_string(), value);
        self.persist(&map)
    }

    fn delete(&self, name: &str) -> Result<(), SecretsError> {
        crate::error::validate_name(name)?;
        let mut map = self.state();
        map.remove(name);
        self.persist(&map)
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
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        Ok(Zeroizing::new(key))
    } else {
        let mut key = Zeroizing::new([0u8; 32]);
        OsRng.fill_bytes(key.as_mut());
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
        .decrypt(Nonce::from_slice(&nonce), ct.as_ref())
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
    OsRng.fill_bytes(&mut nonce_bytes);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
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
