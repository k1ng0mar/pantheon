//! The complete `config.toml` document plus load/save/validate.

use super::config_schema::{PolicyPreset, SecretRef};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_secrets::SecretVault;
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

/// `[decision]`: the auxiliary decision model. Any provider/model the
/// catalog knows (or a raw base URL as provider) — the runtime resolves
/// wire mode and key env the same way it does for chat. Absent = the
/// decision layer stays off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DecisionSection {
    pub provider: String,
    pub model: String,
    /// Env var name holding the API key for the decision endpoint.
    /// Never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FallbackEntry {
    pub provider: String,
    pub model: String,
}

/// `[stt]` / `[tts]`: speech service selection. These are provider-plane
/// services (a local binary or an HTTP endpoint), never model-policy
/// entries — same shape as `[memory]`'s backend selection. Absent = the
/// surface simply has no speech capability.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VoiceSection {
    /// Backend name from the provider registry: `command` | `openai`.
    pub backend: String,
    /// Backend-specific options (cmd/args for command, provider/model
    /// for openai, timeout_secs, ...).
    #[serde(default)]
    pub options: std::collections::HashMap<String, String>,
}

/// `[compression]`: the auxiliary context-compression model. Summarizes
/// the oldest exchanges when a transcript overflows the window. Absent =
/// deterministic dropping only (compression never runs unconfigured).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompressionSection {
    pub provider: String,
    pub model: String,
    /// Env var name holding the API key for the compression endpoint.
    /// Never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<DecisionSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt: Option<VoiceSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts: Option<VoiceSection>,
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
        if let Some(d) = &self.decision {
            if d.provider.trim().is_empty() {
                problems.push("decision.provider is empty".into());
            }
            if d.model.trim().is_empty() {
                problems.push("decision.model is empty".into());
            }
            if let Some(env) = &d.api_key_env {
                let r = SecretRef::from_env(env.clone());
                if let Err(e) = r.validate() {
                    problems.push(format!("decision.api_key_env: {e}"));
                } else if r.resolve().is_none() {
                    problems.push(format!("env var {env} is not set"));
                }
            }
        }
        if let Some(c) = &self.compression {
            if c.provider.trim().is_empty() {
                problems.push("compression.provider is empty".into());
            }
            if c.model.trim().is_empty() {
                problems.push("compression.model is empty".into());
            }
            if let Some(env) = &c.api_key_env {
                let r = SecretRef::from_env(env.clone());
                if let Err(e) = r.validate() {
                    problems.push(format!("compression.api_key_env: {e}"));
                } else if r.resolve().is_none() {
                    problems.push(format!("env var {env} is not set"));
                }
            }
        }
        if let Some(mem) = &self.memory {
            if mem.backend.trim().is_empty() {
                problems.push("memory.backend is empty".into());
            }
        }
        for (section, v) in [("stt", &self.stt), ("tts", &self.tts)] {
            if let Some(v) = v {
                if v.backend.trim().is_empty() {
                    problems.push(format!("{section}.backend is empty"));
                }
                if v.backend == "command" && !v.options.contains_key("cmd") {
                    problems.push(format!(
                        "{section}.options.cmd is required for the command backend"
                    ));
                }
                if v.backend == "openai" && !v.options.contains_key("provider") {
                    problems.push(format!(
                        "{section}.options.provider is required for the openai backend"
                    ));
                }
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

/// Resolve the decision-model target: `PANTHEON_DECISION_*` env overrides
/// the `[decision]` section field-wise. Pure so tests don't touch env.
/// Returns `(provider, model)` only when both resolve and are non-empty.
pub fn decision_target(
    section: Option<&DecisionSection>,
    env_provider: Option<String>,
    env_model: Option<String>,
) -> Option<(String, String)> {
    let pick = |env: Option<String>, cfg: Option<&String>| -> Option<String> {
        env.filter(|v| !v.trim().is_empty())
            .or_else(|| cfg.map(|v| v.to_string()).filter(|v| !v.trim().is_empty()))
    };
    let provider = pick(env_provider, section.map(|s| &s.provider))?;
    let model = pick(env_model, section.map(|s| &s.model))?;
    Some((provider, model))
}

/// `[decision]` + env → the `DecisionRouter` auxiliary entry.
/// `None` = the decision layer stays off for this host.
pub fn decision_aux(cfg: Option<&Config>) -> Option<pantheon_core::model::AuxiliaryModel> {
    let (provider, model) = decision_target(
        cfg.and_then(|c| c.decision.as_ref()),
        std::env::var("PANTHEON_DECISION_PROVIDER").ok(),
        std::env::var("PANTHEON_DECISION_MODEL").ok(),
    )?;
    Some(pantheon_core::model::AuxiliaryModel {
        kind: pantheon_core::model::AuxiliaryKind::DecisionRouter,
        provider,
        model,
    })
}

/// Seed a named vault entry from an env-var name in config, so the session
/// resolves aux endpoint keys at the execution boundary like every other
/// secret. Missing section, missing env, or unset var = no-op.
fn seed_env_key(
    secrets: pantheon_secrets::SecretsBroker,
    env: Option<String>,
    vault_name: &'static str,
) -> pantheon_secrets::SecretsBroker {
    let Some(env) = env else {
        return secrets;
    };
    let Ok(value) = std::env::var(&env) else {
        return secrets;
    };
    let mem = pantheon_secrets::MemoryVault::new();
    let _ = mem.set(vault_name, pantheon_secrets::SecretValue::new(value));
    secrets.with_vault(Box::new(mem))
}

/// Seed `PANTHEON_DECISION_API_KEY` from `[decision].api_key_env`.
pub fn with_decision_key(
    secrets: pantheon_secrets::SecretsBroker,
    cfg: Option<&Config>,
) -> pantheon_secrets::SecretsBroker {
    let env = cfg
        .and_then(|c| c.decision.as_ref())
        .and_then(|d| d.api_key_env.clone());
    seed_env_key(secrets, env, "PANTHEON_DECISION_API_KEY")
}

/// Seed `PANTHEON_COMPRESSION_API_KEY` from `[compression].api_key_env`.
pub fn with_compression_key(
    secrets: pantheon_secrets::SecretsBroker,
    cfg: Option<&Config>,
) -> pantheon_secrets::SecretsBroker {
    let env = cfg
        .and_then(|c| c.compression.as_ref())
        .and_then(|c| c.api_key_env.clone());
    seed_env_key(secrets, env, "PANTHEON_COMPRESSION_API_KEY")
}

/// Seed every configured aux key in one call (the session-builder sites'
/// entry point).
pub fn with_aux_keys(
    secrets: pantheon_secrets::SecretsBroker,
    cfg: Option<&Config>,
) -> pantheon_secrets::SecretsBroker {
    with_compression_key(with_decision_key(secrets, cfg), cfg)
}

/// `[model].api_key_env` → the env-var name holding the chat model key.
/// Never the key itself; `None` = the default `PANTHEON_API_KEY` path.
pub fn model_key_env(cfg: Option<&Config>) -> Option<String> {
    cfg.and_then(|c| c.model.as_ref())
        .and_then(|m| m.api_key_env.clone())
}

/// The one secrets broker every session-builder site constructs: the chat
/// model key (config-named env var, else `PANTHEON_API_KEY`), then every
/// aux key (decision + compression), environment fallback last.
///
/// Session, gateway, pipeline, and `chat --key` all share this so a key
/// configured once resolves identically on every path. An explicit `--key`
/// flag is layered on top with
/// [`SecretsBroker::with_vault_front`](pantheon_secrets::SecretsBroker::with_vault_front)
/// so the flag beats config and environment.
pub fn chat_secrets(cfg: Option<&Config>) -> pantheon_secrets::SecretsBroker {
    with_aux_keys(
        pantheon_secrets::SecretsBroker::from_system_env_with_api_key(
            model_key_env(cfg).as_deref(),
        ),
        cfg,
    )
}

/// `[compression]` + env → the `Compression` auxiliary entry.
/// `None` = compression stays off for this host.
pub fn compression_aux(cfg: Option<&Config>) -> Option<pantheon_core::model::AuxiliaryModel> {
    let (provider, model) = compression_target(
        cfg.and_then(|c| c.compression.as_ref()),
        std::env::var("PANTHEON_COMPRESSION_PROVIDER").ok(),
        std::env::var("PANTHEON_COMPRESSION_MODEL").ok(),
    )?;
    Some(pantheon_core::model::AuxiliaryModel {
        kind: pantheon_core::model::AuxiliaryKind::Compression,
        provider,
        model,
    })
}

/// Resolve the compression-model target: env overrides config
/// field-wise, mirroring [`decision_target`]. Pure so tests don't touch env.
pub fn compression_target(
    section: Option<&CompressionSection>,
    env_provider: Option<String>,
    env_model: Option<String>,
) -> Option<(String, String)> {
    let pick = |env: Option<String>, cfg: Option<&String>| -> Option<String> {
        env.filter(|v| !v.trim().is_empty()).or_else(|| {
            cfg.map(|v| v.to_string())
                .filter(|v| !v.trim().is_empty())
        })
    };
    let provider = pick(env_provider, section.map(|s| &s.provider))?;
    let model = pick(env_model, section.map(|s| &s.model))?;
    Some((provider, model))
}

/// Every configured auxiliary for this host (decision + compression).
/// Lookups are by kind, so order is irrelevant.
pub fn auxiliaries(cfg: Option<&Config>) -> Vec<pantheon_core::model::AuxiliaryModel> {
    [decision_aux(cfg), compression_aux(cfg)]
        .into_iter()
        .flatten()
        .collect()
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
            decision: Some(DecisionSection {
                provider: "local".into(),
                model: "qwen2.5:1.5b".into(),
                api_key_env: None,
            }),
            compression: Some(CompressionSection {
                provider: "local".into(),
                model: "summarizer".into(),
                api_key_env: None,
            }),
            stt: Some(VoiceSection {
                backend: "command".into(),
                options: [("cmd".into(), "whisper-cli".into())].into_iter().collect(),
            }),
            tts: None,
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

    #[test]
    fn decision_target_env_overrides_config_field_wise() {
        let sec = DecisionSection {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            api_key_env: None,
        };
        // No env: config wins.
        assert_eq!(
            decision_target(Some(&sec), None, None),
            Some(("openai".into(), "gpt-4o-mini".into()))
        );
        // Env overrides one field, config fills the other.
        assert_eq!(
            decision_target(Some(&sec), Some("anthropic".into()), None),
            Some(("anthropic".into(), "gpt-4o-mini".into()))
        );
        // Env alone activates the layer (no [decision] section at all).
        assert_eq!(
            decision_target(
                None,
                Some("http://127.0.0.1:8016/v1".into()),
                Some("typed-decisions".into())
            ),
            Some(("http://127.0.0.1:8016/v1".into(), "typed-decisions".into()))
        );
        // Partial env with no section stays off.
        assert_eq!(decision_target(None, Some("openai".into()), None), None);
        // Whitespace-only values count as absent.
        assert_eq!(
            decision_target(Some(&sec), Some("  ".into()), None),
            Some(("openai".into(), "gpt-4o-mini".into()))
        );
    }

    #[test]
    fn decision_section_parses_from_toml() {
        let cfg: Config = toml::from_str(
            "[model]\nprovider = \"local\"\nmodel = \"llama3.2\"\n\
             [decision]\nprovider = \"local\"\nmodel = \"qwen2.5:1.5b\"\n",
        )
        .unwrap();
        let d = cfg.decision.as_ref().expect("decision section parsed");
        assert_eq!(d.provider, "local");
        assert_eq!(d.model, "qwen2.5:1.5b");
        assert_eq!(cfg.validate(), Vec::<String>::new());
        // Configs without [decision] still parse (back-compat).
        let old: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
        assert!(old.decision.is_none());
        assert!(old.compression.is_none());
    }

    #[test]
    fn compression_target_env_overrides_config_field_wise() {
        let sec = CompressionSection {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            api_key_env: None,
        };
        assert_eq!(
            compression_target(Some(&sec), None, None),
            Some(("openai".into(), "gpt-4o-mini".into()))
        );
        assert_eq!(
            compression_target(Some(&sec), Some("local".into()), None),
            Some(("local".into(), "gpt-4o-mini".into()))
        );
        // Env alone activates compression (no [compression] section).
        assert_eq!(
            compression_target(
                None,
                Some("http://127.0.0.1:8017/v1".into()),
                Some("summarizer".into())
            ),
            Some(("http://127.0.0.1:8017/v1".into(), "summarizer".into()))
        );
        // Partial env with no section stays off.
        assert_eq!(compression_target(None, Some("openai".into()), None), None);
    }

    #[test]
    fn auxiliaries_combine_decision_and_compression() {
        let cfg: Config = toml::from_str(
            "[model]\nprovider = \"local\"\nmodel = \"llama3.2\"\n\
             [decision]\nprovider = \"local\"\nmodel = \"qwen2.5:1.5b\"\n\
             [compression]\nprovider = \"local\"\nmodel = \"summarizer\"\n",
        )
        .unwrap();
        let aux = auxiliaries(Some(&cfg));
        assert_eq!(aux.len(), 2);
        assert!(aux.iter().any(|a| matches!(
            a.kind,
            pantheon_core::model::AuxiliaryKind::DecisionRouter
        )));
        assert!(aux
            .iter()
            .any(|a| matches!(a.kind, pantheon_core::model::AuxiliaryKind::Compression)));
        // No aux configured = empty vec, layers off.
        let bare: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
        assert!(auxiliaries(Some(&bare)).is_empty());
    }

    #[test]
    fn voice_sections_parse_and_validate() {
        let cfg: Config = toml::from_str(
            "[model]\nprovider = \"local\"\nmodel = \"llama3.2\"\n\
             [stt]\nbackend = \"command\"\n[stt.options]\ncmd = \"whisper-cli\"\n\
             args = \"-m m.bin -f {file} -nt\"\n\
             [tts]\nbackend = \"openai\"\n[tts.options]\nprovider = \"openai\"\nmodel = \"tts-1\"\n",
        )
        .unwrap();
        let stt = cfg.stt.as_ref().expect("stt section");
        assert_eq!(stt.backend, "command");
        assert_eq!(stt.options["cmd"], "whisper-cli");
        assert_eq!(cfg.tts.as_ref().unwrap().backend, "openai");
        // Fully specified voice config validates clean.
        assert_eq!(cfg.validate(), Vec::<String>::new());

        // Backend-specific required options are checked.
        let bad: Config = toml::from_str(
            "[model]\nprovider = \"p\"\nmodel = \"m\"\n\
             [stt]\nbackend = \"command\"\n\
             [tts]\nbackend = \"openai\"\n",
        )
        .unwrap();
        let problems = bad.validate();
        assert!(problems.iter().any(|p| p.contains("stt.options.cmd")));
        assert!(problems.iter().any(|p| p.contains("tts.options.provider")));
        // Absent sections = no voice capability, no complaints.
        let none: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
        assert!(none.stt.is_none() && none.tts.is_none());
        assert_eq!(none.validate(), Vec::<String>::new());
    }

    #[test]
    fn model_key_env_reads_config_not_the_key() {
        let cfg: Config = toml::from_str(
            "[model]\nprovider = \"p\"\nmodel = \"m\"\napi_key_env = \"MY_KEY_VAR\"\n",
        )
        .unwrap();
        assert_eq!(model_key_env(Some(&cfg)).as_deref(), Some("MY_KEY_VAR"));
        // Configured without api_key_env (or no config at all) = the
        // default PANTHEON_API_KEY path, never a missing key.
        let bare: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
        assert_eq!(model_key_env(Some(&bare)), None);
        assert_eq!(model_key_env(None), None);
    }

    #[test]
    fn chat_secrets_resolves_the_config_named_model_key() {
        // Unique var name so parallel tests cannot collide; the
        // config-named var must win over the plain PANTHEON_API_KEY
        // fallback (this is the chat path bug: run honored it, chat
        // passed None).
        std::env::set_var("PANTHEON_TEST_CHAT_KEY_A7F3", "sk-from-config-env");
        let cfg: Config = toml::from_str(
            "[model]\nprovider = \"p\"\nmodel = \"m\"\n\
             api_key_env = \"PANTHEON_TEST_CHAT_KEY_A7F3\"\n",
        )
        .unwrap();
        let broker = chat_secrets(Some(&cfg));
        let k = broker
            .resolve("PANTHEON_API_KEY")
            .unwrap()
            .expect("config-named key env resolves into PANTHEON_API_KEY");
        assert_eq!(k.expose(), "sk-from-config-env");
        std::env::remove_var("PANTHEON_TEST_CHAT_KEY_A7F3");
    }

    #[test]
    fn chat_secrets_seeds_every_aux_key() {
        // with_aux_keys was the documented session-builder entry point but
        // had zero callers: the compression key never seeded on any path.
        std::env::set_var("PANTHEON_TEST_DEC_KEY_A7F3", "dec-1");
        std::env::set_var("PANTHEON_TEST_CMP_KEY_A7F3", "cmp-1");
        let cfg: Config = toml::from_str(
            "[model]\nprovider = \"p\"\nmodel = \"m\"\n\
             [decision]\nprovider = \"p\"\nmodel = \"d\"\n\
             api_key_env = \"PANTHEON_TEST_DEC_KEY_A7F3\"\n\
             [compression]\nprovider = \"p\"\nmodel = \"c\"\n\
             api_key_env = \"PANTHEON_TEST_CMP_KEY_A7F3\"\n",
        )
        .unwrap();
        let broker = chat_secrets(Some(&cfg));
        assert_eq!(
            broker
                .resolve("PANTHEON_DECISION_API_KEY")
                .unwrap()
                .map(|s| s.expose().to_string()),
            Some("dec-1".into())
        );
        assert_eq!(
            broker
                .resolve("PANTHEON_COMPRESSION_API_KEY")
                .unwrap()
                .map(|s| s.expose().to_string()),
            Some("cmp-1".into())
        );
        std::env::remove_var("PANTHEON_TEST_DEC_KEY_A7F3");
        std::env::remove_var("PANTHEON_TEST_CMP_KEY_A7F3");
    }
}
