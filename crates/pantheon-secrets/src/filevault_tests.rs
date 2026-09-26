//! Tests for `pantheon_secrets::filevault::tests` — sibling file so sources stay test-free.
use super::*;
use tempfile::tempdir;

#[test]
fn roundtrip_persists_across_reopen() {
    let dir = tempdir().unwrap();
    let data = dir.path().join("vault.json");
    let key = dir.path().join("vault.key");

    {
        let v = EncryptedFileVault::open(&data, &key).unwrap();
        v.set("db.password", SecretValue::new("p@ss")).unwrap();
        v.set("api.key", SecretValue::new("sk-123")).unwrap();
    }
    // Fresh instance, same key file: data must come back intact.
    let v2 = EncryptedFileVault::open(&data, &key).unwrap();
    assert_eq!(
        v2.get("db.password")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("p@ss".into())
    );
    assert_eq!(
        v2.get("api.key").unwrap().map(|s| s.expose().to_string()),
        Some("sk-123".into())
    );
    assert_eq!(v2.names().unwrap(), vec!["api.key", "db.password"]);
}

#[test]
fn delete_persists() {
    let dir = tempdir().unwrap();
    let data = dir.path().join("vault.json");
    let key = dir.path().join("vault.key");
    let v = EncryptedFileVault::open(&data, &key).unwrap();
    v.set("x", SecretValue::new("1")).unwrap();
    v.delete("x").unwrap();
    assert_eq!(v.get("x").unwrap(), None);
    let v2 = EncryptedFileVault::open(&data, &key).unwrap();
    assert_eq!(v2.get("x").unwrap(), None);
}

#[test]
fn tampered_file_fails_loudly() {
    let dir = tempdir().unwrap();
    let data = dir.path().join("vault.json");
    let key = dir.path().join("vault.key");
    let v = EncryptedFileVault::open(&data, &key).unwrap();
    v.set("secret", SecretValue::new("value")).unwrap();

    // Flip one byte in the ciphertext.
    let mut raw = fs::read(&data).unwrap();
    let idx = raw.len() / 2;
    raw[idx] ^= 0x01;
    fs::write(&data, &raw).unwrap();

    let err = EncryptedFileVault::open(&data, &key).unwrap_err();
    assert!(matches!(err, SecretsError::Crypto(_)));
}

#[test]
fn wrong_key_fails_loudly() {
    let dir = tempdir().unwrap();
    let data = dir.path().join("vault.json");
    let key = dir.path().join("vault.key");
    let v = EncryptedFileVault::open(&data, &key).unwrap();
    v.set("secret", SecretValue::new("value")).unwrap();

    // Second key file => different key, must not decrypt.
    let key2 = dir.path().join("other.key");
    let err = EncryptedFileVault::open(&data, &key2).unwrap_err();
    assert!(matches!(err, SecretsError::Crypto(_)));
}

#[test]
fn file_is_encrypted_at_rest() {
    let dir = tempdir().unwrap();
    let data = dir.path().join("vault.json");
    let key = dir.path().join("vault.key");
    let v = EncryptedFileVault::open(&data, &key).unwrap();
    v.set("db.password", SecretValue::new("supersecret"))
        .unwrap();
    let raw = fs::read_to_string(&data).unwrap();
    assert!(!raw.contains("supersecret"));
}

#[test]
fn key_file_perms_are_private() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let key = dir.path().join("vault.key");
        let _ = EncryptedFileVault::open(dir.path().join("vault.json"), &key).unwrap();
        let mode = fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
