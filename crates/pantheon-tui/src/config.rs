//! The complete `config.toml` document plus load/save/validate.

use super::config_schema::{PolicyPreset, SecretRef};
use pantheon_agent::agent_profile::{EffectiveProfile, ProfileError, ProfileRegistry};
use pantheon_api::error::{Layer, PantheonError};
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
    /// Reasoning effort for chat turns: off|minimal|low|medium|high.
    /// Absent (or `PANTHEON_REASONING` unset) means off — no effort param
    /// is sent and every provider behaves exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Exact thinking budget in tokens for budget wires (Anthropic).
    /// Overrides the level mapping; ignored on effort-string wires.
    /// 0 disables thinking entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<u32>,
}

/// One aux-model slot: every `[judge]`, `[compression]`, `[title_gen]`,
/// `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]` and
/// `[mcp_synthesis]` section has exactly this shape — provider + model +
/// optional key env name. The named section types below are aliases so
/// existing construction sites keep compiling untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuxSection {
    pub provider: String,
    pub model: String,
    /// Env var name holding the API key for the endpoint.
    /// Never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

/// `[judge]`: the auxiliary judge model. Any provider/model the
/// catalog knows (or a raw base URL as provider) — the runtime resolves
/// wire mode and key env the same way it does for chat. Absent = `auto`:
/// the run's default model answers judge queries (route select, tool
/// gate) — judging always runs, it just gets cheaper when configured.
pub type JudgeSection = AuxSection;

/// `[title_gen]`: the auxiliary session-title model. Names a conversation
/// from its first user prompt (fire-and-forget beside the first turn).
/// Absent = `auto`: the runtime uses the run's default model instead —
/// titles always work, they just get cheaper/smaller when configured.
pub type TitleGenSection = AuxSection;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FallbackEntry {
    pub provider: String,
    pub model: String,
}

/// One model row inside `[custom_providers.<name>.models]`.
///
/// Present so a custom endpoint's models appear in the catalog and can be
/// picked by name. A migrated provider used to register with no models at all,
/// which meant `pantheon providers` showed a bare endpoint and the operator had
/// to type a model id they could not see listed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CustomModel {
    /// Model id as the endpoint spells it.
    pub id: String,
    /// Context window in tokens. Omitted = unknown, which the runtime treats
    /// conservatively rather than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
    /// Provider-imposed max output tokens. Omitted = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// `[custom_providers.<name>]`: a user-defined endpoint (written by
/// `pantheon model` when you pick "Custom provider"). Behaves like a
/// cataloged provider everywhere: base URL + wire mode + key env.
/// The provider id is the table name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CustomProviderSection {
    /// Full base URL, no trailing slash (e.g. `http://127.0.0.1:8015/v1`).
    #[serde(default)]
    pub base_url: String,
    /// Wire format: `openai` (default) or `anthropic`.
    #[serde(default = "default_openai_mode")]
    pub api_mode: String,
    /// Env var name holding the API key. Never the key itself.
    /// Default: `PANTHEON_KEY_<NAME>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// Models this endpoint serves. Optional: an endpoint with none still
    /// works, the operator just has to name the model explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<CustomModel>,
}

fn default_openai_mode() -> String {
    "openai".into()
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
/// `auto`: the run's default model compresses; the deterministic fit
/// stays the correctness path either way.
pub type CompressionSection = AuxSection;

/// `[embeddings]`: the vector-search embedding model. The one auxiliary
/// where `auto` would be wrong: absent = the local hashing embedder,
/// never the chat model — pin a provider here to embed remotely.
pub type EmbeddingsSection = AuxSection;

/// `[search_synthesis]`: the model that turns retrieved passages into a
/// synthesized answer. Absent = `auto`: the run's default model writes
/// the synthesis — configuring it just makes search answers cheaper.
pub type SearchSynthesisSection = AuxSection;

/// `[vision]`: the image-understanding model. Absent = `auto`: the run's
/// default model handles images. Config + client surface only for now —
/// message image plumbing lands with multimodal content.
pub type VisionSection = AuxSection;

/// `[scheduled]`: the model scheduled (background) runs execute with.
/// Absent = `auto`: scheduled jobs run on the run's default model —
/// pin a small/cheap model here so background tasks stop competing with
/// interactive chat.
pub type ScheduledSection = AuxSection;

/// `[mcp_synthesis]`: the model that bounds large MCP tool results into
/// a short note before they enter context (compression's pattern,
/// scoped to MCP results). Absent = `auto`: the run's default model
/// summarizes.
pub type McpSynthesisSection = AuxSection;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemorySection {
    /// Backend name from the catalog: native | http | ...
    pub backend: String,
    #[serde(default)]
    pub options: std::collections::HashMap<String, String>,
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
/// `[agents.<name>]`: a durable identity for one persistent agent (§4).
///
/// The audit's gap: Hermes ships this as `profile.yaml` + `SOUL.md` +
/// `MEMORY.md`/`USER.md` (the 6 `_PROFILE_IDENTITY_MARKERS` files), while
/// Pantheon had no identity config at all — every run was anonymous.
/// This is the durable half: name, persona source files, memory namespace,
/// and capability policy live in config; the prompt assembly that reads
/// them is next. Persona files are referenced by path (repo-relative or
/// absolute), never inlined, so secrets that drift into a SOUL.md stay out
/// of config snapshots.
///
/// This is a **re-export of the core type**, not a second definition. The
/// declaration, inheritance, and namespace rules live in
/// `pantheon_agent::agent_profile` so the runtime can consume a profile
/// without depending on the terminal. An earlier version of this file declared
/// its own struct with four of the fields; it drifted as soon as
/// `inherits` and `model` were added, which is exactly the duplication
/// this re-export removes.
pub use pantheon_agent::agent_profile::AgentProfile as AgentIdentity;

/// Why an `[agents.<name>]` table is not usable, in `doctor`'s wording.
///
/// This used to be `AgentIdentity::validate`, a hand-rolled check of slug
/// shape, policy spelling, and namespace clashes. Every one of those rules
/// now lives in `ProfileRegistry`, where `resolve` and `validate_all`
/// enforce them, and where inheritance adds rules the old version could not
/// express (a child may not claim a parent's namespace, a `profile` that
/// names no declared agent is an error). Keeping a second copy here meant
/// `doctor` could pass a config that the runtime then refused.
fn agent_table_problems(all: &std::collections::HashMap<String, AgentIdentity>) -> Vec<String> {
    let mut reg = ProfileRegistry::new();
    let mut problems = Vec::new();
    // A malformed table name is reported here rather than through
    // `problems`, because it cannot be inserted and would otherwise
    // hide every other problem in the file.
    for (name, agent) in all {
        if let Err(e) = reg.insert(name, agent.clone()) {
            problems.push(e.to_string());
        }
    }
    // `problems` covers the registry-wide rules: unknown policies, namespace
    // clashes, inheritance cycles, parents that do not exist.
    problems.extend(reg.problems("coder").into_iter().map(|e| e.to_string()));
    problems
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    /// Free-form install label (`profile = "dev"`). Informational only;
    /// it does NOT select an agent. See [`Config::agent`].
    pub profile: Option<String>,
    /// The agent profile this install runs as (`agent = "zeus"`). Must name
    /// a declared `[agents.<name>]` table.
    ///
    /// Deliberately a different key from `profile`: that one predates agent
    /// profiles, is written by `setup` as "default", and is read by nothing
    /// that runs an agent. Overloading it would have made every existing
    /// config select a profile that was never declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub model: Option<ModelSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge: Option<JudgeSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embeddings: Option<EmbeddingsSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_synthesis: Option<SearchSynthesisSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<VisionSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<ScheduledSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_synthesis: Option<McpSynthesisSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_gen: Option<TitleGenSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt: Option<VoiceSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts: Option<VoiceSection>,
    pub policy: Option<PolicyPreset>,
    pub memory: Option<MemorySection>,
    /// Retained so an existing config.toml with a `[tools]` table still
    /// loads. The keys are inert: tool registration is unconditional and
    /// nothing reads them. Kept as an ignored value rather than a typed
    /// struct so an unfamiliar shape in a user's file is not a parse
    /// error, and so the decision to drop tool packs stays reversible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<toml::Value>,
    pub server: Option<ServerSection>,
    /// User-defined providers (`pantheon model` → Custom provider).
    /// Empty for configs written before this existed (back-compat).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub custom_providers: std::collections::HashMap<String, CustomProviderSection>,
    /// Durable per-agent identities (§4). Empty = all runs anonymous, the
    /// pre-identity behavior (back-compat).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub agents: std::collections::HashMap<String, AgentIdentity>,
}

impl Config {
    pub fn path(data_dir: &Path) -> std::path::PathBuf {
        data_dir.join("config.toml")
    }
    /// Load the config, or explain why it could not be read.
    ///
    /// `load(...).ok()` at a dozen call sites threw away a `CONFIG_PARSE`
    /// error that names the file, line, and column. A single typo then
    /// looked like a working install that could not reach a provider: the
    /// session fell back to `local`/`llama3.2` and failed against
    /// localhost with no mention of the file that was actually broken.
    ///
    /// A missing config is not an error, so this returns `None` for that
    /// case and prints for a config that exists but does not parse.
    pub fn load_or_report(data_dir: &Path) -> Option<Self> {
        match Self::load(data_dir) {
            Ok(c) => Some(c),
            Err(e) if e.code == "CONFIG_OPEN" => None,
            Err(e) => {
                eprintln!("pantheon: {}", e.cause);
                eprintln!("pantheon: fix: {}", e.remediation);
                std::process::exit(2);
            }
        }
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
    /// The agent profiles declared in this config, as a resolvable registry.
    ///
    /// This is the bridge from config text to the runtime's profile layer.
    /// Building it here (rather than at each call site) means every entry
    /// point — terminal, scheduler, AG-UI — sees the same declarations, and
    /// an inheritance chain broken by a typo is reported identically
    /// everywhere.
    pub fn profile_registry(&self) -> Result<ProfileRegistry, ProfileError> {
        let mut reg = ProfileRegistry::new();
        for (name, agent) in &self.agents {
            reg.insert(name, agent.clone()).map_err(|e| match e {
                // `insert` reports the slug problem; `validate` is the
                // existing doctor path for it, so don't double-report.
                ProfileError::InvalidName { .. } => e,
                other => other,
            })?;
        }
        reg.validate_all(self.default_policy_preset())?;
        Ok(reg)
    }

    /// The policy preset an agent gets when neither it nor any ancestor
    /// names one. The global config wins, so a profile inherits the
    /// install's baseline rather than a hardcoded runtime default.
    fn default_policy_preset(&self) -> &'static str {
        self.policy
            .map(crate::config_schema::PolicyPreset::as_str)
            .unwrap_or("coder")
    }

    /// Resolve the agent profile this config selects.
    ///
    /// Selection order: an explicit `--agent`, else `agent = "..."`, else
    /// `default`. A name that is not declared is an error, never a silent
    /// fallback — an operator who asked for `zeus` and silently got the
    /// default agent's memory and persona would have no way to notice.
    ///
    /// A config with no `[agents]` table at all is not an error: that is
    /// every install from before profiles, and those runs stay anonymous
    /// until the user declares one. Only a *named* agent must exist.
    pub fn resolve_profile(
        &self,
        override_name: Option<&str>,
    ) -> Result<Option<EffectiveProfile>, ProfileError> {
        let selected = override_name
            .or(self.agent.as_deref())
            .unwrap_or(pantheon_runtime::DEFAULT_PROFILE);
        if self.agents.is_empty() && override_name.is_none() && self.agent.is_none() {
            // Nothing declared and nothing asked for: anonymous, as before.
            return Ok(None);
        }
        let reg = self.profile_registry()?;
        reg.resolve(selected, self.default_policy_preset())
            .map(Some)
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
            if let Some(r) = m.reasoning.as_deref().filter(|s| !s.trim().is_empty()) {
                if pantheon_api::model::ReasoningLevel::parse(r).is_none() {
                    problems.push(format!(
                        "model.reasoning {r:?} is not off|minimal|low|medium|high|xhigh|max (resolves to off)"
                    ));
                }
            }
            if let Some(b) = m.reasoning_budget {
                if b > 0 && b < 1024 {
                    problems.push(format!(
                        "model.reasoning_budget {b} is below the 1024-token budget-wire minimum (skipped at request time)"
                    ));
                }
            }
        } else {
            problems.push("no [model] section: run `pantheon setup`".into());
        }
        // Every aux section validates identically: non-empty target
        // fields + a resolvable api_key_env when named.
        fn aux_problem(name: &str, sec: (&str, &str, &Option<String>), problems: &mut Vec<String>) {
            let (provider, model, api_key_env) = sec;
            if provider.trim().is_empty() {
                problems.push(format!("{name}.provider is empty"));
            }
            if model.trim().is_empty() {
                problems.push(format!("{name}.model is empty"));
            }
            if let Some(env) = api_key_env {
                let r = SecretRef::from_env(env.clone());
                if let Err(e) = r.validate() {
                    problems.push(format!("{name}.api_key_env: {e}"));
                } else if r.resolve().is_none() {
                    problems.push(format!("env var {env} is not set"));
                }
            }
        }
        for slot in AUX_SLOTS {
            if let Some(s) = cfg_section(self, slot) {
                aux_problem(
                    slot.name,
                    (&s.provider, &s.model, &s.api_key_env),
                    &mut problems,
                );
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
        // Agent identity tables validate at load: slugs, known policies,
        // no namespace clashes. An invalid [agents.*] table fails doctor
        // loudly instead of silently running anonymous.
        // One registry-wide check: the rules are per-registry (namespace
        // clashes, inheritance chains), so validating table by table would
        // report the same fault once per profile. Every problem is
        // reported, not just the first.
        problems.extend(agent_table_problems(&self.agents));
        // A selected `agent` that names no declared profile is a config
        // error. Note this is `agent`, not `profile`: `profile` is a
        // free-form informational label that `setup` writes as "default"
        // with no `[agents]` table at all, so treating it as a selector
        // would fail every install created before agent profiles existed.
        if let Some(selected) = &self.agent {
            if !self.agents.contains_key(selected) {
                problems.push(format!(
                    "agent {selected:?} is not declared; add [agents.{selected}] or \
                     remove the agent setting"
                ));
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

/// Resolve an aux-model target: `PANTHEON_*` env overrides the section
/// field-wise. Pure so tests don't touch env. The single generic behind
/// every `*_target` below.
fn aux_target(
    section: Option<(&String, &String)>,
    env_provider: Option<String>,
    env_model: Option<String>,
) -> Option<(String, String)> {
    let pick = |env: Option<String>, cfg: Option<&String>| -> Option<String> {
        env.filter(|v| !v.trim().is_empty())
            .or_else(|| cfg.map(|v| v.to_string()).filter(|v| !v.trim().is_empty()))
    };
    let (sp, sm) = match section {
        Some((p, m)) => (Some(p), Some(m)),
        None => (None, None),
    };
    Some((pick(env_provider, sp)?, pick(env_model, sm)?))
}

/// One aux slot: everything that varies per capability. The table below
/// drives target resolution, key seeding, validation, and the
/// `auxiliaries()` fan-out — adding a capability means adding one row.
struct AuxSlot {
    kind: pantheon_api::model::AuxiliaryKind,
    /// Config section name (for diagnostics).
    name: &'static str,
    /// `PANTHEON_<PREFIX>_PROVIDER` / `PANTHEON_<PREFIX>_MODEL`.
    env_prefix: &'static str,
    /// Vault entry the section's key env seeds.
    vault_name: &'static str,
    /// Absent section falls back to `auto` (the default model).
    /// False only for embeddings (absent = local embedder, never chat).
    auto: bool,
    section: for<'a> fn(&'a Config) -> Option<&'a AuxSection>,
}

const AUX_SLOTS: &[AuxSlot] = &[
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::Judge,
        name: "judge",
        env_prefix: "JUDGE",
        vault_name: "PANTHEON_JUDGE_API_KEY",
        auto: true,
        section: |c| c.judge.as_ref(),
    },
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::Compression,
        name: "compression",
        env_prefix: "COMPRESSION",
        vault_name: "PANTHEON_COMPRESSION_API_KEY",
        auto: true,
        section: |c| c.compression.as_ref(),
    },
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::TitleGen,
        name: "title_gen",
        env_prefix: "TITLEGEN",
        vault_name: "PANTHEON_TITLEGEN_API_KEY",
        auto: true,
        section: |c| c.title_gen.as_ref(),
    },
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::Embeddings,
        name: "embeddings",
        env_prefix: "EMBEDDINGS",
        vault_name: "PANTHEON_EMBEDDINGS_API_KEY",
        auto: false,
        section: |c| c.embeddings.as_ref(),
    },
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::SearchSynthesis,
        name: "search_synthesis",
        env_prefix: "SEARCH_SYNTHESIS",
        vault_name: "PANTHEON_SEARCH_SYNTHESIS_API_KEY",
        auto: true,
        section: |c| c.search_synthesis.as_ref(),
    },
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::Vision,
        name: "vision",
        env_prefix: "VISION",
        vault_name: "PANTHEON_VISION_API_KEY",
        auto: true,
        section: |c| c.vision.as_ref(),
    },
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::Scheduled,
        name: "scheduled",
        env_prefix: "SCHEDULED",
        vault_name: "PANTHEON_SCHEDULED_API_KEY",
        auto: true,
        section: |c| c.scheduled.as_ref(),
    },
    AuxSlot {
        kind: pantheon_api::model::AuxiliaryKind::McpSynthesis,
        name: "mcp_synthesis",
        env_prefix: "MCP_SYNTHESIS",
        vault_name: "PANTHEON_MCP_SYNTHESIS_API_KEY",
        auto: true,
        section: |c| c.mcp_synthesis.as_ref(),
    },
];

/// Borrow one slot's section for validation.
fn cfg_section<'a>(cfg: &'a Config, slot: &AuxSlot) -> Option<&'a AuxSection> {
    (slot.section)(cfg)
}

/// Resolve one slot's target from its section + env pair.
fn slot_target(slot: &AuxSlot, cfg: Option<&Config>) -> Option<(String, String)> {
    let section = cfg.and_then(slot.section);
    aux_target(
        section.map(|s| (&s.provider, &s.model)),
        std::env::var(format!("PANTHEON_{}_PROVIDER", slot.env_prefix)).ok(),
        std::env::var(format!("PANTHEON_{}_MODEL", slot.env_prefix)).ok(),
    )
}

/// Resolve one slot's auxiliary entry (pinned target only, no `auto`).
fn slot_aux(slot: &AuxSlot, cfg: Option<&Config>) -> Option<pantheon_api::model::AuxiliaryModel> {
    let (provider, model) = slot_target(slot, cfg)?;
    Some(pantheon_api::model::AuxiliaryModel {
        kind: slot.kind.clone(),
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

/// Seed every configured aux key in one call (the session-builder sites'
/// entry point).
pub fn with_aux_keys(
    secrets: pantheon_secrets::SecretsBroker,
    cfg: Option<&Config>,
) -> pantheon_secrets::SecretsBroker {
    let mut secrets = secrets;
    for slot in AUX_SLOTS {
        let env = cfg
            .and_then(slot.section)
            .and_then(|s| s.api_key_env.clone());
        secrets = seed_env_key(secrets, env, slot.vault_name);
    }
    secrets
}

/// `[model].api_key_env` → the env-var name holding the chat model key.
/// Never the key itself; `None` = the default `PANTHEON_API_KEY` path.
pub fn model_key_env(cfg: Option<&Config>) -> Option<String> {
    cfg.and_then(|c| c.model.as_ref())
        .and_then(|m| m.api_key_env.clone())
}

/// Env-var-safe version of a provider id: `my-llm` → `MY_LLM`.
/// Delegates to [`pantheon_providers::catalog::env_part`] — one cleaner for
/// every `PANTHEON_*` name.
pub fn sanitize_env_suffix(id: &str) -> String {
    pantheon_providers::catalog::env_part(id)
}

/// Effective key env var for a provider id: explicit `key_env` wins,
/// otherwise `PANTHEON_KEY_<ID>`. Naming only — no env lookup. (The
/// runtime read path is `catalog::key_for`, which resolves this same name
/// against the environment; keep the two in agreement via
/// `catalog::env_part`.)
pub fn provider_key_env(provider_id: &str, explicit: Option<&str>) -> String {
    explicit
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", sanitize_env_suffix(provider_id)))
}

/// Insert (or replace) one `[custom_providers.<name>]` row, persist the
/// config, and register it with the global catalog. The single writer
/// behind `pantheon provider add` and the `pantheon model` builtin-override
/// path. Returns whether the id shadows a builtin catalog id (the caller
/// announces it).
pub fn upsert_custom_row(
    data_dir: &std::path::Path,
    name: &str,
    base_url: &str,
    api_mode: pantheon_providers::catalog::ApiMode,
    key_env: &str,
) -> Result<bool, String> {
    use pantheon_providers::catalog::ApiMode as Mode;
    let mut cfg = Config::load(data_dir).unwrap_or_default();
    let shadow = pantheon_providers::catalog::providers()
        .iter()
        .any(|p| p.id == name);
    cfg.custom_providers.insert(
        name.to_string(),
        CustomProviderSection {
            base_url: base_url.to_string(),
            api_mode: match api_mode {
                Mode::OpenAi => "openai".into(),
                Mode::Anthropic => "anthropic".into(),
            },
            key_env: Some(key_env.to_string()),
            // Preserve whatever the operator has already named on this
            // endpoint. Replacing the whole row used to drop them, so
            // re-picking a provider silently emptied its model list.
            models: cfg
                .custom_providers
                .get(name)
                .map(|s| s.models.clone())
                .unwrap_or_default(),
        },
    );
    cfg.save(data_dir).map_err(|e| e.to_string())?;
    register_custom_providers(&cfg);
    Ok(shadow)
}

/// Record a model the operator named by hand on a custom endpoint.
///
/// This is the only thing that ever adds a row to `[custom_providers.*].models`.
/// A model list harvested from another agent's config is deliberately *not*
/// written: it is a snapshot of a third-party endpoint and goes stale. This
/// records a name a human actually chose, which stays true.
///
/// Idempotent, and preserves any limits already known for that model.
pub fn remember_custom_model(
    data_dir: &std::path::Path,
    provider: &str,
    model: &str,
) -> Result<bool, String> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(false);
    }
    let mut cfg = Config::load(data_dir).unwrap_or_default();
    let Some(sec) = cfg.custom_providers.get_mut(provider) else {
        return Err(format!(
            "no custom provider {provider:?}; add it with `pantheon provider add --name {provider}`"
        ));
    };
    if sec.models.iter().any(|m| m.id == model) {
        return Ok(false);
    }
    sec.models.push(CustomModel {
        id: model.to_string(),
        context_limit: None,
        max_output_tokens: None,
    });
    sec.models.sort_by(|a, b| a.id.cmp(&b.id));
    cfg.save(data_dir).map_err(|e| e.to_string())?;
    register_custom_providers(&cfg);
    Ok(true)
}

/// Load `<data_dir>/.env` (exports always win) and register
/// `[custom_providers.*]` with the catalog. Every verb entry calls this
/// once before doing anything else; forgetting it silently breaks
/// template and key resolution.
pub fn init_env_and_catalog(data_dir: &std::path::Path) {
    crate::dotenv::load_dotenv(data_dir);
    if let Ok(cfg) = Config::load(data_dir) {
        register_custom_providers(&cfg);
    }
}

/// Register every `[custom_providers.*]` entry with the global catalog so
/// the runtime resolves base URL / wire mode / key env for them. Called
/// once at terminal startup after loading the config; idempotent.
pub fn register_custom_providers(cfg: &Config) {
    for (name, sec) in &cfg.custom_providers {
        if sec.base_url.trim().is_empty() {
            continue;
        }
        let api_mode = match sec.api_mode.trim().to_ascii_lowercase().as_str() {
            "anthropic" => pantheon_providers::catalog::ApiMode::Anthropic,
            _ => pantheon_providers::catalog::ApiMode::OpenAi,
        };
        // Register the endpoint's models so `pantheon providers` lists them and
        // the model picker can name one. `model_meta` supplies conservative
        // defaults (tools on, vision/reasoning off, streaming on) and any
        // declared limit overrides them.
        let models: Vec<pantheon_providers::catalog::ModelMeta> = sec
            .models
            .iter()
            .filter(|m| !m.id.trim().is_empty())
            .map(|m| {
                let mut meta = pantheon_providers::catalog::model_meta(name, m.id.trim());
                if let Some(c) = m.context_limit {
                    meta.context_limit = Some(c);
                }
                if let Some(o) = m.max_output_tokens {
                    meta.max_output_tokens = Some(o);
                }
                meta
            })
            .collect();
        pantheon_providers::catalog::register_custom_provider(
            pantheon_providers::catalog::ProviderMeta {
                id: name.clone(),
                label: name.clone(),
                base_url: sec.base_url.trim().trim_end_matches('/').to_string(),
                api_mode,
                base_env: String::new(),
                key_env: provider_key_env(name, sec.key_env.as_deref()),
                key_header: "Authorization".into(),
                models,
                prominent: true,
                dev: false,
                tag: "custom".into(),
            },
        );
    }
}

/// The one secrets broker every session-builder site constructs: the chat
/// model key (config-named env var, else `PANTHEON_API_KEY`), then every
/// aux key (judge, compression, title, embeddings, search synthesis,
/// vision, scheduled, MCP synthesis), environment fallback last.
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

/// Resolve the model policy for one session: explicit override > environment >
/// `config.toml` > the hardcoded local default, plus the configured fallback
/// chain and every auxiliary slot.
///
/// This lives here, not in an interface module, because it is a property of
/// the config document rather than of any surface. The TUI, the AG-UI
/// session factory, and the scheduler all resolve a session the same way, and
/// a resolution rule that lives beside a UI keeps drifting from the one
/// beside the verb that replaced it.
pub fn build_model_policy(
    cfg: Option<&Config>,
    provider: Option<String>,
    model: Option<String>,
) -> pantheon_api::model::ModelPolicy {
    let cfg_model = cfg
        .and_then(|c| c.model.clone())
        .map(|m| (m.provider, m.model));
    let default = pantheon_api::model::DefaultModel {
        provider: provider
            .or_else(|| cfg_model.as_ref().map(|(p, _)| p.clone()))
            .or_else(|| std::env::var("PANTHEON_PROVIDER").ok())
            .unwrap_or_else(|| "local".into()),
        model: model
            .or_else(|| cfg_model.as_ref().map(|(_, m)| m.clone()))
            .or_else(|| std::env::var("PANTHEON_MODEL").ok())
            .unwrap_or_else(|| "llama3.2".into()),
    };
    let mut chain = pantheon_api::model::FallbackChain::default();
    if let Some(fallbacks) = cfg
        .and_then(|c| c.model.as_ref())
        .map(|m| m.fallbacks.clone())
    {
        for f in fallbacks {
            chain.fallbacks.push(pantheon_api::model::DefaultModel {
                provider: f.provider,
                model: f.model,
            });
        }
    }
    pantheon_api::model::ModelPolicy {
        reasoning_budget: resolve_reasoning_budget(cfg),
        reasoning: resolve_reasoning(cfg),
        default: default.clone(),
        fallbacks: chain,
        auxiliaries: auxiliaries(cfg, &default),
    }
}

/// Exact thinking budget: `PANTHEON_REASONING_BUDGET` wins, then
/// `[model].reasoning_budget`. Zero disables (reads as "no budget").
/// Applies to budget wires only; effort-string wires ignore it.
fn resolve_reasoning_budget(cfg: Option<&Config>) -> Option<u32> {
    if let Ok(v) = std::env::var("PANTHEON_REASONING_BUDGET") {
        if let Ok(n) = v.trim().parse::<u32>() {
            return Some(n);
        }
    }
    cfg.and_then(|c| c.model.as_ref())
        .and_then(|m| m.reasoning_budget)
}

/// Reasoning effort for chat turns: `PANTHEON_REASONING` wins, then
/// `[model].reasoning`, then off. Unknown strings resolve to off — the
/// safe direction is sending no param, and `doctor` flags the typo (see
/// `Config::validate`) rather than failing the session.
fn resolve_reasoning(cfg: Option<&Config>) -> pantheon_api::model::ReasoningLevel {
    use pantheon_api::model::ReasoningLevel;
    if let Ok(v) = std::env::var("PANTHEON_REASONING") {
        if let Some(level) = ReasoningLevel::parse(&v) {
            return level;
        }
    }
    cfg.and_then(|c| c.model.as_ref())
        .and_then(|m| m.reasoning.as_deref())
        .and_then(ReasoningLevel::parse)
        .unwrap_or_default()
}

/// Every auxiliary for this host with a resolved target: an explicit
/// `[judge]` / `[compression]` / `[title_gen]` / `[search_synthesis]` /
/// `[vision]` / `[scheduled]` / `[mcp_synthesis]` section (or its env
/// override) wins; otherwise `auto` — the run's default model. Aux
/// models default to auto, so an absent section never switches a
/// capability off, it just means "use what you already use for chat".
///
/// The documented exception is `Embeddings`: absent = the local hashing
/// embedder, never the chat model — so an entry appears only when
/// `[embeddings]` (or its env) actually pins a target.
pub fn auxiliaries(
    cfg: Option<&Config>,
    default: &pantheon_api::model::DefaultModel,
) -> Vec<pantheon_api::model::AuxiliaryModel> {
    use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel};
    let auto = |kind: AuxiliaryKind| AuxiliaryModel {
        kind,
        provider: default.provider.clone(),
        model: default.model.clone(),
    };
    let mut out = Vec::with_capacity(AUX_SLOTS.len());
    for slot in AUX_SLOTS {
        match slot_aux(slot, cfg) {
            Some(pinned) => out.push(pinned),
            // Embeddings is the documented exception: absent = the local
            // hashing embedder, never the chat model — no `auto` entry.
            None if slot.auto => out.push(auto(slot.kind.clone())),
            None => {}
        }
    }
    out
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
