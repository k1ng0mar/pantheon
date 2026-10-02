//! Website-login credentials for browser sessions.
//!
//! A login is `(id, site, username, password)`. The password lives in a
//! [`SecretsBroker`] vault (`logins.env` inside the data dir - deliberately
//! NOT `.env`, so login passwords never surface in the `.env` key
//! manager); the metadata (site, username, the secret's broker name) lives
//! in `logins.json` next to it. The password is only ever exposed through
//! [`LoginStore::password_for`], for server-side use by the browser
//! session. API responses use [`LoginCredential::masked`]: the password is
//! always the `••••` placeholder, never secret material, not even partial.

use crate::{DotenvVault, EnvVault, SecretValue, SecretsBroker, SecretsError};
use std::path::{Path, PathBuf};

/// Broker vault file holding login passwords (`<data_dir>/logins.env`).
pub const LOGINS_FILE: &str = "logins.env";
/// Metadata file holding login site/username records (`<data_dir>/logins.json`).
pub const LOGINS_META_FILE: &str = "logins.json";

/// Masked placeholder returned in place of every password in API
/// responses. Never the real value, not even a prefix.
pub const MASKED: &str = "••••";

/// One website login: site + username are metadata; the password lives in
/// the broker under [`LoginCredential::secret_name`] and is never stored
/// on this struct.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct LoginCredential {
    /// Stable id, derived from the site at creation (`github-com-3f9a`).
    pub id: String,
    /// Site this login is for (`github.com`, `https://x.com/login`
    /// stored as given, matched by substring at login time).
    pub site: String,
    /// Login username / email.
    pub username: String,
    /// Broker secret name holding the password.
    pub secret_name: String,
}

impl LoginCredential {
    /// The API-safe view: `site`, `username`, and the masked password
    /// placeholder. The real password never leaves the server.
    pub fn masked(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "site": self.site,
            "username": self.username,
            "password": MASKED,
        })
    }
}

/// CRUD over the login store. Backed by a broker whose only vault is
/// `logins.env` (no process-env fallback, so `names`/`resolve` reflect
/// the file and nothing else) plus the `logins.json` metadata file.
///
/// The store is deliberately dumb about the browser: it hands the
/// password to server-side callers via [`LoginStore::password_for`]; the
/// login *flow* (fill + submit) belongs to the browser tool surface.
pub struct LoginStore {
    broker: SecretsBroker,
    meta_path: PathBuf,
}

impl LoginStore {
    /// Open the store rooted at `data_dir` (`logins.env` + `logins.json`
    /// live there). Creates nothing until the first write.
    pub fn open(data_dir: impl AsRef<Path>) -> Self {
        let data_dir = data_dir.as_ref().to_path_buf();
        let broker = SecretsBroker::new()
            .with_vault(Box::new(DotenvVault::new_file(&data_dir, LOGINS_FILE)))
            .with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()));
        Self {
            broker,
            meta_path: data_dir.join(LOGINS_META_FILE),
        }
    }

    /// Broker secret name for a login id. Uppercase + digits + `_` so it
    /// passes dotenv key validation.
    fn secret_name_for(id: &str) -> String {
        format!(
            "PANTHEON_LOGIN_{}",
            id.to_ascii_uppercase().replace('-', "_")
        )
    }

    /// Read the metadata file. Missing/empty is a clean empty list;
    /// present-but-unparseable is an error - never silently treated as
    /// empty, because the next write would then overwrite (wipe) the
    /// corrupt file.
    fn read_meta(&self) -> Result<Vec<LoginCredential>, SecretsError> {
        let text = std::fs::read_to_string(&self.meta_path).unwrap_or_default();
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        serde_json::from_str(&text).map_err(|e| {
            SecretsError::Invalid(format!(
                "corrupt {}: {e} (not overwritten; repair or restore the file)",
                LOGINS_META_FILE
            ))
        })
    }

    fn write_meta(&self, creds: &[LoginCredential]) -> Result<(), SecretsError> {
        let text = serde_json::to_string_pretty(creds)
            .map_err(|e| SecretsError::Invalid(format!("serialize logins.json: {e}")))?;
        if let Some(parent) = self.meta_path.parent() {
            std::fs::create_dir_all(parent).map_err(SecretsError::Io)?;
        }
        // Atomic-ish: temp file + rename, owner-only like the vault files.
        let tmp = self.meta_path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(SecretsError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.meta_path).map_err(SecretsError::Io)?;
        Ok(())
    }

    /// All logins, metadata only (passwords are never in the metadata).
    /// A corrupt metadata file is an error, not an empty list.
    pub fn list(&self) -> Result<Vec<LoginCredential>, SecretsError> {
        self.read_meta()
    }

    /// Find one login by id.
    pub fn get(&self, id: &str) -> Result<Option<LoginCredential>, SecretsError> {
        Ok(self.read_meta()?.into_iter().find(|c| c.id == id))
    }

    /// Resolve the password for server-side use (browser login flow).
    /// `None` = unknown id or no password stored.
    pub fn password_for(&self, id: &str) -> Result<Option<SecretValue>, SecretsError> {
        let Some(cred) = self.get(id)? else {
            return Ok(None);
        };
        self.broker.resolve(&cred.secret_name)
    }

    /// Create a login. The id is derived from the site; collisions get a
    /// numeric suffix. Fails on empty site/username/password.
    pub fn create(
        &self,
        site: &str,
        username: &str,
        password: &str,
    ) -> Result<LoginCredential, SecretsError> {
        let site = site.trim();
        let username = username.trim();
        if site.is_empty() {
            return Err(SecretsError::Invalid("site must not be empty".into()));
        }
        if username.is_empty() {
            return Err(SecretsError::Invalid("username must not be empty".into()));
        }
        if password.is_empty() {
            return Err(SecretsError::Invalid("password must not be empty".into()));
        }
        if password.contains('\n') || password.contains('\r') {
            return Err(SecretsError::Invalid("password must be single-line".into()));
        }
        let mut creds = self.read_meta()?;
        let slug: String = site
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        let slug = slug.chars().take(32).collect::<String>();
        let base = if slug.is_empty() {
            "login".into()
        } else {
            slug
        };
        let mut id = base.clone();
        let mut n = 2;
        while creds.iter().any(|c| c.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        let secret_name = Self::secret_name_for(&id);
        self.broker.set(&secret_name, SecretValue::new(password))?;
        let cred = LoginCredential {
            id,
            site: site.to_string(),
            username: username.to_string(),
            secret_name,
        };
        creds.push(cred.clone());
        self.write_meta(&creds)?;
        Ok(cred)
    }

    /// Update username and/or password. At least one must be given.
    /// Password updates are write-only (the old value is never readable).
    pub fn update(
        &self,
        id: &str,
        site: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<LoginCredential, SecretsError> {
        if site.map(str::trim).unwrap_or("").is_empty()
            && username.map(str::trim).unwrap_or("").is_empty()
            && password.unwrap_or("").is_empty()
        {
            return Err(SecretsError::Invalid(
                "nothing to update: supply site, username and/or password".into(),
            ));
        }
        let mut creds = self.read_meta()?;
        let pos = creds
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| SecretsError::Invalid(format!("unknown login '{id}'")))?;
        if let Some(s) = site {
            let s = s.trim();
            if s.is_empty() {
                return Err(SecretsError::Invalid("site must not be empty".into()));
            }
            creds[pos].site = s.to_string();
        }
        if let Some(u) = username {
            let u = u.trim();
            if u.is_empty() {
                return Err(SecretsError::Invalid("username must not be empty".into()));
            }
            creds[pos].username = u.to_string();
        }
        if let Some(p) = password {
            if p.is_empty() {
                return Err(SecretsError::Invalid("password must not be empty".into()));
            }
            if p.contains('\n') || p.contains('\r') {
                return Err(SecretsError::Invalid("password must be single-line".into()));
            }
            self.broker
                .set(&creds[pos].secret_name, SecretValue::new(p))?;
        }
        self.write_meta(&creds)?;
        Ok(creds[pos].clone())
    }

    /// Delete a login: metadata row and its vault password both go.
    /// Unknown id is a no-op returning `false`; deleted returns `true`.
    /// A vault-delete failure is propagated (not swallowed): the metadata
    /// row is only removed once the password is actually gone, so a
    /// half-deleted login can never be reported as deleted.
    pub fn delete(&self, id: &str) -> Result<bool, SecretsError> {
        let mut creds = self.read_meta()?;
        let Some(pos) = creds.iter().position(|c| c.id == id) else {
            return Ok(false);
        };
        let cred = creds.remove(pos);
        self.broker.delete(&cred.secret_name)?;
        self.write_meta(&creds)?;
        Ok(true)
    }
}
