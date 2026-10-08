//! The complete `config.toml` document plus load/save/validate.
//!
//! This is the shared, client-agnostic home of the config document:
//! the TUI, the web dashboard, and every other client read and write
//! the same types. Client/composition-specific resolution (auxiliary
//! model wiring, secrets brokers, runtime budget structs) lives with
//! the clients, not here - this module never depends on anything
//! above the API leaf.

use crate::agent_profile::{EffectiveProfile, ProfileError, ProfileRegistry};
use crate::config_schema::{PolicyPreset, SecretRef};
use crate::error::{Layer, PantheonError};
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
    /// Absent (or `PANTHEON_REASONING` unset) means off - no effort param
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
/// `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`,
/// `[mcp_synthesis]`, `[extraction]`, `[rerank]`, `[planner]` and
/// `[verify]` section has exactly this shape. The named section types
/// below are aliases so existing construction sites keep compiling
/// untouched.
///
/// Hermes-style inheritance: `provider = "default"` (or omitted) inherits
/// `[model]`'s provider, an empty/omitted `model` inherits `[model]`'s
/// model, and an omitted `api_key_env` on a default-inheriting provider
/// inherits `[model].api_key_env`. Each slot stays independent - pin a
/// different provider/model per slot when you want to, inherit when you
/// don't.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuxSection {
    /// Provider id, or `"default"` (or empty) to inherit `[model]`'s.
    #[serde(default)]
    pub provider: String,
    /// Model name, or empty to inherit `[model]`'s.
    #[serde(default)]
    pub model: String,
    /// Env var name holding the API key for the endpoint.
    /// Never the key itself. Only needed for a non-default provider;
    /// a default-inheriting provider inherits `[model].api_key_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Per-aux request timeout in seconds (Hermes-style `timeout`).
    /// Absent = the client's built-in default for the capability
    /// (10s title/judge, 15s embeddings, 30s compression, 60s
    /// reflection/consolidation/repair, 120s everything else).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// `[judge]`: the auxiliary judge model. Any provider/model the
/// catalog knows (or a raw base URL as provider) - the runtime resolves
/// wire mode and key env the same way it does for chat. Absent = `auto`:
/// the run's default model answers judge queries (route select, tool
/// gate) - judging always runs, it just gets cheaper when configured.
pub type JudgeSection = AuxSection;

/// `[title_gen]`: the auxiliary session-title model. Names a conversation
/// from its first user prompt (fire-and-forget beside the first turn).
/// Absent = `auto`: the runtime uses the run's default model instead
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
    /// Operator-declared capability: this model takes native video input
    /// on this endpoint (e.g. a Qwen-Omni model on an OpenAI-compatible
    /// one). Pantheon trusts the declaration and sends the real video
    /// bytes through the endpoint's wire format.
    #[serde(default)]
    pub video: bool,
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

/// STT backend ids the runtime can construct (`open_stt` in
/// pantheon-providers: `command`, the OpenAI-wire trio, and the bespoke
/// live backends).
///
/// `pantheon-api` cannot depend on `pantheon-providers` - that would be
/// circular - so the list is duplicated here. A test in `pantheon-tui`
/// pins the voice registry against these constants, so a new backend
/// cannot land in the registry without updating validation.
pub const STT_BACKENDS: &[&str] = &[
    "command",
    "openai",
    "groq",
    "mistral",
    "deepgram",
    "elevenlabs",
    "xai",
    "assemblyai",
];

/// TTS backend ids the runtime can construct (`open_tts` in
/// pantheon-providers: `command`, the local shell-out backends, and the
/// bespoke live backends). Same duplication caveat as [`STT_BACKENDS`].
pub const TTS_BACKENDS: &[&str] = &[
    "command",
    "piper-local",
    "kokoro-local",
    "fishspeech-local",
    "openai",
    "elevenlabs",
    "deepgram",
    "gemini",
    "fishaudio",
];

/// `[stt]` / `[tts]`: speech service selection. These are provider-plane
/// services (a local binary or an HTTP endpoint), never model-policy
/// entries - same shape as `[memory]`'s backend selection. Absent = the
/// surface simply has no speech capability.
///
/// Backends: `command` (local binary: `cmd`, `args` template with
/// `{file}`/`{language}` for STT and `{voice}`/`{format}` for TTS,
/// `timeout_secs`) or a named provider (OpenAI-compatible HTTP:
/// `provider` - catalog id or base URL - plus `model`, and `api_key_env`
/// naming the env var that holds the key; the key resolves through the
/// secrets broker, falling back to the catalog row's key env). Named
/// STT providers: groq, openai, mistral, xai, elevenlabs, deepgram,
/// assemblyai. Named TTS providers: piper-local, kokoro-local, elevenlabs,
/// deepgram, openai, gemini, fishaudio, fishspeech-local.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VoiceSection {
    /// Backend name from the provider registry: `command` or a named
    /// STT/TTS provider (see above).
    pub backend: String,
    /// Backend-specific options (cmd/args for command, provider/model
    /// for openai, timeout_secs, ...).
    #[serde(default)]
    pub options: std::collections::HashMap<String, String>,
}

/// `[voice]`: live voice-mode session knobs for the mobile
/// `/agui/voice/live` WebSocket (docs/live-voice-mode.md). The live session
/// is refused unless `[tools] voice` is on, `[stt]` and `[tts]` are both
/// present and constructible, AND `live_enabled = true` here. Absent =
/// live mode off with default limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveVoiceSection {
    /// Master opt-in for live voice mode. Default false - text chat stays
    /// the default everywhere; this is a feature, never the default.
    #[serde(default)]
    pub live_enabled: bool,
    /// Hard cap on one live session, seconds. Default 600.
    #[serde(default = "default_live_max_session_secs")]
    pub live_max_session_secs: u64,
    /// Hard cap on one utterance, seconds: runaway utterances are
    /// force-closed. Default 30.
    #[serde(default = "default_live_max_utterance_secs")]
    pub live_max_utterance_secs: u64,
    /// Energy-VAD silence bound: once speech was detected, auto-close the
    /// utterance after this many silent milliseconds. Default 1200.
    #[serde(default = "default_live_silence_timeout_ms")]
    pub live_silence_timeout_ms: u64,
}

impl Default for LiveVoiceSection {
    fn default() -> Self {
        Self {
            live_enabled: false,
            live_max_session_secs: default_live_max_session_secs(),
            live_max_utterance_secs: default_live_max_utterance_secs(),
            live_silence_timeout_ms: default_live_silence_timeout_ms(),
        }
    }
}

/// `[gateway.channels.<name>]`: per-channel gateway options. The table
/// name is the channel name (`telegram`, `discord` for the legacy
/// single-bot shape; any slug for explicit multi-bot channels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GatewayChannelSection {
    /// Speak agent replies as voice messages on this channel (Telegram
    /// `sendVoice`, Discord audio attachment). Default off - text stays
    /// the default. Requires a `[tts]` backend; without one the channel
    /// keeps sending text.
    #[serde(default)]
    pub voice_replies: bool,
    /// Platform this channel speaks: `"telegram"` or `"discord"`. Absent
    /// = inferred from the channel name when it is exactly `telegram` or
    /// `discord` (the legacy shape). A channel that sets this is an
    /// *explicit* channel: the gateway starts one poller per explicit
    /// channel, so two channels can both be `telegram` with different
    /// bot tokens and different agent profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Secret name holding this channel's bot token, resolved through the
    /// secrets broker (process env wins, then `<data_dir>/gateway.env`).
    /// Absent = the platform's standard token (`PANTHEON_TELEGRAM_BOT_TOKEN`
    /// / `PANTHEON_DISCORD_TOKEN`), which keeps single-bot setups working
    /// untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<String>,
    /// Agent profile serving this channel. Absent = the default profile
    /// resolution (explicit `--profile`, else `agent = "..."`, else
    /// `default`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl GatewayChannelSection {
    /// The platform this channel speaks, or `None` when neither `platform`
    /// nor the channel name says. Anything other than `telegram` /
    /// `discord` is a config error (see [`Config::validate`]).
    pub fn resolved_platform(&self, channel_name: &str) -> Option<String> {
        if let Some(p) = self.platform.as_deref() {
            let p = p.trim().to_lowercase();
            return Some(p);
        }
        match channel_name {
            "telegram" | "discord" => Some(channel_name.to_string()),
            _ => None,
        }
    }

    /// True when this channel entry opts into the explicit multi-bot
    /// shape (declares its platform). Explicit channels are started
    /// per-entry; legacy name-only entries keep the old behavior.
    pub fn is_explicit(&self) -> bool {
        self.platform.as_ref().is_some_and(|p| !p.trim().is_empty())
    }
}

/// `[gateway]`: gateway channel options. Absent = every channel default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GatewaySection {
    /// Per-channel options by channel name.
    #[serde(default)]
    pub channels: std::collections::HashMap<String, GatewayChannelSection>,
}

/// `[compression]`: the auxiliary context-compression model. Summarizes
/// the oldest exchanges when a transcript overflows the window. Absent =
/// `auto`: the run's default model compresses; the deterministic fit
/// stays the correctness path either way.
///
/// `target_percent` (1-100, default 12) sets the summary size as a
/// percentage of the absorbed transcript chars: higher keeps more detail
/// (less aggressive), lower compresses harder. 0 and >100 are config
/// errors; absent keeps today's fixed eighth-of-material target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompressionSection {
    #[serde(flatten)]
    pub aux: AuxSection,
    #[serde(default, deserialize_with = "de_target_percent")]
    pub target_percent: Option<u8>,
}

/// `target_percent` must be 1-100. `u8` already rejects >255 at parse;
/// this rejects the remaining invalid values (0 and 101-255) with a
/// config error instead of silently clamping.
fn de_target_percent<'de, D>(deserializer: D) -> Result<Option<u8>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value: Option<u8> = Option::deserialize(deserializer)?;
    match value {
        Some(0) => Err(serde::de::Error::custom(
            "compression.target_percent must be 1-100",
        )),
        Some(p) if p > 100 => Err(serde::de::Error::custom(
            "compression.target_percent must be 1-100",
        )),
        value => Ok(value),
    }
}

/// `[embeddings]`: the vector-search embedding model. The one auxiliary
/// where `auto` would be wrong: absent = the local hashing embedder,
/// never the chat model - pin a provider here to embed remotely.
pub type EmbeddingsSection = AuxSection;

/// `[search_synthesis]`: the model that turns retrieved passages into a
/// synthesized answer. Absent = `auto`: the run's default model writes
/// the synthesis - configuring it just makes search answers cheaper.
pub type SearchSynthesisSection = AuxSection;

/// `[vision]`: the image-understanding model. Absent = `auto`: the run's
/// default model handles images. Config + client surface only for now
/// message image plumbing lands with multimodal content.
pub type VisionSection = AuxSection;

/// `[video]`: the video-understanding model. Absent = `auto`: the run's
/// default model handles video. Mirrors `[vision]` exactly: config +
/// client surface only for now - the video-understanding pipeline
/// (keyframe sampling, temporal QA) is future work; the slot exists so a
/// model can be pinned ahead of it landing.
pub type VideoSection = AuxSection;

/// `[scheduled]`: the model scheduled (background) runs execute with.
/// Absent = `auto`: scheduled jobs run on the run's default model
/// pin a small/cheap model here so background tasks stop competing with
/// interactive chat.
pub type ScheduledSection = AuxSection;

/// Default completed turns before an automatic reflection pass.
pub const DEFAULT_REFLECT_AUTO_TURNS: u32 = 20;

/// Default proposal cap per reflection pass.
pub const DEFAULT_REFLECT_MAX_PROPOSALS: usize = 5;

fn default_reflect_auto_turns() -> u32 {
    DEFAULT_REFLECT_AUTO_TURNS
}

fn default_reflect_max_proposals() -> usize {
    DEFAULT_REFLECT_MAX_PROPOSALS
}

/// `[reflect]`: Reflection - Pantheon's ledger-native self-improvement
/// loop, plus the auxiliary model pin for its LLM-backed steps, in one
/// table.
///
/// Behavior knobs (`enabled`, `auto_turns`, `max_proposals`) and the model
/// pin share the table the same way `[scheduled]` doubles as both the
/// schedule-model pin and the background-runs section: one `[reflect]`
/// table is everything the feature needs. Absent `provider`/`model` =
/// `auto`: the run's default model answers reflection LLM calls
/// configure a small/cheap model here (or
/// `PANTHEON_REFLECTION_PROVIDER`/`PANTHEON_REFLECTION_MODEL`) so
/// background self-improvement never competes with interactive chat.
///
/// DEPRECATED as a behavior-knob table: `[nightly]` is the single
/// authoritative section for the unified nightly pass. This table still
/// parses (old configs keep loading) and, when `[nightly]` is absent,
/// the unified pass honors `enabled` and `auto_turns` from it
/// field-by-field as a migration fallback - but when `[nightly]` is
/// present it is ignored entirely. Prefer `[nightly]`; `pantheon
/// doctor` nudges you to move the knobs across. The auxiliary-model pin
/// (`provider`/`model`/`api_key_env`/`timeout`) legitimately stays here:
/// `[nightly]` carries no model pin, and the pass's proposal-refinement
/// step always resolves through the Reflection aux slot.
///
/// LLM-backed reflection steps are **off by default** (`enabled = false`):
/// the deterministic signal-extraction pipeline always runs, but anything
/// spending model tokens needs explicit opt-in. Every LLM call the
/// reflection pipeline makes resolves through the
/// [`AuxiliaryKind::Reflection`](crate::model::AuxiliaryKind)
/// slot - never the chat model directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReflectSection {
    /// Allow LLM-backed reflection steps. Default false.
    #[serde(default)]
    pub enabled: bool,
    /// Completed turns before an automatic reflection pass runs. Default
    /// 20. `0` disables the automatic trigger.
    #[serde(default = "default_reflect_auto_turns")]
    pub auto_turns: u32,
    /// Maximum proposals generated per pass. Default 5.
    #[serde(default = "default_reflect_max_proposals")]
    pub max_proposals: usize,
    /// Auxiliary model pin for reflection LLM calls. Absent = `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Auxiliary model pin for reflection LLM calls. Absent = `auto`.
    /// `"default"` (or empty) explicitly inherits the `[model]` model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Env var name holding the API key for the endpoint. Never the key
    /// itself. Seeds the `PANTHEON_REFLECTION_API_KEY` vault entry.
    /// Only needed for a non-default provider; a default-inheriting
    /// provider inherits `[model].api_key_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Per-aux request timeout in seconds (Hermes-style `timeout`).
    /// Absent = the client's built-in default for the capability
    /// (10s title/judge, 15s embeddings, 30s compression, 60s
    /// reflection/consolidation, 120s everything else).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// `[consolidation]`: Consolidation - Pantheon's background memory
/// consolidation, plus the auxiliary model pin for its LLM-backed
/// distill step, in one table.
///
/// Behavior knobs (`enabled`, `half_life_days`, `min_sessions`,
/// `min_score`, `cron`) and the model pin share the table the same way
/// `[reflect]` does: one `[consolidation]` table is everything the
/// feature needs. Absent `provider`/`model` = `auto`: the run's default
/// model answers consolidation LLM calls - configure a small/cheap
/// model here (or `PANTHEON_CONSOLIDATION_PROVIDER` /
/// `PANTHEON_CONSOLIDATION_MODEL`) so nightly memory consolidation
/// never competes with interactive chat.
///
/// LLM-backed consolidation steps are **off by default**
/// (`enabled = false`): the deterministic stage/weigh/promote pipeline
/// always runs, but anything spending model tokens needs explicit
/// opt-in. Every LLM call the consolidation pipeline makes resolves
/// through the
/// [`AuxiliaryKind::Consolidation`](crate::model::AuxiliaryKind)
/// slot - never the chat model directly.
///
/// DEPRECATED as a behavior-knob table: `[nightly]` is the single
/// authoritative section for the unified nightly pass. This table still
/// parses (old configs keep loading) and, when `[nightly]` is absent,
/// the unified pass honors `enabled`, `min_sessions`, and `cron` from it
/// field-by-field as a migration fallback - but when `[nightly]` is
/// present it is ignored entirely. Prefer `[nightly]`; `pantheon
/// doctor` nudges you to move the knobs across. The auxiliary-model pin
/// (`provider`/`model`/`api_key_env`/`timeout`) legitimately stays here:
/// `[nightly]` carries no model pin, and the pass's memory-distillation
/// step always resolves through the Consolidation aux slot.
///
/// The decay curve is gone: `half_life_days` and `min_score` are accepted
/// and ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ConsolidationSection {
    /// Allow LLM-backed consolidation steps (candidate distillation).
    /// Default false.
    #[serde(default)]
    pub enabled: bool,
    /// IGNORED. The exponential decay curve was removed; promotion is
    /// plain frequency + recency (`min_sessions` / `max_age_days`).
    /// Kept so old configs still parse.
    #[serde(default = "default_consolidation_half_life")]
    pub half_life_days: f64,
    /// Distinct sessions a candidate must appear in before promotion.
    /// Default 3.
    #[serde(default = "default_consolidation_min_sessions")]
    pub min_sessions: usize,
    /// IGNORED. The decayed-score threshold was removed with the decay
    /// curve. Kept so old configs still parse.
    #[serde(default = "default_consolidation_min_score")]
    pub min_score: f64,
    /// Default cron for `pantheon consolidate --schedule`. Default
    /// `0 3 * * *` (03:00 nightly).
    #[serde(default = "default_consolidation_cron")]
    pub cron: String,
    /// Auxiliary model pin for consolidation LLM calls. Absent = `auto`.
    /// `"default"` explicitly inherits the `[model]` provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Auxiliary model pin for consolidation LLM calls. Absent = `auto`.
    /// `"default"` (or empty) explicitly inherits the `[model]` model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Env var name holding the API key for the endpoint. Never the key
    /// itself. Seeds the `PANTHEON_CONSOLIDATION_API_KEY` vault entry.
    /// Only needed for a non-default provider; a default-inheriting
    /// provider inherits `[model].api_key_env`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Per-aux request timeout in seconds (Hermes-style `timeout`).
    /// Absent = the client's built-in default for the capability
    /// (10s title/judge, 15s embeddings, 30s compression, 60s
    /// reflection/consolidation, 120s everything else).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

fn default_consolidation_half_life() -> f64 {
    14.0
}

fn default_consolidation_min_sessions() -> usize {
    3
}

fn default_consolidation_min_score() -> f64 {
    2.0
}

pub fn default_consolidation_cron() -> String {
    "0 3 * * *".to_string()
}

/// `[nightly]`: the unified nightly self-improvement pass.
///
/// One pass replaces the old `pantheon reflect` + `pantheon consolidate`
/// split: a single ledger scan feeds signal proposals (skill/persona/
/// memory lesson) and memory promotion, with eval-gating, replay
/// validation (strict improvement on held-out tasks), human approval for
/// skills/personas, and a JSONL audit trail.
///
/// LLM-backed steps (proposal refinement via the Reflection aux slot,
/// memory distillation via the Consolidation aux slot) are **off by
/// default**: the whole pass is off unless it is enabled, and anything
/// spending model tokens additionally wants a model pin. `enabled` is
/// the master switch ([`nightly_enabled`]): `Some(false)` turns the pass
/// off and always wins, even with a model pinned; `Some(true)` turns it
/// on; `None` (absent) turns it on iff a `[nightly.model]` pin - or the
/// `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env overrides
/// is present, else off. The pipeline and its repair loop do nothing
/// unless this resolves on.
///
/// When the pass is on, its LLM-backed steps resolve through the
/// `[nightly.model]` pin when one is configured (never the chat model
/// directly) - configure a small/cheap model there so background
/// self-improvement never competes with interactive chat. With
/// `enabled = true` and no pin, the steps fall back to the Reflection /
/// Consolidation auxiliary slots.
///
/// Memory promotion rule: a candidate promotes when seen in at least
/// `min_sessions` distinct runs with the newest observation inside
/// `max_age_days`. This replaces the old `[consolidation]`
/// `half_life_days` / `min_score` decay curve - plain frequency +
/// recency, no exponentials.
///
/// Legacy `[reflect]` / `[consolidation]` tables still load; when
/// `[nightly]` is absent, [`nightly_config`] honors them field-by-field
/// as a deprecated migration fallback (`enabled` = either legacy flag,
/// `auto_turns` from `[reflect]`, `min_sessions` from `[consolidation]`,
/// `cron` from `[consolidation]`). When `[nightly]` is present it is the
/// single authoritative section and the legacy tables are ignored
/// entirely for the pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NightlySection {
    /// Master switch for the nightly pass. `None` (absent) is the default:
    /// the pass is enabled only when a model pin is present (a
    /// `[nightly.model]` table with a non-empty provider or model, or
    /// the `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env
    /// overrides) - see [`nightly_enabled`]. `Some(true)` forces the pass
    /// on, `Some(false)` forces it off; an explicit `false` always wins,
    /// even with a model pin present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Model pin for the nightly pass's own LLM steps, following the
    /// [`AuxSection`] pattern (`provider` / `model` / `api_key_env` /
    /// `timeout`):
    ///
    /// ```toml
    /// [nightly.model]
    /// provider = "openai"
    /// model = "gpt-4o-mini"
    /// api_key_env = "OPENAI_API_KEY"   # seeds the PANTHEON_NIGHTLY_API_KEY vault entry
    /// ```
    ///
    /// `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env
    /// overrides win field-wise over the table, like every other aux
    /// slot.
    ///
    /// Presence of the pin = the pass is enabled, unless explicitly
    /// disabled (`enabled = false` always wins). Absent = unpinned: LLM
    /// steps resolve through the Reflection / Consolidation auxiliary
    /// slots like before. `api_key_env` names the env var holding the key
    /// - never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AuxSection>,
    /// Completed turns before an automatic nightly pass runs. Default
    /// 20. `0` disables the automatic trigger.
    #[serde(default = "default_reflect_auto_turns")]
    pub auto_turns: u32,
    /// Distinct runs a memory candidate must appear in before promotion.
    /// Default 3.
    #[serde(default = "default_consolidation_min_sessions")]
    pub min_sessions: usize,
    /// Newest observation must be within this many days for promotion.
    /// Default 30.
    #[serde(default = "default_nightly_max_age_days")]
    pub max_age_days: i64,
    /// Default cron for scheduled nightly passes. Default `0 3 * * *`
    /// (03:00 nightly).
    #[serde(default = "default_consolidation_cron")]
    pub cron: String,
    /// Headless agent command used to replay held-out validation tasks
    /// (`<command> <prompt>`; stdout is the transcript). Only needed for
    /// tasks without their own exec spec - tasks defined with
    /// `replay-tasks add --exec-cmd ...` run on the built-in headless
    /// runner with no configuration. Absent and no exec spec = replays
    /// fail loudly and the replay gate rejects every skill/persona
    /// proposal - strict improvement cannot be measured without a
    /// runner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_command: Option<String>,
    /// Repair-phase bound: consecutive MCP-server health failures
    /// before the server is disabled with escalation. Default 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_mcp_max_failures: Option<u32>,
    /// Repair-phase bound: scheduled-task failures before the task is
    /// disabled with escalation. Default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_schedule_max_failures: Option<u32>,
    /// Repair-phase bound: tools with fewer than this many calls are
    /// skipped by the tool-probe pass. Default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_tool_min_calls: Option<usize>,
    /// Repair-phase bound: tool names exempt from probe escalation
    /// a failing probe on a listed tool escalates instead of disabling.
    /// Default empty (no exemptions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair_tool_probe_allowlist: Option<Vec<String>>,
}

/// Whether a [`NightlySection`] carries a usable model pin: a
/// `[nightly.model]` table with a non-empty provider or model. An empty
/// pin table (`[nightly.model]` with nothing in it) does not count - it
/// is the same as no pin at all. `provider = "default"` counts: the user
/// explicitly pinned the pass to the default model.
pub fn nightly_model_pin_present(section: &NightlySection) -> bool {
    section
        .model
        .as_ref()
        .is_some_and(|m| !m.provider.trim().is_empty() || !m.model.trim().is_empty())
}

/// Whether the `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env
/// overrides name a model. Part of the pin for enablement purposes, the
/// same way env overrides are first-class for every other aux slot.
pub fn nightly_env_pin_present() -> bool {
    std::env::var("PANTHEON_NIGHTLY_PROVIDER").is_ok_and(|v| !v.trim().is_empty())
        || std::env::var("PANTHEON_NIGHTLY_MODEL").is_ok_and(|v| !v.trim().is_empty())
}

/// The single enable rule for the nightly pass. Nightly is **off by
/// default**; exactly one of four enable paths turns it on:
///
/// 1. model pin: `[nightly.model]` present (absent `enabled` flag);
/// 2. `/nightly on` in the TUI (writes `enabled = true`);
/// 3. config edit (`enabled = true` under `[nightly]`);
/// 4. dashboard / mobile-app toggle (`POST /api/nightly/enabled`).
///
/// Resolution: `Some(false)` wins over everything (explicit off, even
/// with a model pin); `Some(true)` forces on (even with no pin
/// callers should warn that no model is pinned); `None` is on iff a
/// model pin is present - the `[nightly.model]` table or the
/// `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env overrides
/// else off.
///
/// This is the master switch: the nightly pipeline and its repair loop
/// do nothing unless this resolves on.
pub fn nightly_enabled(section: &NightlySection) -> bool {
    match section.enabled {
        Some(false) => false,
        Some(true) => true,
        None => nightly_model_pin_present(section) || nightly_env_pin_present(),
    }
}

/// Human-readable reason for a [`nightly_enabled`] verdict: the explicit
/// flag state, or the pin-implied default. Used by `/nightly status`,
/// `pantheon doctor`, and the dashboard so the user sees *why* the loop
/// is on or off.
pub fn nightly_enabled_reason(section: &NightlySection) -> &'static str {
    match section.enabled {
        Some(true) => "explicit flag on",
        Some(false) => "explicit flag off",
        None if nightly_model_pin_present(section) => "on via [nightly.model] pin",
        None if nightly_env_pin_present() => "on via PANTHEON_NIGHTLY_* env",
        None => "off (no flag, no model pin)",
    }
}

impl Default for NightlySection {
    /// Matches the serde field defaults: no explicit flag (off unless a
    /// model pin is present), no pin, and the standard knob defaults.
    fn default() -> Self {
        Self {
            enabled: None,
            model: None,
            auto_turns: DEFAULT_REFLECT_AUTO_TURNS,
            min_sessions: default_consolidation_min_sessions(),
            max_age_days: default_nightly_max_age_days(),
            cron: default_consolidation_cron(),
            replay_command: None,
            repair_mcp_max_failures: None,
            repair_schedule_max_failures: None,
            repair_tool_min_calls: None,
            repair_tool_probe_allowlist: None,
        }
    }
}

fn default_nightly_max_age_days() -> i64 {
    30
}

/// Default event-history retention when `[retention]` is absent.
pub const DEFAULT_RETENTION_DAYS: u32 = 90;

fn default_retention_days() -> u32 {
    DEFAULT_RETENTION_DAYS
}

fn default_true() -> bool {
    true
}

fn default_live_max_session_secs() -> u64 {
    600
}
fn default_live_max_utterance_secs() -> u64 {
    30
}
fn default_live_silence_timeout_ms() -> u64 {
    1200
}

fn default_temporal_min_gap_secs() -> u64 {
    7200
}

/// `[temporal]`: tacit temporal awareness - the model notices when a
/// conversation has meaningfully aged, without timestamping every
/// message. Before a turn's first model call the pipeline measures the
/// idle gap since the last assistant turn (from the durable ledger, so
/// it is restart-safe) and, when the gap matters, appends one coarse,
/// human-friendly hint to the outgoing user message - for the API call
/// only, never persisted, never on the system prompt.
///
/// Zero tokens by construction (pure string injection), so this defaults
/// to on. Absent section = all defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TemporalSection {
    /// Master switch. Default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Idle seconds before an elapsed-gap hint fires. Default 7200 (2h).
    /// `0` disables the elapsed-gap trigger; the date-rollover trigger
    /// still works.
    #[serde(default = "default_temporal_min_gap_secs")]
    pub min_gap_secs: u64,
    /// Hint when the local calendar date rolled over since the last
    /// turn, even on a short gap. Default true.
    #[serde(default = "default_true")]
    pub notify_date_change: bool,
    /// IANA timezone name, e.g. `timezone = "Africa/Lagos"`. Absent =
    /// the system local timezone; unparseable falls back to UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
}

impl From<&TemporalSection> for crate::temporal::TemporalConfig {
    fn from(s: &TemporalSection) -> Self {
        crate::temporal::TemporalConfig {
            enabled: s.enabled,
            min_gap_secs: s.min_gap_secs,
            notify_date_change: s.notify_date_change,
            timezone: s.timezone.clone(),
        }
    }
}

/// `[retention]`: how long the event ledger keeps transcripts.
/// The ledger is append-only and grows forever; the scheduled maintenance
/// pass prunes it. `keep_days = 0` disables pruning (unbounded growth).
/// Finished runs keep their status/title rows and lose only old events;
/// runs that are still open are never pruned, however old their events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionSection {
    /// Days of event history to keep. Default 90. `0` = disabled.
    #[serde(default = "default_retention_days")]
    pub keep_days: u32,
}

/// Run budgets (`[budget]` in config.toml). Every key is optional and
/// every key is overridable per session via `/set` (and `/tokens` for
/// the token cap). A `0` is treated as unset - a zero cap would end
/// every run before it starts, so it falls back to the default instead
/// of silently bricking the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BudgetSection {
    /// Max agent turns per run. Default 16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// Max tool calls per run. Default 32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// Stop the run after this many consecutive failed tool calls.
    /// Default 5. `0` disables the cap. Distinct from `max_tool_calls`:
    /// that bounds total work, this bounds *fruitless* work, so a model
    /// retrying the same broken command cannot burn the whole budget.
    /// Any success resets the streak.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_tool_failures: Option<u32>,
    /// Max delegation depth. Default 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delegate_depth: Option<u32>,
    /// Max pipeline iterations (`pantheon pipeline`). Default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// Per-request OUTPUT cap for chat requests: the most tokens the
    /// model may emit in one response. Precedence is
    /// `/tokens N` > `[budget].max_tokens` > the model's known maximum
    /// output (16k fallback when unknown); the winner is clamped to the
    /// model's known maximum. This never counted input tokens - it is
    /// not a run budget. Absent = uncapped by config; the model/provider
    /// default then applies. Strictly optional: Pantheon never requires it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Default max tokens for a delegated child's generation. A
    /// per-call `budget` on the `delegate` tool overrides it. Unset =
    /// the child uses the model default. The child gets its own separate
    /// token budget: child generation never counts against the parent's
    /// budget. `0` is treated as unset (the model default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate_child_max_tokens: Option<u32>,
    /// Max total `delegate` tool calls per run. Default 8 - the anti
    /// spawn-army cap on total child activity per run. `0` is treated as
    /// unset and falls back to the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delegations: Option<u32>,
}

impl BudgetSection {
    fn nz(value: Option<u32>, default: u32) -> u32 {
        value.filter(|&v| v > 0).unwrap_or(default)
    }

    /// Pipeline iteration cap for `pantheon pipeline`. Default 3.
    pub fn pipeline_iterations(&self) -> u32 {
        Self::nz(self.max_iterations, 3)
    }

    /// Default token budget for a delegated child's generation. `0`
    /// and unset both mean "no config override" (None): the child
    /// falls back to the model default. A per-call `budget` on the
    /// `delegate` tool overrides this default. The budget returned
    /// here is the child's own - it never counts against the parent.
    pub fn delegate_child_budget(&self) -> Option<u32> {
        self.delegate_child_max_tokens.filter(|&v| v > 0)
    }

    /// Max total `delegate` tool calls per run. Default
    /// [`DEFAULT_MAX_DELEGATIONS`]; `0`/unset falls back to the
    /// default rather than banning delegation outright.
    pub fn max_delegations_or_default(&self) -> u32 {
        Self::nz(self.max_delegations, DEFAULT_MAX_DELEGATIONS)
    }
}

/// Default cap on total `delegate` tool calls per run, when
/// `[budget].max_delegations` is unset or `0`. Bounds total child
/// activity per run so a runaway parent cannot spawn an army.
/// Hardcoded floor, configurable upward via the key itself.
pub const DEFAULT_MAX_DELEGATIONS: u32 = 8;

/// Fallback per-request output cap, used only when the session model's
/// maximum output is unknown (no catalog entry and no custom-model
/// override). Named models contribute their own known cap instead
/// see [`resolve_max_output_tokens`].
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_000;

/// Minimum context window Pantheon will run a session model with, in
/// tokens. A model whose known window is smaller is refused at chain
/// build (see [`check_context_window`]) - below this floor the agent
/// loop cannot hold a working transcript plus tool traffic. Hardcoded,
/// not user-configurable: the floor protects the run, not the bill.
pub const MIN_CONTEXT_WINDOW: u32 = 32_000;

/// Resolve the per-request OUTPUT cap for one chat request.
///
/// Precedence: the session override (`/tokens N`) wins over the
/// `[budget].max_tokens` config value, which wins over the model's
/// known maximum output (catalog `ModelMeta.max_output_tokens` or
/// `CustomModel.max_output_tokens`). [`DEFAULT_MAX_OUTPUT_TOKENS`]
/// applies only when the model's maximum output is unknown.
///
/// The winner is then clamped to the model's known maximum output
/// the clamp applies to user overrides too (`/tokens 20000` on a
/// model capped at 8192 resolves to 8192, silently, no error). With
/// no known maximum the winner stands as-is.
pub fn resolve_max_output_tokens(
    session: Option<u32>,
    budget: Option<u32>,
    model_max_output: Option<u32>,
) -> u32 {
    let winner = session
        .or(budget)
        .or(model_max_output)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    match model_max_output {
        Some(cap) => winner.min(cap),
        None => winner,
    }
}

/// Refuse a session model whose known context window is below
/// [`MIN_CONTEXT_WINDOW`]. `None` (unknown window) fails open - the
/// provider is trusted to enforce its own limit. Called at chain
/// build so a too-small model is rejected before any request goes out.
pub fn check_context_window(
    model_name: &str,
    context_limit: Option<u32>,
) -> Result<(), crate::error::PantheonError> {
    match context_limit {
        Some(window) if window < MIN_CONTEXT_WINDOW => Err(crate::error::PantheonError::new(
            "MODEL_CONTEXT_TOO_SMALL",
            crate::error::Layer::Provider,
            false,
            format!(
                "model '{model_name}' has a {window}-token context window, below the \
                 {MIN_CONTEXT_WINDOW}-token minimum Pantheon requires for a session model"
            ),
            "choose a session model with a context window of at least 32k tokens",
            "",
        )),
        _ => Ok(()),
    }
}

/// `/goal` behavior (`[goal]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GoalSection {
    /// Max iterations pursuing one `/goal` before the TUI stops the
    /// session's turns and asks. Default 10. Overridable per session
    /// via `/goal iterations <n>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
}

/// Browser automation (`[browser]` in config.toml). Drives the
/// `browser_*` tools through one of the six browser backends (see
/// `pantheon-web`'s browser lineup; default is gsd-browser). The model
/// uses these to *do things on a live site*; looking things up is
/// `web_search` (`[websearch]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ComputerUseSection {
    /// Driver id. Today only `cua-driver` (trycua/cua, MIT). Unknown ids
    /// warn and fall back to `cua-driver`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// Path to the `cua-driver` binary. Absent = resolved from PATH.
    /// Install: `/bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)"`
    /// (Linux/macOS/Windows; no admin needed). The driver is a background
    /// desktop driver: on Linux it needs X11/XWayland and a running
    /// `cua-driver serve` daemon inside your graphical session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

/// Browser automation (`[browser]` in config.toml). Drives the
/// `browser_*` tools through one of the six browser backends (see
/// `pantheon-web`'s browser lineup; default is gsd-browser). The model
/// uses these to *do things on a live site*; looking things up is
/// `web_search` (`[websearch]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BrowserSection {
    /// Master switch. Default true; when false no `browser_*` tools are
    /// registered at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Backend id: `gsd` (default), `chromiumoxide`, `steel`,
    /// `browserbase`, `lightpanda`, `playwright`, or `camofox`. Unknown
    /// ids warn and fall back to `gsd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Path to the gsd-browser binary. Absent = resolved from PATH.
    /// Install: `npm install -g @opengsd/gsd-browser` (or build
    /// https://github.com/open-gsd/gsd-browser from source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// `browser_act` clicks the top semantic-intent candidate with no
    /// minimum score upstream. Default true = the call carries the
    /// `browser.act` capability and the run parks for human approval.
    /// Set false only if you trust autonomous low-confidence clicks.
    /// Only the `gsd` backend registers `browser_act`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act_require_approval: Option<bool>,
    /// Idle seconds before a run's browser daemon is stopped. Default 900.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    /// Secret name holding the gsd-browser auth-vault key, resolved via
    /// the secrets broker and injected as `GSD_BROWSER_VAULT_KEY`.
    /// Default "GSD_BROWSER_VAULT_KEY". Unset/empty = no vault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_key_secret: Option<String>,
    /// Secret name holding the Steel API key (cloud sessions).
    /// Default "STEEL_API_KEY". Unset/empty = Steel backend unusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steel_api_key_secret: Option<String>,
    /// Steel REST base URL override for self-hosted Steel
    /// (`docker run ghcr.io/steel-dev/steel-browser`). Absent = the
    /// Steel cloud (`https://api.steel.dev`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steel_base_url: Option<String>,
    /// Secret name holding the Browserbase API key.
    /// Default "BROWSERBASE_API_KEY". Unset/empty = Browserbase
    /// backend unusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browserbase_api_key_secret: Option<String>,
    /// Browserbase project id (required by `POST /v1/sessions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browserbase_project_id: Option<String>,
    /// CDP websocket URL of a running Lightpanda server
    /// (`lightpanda serve`). The only Lightpanda transport: binary
    /// launch is not supported (its CLI is not
    /// Chromium-flag-compatible), so run `lightpanda serve` yourself
    /// and point this at it. Unset = Lightpanda backend unusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lightpanda_cdp_url: Option<String>,
    /// Path to the `playwright-cli` binary (`@playwright/cli`).
    /// Absent = resolved from PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playwright_binary: Option<String>,
    /// Path to the Chrome/Chromium binary for the `chromiumoxide`
    /// backend. Absent = chromiumoxide's auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_binary: Option<String>,
    /// Headless mode for the `chromiumoxide` backend. Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    /// Per-command timeout in seconds for browser backends.
    /// Default 120.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Camoufox anti-detect backend (`[browser.camofox]`). Absent =
    /// the launcher defaults (headless, BrowserForge fingerprints).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camofox: Option<CamofoxSection>,
}

/// Camoufox anti-detect browser (`[browser.camofox]` in config.toml).
/// Maps 1:1 onto the launcher options consumed by the camofox backend
/// (see `pantheon-web`'s `CamofoxConfig`); unset properties are
/// auto-filled from BrowserForge fingerprints by the launcher itself,
/// so prefer leaving them unset over inventing inconsistent values.
/// Every key is optional; defaults match the backend's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CamofoxSection {
    /// Python interpreter used to run the JSON-over-stdio shim.
    /// Absent = `python3` on PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// Launch headless. Default true. (`headless = "virtual"` needs
    /// `xvfb`; use `headless_virtual` for that instead.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    /// Xvfb "virtual" headless mode instead of true headless (needs
    /// `xvfb`). Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless_virtual: Option<bool>,
    /// Fingerprint OS: `windows`, `macos`, or `linux`. Absent = the
    /// launcher's BrowserForge default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Human-like cursor movement, in seconds. Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub humanize_secs: Option<f64>,
    /// Derive timezone/locale/geolocation from the proxy IP (needs the
    /// `[geoip]` install extra). Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoip: Option<bool>,
    /// Explicit locale override, e.g. `"en-US"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// Explicit timezone override, e.g. `"America/New_York"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// Proxy server URL, e.g. `"http://proxy:8080"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_server: Option<String>,
    /// Proxy username (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,
    /// Secret name holding the proxy password, resolved via the secrets
    /// broker at registration. Never a raw value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_password_secret: Option<String>,
    /// Block image loads (perf). Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_images: Option<bool>,
    /// Block WebRTC (IP-leak prevention). Default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_webrtc: Option<bool>,
    /// Use BrowserForge-backed real fingerprint presets. Default true
    /// (upstream recommendation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_preset: Option<bool>,
    /// Per-command timeout in seconds. Absent = `[browser] timeout_secs`
    /// (default 120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Seconds to wait for the browser to launch on session start.
    /// First launch after a fetch can take minutes. Default 300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
}

/// Web search (`[websearch]` in config.toml). Drives the `web_search`
/// tool (see `pantheon-web`): query -> snippets. This is for
/// *looking things up*; interacting with a site is `browser_*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WebsearchSection {
    /// Master switch. Default true. The tool is only registered when a
    /// key also resolves - a keyless `web_search` would be a tool that
    /// can never work, so it stays out of the model's tool list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Provider name: one of the `pantheon-web` provider registry ids
    /// (`tinyfish` (recommended), `tavily`, `ollama`, `exa`,
    /// `marginalia`, `brave`, `firecrawl`, `searxng`, `perplexity`).
    /// The setup wizard lists them with their
    /// auth requirements. An unknown id leaves `web_search`
    /// unregistered with a warning at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Secret name holding the provider API key, resolved via the
    /// secrets broker. Default is the chosen provider's own default
    /// (e.g. "TAVILY_API_KEY" for Tavily). Never hardcoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_secret: Option<String>,
    /// Endpoint override, used by self-hosted providers (e.g. a local
    /// SearXNG instance URL). Hosted providers ignore it. Absent = the
    /// provider's default endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Default result count per query. Default 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u8>,
}

/// One toggleable tool group from the setup wizard's Tools screen.
///
/// Session search is deliberately absent: it is default-on and has no
/// toggle, so it can never be switched off here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolGroup {
    WebSearch,
    Browser,
    Terminal,
    Files,
    Memory,
    Skills,
    Tasks,
    Delegation,
    AskUser,
    Vault,
    Voice,
    Vision,
    VideoAnalysis,
    ComputerUse,
    Plugins,
    CodeIntel,
}

impl ToolGroup {
    /// All groups, in Tools-screen order.
    pub fn all() -> [ToolGroup; 16] {
        [
            ToolGroup::WebSearch,
            ToolGroup::Browser,
            ToolGroup::Terminal,
            ToolGroup::Files,
            ToolGroup::Memory,
            ToolGroup::Skills,
            ToolGroup::Tasks,
            ToolGroup::Delegation,
            ToolGroup::AskUser,
            ToolGroup::Vault,
            ToolGroup::Voice,
            ToolGroup::Vision,
            ToolGroup::VideoAnalysis,
            ToolGroup::ComputerUse,
            ToolGroup::Plugins,
            ToolGroup::CodeIntel,
        ]
    }

    /// The `[tools]` config key for the group.
    pub fn key(self) -> &'static str {
        match self {
            ToolGroup::WebSearch => "web_search",
            ToolGroup::Browser => "browser",
            ToolGroup::Terminal => "terminal",
            ToolGroup::Files => "files",
            ToolGroup::Memory => "memory",
            ToolGroup::Skills => "skills",
            ToolGroup::Tasks => "tasks",
            ToolGroup::Delegation => "delegation",
            ToolGroup::AskUser => "ask_user",
            ToolGroup::Vault => "vault",
            ToolGroup::Voice => "voice",
            ToolGroup::Vision => "vision",
            ToolGroup::VideoAnalysis => "video_analysis",
            ToolGroup::ComputerUse => "computer_use",
            ToolGroup::Plugins => "plugins",
            ToolGroup::CodeIntel => "code_intel",
        }
    }

    /// Parse a `[tools]` key back to the group. Unknown keys are `None`
    /// so a typo degrades to "ignored" rather than a hard failure.
    pub fn parse(s: &str) -> Option<ToolGroup> {
        Self::all().into_iter().find(|g| g.key() == s)
    }

    /// Short human label for the Tools screen.
    pub fn label(self) -> &'static str {
        match self {
            ToolGroup::WebSearch => "Web Search",
            ToolGroup::Browser => "Browser",
            ToolGroup::Terminal => "Terminal",
            ToolGroup::Files => "Files",
            ToolGroup::Memory => "Memory",
            ToolGroup::Skills => "Skills",
            ToolGroup::Tasks => "Tasks",
            ToolGroup::Delegation => "Delegation",
            ToolGroup::AskUser => "Ask User",
            ToolGroup::Vault => "Vault",
            ToolGroup::Voice => "Voice (STT+TTS)",
            ToolGroup::Vision => "Vision",
            ToolGroup::VideoAnalysis => "Video Analysis",
            ToolGroup::ComputerUse => "Computer Use",
            ToolGroup::Plugins => "Plugins & MCP",
            ToolGroup::CodeIntel => "Code Intel (LSP + git-undo)",
        }
    }
}

/// Tool-group enablement (`[tools]` in config.toml). Written by the
/// setup wizard's Tools screen; the runtime only registers enabled
/// groups - a disabled group never appears in the model's tool list.
///
/// Every field is `Option<bool>` with absent = enabled, so a config
/// written before this section existed behaves exactly as before (all
/// groups on). Session search has no field here: it is default-on and
/// always registered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolsSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_user: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<bool>,
    /// STT/TTS backends (`[stt]` / `[tts]`). Off = the sections are not
    /// honored: no voice backend is constructed or reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<bool>,
    /// Vision aux slot. Off = the `[vision]` entry never resolves, so a
    /// configured vision model cannot be consulted by accident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// Video-analysis aux slot. Off = the `[video]` entry never resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_analysis: Option<bool>,
    /// Computer-use (CUA driver). The driver is Linux pre-release; the
    /// flag is the durable record and gates its tool registration when
    /// the driver lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use: Option<bool>,
    /// Plugin tools and MCP server projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<bool>,
    /// Code-intel tool group: LSP diagnostics (`lsp.open` /
    /// `lsp.diagnostics` / `lsp.shutdown`) and repo-level git undo
    /// (`gitundo.snapshot` / `gitundo.list` / `gitundo.restore` /
    /// `gitundo.delete`). Off = none of those tools are registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_intel: Option<bool>,
}

impl ToolsSection {
    /// Effective enablement for a group: absent = on.
    pub fn is_enabled(&self, group: ToolGroup) -> bool {
        let flag = match group {
            ToolGroup::WebSearch => self.web_search,
            ToolGroup::Browser => self.browser,
            ToolGroup::Terminal => self.terminal,
            ToolGroup::Files => self.files,
            ToolGroup::Memory => self.memory,
            ToolGroup::Skills => self.skills,
            ToolGroup::Tasks => self.tasks,
            ToolGroup::Delegation => self.delegation,
            ToolGroup::AskUser => self.ask_user,
            ToolGroup::Vault => self.vault,
            ToolGroup::Voice => self.voice,
            ToolGroup::Vision => self.vision,
            ToolGroup::VideoAnalysis => self.video_analysis,
            ToolGroup::ComputerUse => self.computer_use,
            ToolGroup::Plugins => self.plugins,
            ToolGroup::CodeIntel => self.code_intel,
        };
        flag.unwrap_or(true)
    }

    /// Build a section from an enabled-group list (the wizard's answer):
    /// only deviations from the all-on default are written, so a default
    /// setup keeps the config quiet about a choice it did not make.
    pub fn from_enabled(groups: &[ToolGroup]) -> Option<ToolsSection> {
        if groups.len() == ToolGroup::all().len() {
            return None;
        }
        let mut s = ToolsSection::default();
        for g in ToolGroup::all() {
            if !groups.contains(&g) {
                let flag = match g {
                    ToolGroup::WebSearch => &mut s.web_search,
                    ToolGroup::Browser => &mut s.browser,
                    ToolGroup::Terminal => &mut s.terminal,
                    ToolGroup::Files => &mut s.files,
                    ToolGroup::Memory => &mut s.memory,
                    ToolGroup::Skills => &mut s.skills,
                    ToolGroup::Tasks => &mut s.tasks,
                    ToolGroup::Delegation => &mut s.delegation,
                    ToolGroup::AskUser => &mut s.ask_user,
                    ToolGroup::Vault => &mut s.vault,
                    ToolGroup::Voice => &mut s.voice,
                    ToolGroup::Vision => &mut s.vision,
                    ToolGroup::VideoAnalysis => &mut s.video_analysis,
                    ToolGroup::ComputerUse => &mut s.computer_use,
                    ToolGroup::Plugins => &mut s.plugins,
                    ToolGroup::CodeIntel => &mut s.code_intel,
                };
                *flag = Some(false);
            }
        }
        Some(s)
    }
}

/// Third-party packages the skill library needs (`[skill_deps]` in
/// config.toml). These are install-time dependencies of Pantheon itself:
/// the setup wizard's "Skill dependencies" screen detects each one and
/// offers install-or-skip, instead of every skill carrying its own
/// manual `pip install` step.
///
/// Only the skips are recorded here. A present dependency writes
/// nothing; a skipped one lands in `skipped` so `pantheon doctor` can
/// report the gap later - the same "the config records the choice;
/// doctor reports the gap" contract the setup wizard uses for skipped
/// local provider binaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SkillDepsSection {
    /// Stable dependency ids the user skipped at install-or-skip time
    /// (see the skill-deps registry in `pantheon-tui`). Sorted when
    /// written so the file stays diff-stable.
    #[serde(default)]
    pub skipped: Vec<String>,
}

/// Cloudflare integration (`[cloudflare]` in config.toml).
///
/// Gates the two things a Cloudflare integration needs: whether the
/// resolved `CLOUDFLARE_API_TOKEN` may be injected into `cf` child
/// processes at all (env injection is opt-in, never ambient), and which
/// secret name holds the token. Absent section = the integration is off:
/// `cf` calls still classify and still need approval, but the shell child
/// runs tokenless and every authenticated call fails with cf's own auth
/// error, which the agent can surface instead of acting on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CloudflareSection {
    /// Master switch. `false` = no token injection, no doctor pass, no
    /// setup writes. Default false: a config that merely mentions the
    /// section does not silently enable child-env credentials.
    #[serde(default)]
    pub enabled: bool,
    /// Secret name holding the API token, resolved via the secrets
    /// broker (env, then keyring, then encrypted file). Default
    /// `CLOUDFLARE_API_TOKEN`. The broker never logs the value; the
    /// child env carries it and nothing else about Cloudflare does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token_secret: Option<String>,
    /// Account id pinned for single-account setups (`cf -z`-style
    /// disambiguation is left to the agent; this pin only feeds
    /// doctor's account check). Optional.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// TUI chrome (`[tui]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TuiSection {
    /// Active theme name (`pantheon`, `dark`, `light`). Absent = default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// Modal vim editing for the composer (`/vim`). Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim: Option<bool>,
}

impl Default for RetentionSection {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

// ANCHOR(mcp-workstream): `[mcp]` / `[mcp.servers.<name>]` config section.
// The config file is the primary MCP server source; migration declaration
// files (`<data_dir>/mcp/*.json`) fill in names the config does not define.

/// MCP servers (`[mcp]` in config.toml). The runtime manager
/// (`pantheon-mcp`) spawns/connects the enabled servers and projects
/// their tools into the `ToolRegistry` as `mcp_<server>_<tool>`.
///
/// ## Enablement: which gate wins
///
/// A server launches iff it is **enabled** AND its identity is trusted:
///
/// - **Bundled** catalog servers are first-party: the `enabled` flag is
///   the only gate. Fresh installs enable zero of them - enabling is
///   always explicit (this file, the dashboard's Tools & MCPs page, the
///   mobile app, or the agent proposing through the approval flow).
/// - **Custom** servers additionally need operator consent recorded in
///   the unified approval store (`<data_dir>/mcp/.approvals.json`,
///   [`crate::approval`]): an unapproved third-party server never
///   connects no matter what the flag says.
///
/// Disabling flips the switch only - a recorded approval persists, so
/// re-enabling the same binary/URL resumes without re-consent. The
/// approval binds the server's content hash: if the binary or URL
/// changes, the approval lapses and consent is asked again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct McpSection {
    /// Master switch. Default true; when false no MCP server is
    /// launched at all, regardless of per-server flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// One table per server: `[mcp.servers.<name>]`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub servers: std::collections::HashMap<String, McpServerEntry>,
}

/// One MCP server (`[mcp.servers.<name>]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// Transport: `stdio`, `sse`, or `http` (streamable HTTP). Default
    /// `stdio`.
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    /// Program to spawn (stdio only). Resolved on PATH or as a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Arguments for the program (stdio only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Extra environment for the server process (stdio only). A value
    /// starting with `env:` resolves from the operator's environment at
    /// spawn time (e.g. `GITHUB_TOKEN = "env:GITHUB_TOKEN"` reads
    /// `$GITHUB_TOKEN`); any other value is a literal. Never log these
    /// values: the manager only ever prints variable *names*.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    /// Endpoint URL (sse/http only): the SSE stream URL, or the
    /// streamable-HTTP endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Per-server switch. Default false: a fresh install enables zero
    /// servers, and enabling is always explicit (config file, dashboard
    /// Tools & MCPs page, mobile app, or the agent proposing through the
    /// approval flow). See [`McpSection`] for how this flag interacts
    /// with the approval store.
    ///
    /// The default is `false` for **every** server, bundled or custom:
    /// serde defaults cannot see the table's key name, so a bundled-vs-
    /// custom two-tier default is not expressible at parse time - and a
    /// uniform fail-closed default is what Umar's explicit-enablement
    /// rule wants anyway. Every enablement path (the bundled catalog's
    /// `set_bundled_enabled`, the dashboard/app writes, the agent's
    /// `enable_mcp` tool) materializes `enabled` explicitly, so this
    /// default only governs hand-written or legacy tables.
    ///
    /// Upgrade note: tables written before this default existed omit
    /// `enabled` and previously parsed as *enabled*; they now parse as
    /// *disabled*. That is deliberate and fail-safe - re-enable once,
    /// explicitly. No automatic migration rewrites user configs to
    /// `enabled = true`: silently switching on process-spawning
    /// integrations the operator never explicitly approved would defeat
    /// the explicit-enablement rule this default exists to enforce.
    #[serde(default = "mcp_server_enabled_default")]
    pub enabled: bool,
    /// Per-request timeout in seconds. Absent = 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Extra HTTP headers for sse/http transports, sent with every
    /// request to the server. Values starting with `env:` resolve from
    /// the operator's environment at connect time (same convention as
    /// `env`); any other value is a literal. This is how a remote MCP
    /// endpoint that expects `Authorization: Bearer <token>` gets it.
    /// Header names are logged, values never are.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub headers: std::collections::HashMap<String, String>,
}

fn default_mcp_transport() -> String {
    "stdio".to_string()
}

fn mcp_server_enabled_default() -> bool {
    false
}

impl McpServerEntry {
    /// Config problems for one server table, as `validate()` strings.
    pub fn problems(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        match self.transport.as_str() {
            "stdio" => {
                if self
                    .command
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    out.push(format!(
                        "mcp.servers.{name}: transport is stdio but no command is set"
                    ));
                }
                if self.url.is_some() {
                    out.push(format!(
                        "mcp.servers.{name}: url is ignored for stdio transport"
                    ));
                }
            }
            "sse" | "http" => {
                if self.url.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    out.push(format!(
                        "mcp.servers.{name}: transport is {} but no url is set",
                        self.transport
                    ));
                }
                if self.command.is_some() {
                    out.push(format!(
                        "mcp.servers.{name}: command is ignored for {} transport",
                        self.transport
                    ));
                }
            }
            other => out.push(format!(
                "mcp.servers.{name}: unknown transport {other:?} (stdio|sse|http)"
            )),
        }
        if self.timeout_secs == Some(0) {
            out.push(format!(
                "mcp.servers.{name}: timeout_secs is 0: requests would instantly time out"
            ));
        }
        out
    }
}

/// One bundled plugin's enablement (`[plugins.<name>]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginEntry {
    /// Master switch for this plugin. When the entry exists, this beats
    /// the plugin manifest's own `enabled` flag - the config file is the
    /// single enablement state (config file, dashboard, mobile app, or
    /// the agent proposing through the approval flow). When the entry is
    /// absent, the bundled manifest's `enabled` flag is the default
    /// (false for every bundled plugin except noisegate).
    #[serde(default)]
    pub enabled: bool,
    /// Catalog kind (`tool` / `hook`), stamped when the entry is written
    /// through a Pantheon surface so the table is self-describing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Catalog version the entry was written for. Informational only;
    /// a catalog version bump does not lapse the enablement the way a
    /// third-party code change lapses an approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// `[mcp_synthesis]`: the model that bounds large MCP tool results into
/// a short note before they enter context (compression's pattern,
/// scoped to MCP results). Absent = `auto`: the run's default model
/// summarizes.
pub type McpSynthesisSection = AuxSection;

/// `[extraction]`: the structured-extraction model. Pulls fields and
/// records out of prose and tool outputs into typed values the runtime
/// can act on. Absent = `auto`: the run's default model extracts. No
/// call sites yet - pin a model here ahead of the extraction workload.
pub type ExtractionSection = AuxSection;

/// `[rerank]`: the rerank model. Scores and orders search and
/// memory-retrieval candidates before they enter context. Absent =
/// `auto`: the run's default model reranks. No call sites yet - pin a
/// model here ahead of the reranking workload.
pub type RerankSection = AuxSection;

/// `[planner]`: the planner model for a future planner/worker split,
/// where planning and execution run on different models. Absent =
/// `auto`: the run's default model plans. No call sites yet - pin a
/// model here ahead of the planner workload.
pub type PlannerSection = AuxSection;

/// `[repair]`: the repair model for the nightly fix loop - the only
/// slot that revises drafts (eval-reject and replay-reject paths)
/// and for diagnosis/repair of broken MCP servers, scheduled tasks,
/// and tools. Absent = OFF: the slot adds no policy entry and fix-loop
/// draft revision is unavailable (the loop falls back to plain retries,
/// then escalation).
///
/// `max_repairs_per_task_per_day` caps how many repair investigations
/// one scheduled task may trigger per UTC day (default 3): without it a
/// flapping task burns API budget on investigator sessions without
/// bound. 0 disables repair investigations entirely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RepairSection {
    #[serde(flatten)]
    pub aux: AuxSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repairs_per_task_per_day: Option<u32>,
}

impl RepairSection {
    /// Effective per-task daily repair cap: the configured value, else 3.
    pub fn repair_cap_per_day(&self) -> u32 {
        self.max_repairs_per_task_per_day.unwrap_or(3)
    }
}

impl From<AuxSection> for RepairSection {
    fn from(aux: AuxSection) -> Self {
        Self {
            aux,
            max_repairs_per_task_per_day: None,
        }
    }
}

/// `RepairSection` behaves as its flattened aux slot wherever an
/// `&AuxSection` is expected (model resolution, the TUI pin lists).
impl std::ops::Deref for RepairSection {
    type Target = AuxSection;
    fn deref(&self) -> &AuxSection {
        &self.aux
    }
}

impl std::ops::DerefMut for RepairSection {
    fn deref_mut(&mut self) -> &mut AuxSection {
        &mut self.aux
    }
}

/// `[verify]`: the adversarial verifier model. After a delegated
/// sub-agent completes, it re-reads the task goal plus the child's
/// claimed result and tries to falsify the claim. Absent = OFF: no
/// entry in the policy and no verification runs. Pin a cheap model
/// here to turn post-delegation verification on.
pub type VerifySection = AuxSection;

/// Default per-agent spawn cap for `[swarm]`: one agent may have this
/// many live children before further spawns are refused.
fn default_swarm_max_subagents() -> u32 {
    4
}
/// Default delegation depth for `[swarm]`: the primary agent is depth
/// 0, its children depth 1, and so on; deeper spawns are refused.
fn default_swarm_max_depth() -> u32 {
    2
}
/// Default live-agent ceiling for `[swarm]`.
fn default_swarm_max_concurrent() -> u32 {
    4
}

/// `[swarm]`: caps for sub-agent delegation and swarm runs. Every key
/// is optional; absent keys (or an absent section) take the documented
/// defaults. These are the config-side spelling of
/// `pantheon_runtime::swarm::Caps` - see `Caps::from_swarm_section`
/// for the mapping (`max_subagents` becomes the per-agent spawn cap).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmSection {
    /// Per-agent spawn cap: how many sub-agents one agent may spawn
    /// before further spawns are refused. Default 4.
    #[serde(default = "default_swarm_max_subagents")]
    pub max_subagents: u32,
    /// Maximum delegation depth (primary = 0). Default 2.
    #[serde(default = "default_swarm_max_depth")]
    pub max_depth: u32,
    /// Maximum live sub-agents across the swarm. Default 4.
    #[serde(default = "default_swarm_max_concurrent")]
    pub max_concurrent: u32,
    /// When false, a child agent (depth >= 1) that attempts to delegate
    /// gets a structured refusal instead of a grandchild. Default true.
    #[serde(default = "default_true")]
    pub allow_child_spawn: bool,
}

impl Default for SwarmSection {
    fn default() -> Self {
        Self {
            max_subagents: default_swarm_max_subagents(),
            max_depth: default_swarm_max_depth(),
            max_concurrent: default_swarm_max_concurrent(),
            allow_child_spawn: true,
        }
    }
}

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

/// `[secrets]`: the run's secrets-boundary policy. Both lists are empty by
/// default (fail closed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SecretsSection {
    /// Env vars readable through the `env:` secret-name form. Entries are
    /// exact var names (`"MY_KEY"`) or `PREFIX_*` wildcards
    /// (`"PANTHEON_*"`); `"*"` alone allows all (explicit opt-out).
    ///
    /// Default: empty - `env:` lookups resolve nothing. Secrets must come
    /// from `PANTHEON_SECRET_*` or a durable vault, so a name like
    /// `env:AWS_SECRET_ACCESS_KEY` can never be used to exfiltrate an
    /// arbitrary host variable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_allowlist: Vec<String>,
    /// Manifest-declared env vars the plugin supervisor may copy from the
    /// host into plugin subprocesses (same entry syntax as above).
    ///
    /// Default: empty - plugins receive PATH plus Pantheon-set vars only.
    /// A project-controlled manifest can declare any name it likes, so a
    /// declared name alone never crosses the boundary; only an entry here
    /// lets a host var (including API keys) reach plugin code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugin_env_allowlist: Vec<String>,
}

/// The whole config file. Everything optional-tolerant so doctor can
/// describe exactly what is missing instead of failing to parse.
/// `[agents.<name>]`: a durable identity for one persistent agent (§4).
///
/// The audit's gap: Hermes ships this as `profile.yaml` + `SOUL.md` +
/// `MEMORY.md`/`USER.md` (the 6 `_PROFILE_IDENTITY_MARKERS` files), while
/// Pantheon had no identity config at all - every run was anonymous.
/// This is the durable half: name, persona source files, memory namespace,
/// and capability policy live in config; the prompt assembly that reads
/// them is next. Persona files are referenced by path (repo-relative or
/// absolute), never inlined, so secrets that drift into a SOUL.md stay out
/// of config snapshots.
///
/// This is a **re-export of the core type**, not a second definition. The
/// declaration, inheritance, and namespace rules live in
/// `pantheon_api::agent_profile`: profiles are config-document types,
/// so they live at the API leaf where the runtime can consume them
/// without depending on any client. An earlier version of this file declared
/// its own struct with four of the fields; it drifted as soon as
/// `inherits` and `model` were added, which is exactly the duplication
/// this re-export removes.
pub use crate::agent_profile::AgentProfile as AgentIdentity;

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
    /// The agent this install runs as (`agent = "zeus"`). Must name a
    /// declared `[agents.<name>]` table. This is the key that selects an
    /// agent - a legacy free-form `profile` label key was removed; old
    /// configs that still carry it load fine, the key is ignored.
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
    /// `[video]`: the video-understanding model pin. Absent = `auto`:
    /// the run's default model. No call sites yet; pin ahead of the
    /// video pipeline landing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<ScheduledSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_synthesis: Option<McpSynthesisSection>,
    /// `[extraction]`: structured-extraction model pin. Absent = `auto`.
    /// No call sites yet; pin ahead of the workload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction: Option<ExtractionSection>,
    /// `[rerank]`: rerank model pin. Absent = `auto`. No call sites yet;
    /// pin ahead of the workload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank: Option<RerankSection>,
    /// `[planner]`: planner model pin for a future planner/worker split.
    /// Absent = `auto`. No call sites yet; pin ahead of the workload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<PlannerSection>,
    /// `[repair]`: repair model pin. Absent = OFF: the slot adds no
    /// policy entry and fix-loop draft revision is unavailable - the
    /// loop falls back to plain retries, then escalation. The nightly
    /// fix loop's draft revision (eval-reject and replay-reject paths)
    /// resolves only through this slot - never the Reflection slot.
    /// Broken-MCP/schedule/tool diagnosis (workstream 3) will resolve
    /// it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairSection>,
    /// `[verify]`: adversarial verifier model pin. Absent = OFF: the
    /// slot adds no policy entry and post-delegation verification never
    /// runs. Pin a cheap model here to turn it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifySection>,
    /// `[swarm]`: sub-agent delegation caps. Absent = all defaults
    /// ([`SwarmSection::default`]). Partial tables are fine: each key
    /// defaults independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm: Option<SwarmSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_gen: Option<TitleGenSection>,
    /// `[reflect]`: behavior knobs + auxiliary model pin for Reflection.
    /// DEPRECATED for the behavior knobs (moved to `[nightly]`, which is
    /// authoritative); still honored field-by-field when `[nightly]` is
    /// absent. The model pin (`provider`/`model`/`timeout`) stays here.
    /// Absent = reflection LLM steps off (`enabled = false`), and if
    /// enabled later, `auto` = the run's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflect: Option<ReflectSection>,
    /// `[consolidation]`: behavior knobs + auxiliary model pin for
    /// Consolidation. DEPRECATED for the behavior knobs (moved to
    /// `[nightly]`, which is authoritative); still honored field-by-field
    /// when `[nightly]` is absent. The model pin
    /// (`provider`/`model`/`timeout`) stays here. Absent = consolidation
    /// LLM steps off (`enabled = false`), and if enabled later, `auto` =
    /// the run's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidation: Option<ConsolidationSection>,
    /// `[nightly]`: the unified nightly self-improvement pass
    /// (`pantheon nightly`). The single authoritative section - it merges
    /// the old `[reflect]` and `[consolidation]` behavior knobs. When
    /// absent, the legacy sections are honored field-by-field as a
    /// deprecated migration fallback (see [`nightly_config`]); when
    /// present, the legacy sections are ignored entirely for the pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nightly: Option<NightlySection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt: Option<VoiceSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts: Option<VoiceSection>,
    /// `[voice]`: live voice-mode knobs. Absent = live mode disabled with
    /// default limits (see [`LiveVoiceSection`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<LiveVoiceSection>,
    pub policy: Option<PolicyPreset>,
    /// `permission_mode`: how much scrutiny an approval-gated tool call
    /// gets (`ask` / `smart` / `allow_all`). Absent = `ask`, the
    /// conservative default: a run parks exactly where the deterministic
    /// policy says. `smart` consults the `[judge]` auxiliary; `allow_all`
    /// clears approval parks but never overrides a policy `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    pub memory: Option<MemorySection>,
    pub server: Option<ServerSection>,
    /// Secrets-boundary policy (`env:` lookups, plugin subprocess env).
    /// Absent = both allowlists empty (fail closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretsSection>,
    /// Event-ledger retention (`[retention]`). Absent = 90-day default;
    /// `keep_days = 0` disables pruning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionSection>,
    /// Tacit temporal awareness (`[temporal]`). Absent = enabled with
    /// defaults (2h gap, date-rollover hints, system local timezone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal: Option<TemporalSection>,
    /// Run budgets (`[budget]`). Absent = runtime defaults (16 turns,
    /// 32 tool calls, depth 2, uncapped tokens, 3 pipeline iterations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetSection>,
    /// `/goal` behavior (`[goal]`). Absent = 10-iteration default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalSection>,
    /// Browser automation (`[browser]`). Absent = enabled with defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserSection>,
    /// `[gateway]`: gateway channel options (`[gateway.channels.<name>]`).
    /// Absent = defaults (text replies everywhere, no per-channel voice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewaySection>,
    /// Desktop control through the CUA driver (`[computer_use]`).
    /// Absent = the ComputerUse tool group toggle decides; the driver is
    /// used when its binary is on PATH. Set `driver` to pick the driver
    /// explicitly (today only `cua-driver`), `binary` to pin its path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use: Option<ComputerUseSection>,
    /// Web search (`[websearch]`). Absent = enabled with defaults; the
    /// tool only registers when an API key resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websearch: Option<WebsearchSection>,
    /// Tool-group enablement (`[tools]`). Absent = every group enabled;
    /// the runtime only registers enabled groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsSection>,
    /// Third-party skill dependencies (`[skill_deps]`). Absent = the
    /// wizard's skill-deps screen never ran; present with an empty
    /// `skipped` list = everything it checked was found or installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_deps: Option<SkillDepsSection>,
    /// Cloudflare integration (`[cloudflare]`). Absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloudflare: Option<CloudflareSection>,
    /// MCP servers (`[mcp]` / `[mcp.servers.<name>]`). Absent = the
    /// launcher is off. ANCHOR(mcp-workstream): config half of MCP wiring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpSection>,
    /// Bundled plugin enablement (`[plugins]` / `[plugins.<name>]`).
    /// This is the single source of truth for bundled-plugin enablement,
    /// shared by the config file, the dashboard, the mobile app, and the
    /// agent's `enable_plugin` tool. A bundled plugin not named here (or
    /// named without `enabled`) is disabled: bundled plugins ship off.
    /// Third-party plugins are unaffected - their gate is the approval
    /// store (`pantheon_api::approval`), not this table. Entries under
    /// names the bundled catalog does not know are inert.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub plugins: std::collections::HashMap<String, PluginEntry>,
    /// TUI chrome (`[tui]`). Absent = defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<TuiSection>,
    /// User-defined providers (`pantheon model` → Custom provider).
    /// Empty for configs written before this existed (back-compat).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub custom_providers: std::collections::HashMap<String, CustomProviderSection>,
    /// Durable per-agent identities (§4). Empty = all runs anonymous, the
    /// pre-identity behavior (back-compat).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub agents: std::collections::HashMap<String, AgentIdentity>,
}

/// Process-lifetime agent-profile override, set once from CLI flags
/// (`pantheon --profile <name>` / `-p` / `--agent`) before any session
/// exists. It slots into [`Config::resolve_profile`] between an explicit
/// call-site override and the `agent = "..."` config value, so the TUI
/// launch path - which resolves with `None` - picks it up without any
/// session-construction code needing a new parameter. Set-once: the
/// second call wins, and it is never read before the CLI has parsed.
static PROFILE_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

/// Set the process-lifetime profile override (the `--profile` flag).
pub fn set_profile_override(name: Option<String>) {
    let slot = PROFILE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = name;
}

/// The current process-lifetime profile override, if the CLI set one.
pub fn profile_override() -> Option<String> {
    PROFILE_OVERRIDE
        .get()
        .and_then(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// Top-level config keys the loader understands.
///
/// Load policy for unknown keys: WARN on stderr, non-fatal. A typo'd
/// section must not nuke a working install (so this is a warning, not
/// an error), but it must not pass silently either - `doctor` used to
/// bless configs whose real sections were misspelled. `pantheon config
/// set` keeps its loud rejection; this is the load path's quieter twin,
/// applied uniformly in [`Config::load`].
///
/// Only top-level keys are checked: tables like `[custom_providers.*]`
/// and `[agents.*]` take arbitrary names by design, and per-section
/// key lists would rot. A typo *inside* a section (e.g.
/// `model.providr`) is still silent - the section structs would need
/// `deny_unknown_fields` for that, which is a separate change.
///
/// Kept in sync with the `Config` struct by
/// `known_config_keys_match_struct_fields` below: the test builds an
/// exhaustive `Config` literal (no `..Default::default()`), so adding a
/// field breaks compilation until this list grows with it.
const KNOWN_CONFIG_KEYS: &[&str] = &[
    "agent",
    "model",
    "judge",
    "embeddings",
    "search_synthesis",
    "vision",
    "video",
    "scheduled",
    "mcp_synthesis",
    "extraction",
    "rerank",
    "planner",
    "repair",
    "verify",
    "swarm",
    "compression",
    "title_gen",
    "reflect",
    "consolidation",
    "nightly",
    "stt",
    "tts",
    "voice",
    "policy",
    "memory",
    "server",
    "secrets",
    "retention",
    "temporal",
    "budget",
    "goal",
    "browser",
    "gateway",
    "computer_use",
    "websearch",
    "tools",
    "skill_deps",
    "cloudflare",
    "mcp",
    "plugins",
    "tui",
    "custom_providers",
    "agents",
];

/// Unknown top-level keys in a config document, sorted. Pure so the
/// load policy is unit-testable without touching the filesystem.
pub fn unknown_config_keys(text: &str) -> Vec<String> {
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let mut unknown: Vec<String> = table
        .keys()
        .filter(|k| !KNOWN_CONFIG_KEYS.contains(&k.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    unknown
}

impl Config {
    pub fn path(data_dir: &Path) -> std::path::PathBuf {
        data_dir.join("config.toml")
    }
    /// Effective event-retention window in days. Absent `[retention]` =
    /// the 90-day default; `keep_days = 0` disables pruning.
    pub fn retention_days(&self) -> u32 {
        self.retention
            .as_ref()
            .map(|r| r.keep_days)
            .unwrap_or(DEFAULT_RETENTION_DAYS)
    }
    /// Max `/goal` iterations. Absent `[goal]` = 10.
    pub fn goal_iterations(&self) -> u32 {
        self.goal
            .as_ref()
            .and_then(|g| g.max_iterations)
            .filter(|&v| v > 0)
            .unwrap_or(10)
    }
    /// Effective live voice-mode settings. Absent `[voice]` = defaults
    /// (live mode disabled; see [`LiveVoiceSection`]).
    pub fn live_voice(&self) -> LiveVoiceSection {
        self.voice.clone().unwrap_or_default()
    }
    /// Effective swarm caps. Absent `[swarm]` = [`SwarmSection::default`];
    /// a partial table fills the missing keys with defaults.
    pub fn swarm_section(&self) -> SwarmSection {
        self.swarm.clone().unwrap_or_default()
    }
    /// Max pipeline iterations (`pantheon pipeline`). Absent `[budget]` = 3.
    pub fn pipeline_iterations(&self) -> u32 {
        self.budget
            .as_ref()
            .map(BudgetSection::pipeline_iterations)
            .unwrap_or(3)
    }
    /// Is the bundled plugin `name` enabled? Absent `[plugins.<name>]`
    /// (or an entry without `enabled`) = disabled: bundled plugins are
    /// all off by default. Unknown names are disabled too - entries for
    /// names outside the bundled catalog are inert.
    pub fn plugin_enabled(&self, name: &str) -> bool {
        self.plugins.get(name).map(|e| e.enabled).unwrap_or(false)
    }
    /// Is the MCP server `name` enabled? Absent `[mcp.servers.<name>]`
    /// (or an entry without `enabled`) = disabled: bundled servers are
    /// all off by default. Unknown names are disabled too - entries for
    /// names outside the bundled catalog are inert (custom servers are
    /// still read from their declaration files by the launcher; this
    /// helper only answers the config-section question).
    pub fn mcp_server_enabled(&self, name: &str) -> bool {
        self.mcp
            .as_ref()
            .and_then(|m| m.servers.get(name))
            .map(|e| e.enabled)
            .unwrap_or(false)
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
        toml::from_str(&text)
            .map_err(|e| {
                PantheonError::new(
                    "CONFIG_PARSE",
                    Layer::Runtime,
                    false,
                    format!("parse {}: {e}", path.display()),
                    "fix the TOML or rerun setup",
                    "",
                )
            })
            // Unknown keys warn on stderr, non-fatal (see
            // KNOWN_CONFIG_KEYS): a typo'd section must be visible, never
            // silently dropped, and never a reason to refuse a load.
            .inspect(|_| {
                let unknown = unknown_config_keys(&text);
                if !unknown.is_empty() {
                    eprintln!(
                        "pantheon: warning: {} has unknown key(s): {} - ignored; check for typos",
                        path.display(),
                        unknown.join(", "),
                    );
                }
            })
    }
    /// The agent profiles declared in this config, as a resolvable registry.
    ///
    /// This is the bridge from config text to the runtime's profile layer.
    /// Building it here (rather than at each call site) means every entry
    /// point - terminal, scheduler, AG-UI - sees the same declarations, and
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
    /// fallback - an operator who asked for `zeus` and silently got the
    /// default agent's memory and persona would have no way to notice.
    ///
    /// A config with no `[agents]` table at all is not an error: that is
    /// every install from before profiles, and those runs stay anonymous
    /// until the user declares one. Only a *named* agent must exist.
    pub fn resolve_profile(
        &self,
        override_name: Option<&str>,
    ) -> Result<Option<EffectiveProfile>, ProfileError> {
        // Selection order: explicit call-site override, else the CLI
        // `--profile` flag, else `agent = "..."`, else `default`.
        let cli_override = profile_override();
        let selected = override_name
            .or(cli_override.as_deref())
            .or(self.agent.as_deref())
            .unwrap_or(crate::agent_profile::DEFAULT_PROFILE);
        if self.agents.is_empty()
            && override_name.is_none()
            && cli_override.is_none()
            && self.agent.is_none()
        {
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
                if crate::model::ReasoningLevel::parse(r).is_none() {
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
        // Every aux section validates identically: a resolvable
        // api_key_env when named, and a non-zero timeout. Empty
        // provider/model is valid - it inherits the `[model]` target
        // (Hermes-style), so there is no "empty field" error anymore.
        fn aux_problem(
            name: &str,
            api_key_env: &Option<String>,
            timeout: &Option<u64>,
            problems: &mut Vec<String>,
        ) {
            if let Some(env) = api_key_env {
                let r = SecretRef::from_env(env.clone());
                if let Err(e) = r.validate() {
                    problems.push(format!("{name}.api_key_env: {e}"));
                } else if r.resolve().is_none() {
                    problems.push(format!("env var {env} is not set"));
                }
            }
            if *timeout == Some(0) {
                problems.push(format!(
                    "{name}.timeout is 0: aux requests would instantly time out"
                ));
            }
        }
        for slot in AUX_SLOTS {
            if let Some(s) = cfg_section(self, slot) {
                aux_problem(slot.name, &s.api_key_env, &s.timeout, &mut problems);
            }
        }
        // `[reflect]` carries its own model pin; each field inherits
        // independently now, so a half-set pin is valid (set provider +
        // inherited model, or vice versa).
        if let Some(r) = &self.reflect {
            aux_problem("reflect", &r.api_key_env, &r.timeout, &mut problems);
        }
        // `[consolidation]` likewise: each pin field inherits independently.
        if let Some(c) = &self.consolidation {
            aux_problem("consolidation", &c.api_key_env, &c.timeout, &mut problems);
        }
        if let Some(mem) = &self.memory {
            if mem.backend.trim().is_empty() {
                problems.push("memory.backend is empty".into());
            }
        }
        for (section, v) in [("stt", &self.stt), ("tts", &self.tts)] {
            if let Some(v) = v {
                // The accepted backends are the ones the runtime can
                // construct (`open_stt` / `open_tts` in pantheon-providers),
                // mirrored in STT_BACKENDS / TTS_BACKENDS. The old
                // command|openai-only check rejected configs the wizard
                // itself writes (groq, piper-local, ...), which sent
                // `doctor` into a "rerun setup" loop that rewrote identical
                // values.
                let known = if section == "stt" {
                    STT_BACKENDS
                } else {
                    TTS_BACKENDS
                };
                if v.backend.trim().is_empty() {
                    problems.push(format!("{section}.backend is empty"));
                } else if !known.contains(&v.backend.as_str()) {
                    problems.push(format!(
                        "{section}.backend {:?} is unknown ({}); check [{section}].backend in config.toml",
                        v.backend,
                        known.join("|")
                    ));
                }
                // Per-backend required options, mirroring the runtime's
                // from_options constructors: the shell-out backends need
                // their command, piper-local needs its voice model id, and
                // the legacy "openai" shape needs the provider it aliases.
                // (kokoro-local / fishspeech-local auto-detect their CLI,
                // so `cmd` stays optional for them.)
                let opt = |k: &str| v.options.get(k).map(|s| s.trim()).unwrap_or("");
                let required = match (section, v.backend.as_str()) {
                    ("stt", "command") | ("tts", "command") => Some("cmd"),
                    ("stt", "openai") | ("tts", "openai") => Some("provider"),
                    ("tts", "piper-local") => Some("voice"),
                    _ => None,
                };
                if let Some(k) = required {
                    if opt(k).is_empty() {
                        problems.push(format!(
                            "{section}.options.{k} is required for the {:?} backend",
                            v.backend
                        ));
                    }
                }
                if let Some(t) = v.options.get("timeout_secs") {
                    if t.trim().parse::<u64>().ok().filter(|&n| n > 0).is_none() {
                        problems.push(format!(
                            "{section}.options.timeout_secs {t:?} is not a positive integer"
                        ));
                    }
                }
                if let Some(env) = v.options.get("api_key_env") {
                    if env.trim().is_empty() {
                        problems.push(format!("{section}.options.api_key_env is empty"));
                    }
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
        if let Some(mcp) = &self.mcp {
            let mut names: Vec<&String> = mcp.servers.keys().collect();
            names.sort();
            for name in names {
                if let Some(entry) = mcp.servers.get(name) {
                    problems.extend(entry.problems(name));
                }
            }
        }
        // `[custom_providers.*]`: the runtime maps api_mode with a
        // lowercased match whose fallthrough is OpenAI, so a typo like
        // "anthroic" silently runs the wrong wire protocol. Reject
        // anything that is not openai|anthropic here instead.
        {
            let mut names: Vec<&String> = self.custom_providers.keys().collect();
            names.sort();
            for name in names {
                if let Some(sec) = self.custom_providers.get(name) {
                    match sec.api_mode.trim().to_ascii_lowercase().as_str() {
                        "openai" | "anthropic" => {}
                        other => problems.push(format!(
                            "custom_providers.{name}.api_mode {other:?} is not openai|anthropic"
                        )),
                    }
                }
            }
        }
        if let Some(gw) = &self.gateway {
            let mut names: Vec<&String> = gw.channels.keys().collect();
            names.sort();
            for name in names {
                if let Some(ch) = gw.channels.get(name) {
                    if let Some(p) = ch.platform.as_deref().map(str::trim) {
                        if !p.is_empty() {
                            let lower = p.to_lowercase();
                            if lower != "telegram" && lower != "discord" {
                                problems.push(format!(
                                    "gateway.channels.{name}.platform {p:?} is not telegram|discord"
                                ));
                            }
                        }
                    }
                    if ch.is_explicit() && ch.resolved_platform(name).is_none() {
                        problems.push(format!(
                            "gateway.channels.{name} declares no usable platform"
                        ));
                    }
                }
            }
        }
        problems
    }
}

/// One aux slot: everything that varies per capability. The table below
/// drives target resolution, key seeding, validation, and the
/// `auxiliaries()` fan-out - adding a capability means adding one row.
pub struct AuxSlot {
    pub kind: crate::model::AuxiliaryKind,
    /// Config section name (for diagnostics).
    pub name: &'static str,
    /// `PANTHEON_<PREFIX>_PROVIDER` / `PANTHEON_<PREFIX>_MODEL`.
    pub env_prefix: &'static str,
    /// Vault entry the section's key env seeds.
    pub vault_name: &'static str,
    /// Absent section falls back to `auto` (the default model).
    /// False only for embeddings (absent = local embedder, never chat).
    pub auto: bool,
    pub section: for<'a> fn(&'a Config) -> Option<&'a AuxSection>,
}

pub const AUX_SLOTS: &[AuxSlot] = &[
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Judge,
        name: "judge",
        env_prefix: "JUDGE",
        vault_name: "PANTHEON_JUDGE_API_KEY",
        auto: true,
        section: |c| c.judge.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Compression,
        name: "compression",
        env_prefix: "COMPRESSION",
        vault_name: "PANTHEON_COMPRESSION_API_KEY",
        auto: true,
        section: |c| c.compression.as_ref().map(|s| &s.aux),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::TitleGen,
        name: "title_gen",
        env_prefix: "TITLEGEN",
        vault_name: "PANTHEON_TITLEGEN_API_KEY",
        auto: true,
        section: |c| c.title_gen.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Embeddings,
        name: "embeddings",
        env_prefix: "EMBEDDINGS",
        vault_name: "PANTHEON_EMBEDDINGS_API_KEY",
        auto: false,
        section: |c| c.embeddings.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::SearchSynthesis,
        name: "search_synthesis",
        env_prefix: "SEARCH_SYNTHESIS",
        vault_name: "PANTHEON_SEARCH_SYNTHESIS_API_KEY",
        auto: true,
        section: |c| c.search_synthesis.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Vision,
        name: "vision",
        env_prefix: "VISION",
        vault_name: "PANTHEON_VISION_API_KEY",
        auto: true,
        section: |c| c.vision.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Video,
        name: "video",
        env_prefix: "VIDEO",
        vault_name: "PANTHEON_VIDEO_API_KEY",
        auto: true,
        section: |c| c.video.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Scheduled,
        name: "scheduled",
        env_prefix: "SCHEDULED",
        vault_name: "PANTHEON_SCHEDULED_API_KEY",
        auto: true,
        section: |c| c.scheduled.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::McpSynthesis,
        name: "mcp_synthesis",
        env_prefix: "MCP_SYNTHESIS",
        vault_name: "PANTHEON_MCP_SYNTHESIS_API_KEY",
        auto: true,
        section: |c| c.mcp_synthesis.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Extraction,
        name: "extraction",
        env_prefix: "EXTRACTION",
        vault_name: "PANTHEON_EXTRACTION_API_KEY",
        auto: true,
        section: |c| c.extraction.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Rerank,
        name: "rerank",
        env_prefix: "RERANK",
        vault_name: "PANTHEON_RERANK_API_KEY",
        auto: true,
        section: |c| c.rerank.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Planner,
        name: "planner",
        env_prefix: "PLANNER",
        vault_name: "PANTHEON_PLANNER_API_KEY",
        auto: true,
        section: |c| c.planner.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Repair,
        name: "repair",
        env_prefix: "REPAIR",
        vault_name: "PANTHEON_REPAIR_API_KEY",
        auto: false,
        section: |c| c.repair.as_ref().map(|r| &r.aux),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Verify,
        name: "verify",
        env_prefix: "VERIFY",
        vault_name: "PANTHEON_VERIFY_API_KEY",
        // The documented exception alongside embeddings: absent section
        // = the slot is OFF, never `auto`. Verification only runs when
        // the operator explicitly pins a model here - an unverified
        // delegation must be a choice, not an accident of defaults.
        auto: false,
        section: |c| c.verify.as_ref(),
    },
];

/// Borrow one slot's section for validation.
pub fn cfg_section<'a>(cfg: &'a Config, slot: &AuxSlot) -> Option<&'a AuxSection> {
    (slot.section)(cfg)
}

#[cfg(test)]
mod config_max_tokens_tests {
    use super::*;

    /// Item 4: known 64k model, nothing set -> the model's own cap.
    #[test]
    fn resolver_prefers_model_max_output_when_nothing_set() {
        assert_eq!(resolve_max_output_tokens(None, None, Some(64_000)), 64_000);
    }

    /// Item 4: unknown model maximum -> the 16k fallback only.
    #[test]
    fn resolver_falls_back_to_16k_when_model_max_unknown() {
        assert_eq!(
            resolve_max_output_tokens(None, None, None),
            DEFAULT_MAX_OUTPUT_TOKENS
        );
        assert_eq!(DEFAULT_MAX_OUTPUT_TOKENS, 16_000);
    }

    /// Item 4: the user override wins the precedence chain but is still
    /// clamped to the model's known maximum (`/tokens 20000` on an
    /// 8192-cap model -> 8192, no error).
    #[test]
    fn resolver_clamps_session_override_to_model_cap() {
        assert_eq!(
            resolve_max_output_tokens(Some(20_000), None, Some(8_192)),
            8_192
        );
        assert_eq!(
            resolve_max_output_tokens(Some(4_000), None, Some(8_192)),
            4_000
        );
    }

    /// Item 4: full precedence - session > [budget] > model default.
    #[test]
    fn resolver_precedence_session_over_budget_over_model() {
        // Budget alone beats the model default.
        assert_eq!(
            resolve_max_output_tokens(None, Some(32_000), Some(64_000)),
            32_000
        );
        // Session beats budget.
        assert_eq!(
            resolve_max_output_tokens(Some(10_000), Some(32_000), Some(64_000)),
            10_000
        );
        // Budget above the model cap still clamps.
        assert_eq!(
            resolve_max_output_tokens(None, Some(100_000), Some(64_000)),
            64_000
        );
    }

    /// Item 4: a 16k-window model is refused with its name and window
    /// in the error; an unknown window fails open.
    #[test]
    fn context_floor_refuses_small_windows_and_names_the_model() {
        let err = check_context_window("tiny-model", Some(16_000)).expect_err("16k < 32k floor");
        assert_eq!(err.code, "MODEL_CONTEXT_TOO_SMALL");
        let msg = format!("{err:?}");
        assert!(msg.contains("tiny-model"), "error names the model: {msg}");
        assert!(msg.contains("16000"), "error names the window: {msg}");
        assert!(check_context_window("big-model", Some(128_000)).is_ok());
        assert!(
            check_context_window("big-model", Some(32_000)).is_ok(),
            "floor is inclusive"
        );
        assert!(
            check_context_window("mystery-model", None).is_ok(),
            "unknown fails open"
        );
    }
}

#[cfg(test)]
mod config_budget_delegation_tests {
    use super::*;

    fn budget_toml(budget_body: &str) -> String {
        format!("[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n\n[budget]\n{budget_body}\n")
    }

    /// Both delegation keys set in TOML: the values survive a full
    /// `Config` load + `validate()`, and serialize back out.
    #[test]
    fn delegation_keys_round_trip_through_config_toml() {
        let toml = budget_toml("delegate_child_max_tokens = 12000\nmax_delegations = 4\n");
        let cfg: Config = toml::from_str(&toml).expect("config parses");
        assert!(cfg.validate().is_empty(), "no validation problems");
        let budget = cfg.budget.as_ref().expect("[budget] present");
        assert_eq!(budget.delegate_child_max_tokens, Some(12_000));
        assert_eq!(budget.max_delegations, Some(4));
        assert_eq!(budget.delegate_child_budget(), Some(12_000));
        assert_eq!(budget.max_delegations_or_default(), 4);

        // And the keys serialize back into the document.
        let back = toml::to_string(&cfg).expect("serializes");
        let reparsed: Config = toml::from_str(&back).expect("reparses");
        assert_eq!(reparsed.budget, cfg.budget);
    }

    /// Absent keys: no child override and the 8-call default.
    #[test]
    fn delegation_keys_absent_means_defaults() {
        let toml = budget_toml("max_turns = 20\n");
        let cfg: Config = toml::from_str(&toml).expect("config parses");
        assert!(cfg.validate().is_empty(), "no validation problems");
        let budget = cfg.budget.expect("[budget] present");
        assert_eq!(budget.delegate_child_max_tokens, None);
        assert_eq!(budget.max_delegations, None);
        assert_eq!(budget.delegate_child_budget(), None);
        assert_eq!(budget.max_delegations_or_default(), 8);
        assert_eq!(DEFAULT_MAX_DELEGATIONS, 8);

        // Missing `[budget]` entirely behaves the same.
        let cfg: Config = toml::from_str("[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n")
            .expect("config parses");
        assert!(cfg.budget.is_none());
        assert_eq!(
            cfg.budget
                .as_ref()
                .map(BudgetSection::max_delegations_or_default)
                .unwrap_or(DEFAULT_MAX_DELEGATIONS),
            8
        );
    }

    /// `0` is treated as unset: child falls back to the model default,
    /// delegations fall back to 8.
    #[test]
    fn delegation_keys_zero_means_unset() {
        let toml = budget_toml("delegate_child_max_tokens = 0\nmax_delegations = 0\n");
        let cfg: Config = toml::from_str(&toml).expect("config parses");
        let budget = cfg.budget.expect("[budget] present");
        assert_eq!(budget.delegate_child_max_tokens, Some(0));
        assert_eq!(
            budget.delegate_child_budget(),
            None,
            "0 child budget = no override"
        );
        assert_eq!(
            budget.max_delegations_or_default(),
            8,
            "0 delegation cap = default"
        );
    }

    /// Unset fields are omitted from serialized output (no key noise in
    /// freshly written config files).
    #[test]
    fn delegation_keys_skip_serializing_when_none() {
        let budget = BudgetSection::default();
        let s = toml::to_string(&budget).expect("serializes");
        assert!(!s.contains("delegate_child_max_tokens"), "{s}");
        assert!(!s.contains("max_delegations"), "{s}");
    }
}

#[cfg(test)]
mod gateway_multi_agent_tests {
    use super::*;

    /// Panic-safe guard: the profile override is process-global, so every
    /// test that sets it must clear it even on failure - a leaked override
    /// would silently change what other tests resolve.
    /// The profile override is a process-global static, so any test that
    /// touches it must hold this lock for the whole body: without it, two
    /// parallel tests interleave on the shared value and one reads the
    /// other's override (`cli_override_beats_the_agent_setting` failed on
    /// CI with `UnknownProfile { zeus }` because a concurrent guard had
    /// set zeus between the set and the read).
    static OVERRIDES_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> =
        std::sync::OnceLock::new();

    /// Holds the OVERRIDES_LOCK guard for the whole test body (not just the
    /// `set_profile_override` call): a guard dropped at the end of the
    /// `.lock()` expression would let a parallel test interleave between
    /// the set and the read. Named, so it lives as long as `OverrideGuard`.
    struct OverrideGuard(std::sync::MutexGuard<'static, ()>);
    impl OverrideGuard {
        fn set(name: &str) -> Self {
            let lock = OVERRIDES_LOCK.get_or_init(|| std::sync::Mutex::new(()));
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            set_profile_override(Some(name.to_string()));
            OverrideGuard(guard)
        }
    }
    impl Drop for OverrideGuard {
        fn drop(&mut self) {
            set_profile_override(None);
            // the MutexGuard (self.0) is dropped by the compiler after this,
            // releasing OVERRIDES_LOCK for the next override test.
        }
    }

    #[test]
    fn channel_platform_resolves_explicit_value_case_insensitively() {
        let ch = GatewayChannelSection {
            platform: Some("Telegram".to_string()),
            ..Default::default()
        };
        assert!(ch.is_explicit());
        assert_eq!(ch.resolved_platform("bot-a").as_deref(), Some("telegram"));
    }

    #[test]
    fn channel_platform_falls_back_to_legacy_channel_names() {
        let ch = GatewayChannelSection::default();
        assert!(!ch.is_explicit());
        assert_eq!(
            ch.resolved_platform("telegram").as_deref(),
            Some("telegram")
        );
        assert_eq!(ch.resolved_platform("discord").as_deref(), Some("discord"));
        assert_eq!(ch.resolved_platform("bot-a"), None);
    }

    #[test]
    fn channel_section_deserializes_the_old_shape_untouched() {
        // A config written before per-channel fields existed parses with
        // the new fields absent: single-bot setups keep working.
        let ch: GatewayChannelSection = toml::from_str("voice_replies = true").unwrap();
        assert!(ch.voice_replies);
        assert_eq!(ch.platform, None);
        assert_eq!(ch.token_secret, None);
        assert_eq!(ch.profile, None);
    }

    #[test]
    fn channel_section_deserializes_the_multi_bot_shape() {
        let ch: GatewayChannelSection = toml::from_str(
            "platform = \"telegram\"\ntoken_secret = \"BOT_A_TOKEN\"\nprofile = \"support\"\nvoice_replies = true",
        )
        .unwrap();
        assert_eq!(ch.platform.as_deref(), Some("telegram"));
        assert_eq!(ch.token_secret.as_deref(), Some("BOT_A_TOKEN"));
        assert_eq!(ch.profile.as_deref(), Some("support"));
        assert!(ch.voice_replies);
    }

    #[test]
    fn validate_rejects_an_unknown_platform() {
        let mut cfg = Config::default();
        let mut gw = GatewaySection::default();
        gw.channels.insert(
            "bot-a".to_string(),
            GatewayChannelSection {
                platform: Some("slack".to_string()),
                ..Default::default()
            },
        );
        cfg.gateway = Some(gw);
        let problems = cfg.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("gateway.channels.bot-a.platform")),
            "expected a platform problem, got: {problems:?}"
        );
    }

    #[test]
    fn validate_accepts_telegram_and_discord() {
        let mut cfg = Config::default();
        let mut gw = GatewaySection::default();
        for (name, platform) in [("bot-a", "telegram"), ("bot-b", "discord")] {
            gw.channels.insert(
                name.to_string(),
                GatewayChannelSection {
                    platform: Some(platform.to_string()),
                    ..Default::default()
                },
            );
        }
        cfg.gateway = Some(gw);
        let problems = cfg.validate();
        assert!(
            !problems.iter().any(|p| p.contains("gateway.channels")),
            "unexpected gateway problems: {problems:?}"
        );
    }

    #[test]
    fn anonymous_when_nothing_declared_and_nothing_asked() {
        // The pre-profiles behavior is untouched: no [agents] table and no
        // override means anonymous, not an error.
        let cfg = Config::default();
        assert!(cfg.resolve_profile(None).unwrap().is_none());
    }

    #[test]
    fn cli_override_with_unknown_name_is_an_error_not_anonymous() {
        // Asking for a profile that does not exist must fail loudly
        // silently running anonymous would attribute the conversation to
        // the wrong identity.
        let _guard = OverrideGuard::set("zeus");
        let cfg = Config::default();
        assert!(cfg.resolve_profile(None).is_err());
    }

    #[test]
    fn cli_override_beats_the_agent_setting() {
        let mut cfg = Config::default();
        cfg.agents
            .insert("alpha".to_string(), AgentIdentity::default());
        cfg.agents
            .insert("beta".to_string(), AgentIdentity::default());
        cfg.agent = Some("alpha".to_string());
        let _guard = OverrideGuard::set("beta");
        let eff = cfg.resolve_profile(None).unwrap().unwrap();
        assert_eq!(eff.name, "beta");
    }

    #[test]
    fn call_site_override_beats_the_cli_override() {
        let mut cfg = Config::default();
        cfg.agents
            .insert("alpha".to_string(), AgentIdentity::default());
        cfg.agents
            .insert("beta".to_string(), AgentIdentity::default());
        let _guard = OverrideGuard::set("beta");
        let eff = cfg.resolve_profile(Some("alpha")).unwrap().unwrap();
        assert_eq!(eff.name, "alpha");
    }

    #[test]
    fn channel_profile_selects_that_profile() {
        // The gateway passes each channel's `profile` as the call-site
        // override: two channels resolve to two different agents.
        let mut cfg = Config::default();
        cfg.agents
            .insert("support".to_string(), AgentIdentity::default());
        cfg.agents
            .insert("coder".to_string(), AgentIdentity::default());
        let a = cfg.resolve_profile(Some("support")).unwrap().unwrap();
        let b = cfg.resolve_profile(Some("coder")).unwrap().unwrap();
        assert_eq!(a.name, "support");
        assert_eq!(b.name, "coder");
        assert_ne!(a.memory_namespace, b.memory_namespace);
    }
}

#[cfg(test)]
mod fix_pass4_leaf1_tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg_with_model() -> Config {
        Config {
            model: Some(ModelSection {
                provider: "openai".to_string(),
                model: "gpt-4o".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn voice_section(backend: &str, options: &[(&str, &str)]) -> VoiceSection {
        VoiceSection {
            backend: backend.to_string(),
            options: options
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// Item 1: the wizard's recommended backends (groq, piper-local)
    /// validate clean - this was the setup/doctor infinite loop.
    #[test]
    fn validate_accepts_registry_voice_backends() {
        let mut cfg = cfg_with_model();
        cfg.stt = Some(voice_section("groq", &[]));
        cfg.tts = Some(voice_section(
            "piper-local",
            &[("voice", "en_US-lessac-high")],
        ));
        let problems = cfg.validate();
        assert!(
            !problems
                .iter()
                .any(|p| p.contains("stt") || p.contains("tts")),
            "unexpected voice problems: {problems:?}"
        );
    }

    /// Item 1: every registry backend id validates.
    #[test]
    fn validate_accepts_all_registry_voice_backends() {
        for backend in STT_BACKENDS {
            let mut cfg = cfg_with_model();
            let options: &[(&str, &str)] = match *backend {
                "command" => &[("cmd", "whisper")],
                "openai" => &[("provider", "openai")],
                _ => &[],
            };
            cfg.stt = Some(voice_section(backend, options));
            let problems = cfg.validate();
            assert!(
                !problems.iter().any(|p| p.contains("stt.backend")),
                "stt backend {backend:?} rejected: {problems:?}"
            );
        }
        for backend in TTS_BACKENDS {
            let mut cfg = cfg_with_model();
            let options: &[(&str, &str)] = match *backend {
                "command" => &[("cmd", "say")],
                "openai" => &[("provider", "openai")],
                "piper-local" => &[("voice", "en_US-lessac-high")],
                _ => &[],
            };
            cfg.tts = Some(voice_section(backend, options));
            let problems = cfg.validate();
            assert!(
                !problems.iter().any(|p| p.contains("tts.backend")),
                "tts backend {backend:?} rejected: {problems:?}"
            );
        }
    }

    /// Item 1: unknown backends still fail, and per-backend required
    /// options are enforced.
    #[test]
    fn validate_rejects_unknown_voice_backend_and_missing_options() {
        let mut cfg = cfg_with_model();
        cfg.stt = Some(voice_section("wat", &[]));
        cfg.tts = Some(voice_section("piper-local", &[]));
        let problems = cfg.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("stt.backend \"wat\" is unknown")),
            "expected unknown stt backend problem, got: {problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.contains("tts.options.voice is required")),
            "expected piper-local voice problem, got: {problems:?}"
        );
    }

    /// Item 11: api_mode typos fail instead of silently mapping to the
    /// wrong wire protocol.
    #[test]
    fn validate_rejects_bad_custom_provider_api_mode() {
        let mut cfg = cfg_with_model();
        cfg.custom_providers.insert(
            "mine".to_string(),
            CustomProviderSection {
                base_url: "https://example.com/v1".to_string(),
                api_mode: "anthroic".to_string(),
                ..Default::default()
            },
        );
        cfg.custom_providers.insert(
            "ok".to_string(),
            CustomProviderSection {
                base_url: "https://example.com/v1".to_string(),
                api_mode: "Anthropic".to_string(),
                ..Default::default()
            },
        );
        let problems = cfg.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("custom_providers.mine.api_mode")),
            "expected api_mode problem, got: {problems:?}"
        );
        assert!(
            !problems.iter().any(|p| p.contains("custom_providers.ok")),
            "unexpected problem for valid api_mode: {problems:?}"
        );
    }

    /// Item 7: unknown top-level keys are reported (pure function).
    #[test]
    fn unknown_config_keys_flags_typos() {
        let text =
            "[modle]\nprovider = \"openai\"\n[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n";
        assert_eq!(unknown_config_keys(text), vec!["modle".to_string()]);
        assert!(unknown_config_keys("[model]\nprovider = \"openai\"\n").is_empty());
        // Unparseable documents report nothing here - the parse error
        // path owns that case.
        assert!(unknown_config_keys("[model\n").is_empty());
    }

    /// Item 7: KNOWN_CONFIG_KEYS tracks the Config struct. The literal
    /// is exhaustive (no `..Default::default()`), so adding a field
    /// breaks compilation until the list grows with it.
    #[test]
    fn known_config_keys_match_struct_fields() {
        let full = Config {
            agent: Some(String::new()),
            model: Some(ModelSection::default()),
            judge: Some(AuxSection::default()),
            embeddings: Some(AuxSection::default()),
            search_synthesis: Some(AuxSection::default()),
            vision: Some(AuxSection::default()),
            video: Some(AuxSection::default()),
            scheduled: Some(AuxSection::default()),
            mcp_synthesis: Some(AuxSection::default()),
            extraction: Some(AuxSection::default()),
            rerank: Some(AuxSection::default()),
            planner: Some(AuxSection::default()),
            repair: Some(RepairSection::default()),
            verify: Some(AuxSection::default()),
            swarm: Some(SwarmSection::default()),
            compression: Some(CompressionSection::default()),
            title_gen: Some(TitleGenSection::default()),
            reflect: Some(ReflectSection::default()),
            consolidation: Some(ConsolidationSection::default()),
            nightly: Some(NightlySection::default()),
            stt: Some(VoiceSection::default()),
            tts: Some(VoiceSection::default()),
            voice: Some(LiveVoiceSection::default()),
            policy: Some(PolicyPreset::default()),
            permission_mode: None,
            memory: Some(MemorySection::default()),
            server: Some(ServerSection::default()),
            secrets: Some(SecretsSection::default()),
            retention: Some(RetentionSection::default()),
            temporal: Some(TemporalSection::default()),
            budget: Some(BudgetSection::default()),
            goal: Some(GoalSection::default()),
            browser: Some(BrowserSection::default()),
            gateway: Some(GatewaySection::default()),
            computer_use: Some(ComputerUseSection::default()),
            websearch: Some(WebsearchSection::default()),
            tools: Some(ToolsSection::default()),
            skill_deps: Some(SkillDepsSection::default()),
            cloudflare: Some(CloudflareSection::default()),
            mcp: Some(McpSection::default()),
            plugins: HashMap::from([(
                "x".to_string(),
                PluginEntry {
                    enabled: true,
                    kind: None,
                    version: None,
                },
            )]),
            tui: Some(TuiSection::default()),
            custom_providers: HashMap::from([("x".to_string(), CustomProviderSection::default())]),
            agents: HashMap::from([("x".to_string(), AgentIdentity::default())]),
        };
        let value = toml::Value::try_from(&full).expect("Config serializes");
        let table = value.as_table().expect("Config is a table");
        let mut serialized: Vec<&str> = table.keys().map(String::as_str).collect();
        serialized.sort_unstable();
        let mut known: Vec<&str> = KNOWN_CONFIG_KEYS.to_vec();
        known.sort_unstable();
        assert_eq!(
            serialized, known,
            "KNOWN_CONFIG_KEYS drifted from Config's fields"
        );
    }
}

#[cfg(test)]
mod config_repair_section_tests {
    use super::*;

    /// The old `[repair]` shape (model pin keys only) still parses, and
    /// the new cap key defaults to 3.
    #[test]
    fn legacy_repair_section_parses_with_default_cap() {
        let cfg: Config = toml::from_str("[repair]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n")
            .expect("config parses");
        let repair = cfg.repair.as_ref().expect("[repair] present");
        assert_eq!(repair.provider, "openai");
        assert_eq!(repair.model, "gpt-4o");
        assert_eq!(repair.max_repairs_per_task_per_day, None);
        assert_eq!(repair.repair_cap_per_day(), 3);
    }

    /// The new cap key parses and is honored; it serializes back out.
    #[test]
    fn repair_cap_key_round_trips() {
        let cfg: Config = toml::from_str(
            "[repair]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\nmax_repairs_per_task_per_day = 5\n",
        )
        .expect("config parses");
        let repair = cfg.repair.as_ref().expect("[repair] present");
        assert_eq!(repair.max_repairs_per_task_per_day, Some(5));
        assert_eq!(repair.repair_cap_per_day(), 5);
        let back = toml::to_string(&cfg).expect("config serializes");
        assert!(back.contains("max_repairs_per_task_per_day = 5"));
    }

    /// `RepairSection` still behaves as its aux slot: the model-policy
    /// resolution sees the same provider/model as before the struct split.
    #[test]
    fn repair_section_derefs_to_aux() {
        let section = RepairSection {
            aux: AuxSection {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                ..AuxSection::default()
            },
            max_repairs_per_task_per_day: Some(1),
        };
        let aux: &AuxSection = &section;
        assert_eq!(aux.model, "gpt-4o");
        assert_eq!(
            RepairSection::from(AuxSection::default()).repair_cap_per_day(),
            3
        );
    }
}
