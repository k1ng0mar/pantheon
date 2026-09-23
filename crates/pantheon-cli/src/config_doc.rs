//! The complete `config.toml` document plus load/save/validate.

use super::config_schema::{PolicyPreset, SecretRef};
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelSection {
    pub provider: String,
    pub model: String,
    /// Env var name holding the API key. Never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Ordered fallback chain: [{provider, model}].
    #[serde(default)]
    pub fallbacks: Vec<FallbackEntry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FallbackEntry {
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemorySection {
    /// Backend name from the catalog: native | http | ...
    pub backend: String,
    #[serde(default)]
    pub options: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolSection {
    /// Enabled built-in tool packs by name.
    #[serde(default)]
    pub packs: Vec<String>,
    /// Plugin names to auto-start with the session.
    #[serde(default)]
    pub plugins: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ServerSection {
    /// AG-UI HTTP port (0 = auto).
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub host: String,
}

/// The whole config file. Everything optional-tolerant so doctor can
/// describe exactly what is missing instead of failing to parse.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    pub profile: Option<String>,
    pub model: Option<ModelSection>,
    pub policy: Option<PolicyPreset>,
    pub memory: Option<MemorySection>,
    pub tools: Option<ToolSection>,
    pub server: Option<ServerSection>,
}

impl Config {
    pub fn path(data_dir: &Path) -> std::path::PathBuf {
        data_dir.join("config.toml")
    }
    pub fn load(data_dir: &Path) -> Result<Self, PantheonError> {
        let path = Self::path(data_dir);
        let text = std::fs::read_to_string(&path).map_err(|e| {
            PantheonError::new(
                "CONFIG_OPEN",
                Layer::Runtime,
                false,
                format!("read {}: {e}", path.display()),
                "run `pantheon setup` to create a config",
                "",
            )
        })?;
        toml::from_str(&text).map_err(|e| {
            PantheonError::new(
                "CONFIG_PARSE",
                Layer::Runtime,
                false,
                format!("parse {}: {e}", path.display()),
                "fix the TOML or rerun setup",
                "",
            )
        })
    }
    pub fn save(&self, data_dir: &Path) -> Result<(), PantheonError> {
        let path = Self::path(data_dir);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let text = toml::to_string_pretty(self).map_err(|e| {
            PantheonError::new(
                "CONFIG_SER",
                Layer::Runtime,
                false,
                e.to_string(),
                "this is a bug: report the config contents",
                "",
            )
        })?;
        // Atomic write: tmp then rename, matching the rest of the codebase.
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| {
                PantheonError::new(
                    "CONFIG_WRITE",
                    Layer::Runtime,
                    false,
                    format!("write {}: {e}", path.display()),
                    "check directory permissions",
                    "",
                )
            })
    }
    /// Validate the config for doctor. Returns one error per problem.
    pub fn validate(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if let Some(m) = &self.model {
            if m.provider.trim().is_empty() {
                problems.push("model.provider is empty".into());
            }
            if m.model.trim().is_empty() {
                problems.push("model.model is empty".into());
            }
            if let Some(env) = &m.api_key_env {
                let r = SecretRef::from_env(env.clone());
                if let Err(e) = r.validate() {
                    problems.push(format!("model.api_key_env: {e}"));
                } else if r.resolve().is_none() {
                    problems.push(format!("env var {env} is not set"));
                }
            }
            for (i, f) in m.fallbacks.iter().enumerate() {
                if f.provider.trim().is_empty() || f.model.trim().is_empty() {
                    problems.push(format!("model.fallbacks[{i}] has empty provider or model"));
                }
            }
        } else {
            problems.push("no [model] section: run `pantheon setup`".into());
        }
        if let Some(mem) = &self.memory {
            if mem.backend.trim().is_empty() {
                problems.push("memory.backend is empty".into());
            }
        }
        if let Some(server) = &self.server {
            if !server.host.is_empty()
                && server.host != "127.0.0.1"
                && server.host != "0.0.0.0"
                && server.host != "localhost"
                && server.host != "::"
            {
                problems.push(format!(
                    "server.host {:?} is not a bindable address",
                    server.host
                ));
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_toml() {
        let dir = std::env::temp_dir().join(format!("pantheon-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = Config {
            profile: Some("dev".into()),
            model: Some(ModelSection {
                provider: "hp-llm-router".into(),
                model: "longcat".into(),
                api_key_env: Some("PANTHEON_API_KEY".into()),
                fallbacks: vec![FallbackEntry {
                    provider: "local".into(),
                    model: "llama3.2".into(),
                }],
            }),
            policy: Some(PolicyPreset::Coder),
            memory: Some(MemorySection {
                backend: "native".into(),
                options: Default::default(),
            }),
            tools: Some(ToolSection {
                packs: vec!["core".into()],
                plugins: vec![],
            }),
            server: Some(ServerSection {
                port: 18789,
                host: "127.0.0.1".into(),
            }),
        };
        cfg.save(&dir).unwrap();
        let loaded = Config::load(&dir).unwrap();
        assert_eq!(loaded, cfg);
        // The file must never contain a raw key, only the env var name.
        let text = std::fs::read_to_string(Config::path(&dir)).unwrap();
        assert!(!text.contains("sk-"));
        assert!(text.contains("PANTHEON_API_KEY"));
    }

    #[test]
    fn validate_reports_missing_model_and_unset_env() {
        let cfg = Config {
            model: Some(ModelSection {
                provider: "p".into(),
                model: "m".into(),
                api_key_env: Some("PANTHEON_DEFINITELY_UNSET_VAR_42".into()),
                fallbacks: vec![],
            }),
            ..Default::default()
        };
        let problems = cfg.validate();
        assert!(problems
            .iter()
            .any(|p| p.contains("PANTHEON_DEFINITELY_UNSET_VAR_42")));
        let empty = Config::default();
        assert!(empty.validate().iter().any(|p| p.contains("[model]")));
    }
}
