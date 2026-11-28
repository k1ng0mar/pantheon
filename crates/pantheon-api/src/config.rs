//! shared config document types + load/save/validate.
//!
//! resolution is per-client; runtime stuff stays there. this crate is the leaf
//! the TUI, dashboard, etc all use.

use crate::agent_profile::{EffectiveProfile, ProfileError, ProfileRegistry};
use crate::config_schema::{PolicyPreset, SecretRef};
use crate::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelSection {
    // a partial `[model]` is fine while the user is filling it in;
    // validation + resolution catch the missing half later
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    /// env var that holds the api key. never the key itself
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// ordered fallback chain: [{provider, model}]
    #[serde(default)]
    pub fallbacks: Vec<FallbackEntry>,
    /// reasoning effort for chat turns: off|minimal|low|medium|high.
    /// absent (or `PANTHEON_REASONING` unset) = off - no effort param sent,
    /// every provider behaves exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// thinking budget in tokens for budget wires (anthropic).
    /// overrides the level mapping; ignored on effort-string wires.
    /// 0 disables thinking entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_budget: Option<u32>,
    /// opt-in depth directives: when true, a prompt that asks for depth
    /// ("think hard", "ultrathink", "thorough analysis") runs that one turn
    /// at a higher reasoning effort. Absent/false = off: effort costs tokens,
    /// so this is never on by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth_directives: Option<bool>,
    /// provider http timeout in ms (`timeout_ms`).
    /// single-shot = overall deadline; streams = per-read/connection deadline,
    /// never a total cap, so a slow-but-alive provider isn't cut off
    /// mid-answer. absent or 0 = 120s default; `PANTHEON_HTTP_TIMEOUT_MS`
    /// overrides; >1h clamps to 1h.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// one aux-model slot: every `[judge]`, `[compression]`, `[title_gen]`,
/// `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`,
/// `[mcp_synthesis]`, `[extraction]`, `[rerank]`, `[planner]`, `[verify]`
/// section is exactly this shape. the named section types below are aliases
/// so existing construction sites keep compiling.
///
/// inheritance: `provider = "default"` (or omitted) -> `[model]`'s provider,
/// empty/omitted `model` -> `[model]`'s model, and omitted `api_key_env` on a
/// default-inheriting provider -> `[model].api_key_env`. each slot is
/// independent - pin different provider/model per slot, or inherit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuxSection {
    /// provider id, or `"default"`/empty to inherit `[model]`'s
    #[serde(default)]
    pub provider: String,
    /// model name, or empty to inherit `[model]`'s
    #[serde(default)]
    pub model: String,
    /// per-aux request timeout in seconds (`timeout`). absent = client default
    /// for the capability (10s title/judge, 15s embeddings, 30s compression,
    /// 60s reflection/consolidation/repair, 120s otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// `[judge]`: aux judge model. any provider/model in the catalog (or a raw
/// base URL as provider); runtime resolves wire mode + key env like chat.
/// absent = `auto` -> run's default model answers judge queries (route select,
/// tool gate). judging always runs, it's just cheaper when configured.
pub type JudgeSection = AuxSection;

/// `[title_gen]`: aux session-title model. names a conversation from its first
/// user prompt (fire-and-forget next to the first turn). absent = `auto` ->
/// default model; titles always work, just cheaper/smaller when configured.
pub type TitleGenSection = AuxSection;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FallbackEntry {
    pub provider: String,
    pub model: String,
    /// env var holding this fallback's api key. absent = resolve through the
    /// provider's own catalog/env credential. see
    /// [`crate::model::DefaultModel::key_env`] for why an absent entry does NOT
    /// inherit the default entry's key value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
}

/// one model row inside `[custom_providers.<name>.models]`.
///
/// exists so a custom endpoint's models show up in the catalog and can be
/// picked by name, instead of a bare endpoint with no listable ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CustomModel {
    /// model id as the endpoint spells it
    pub id: String,
    /// context window in tokens. omitted = unknown; runtime treats that
    /// conservatively rather than guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u32>,
    /// max output tokens per provider. omitted = unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// operator-declared: this model takes native video on this endpoint (e.g.
    /// qwen-omni on an openai-compatible one). we trust the declaration and
    /// send real video bytes through the endpoint's wire format.
    #[serde(default)]
    pub video: bool,
    /// operator-declared capabilities, same trust model as `video`. without
    /// these a vision-capable custom model stayed `vision=false` from the
    /// conservative defaults, and the chain's vision gate refused every image
    /// turn on it; same story for non-streaming / tool-less endpoints.
    /// defaults match `model_meta` (tools/streaming on, vision off) so old
    /// rows parse unchanged.
    #[serde(default)]
    pub vision: bool,
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default = "default_true")]
    pub streaming: bool,
}

/// `[custom_providers.<name>]`: user-defined endpoint (written by
/// `pantheon model` when you pick "Custom provider"). behaves like a cataloged
/// provider everywhere: base url + wire mode + key env. provider id is the
/// table name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CustomProviderSection {
    /// display name shown in pickers and the composer. absent (or blank) =
    /// the table name, which is also the provider id. Lets a provider be
    /// called "LLM Router" while its id stays `llm-router` (the id is what
    /// goes in `provider =` and `PANTHEON_KEY_<ID>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// full base url, no trailing slash (e.g. `http://127.0.0.1:8015/v1`)
    #[serde(default)]
    pub base_url: String,
    /// wire format: `openai` (default) or `anthropic`
    #[serde(default = "default_openai_mode")]
    pub api_mode: String,
    /// env var that holds the api key (never the key itself); default
    /// `PANTHEON_KEY_<NAME>`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// models this endpoint serves. optional - an endpoint with none still
    /// works, the operator just has to name the model explicitly.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<CustomModel>,
}

fn default_openai_mode() -> String {
    "openai".into()
}

/// STT backend ids the runtime can construct (`open_stt` in
/// pantheon-providers: `command`, the openai-wire trio, bespoke live backends).
///
/// `pantheon-api` can't depend on `pantheon-providers` (circular), so the list
/// is duplicated here. a test in `pantheon-tui` pins the voice registry
/// against these constants, so a new backend can't land without updating
/// validation.
pub const STT_BACKENDS: &[&str] = &[
    "command",
    "openai",
    "groq",
    "mistral",
    "deepgram",
    "elevenlabs",
    "xai",
    "assemblyai",
    "whistle",
    "parakeet",
    "photon",
    "parakeet-redux",
];

/// TTS backend ids the runtime can construct (`open_tts` in
/// pantheon-providers: `command`, local shell-out backends, bespoke live
/// backends). same duplication caveat as [`STT_BACKENDS`].
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

/// `[browser] backend` ids the runtime can construct, mirroring
/// `pantheon-web`'s `BackendKind::parse` (canonical ids + the aliases it
/// accepts). same duplication caveat as [`STT_BACKENDS`]: `pantheon-api`
/// can't depend on `pantheon-web`.
pub const BROWSER_BACKENDS: &[&str] = &[
    "gsd",
    "gsd-browser",
    "chromiumoxide",
    "chromium-oxide",
    "native",
    "cdp",
    "steel",
    "browserbase",
    "playwright",
    "playwright-cli",
    "camofox",
    "camoufox",
];

/// `[websearch] provider` ids the runtime can construct, mirroring
/// `pantheon-web`'s `build_provider` match arms. same duplication caveat
/// as [`STT_BACKENDS`].
pub const WEBSEARCH_PROVIDERS: &[&str] = &[
    "tinyfish",
    "tavily",
    "exa",
    "brave",
    "firecrawl",
    "ollama",
    "perplexity",
    "marginalia",
    "searxng",
    "exa_free",
    "parallel_free",
    "firecrawl_free",
    "keenable_free",
    "ddgs",
];

/// `[tui] theme` names the TUI can render, mirroring
/// `pantheon-tui`'s `Theme::from_name`. same duplication caveat as
/// [`STT_BACKENDS`].
pub const TUI_THEMES: &[&str] = &["pantheon", "dark", "light"];

/// `[stt]` / `[tts]`: speech service selection. provider-plane services (a
/// local binary or an HTTP endpoint), not model-policy entries - same shape as
/// `[memory]`'s backend selection. absent = no speech capability.
///
/// backends: `command` (local binary: `cmd`, `args` template with
/// `{file}`/`{language}` for STT, `{voice}`/`{format}` for TTS,
/// `timeout_secs`) or a named provider (openai-compatible HTTP:
/// `provider` - catalog id or base url - plus `model`, and `api_key_env`
/// naming the env var holding the key; the key resolves through the secrets
/// broker, falling back to the catalog row's key env). named STT providers:
/// groq, openai, mistral, xai, elevenlabs, deepgram, assemblyai. named TTS
/// providers: piper-local, kokoro-local, elevenlabs, deepgram, openai, gemini,
/// fishaudio, fishspeech-local.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct VoiceSection {
    /// backend name from the provider registry: `command` or a named
    /// STT/TTS provider (see above).
    pub backend: String,
    /// backend-specific options (cmd/args for command, provider/model
    /// for openai, timeout_secs, ...).
    #[serde(default)]
    pub options: std::collections::HashMap<String, String>,
}

/// `[voice]`: live voice-mode knobs for the mobile `/agui/voice/live`
/// websocket (docs/live-voice-mode.md). refused unless `[tools] voice` is on,
/// `[stt]` + `[tts]` are both present and constructible, and `live_enabled`
/// here. absent = live mode off with default limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveVoiceSection {
    /// master opt-in. default false - text chat is the default everywhere;
    /// this is a feature, never the default.
    #[serde(default)]
    pub live_enabled: bool,
    /// hard cap on one live session, seconds. default 600.
    #[serde(default = "default_live_max_session_secs")]
    pub live_max_session_secs: u64,
    /// hard cap on one utterance, seconds; runaway utterances get
    /// force-closed. default 30.
    #[serde(default = "default_live_max_utterance_secs")]
    pub live_max_utterance_secs: u64,
    /// energy-VAD silence bound: once speech is detected, auto-close the
    /// utterance after this many silent ms. default 1200.
    #[serde(default = "default_live_silence_timeout_ms")]
    pub live_silence_timeout_ms: u64,
    /// optional provider-native realtime backend. `"openai"` speaks the
    /// openai realtime websocket directly (full-duplex audio + provider turn
    /// detection); absent keeps the local STT -> agent -> TTS path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_backend: Option<String>,
    /// realtime model id. absent = `gpt-4o-realtime-preview`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_model: Option<String>,
    /// provider voice id. absent = `alloy`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realtime_voice: Option<String>,
}

impl Default for LiveVoiceSection {
    fn default() -> Self {
        Self {
            live_enabled: false,
            live_max_session_secs: default_live_max_session_secs(),
            live_max_utterance_secs: default_live_max_utterance_secs(),
            live_silence_timeout_ms: default_live_silence_timeout_ms(),
            realtime_backend: None,
            realtime_model: None,
            realtime_voice: None,
        }
    }
}

/// `[gateway.channels.<name>]`: per-channel options. the table name is the
/// channel name (`telegram`, `discord` for the legacy single-bot shape; any
/// slug for explicit multi-bot channels).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GatewayChannelSection {
    /// speak replies as voice on this channel (telegram `sendVoice`,
    /// discord audio attachment). default off - text stays the default.
    /// needs a `[tts]` backend; without one the channel keeps sending text.
    #[serde(default)]
    pub voice_replies: bool,
    /// `"telegram"` or `"discord"`. absent = inferred from the channel name
    /// when it's exactly `telegram`/`discord` (the legacy shape). setting this
    /// makes it an *explicit* channel: the gateway starts one poller per
    /// explicit channel, so two can both be telegram with different tokens
    /// and profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// secret name holding this channel's bot token, resolved through the
    /// secrets broker (process env wins, then `<data_dir>/gateway.env`).
    /// absent = the platform's standard token (`PANTHEON_TELEGRAM_BOT_TOKEN`
    /// / `PANTHEON_DISCORD_TOKEN`), so single-bot setups keep working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_secret: Option<String>,
    /// agent profile serving this channel. absent = default profile
    /// resolution (explicit `--profile`, else `agent = "..."`, else `default`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

impl GatewayChannelSection {
    /// resolved platform, or `None` when neither `platform` nor the channel
    /// name says. anything other than `telegram`/`discord` is a config error
    /// (see [`Config::validate`]).
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

    /// true when this channel declares its platform (opts into multi-bot).
    /// explicit channels start per-entry; name-only entries don't.
    pub fn is_explicit(&self) -> bool {
        self.platform.as_ref().is_some_and(|p| !p.trim().is_empty())
    }
}

/// `[gateway]`: channel options. absent = every channel default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct GatewaySection {
    /// per-channel options by channel name
    #[serde(default)]
    pub channels: std::collections::HashMap<String, GatewayChannelSection>,
}

/// `[compression]`: aux context-compression model. summarizes the oldest
/// exchanges when a transcript overflows the window. absent = `auto` -> run's
/// default model compresses; the deterministic fit stays the correctness path
/// either way.
///
/// `target_percent` (1-100, default 12) = summary size as a % of absorbed
/// transcript chars: higher keeps more detail (less aggressive), lower
/// compresses harder. 0 and >100 are config errors; absent keeps the fixed
/// eighth-of-material target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompressionSection {
    #[serde(flatten)]
    pub aux: AuxSection,
    #[serde(default, deserialize_with = "de_target_percent")]
    pub target_percent: Option<u8>,
}

/// `target_percent` has to be 1-100. `u8` already rejects >255 at parse;
/// this rejects the rest (0 and 101-255) with a config error instead of
/// silently clamping.
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

/// `[search_synthesis]`: model that turns retrieved passages into a
/// synthesized answer. absent = `auto` -> run's default model writes the
/// synthesis; configuring it just makes search answers cheaper.
pub type SearchSynthesisSection = AuxSection;

/// `[vision]`: image-understanding model. absent = `auto` -> run's default
/// model handles images. consulted by the vision host pass before an
/// image-bearing turn reaches the chat model.
pub type VisionSection = AuxSection;

/// `[video]`: video-understanding model. absent = `auto` -> run's default
/// model. the video host pass samples keyframes and calls this slot,
/// never chat.
pub type VideoSection = AuxSection;

/// `[image]`: how the `image` tool produces a picture.
///
/// Deliberately backend-shaped rather than vendor-shaped: the same tool drives
/// an OpenAI-compatible `/images/generations`, a chat model that emits images
/// (`modalities: ["image","text"]`), a local Stable-Diffusion server
/// (`/sdapi/v1/txt2img`), or any HTTP endpoint whose URL takes the prompt as a
/// template. Nothing here names a vendor, so a local server and a hosted API
/// are the same kind of thing.
///
/// absent table = `auto`: an OpenAI-wire provider gets `/images/generations`,
/// anything else is refused with a message naming the backends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ImageSection {
    /// `auto` | `openai_images` | `chat_image` | `a1111` | `url_template`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// provider row that supplies base URL + key. absent = `[model].provider`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// image model id (openai_images / chat_image). absent = the provider's
    /// first model whose id looks like an image model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// full endpoint URL. Required for `a1111` (server root) and
    /// `url_template` (must contain `{prompt}`). Overrides the provider base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// env var holding the key for `endpoint` when it is not a provider row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_env: Option<String>,
    /// wall clock for one generation. absent = 120s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// passed to template/SD backends that take a seed. absent = none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<u32>,
}

/// `[scheduled]`: model scheduled (background) runs execute with.
/// absent = `auto` -> scheduled jobs run on the default model. pin a
/// small/cheap one so background tasks stop fighting interactive chat.
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

/// `[reflect]`: reflection - the ledger-native self-improvement loop, plus
/// the aux model pin for its LLM-backed steps, in one table.
///
/// behavior knobs (`enabled`, `auto_turns`, `max_proposals`) and the model
/// pin share the table, like `[scheduled]` doubling as pin + section: one
/// `[reflect]` table is everything the feature needs. absent
/// `provider`/`model` = `auto`: run's default model answers reflection LLM
/// calls. pin a small/cheap model here (or
/// `PANTHEON_REFLECTION_PROVIDER`/`PANTHEON_REFLECTION_MODEL`) so background
/// self-improvement never competes with interactive chat.
///
/// DEPRECATED as a behavior-knob table: `[nightly]` is the authoritative
/// section for the unified nightly pass. this table still parses (old
/// configs keep loading) and, when `[nightly]` is absent, the unified pass
/// honors `enabled` and `auto_turns` from it field-by-field as a migration
/// fallback - when `[nightly]` is present it's ignored entirely. prefer
/// `[nightly]`; `pantheon doctor` nudges you to move the knobs. the
/// aux-model pin (`provider`/`model`/`api_key_env`/`timeout`) legitimately
/// stays here: `[nightly]` has no model pin, and the pass's
/// proposal-refinement step always resolves through the reflection aux slot.
///
/// LLM-backed reflection steps are off by default (`enabled = false`):
/// the deterministic signal-extraction pipeline always runs; anything
/// spending model tokens needs explicit opt-in. every LLM call the pipeline
/// makes resolves through the
/// [`AuxiliaryKind::Reflection`](crate::model::AuxiliaryKind)
/// slot - never the chat model directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ReflectSection {
    /// allow LLM-backed reflection steps. default false.
    #[serde(default)]
    pub enabled: bool,
    /// completed turns before an automatic reflection pass. default 20.
    /// `0` disables the automatic trigger.
    #[serde(default = "default_reflect_auto_turns")]
    pub auto_turns: u32,
    /// max proposals per pass. default 5.
    #[serde(default = "default_reflect_max_proposals")]
    pub max_proposals: usize,
    /// aux model pin for reflection LLM calls. absent = `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// aux model pin for reflection LLM calls. absent = `auto`;
    /// `"default"`/empty explicitly inherits `[model]`'s model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// per-aux request timeout in seconds (`timeout`). absent = client default
    /// for the capability (10s title/judge, 15s embeddings, 30s compression,
    /// 60s reflection/consolidation, 120s otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// `[consolidation]`: background memory consolidation, plus the aux model pin
/// for its LLM-backed distill step, in one table.
///
/// behavior knobs (`enabled`, `half_life_days`, `min_sessions`, `min_score`,
/// `cron`) and the pin share the table, same as `[reflect]`. absent
/// `provider`/`model` = `auto`: run's default model answers consolidation
/// calls - pin a small/cheap one here (or
/// `PANTHEON_CONSOLIDATION_PROVIDER`/`PANTHEON_CONSOLIDATION_MODEL`) so
/// nightly consolidation never competes with interactive chat.
///
/// LLM-backed steps off by default (`enabled = false`): the deterministic
/// stage/weigh/promote pipeline always runs, anything spending tokens needs
/// opt-in. every LLM call resolves through the
/// [`AuxiliaryKind::Consolidation`](crate::model::AuxiliaryKind) slot,
/// never the chat model directly.
///
/// DEPRECATED as a behavior-knob table: `[nightly]` is the authoritative
/// section for the unified pass. this table still parses, and when
/// `[nightly]` is absent the pass honors `enabled`, `min_sessions` and `cron`
/// from it field-by-field as a migration fallback - when `[nightly]` is
/// present it's ignored entirely. prefer `[nightly]`; `pantheon doctor`
/// nudges you to move the knobs. the aux-model pin legitimately stays here:
/// `[nightly]` has no pin, and the pass's memory-distillation step always
/// resolves through the consolidation aux slot.
///
/// the decay curve is gone: `half_life_days` and `min_score` are accepted
/// and ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ConsolidationSection {
    /// allow LLM-backed consolidation steps (candidate distillation). default
    /// false.
    #[serde(default)]
    pub enabled: bool,
    /// IGNORED. the exponential decay curve is gone; promotion is plain
    /// frequency + recency (`min_sessions` / `max_age_days`). kept so old
    /// configs still parse.
    #[serde(default = "default_consolidation_half_life")]
    pub half_life_days: f64,
    /// distinct sessions a candidate must appear in before promotion. default
    /// 3.
    #[serde(default = "default_consolidation_min_sessions")]
    pub min_sessions: usize,
    /// IGNORED. the decayed-score threshold went with the decay curve. kept
    /// so old configs still parse.
    #[serde(default = "default_consolidation_min_score")]
    pub min_score: f64,
    /// default cron for `pantheon consolidate --schedule`. default
    /// `0 3 * * *` (03:00 nightly).
    #[serde(default = "default_consolidation_cron")]
    pub cron: String,
    /// aux model pin for consolidation LLM calls. absent = `auto`;
    /// `"default"` explicitly inherits `[model]`'s provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// aux model pin for consolidation LLM calls. absent = `auto`;
    /// `"default"`/empty explicitly inherits `[model]`'s model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// per-aux request timeout in seconds (`timeout`). absent = client default
    /// for the capability (10s title/judge, 15s embeddings, 30s compression,
    /// 60s reflection/consolidation, 120s otherwise).
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

/// `[nightly]`: unified nightly self-improvement pass.
///
/// one pass: a single ledger scan feeds signal proposals (skill/persona/memory
/// lesson) and memory promotion, with eval-gating, replay validation (strict
/// improvement on held-out tasks), human approval for skills/personas, and a
/// JSONL audit trail.
///
/// LLM-backed steps (proposal refinement via the reflection aux slot, memory
/// distillation via the consolidation aux slot) are off by default: the
/// whole pass is off unless enabled, and anything spending tokens wants a pin.
/// `enabled` is the master switch ([`nightly_enabled`]): `Some(false)` turns
/// it off and always wins, even with a model pinned; `Some(true)` on; `None`
/// on iff a `[nightly.model]` pin - or the `PANTHEON_NIGHTLY_PROVIDER` /
/// `PANTHEON_NIGHTLY_MODEL` env overrides - is present, else off. the
/// pipeline and its repair loop do nothing unless this resolves on.
///
/// when on, LLM-backed steps resolve through `[nightly.model]` when one is
/// configured (never the chat model directly) - pin a small/cheap model there
/// so background self-improvement never competes with interactive chat. with
/// `enabled = true` and no pin, the steps fall back to the reflection /
/// consolidation aux slots.
///
/// memory promotion: a candidate promotes when seen in >= `min_sessions`
/// distinct runs with the newest observation inside `max_age_days`: plain
/// frequency + recency, no decay curve.
///
/// legacy `[reflect]` / `[consolidation]` tables still load; when
/// `[nightly]` is absent, [`nightly_config`] honors them field-by-field as a
/// deprecated migration fallback (`enabled` = either legacy flag,
/// `auto_turns` from `[reflect]`, `min_sessions` + `cron` from
/// `[consolidation]`). when `[nightly]` is present it's the single
/// authoritative section and the legacy tables are ignored for the pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NightlySection {
    /// master switch. `None` (absent) is the default: enabled only when a
    /// model pin is present (a `[nightly.model]` table with a non-empty
    /// provider or model, or `PANTHEON_NIGHTLY_PROVIDER` /
    /// `PANTHEON_NIGHTLY_MODEL` env overrides) - see [`nightly_enabled`].
    /// `Some(true)` forces on, `Some(false)` forces off; explicit `false`
    /// always wins, even with a model pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// model pin for the nightly pass's own LLM steps, following the
    /// [`AuxSection`] pattern:
    ///
    /// ```toml
    /// [nightly.model]
    /// provider = "openai"
    /// model = "gpt-4o-mini"
    /// api_key_env = "OPENAI_API_KEY"   # seeds the PANTHEON_NIGHTLY_API_KEY vault entry
    /// ```
    ///
    /// `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env overrides
    /// win field-wise over the table, like every other aux slot.
    ///
    /// pin present = pass enabled, unless explicitly disabled (`enabled =
    /// false` always wins). absent = unpinned: LLM steps resolve through the
    /// reflection / consolidation aux slots like before. `api_key_env` names
    /// the env var holding the key - never the key itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<AuxSection>,
    /// completed turns before an automatic pass. default 20; `0` disables
    /// the automatic trigger.
    #[serde(default = "default_reflect_auto_turns")]
    pub auto_turns: u32,
    /// distinct runs a memory candidate must appear in before promotion.
    /// default 3.
    #[serde(default = "default_consolidation_min_sessions")]
    pub min_sessions: usize,
    /// newest observation must be within this many days for promotion.
    /// default 30.
    #[serde(default = "default_nightly_max_age_days")]
    pub max_age_days: i64,
    /// default cron for scheduled nightly passes. default `0 3 * * *`
    /// (03:00 nightly).
    #[serde(default = "default_consolidation_cron")]
    pub cron: String,
    /// Headless agent command used to replay held-out validation tasks
    /// (`<command> <prompt>`; stdout is the transcript). Needed only for tasks without
    /// their own exec spec - tasks defined with `replay-tasks add --exec-cmd ...` run
    /// on the built-in headless runner. Absent with no exec spec, replays fail loudly
    /// and the replay gate rejects every skill/persona proposal: strict improvement
    /// can't be measured without a runner.
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

/// whether a [`NightlySection`] carries a usable model pin: a
/// `[nightly.model]` table with a non-empty provider or model. an empty pin
/// table doesn't count - same as no pin. `provider = "default"` counts: the
/// user explicitly pinned to the default model.
pub fn nightly_model_pin_present(section: &NightlySection) -> bool {
    section
        .model
        .as_ref()
        .is_some_and(|m| !m.provider.trim().is_empty() || !m.model.trim().is_empty())
}

/// whether the `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env
/// overrides name a model. part of the pin for enablement purposes, same way
/// env overrides are first-class for every other aux slot.
pub fn nightly_env_pin_present() -> bool {
    std::env::var("PANTHEON_NIGHTLY_PROVIDER").is_ok_and(|v| !v.trim().is_empty())
        || std::env::var("PANTHEON_NIGHTLY_MODEL").is_ok_and(|v| !v.trim().is_empty())
}

/// the single enable rule for the nightly pass. off by default; exactly one
/// of four paths turns it on:
///
/// 1. model pin: `[nightly.model]` present (absent `enabled` flag);
/// 2. `/nightly on` in the TUI (writes `enabled = true`);
/// 3. config edit (`enabled = true` under `[nightly]`);
/// 4. dashboard / mobile-app toggle (`POST /api/nightly/enabled`).
///
/// resolution: `Some(false)` wins over everything (explicit off, even with a
/// model pin); `Some(true)` forces on (even with no pin - callers should warn
/// no model is pinned); `None` is on iff a pin is present - `[nightly.model]`
/// or the `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env
/// overrides, else off.
///
/// this is the master switch: the pipeline and its repair loop do nothing
/// unless this resolves on.
pub fn nightly_enabled(section: &NightlySection) -> bool {
    match section.enabled {
        Some(false) => false,
        Some(true) => true,
        None => nightly_model_pin_present(section) || nightly_env_pin_present(),
    }
}

/// like [`nightly_enabled`], but a `[nightly]` table that makes no enable
/// decision - no flag, no pin, no `PANTHEON_NIGHTLY_*` env - defers to
/// `legacy_enabled` (the legacy `[reflect]` / `[consolidation]` flags). the
/// table alone doesn't mean it decides. an explicit flag or pin still beats
/// legacy.
pub fn nightly_enabled_with_legacy(section: &NightlySection, legacy_enabled: bool) -> bool {
    match section.enabled {
        Some(v) => v,
        None if nightly_model_pin_present(section) || nightly_env_pin_present() => true,
        None => legacy_enabled,
    }
}

/// human-readable reason for a [`nightly_enabled`] verdict: explicit flag
/// state or the pin-implied default. used by `/nightly status`, `pantheon
/// doctor`, and the dashboard so the user sees *why* the loop is on/off.
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
    /// matches the serde field defaults: no explicit flag (off unless a
    /// model pin is present), no pin, standard knob defaults.
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

/// default event-history retention when `[retention]` is absent
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
/// conversation has meaningfully aged, without timestamping every message.
/// before a turn's first model call the pipeline measures the idle gap since
/// the last assistant turn (from the durable ledger, so restart-safe) and,
/// when it matters, appends one coarse human-friendly hint to the outgoing
/// user message - for the API call only, never persisted, never on the
/// system prompt.
///
/// zero tokens by construction (pure string injection), so it defaults on.
/// absent section = all defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TemporalSection {
    /// master switch. default true.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// idle seconds before an elapsed-gap hint fires. default 7200 (2h).
    /// `0` disables the elapsed-gap trigger; the date-rollover trigger
    /// still works.
    #[serde(default = "default_temporal_min_gap_secs")]
    pub min_gap_secs: u64,
    /// hint when the local calendar date rolled over since the last turn,
    /// even on a short gap. default true.
    #[serde(default = "default_true")]
    pub notify_date_change: bool,
    /// IANA timezone name, e.g. `timezone = "Africa/Lagos"`. absent = system
    /// local timezone; unparseable falls back to UTC.
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
/// the ledger is append-only and grows forever; scheduled maintenance prunes
/// it. `keep_days = 0` disables pruning (unbounded growth). finished runs
/// keep their status/title rows and lose only old events; runs still open
/// are never pruned, however old their events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetentionSection {
    /// days of event history to keep. default 90. `0` = disabled.
    #[serde(default = "default_retention_days")]
    pub keep_days: u32,
}

/// run budgets (`[budget]` in config.toml). every key optional, overridable
/// per-session via `/set` (and `/tokens` for the token cap). `0` = unset - a
/// zero cap would end every run before it starts, so it falls back to the
/// default instead of silently bricking the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BudgetSection {
    /// max agent turns per run. default 16.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    /// max tool calls per run. default 32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u32>,
    /// stop after this many consecutive failed tool calls. default 5;
    /// `0` disables. unlike `max_tool_calls`, which bounds total work,
    /// this bounds *fruitless* work: a model retrying the same broken
    /// command can't burn the whole budget. any success resets the streak.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_consecutive_tool_failures: Option<u32>,
    /// max delegation depth. default 2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delegate_depth: Option<u32>,
    /// max pipeline iterations (`pantheon pipeline`). default 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// per-request OUTPUT cap for chat: most tokens the model may emit in
    /// one response. precedence: `/tokens N` > `[budget].max_tokens` >
    /// model's known max output (16k fallback when unknown); winner is
    /// clamped to the model's known max. this has never counted input
    /// tokens - it's not a run budget. absent = uncapped by config;
    /// model/provider default applies. strictly optional, never required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// default max tokens for a delegated child's generation. a per-call
    /// `budget` on the `delegate` tool overrides it. unset = model default.
    /// the child gets its own budget: it never counts against the parent's.
    /// `0` = unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegate_child_max_tokens: Option<u32>,
    /// max total `delegate` calls per run. default 8 - the anti
    /// spawn-army cap on total child activity. `0` = unset -> default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delegations: Option<u32>,
    /// spend cap in US cents per run. absent = no cap yet. when set, the
    /// run stops with BudgetExhausted instead of spending unboundedly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_cents: Option<u32>,
}

impl BudgetSection {
    fn nz(value: Option<u32>, default: u32) -> u32 {
        value.filter(|&v| v > 0).unwrap_or(default)
    }

    /// pipeline iteration cap for `pantheon pipeline`. default 3.
    pub fn pipeline_iterations(&self) -> u32 {
        Self::nz(self.max_iterations, 3)
    }

    /// default token budget for a delegated child's generation. `0` and
    /// unset both mean "no config override" (None): child falls back to the
    /// model default. a per-call `budget` on the `delegate` tool overrides
    /// this default. the returned budget is the child's own - never counts
    /// against the parent.
    pub fn delegate_child_budget(&self) -> Option<u32> {
        self.delegate_child_max_tokens.filter(|&v| v > 0)
    }

    /// max total `delegate` calls per run. default [`DEFAULT_MAX_DELEGATIONS`];
    /// `0`/unset falls back to the default rather than banning delegation.
    pub fn max_delegations_or_default(&self) -> u32 {
        Self::nz(self.max_delegations, DEFAULT_MAX_DELEGATIONS)
    }
}

/// default cap on total `delegate` calls per run when `[budget].max_delegations`
/// is unset or `0`. bounds total child activity so a runaway parent can't
/// spawn an army. hardcoded floor, configurable upward via the key itself.
pub const DEFAULT_MAX_DELEGATIONS: u32 = 8;

/// fallback per-request output cap, only when the session model's max output
/// is unknown (no catalog entry, no custom-model override). named models
/// contribute their own cap instead - see [`resolve_max_output_tokens`].
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_000;

/// minimum context window, in tokens, pantheon will run a session model
/// with. a model whose known window is smaller is refused at chain build
/// (see [`check_context_window`]) - below this the agent loop can't hold a
/// working transcript plus tool traffic. hardcoded, not user-configurable:
/// the floor protects the run, not the bill.
pub const MIN_CONTEXT_WINDOW: u32 = 32_000;

/// resolve the per-request OUTPUT cap for one chat request.
///
/// precedence: the session override (`/tokens N`) beats `[budget].max_tokens`
/// config, which beats the model's known max output (catalog
/// `ModelMeta.max_output_tokens` or `CustomModel.max_output_tokens`).
/// [`DEFAULT_MAX_OUTPUT_TOKENS`] only when the model's max is unknown.
///
/// the winner is then clamped to the model's known max - the clamp applies
/// to user overrides too (`/tokens 20000` on a model capped at 8192 resolves
/// to 8192, no error). not silent at the UI layer: the TUI surfaces it at
/// `/tokens` set time and `/model` switch time. with no known max, the
/// winner stands as-is.
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

/// refuse a session model whose known context window is below
/// [`MIN_CONTEXT_WINDOW`]. `None` (unknown) fails open - provider is trusted
/// to enforce its own limit. called at chain build so a too-small model is
/// rejected before any request goes out.
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
    /// max iterations on one `/goal` before the TUI stops the session's
    /// turns and asks. default 10. overridable per session via
    /// `/goal iterations <n>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
}

/// browser automation (`[browser]` in config.toml). drives the `browser_*`
/// tools through one of the six browser backends (see `pantheon-web`'s
/// lineup; default gsd-browser). the model uses these to *do things on a
/// live site*; looking things up is `web_search` (`[websearch]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ComputerUseSection {
    /// driver id: `cua-driver` (trycua/cua, MIT, default) or `tapp`
    /// (tapp-mcp). unknown leaves computer use unregistered with a diagnostic
    /// recorded on the session (`/status` reads it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// path to the driver binary. absent = resolved from PATH.
    /// install: `/bin/bash -c "$(curl -fsSL https://cua.ai/driver/install.sh)"`
    /// (linux/macOS/windows; no admin). it's a background desktop driver: on
    /// linux it needs X11/XWayland + a running `cua-driver serve` daemon in
    /// your graphical session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
}

/// browser automation (`[browser]` in config.toml). drives the `browser_*`
/// tools through one of the six browser backends (see `pantheon-web`'s
/// lineup; default gsd-browser). the model uses these to *do things on a
/// live site*; looking things up is `web_search` (`[websearch]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct BrowserSection {
    /// master switch. default true; false registers no `browser_*` tools at
    /// all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// `gsd` (default), `chromiumoxide`, `steel`, `browserbase`, `playwright`,
    /// or `camofox`. unknown ids warn and fall back to `gsd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// path to the gsd-browser binary. absent = resolved from PATH.
    /// install: `npm install -g @opengsd/gsd-browser` (or build
    /// https://github.com/open-gsd/gsd-browser from source).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// `browser_act` clicks the top semantic-intent candidate with no
    /// minimum score upstream. default true = the call carries the
    /// `browser.act` capability and the run parks for human approval.
    /// set false only if you trust autonomous low-confidence clicks.
    /// only the `gsd` backend registers `browser_act`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act_require_approval: Option<bool>,
    /// idle seconds before a run's browser daemon is stopped. default 900.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_secs: Option<u64>,
    /// secret name for the gsd-browser auth-vault key, resolved via the
    /// secrets broker and injected as `GSD_BROWSER_VAULT_KEY`.
    /// default "GSD_BROWSER_VAULT_KEY". unset/empty = no vault.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_key_secret: Option<String>,
    /// secret name for the Steel api key (cloud sessions).
    /// default "STEEL_API_KEY". unset/empty = steel backend unusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steel_api_key_secret: Option<String>,
    /// steel REST base url override for self-hosted steel
    /// (`docker run ghcr.io/steel-dev/steel-browser`). absent = steel cloud
    /// (`https://api.steel.dev`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steel_base_url: Option<String>,
    /// secret name for the Browserbase api key.
    /// default "BROWSERBASE_API_KEY". unset/empty = browserbase unusable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browserbase_api_key_secret: Option<String>,
    /// browserbase project id (required by `POST /v1/sessions`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browserbase_project_id: Option<String>,
    /// path to the `playwright-cli` binary (`@playwright/cli`).
    /// absent = resolved from PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playwright_binary: Option<String>,
    /// path to the Chrome/Chromium binary for `chromiumoxide`.
    /// absent = chromiumoxide's auto-detect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chrome_binary: Option<String>,
    /// headless mode for `chromiumoxide`. default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    /// `--no-sandbox` for `chromiumoxide`. default false: chrome runs
    /// sandboxed unless the operator opts in (headless CI containers running
    /// as root need it, since chrome refuses to launch as root without it).
    /// opting in prints a loud warning at launch; the flag never applies to
    /// remote CDP connections, which launch nothing locally. never default
    /// to unsandboxed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_sandbox: Option<bool>,
    /// per-command timeout in seconds for browser backends. default 120.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// camoufox anti-detect backend. absent = launcher defaults (headless,
    /// BrowserForge fingerprints).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camofox: Option<CamofoxSection>,
}

/// camoufox anti-detect browser (`[browser.camofox]` in config.toml).
/// maps 1:1 onto the launcher options the camofox backend consumes (see
/// `pantheon-web`'s `CamofoxConfig`); unset properties are auto-filled from
/// BrowserForge fingerprints by the launcher, so prefer leaving them unset
/// over inventing inconsistent values. every key optional; defaults match
/// the backend's.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CamofoxSection {
    /// python interpreter for the JSON-over-stdio shim. absent = `python3`
    /// on PATH.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<String>,
    /// launch headless. default true. (`headless = "virtual"` needs
    /// `xvfb`; use `headless_virtual` for that instead.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless: Option<bool>,
    /// xvfb "virtual" headless instead of true headless (needs `xvfb`).
    /// default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headless_virtual: Option<bool>,
    /// fingerprint OS: `windows`, `macos`, or `linux`. absent = launcher's
    /// BrowserForge default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// human-like cursor movement in seconds. absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub humanize_secs: Option<f64>,
    /// derive timezone/locale/geolocation from proxy IP (needs the `[geoip]`
    /// install extra). default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geoip: Option<bool>,
    /// explicit locale override, e.g. `"en-US"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// explicit timezone override, e.g. `"America/New_York"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// proxy server url, e.g. `"http://proxy:8080"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_server: Option<String>,
    /// proxy username (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_username: Option<String>,
    /// secret name for the proxy password, resolved via the secrets broker at
    /// registration. never a raw value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_password_secret: Option<String>,
    /// block image loads (perf). default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_images: Option<bool>,
    /// block WebRTC (IP-leak prevention). default false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_webrtc: Option<bool>,
    /// use BrowserForge-backed real fingerprint presets. default true
    /// (upstream recommendation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_preset: Option<bool>,
    /// per-command timeout in seconds. absent = `[browser] timeout_secs`
    /// (default 120).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// seconds to wait for the browser to launch on session start. first
    /// launch after a fetch can take minutes. default 300.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
}

/// web search (`[websearch]` in config.toml). drives the `web_search` tool
/// (see `pantheon-web`): query -> snippets. for *looking things up*;
/// interacting with a site is `browser_*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WebsearchSection {
    /// master switch. default true. keyed providers register only when a key
    /// resolves; keyless ones (Marginalia, `*_free` fallbacks, `ddgs`)
    /// register without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// one of the `pantheon-web` registry ids
    /// (`tinyfish` (recommended), `tavily`, `ollama`, `exa`,
    /// `marginalia`, `brave`, `firecrawl`, `searxng`, `perplexity`,
    /// `exa_free`, `parallel_free`, `firecrawl_free`, `keenable_free`,
    /// `ddgs`).
    /// the setup wizard lists them with auth requirements. an unknown id
    /// leaves `web_search` unregistered with a warning at session start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// secret name for the provider api key, resolved via the secrets broker.
    /// default = the chosen provider's own default (e.g. "TAVILY_API_KEY" for
    /// tavily). never hardcoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_secret: Option<String>,
    /// endpoint override applied to every provider (not just self-hosted
    /// ones): setting it for a keyed hosted provider redirects that provider's
    /// calls - and its api key - to the override host. only set it for
    /// self-hosted instances (e.g. a local searxng url) or test mirrors.
    /// absent = provider's default endpoint.
    ///
    /// DEPRECATED: one global override leaks one provider's endpoint into
    /// another's requests the moment the provider switches (a Tavily URL
    /// following a switch to Exa sends Exa calls - and the Exa key - to the
    /// Tavily host). use `base_urls` below, one entry per provider. kept for
    /// deserialization so old configs still load; never applied directly. the
    /// one exception is a searxng-only shim in `resolve_websearch_section`
    /// (legacy value applies to searxng alone until the operator moves it to
    /// `base_urls.searxng`). `validate()` warns when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// per-provider endpoint overrides: `[websearch.base_urls]` `exa = "https://..."`.
    /// only the entry for the configured provider is ever applied; an entry
    /// for another never leaks into this provider's requests. absent entry =
    /// provider's default endpoint.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub base_urls: std::collections::HashMap<String, String>,
    /// default result count per query. default 8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u8>,
}

impl WebsearchSection {
    /// the endpoint override for `provider`, if configured for exactly that
    /// provider. case-insensitive on the id; blank values ignored. the legacy
    /// global `base_url` is NOT consulted here - it's never an input to
    /// per-provider resolution.
    pub fn base_url_for(&self, provider: &str) -> Option<&str> {
        let want = provider.trim().to_ascii_lowercase();
        self.base_urls
            .iter()
            .find(|(k, _)| k.trim().to_ascii_lowercase() == want)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    }
}

/// one toggleable tool group from the setup wizard's Tools screen.
///
/// session search isn't here: it's default-on and has no toggle, so it can
/// never be switched off.
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
    /// all groups, in Tools-screen order
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

    /// the `[tools]` config key for this group
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

    /// parse a `[tools]` key back. unknown keys are `None`, so a typo
    /// degrades to "ignored" rather than a hard failure.
    pub fn parse(s: &str) -> Option<ToolGroup> {
        Self::all().into_iter().find(|g| g.key() == s)
    }

    /// short human label for the Tools screen
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
            ToolGroup::CodeIntel => "Code Intel (LSP + git-undo + worktrees + code graph)",
        }
    }
}

/// tool-group enablement (`[tools]` in config.toml). written by the setup
/// wizard's Tools screen; the runtime registers only enabled groups - a
/// disabled group never appears in the model's tool list.
///
/// every field is `Option<bool>` with absent = enabled, so an absent section
/// leaves all groups on. two tools ride existing groups instead of their own:
/// `session_search` is gated by `memory` (past-conversation recall) and the
/// OAuth connector tools (`github_api`, `google_api`, `notion_api`,
/// `spotify_api`) are gated by `plugins` (external integrations, like MCP
/// server tools).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolsSection {
    /// Surface profile. `full` (default) registers everything the group
    /// toggles allow. `safe` registers only non-mutating tools (file reads,
    /// search, memory reads, `ask_user`, `load_tools`, `schedule` list) — the
    /// profile an untrusted origin (a webhook payload, a shared channel) must
    /// run under so the model is not even offered a shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
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
    /// STT/TTS backends (`[stt]` / `[tts]`). off = sections not honored:
    /// no voice backend constructed or reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<bool>,
    /// vision aux slot. off = `[vision]` never resolves, so a configured
    /// vision model can't be consulted by accident.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// video-analysis aux slot. off = `[video]` never resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_analysis: Option<bool>,
    /// computer-use (CUA driver). driver is linux pre-release; the flag is
    /// the durable record and gates its tool registration when the driver
    /// lands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use: Option<bool>,
    /// plugin tools and MCP server projections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<bool>,
    /// code-intel group: LSP diagnostics (`lsp.open` / `lsp.diagnostics` /
    /// `lsp.shutdown`), repo-level git undo (`gitundo.snapshot` /
    /// `gitundo.list` / `gitundo.restore` / `gitundo.delete`), managed
    /// worktrees, and the native code graph. off = none registered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_intel: Option<bool>,
}

impl ToolsSection {
    /// effective enablement: absent = on.
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

    /// build a section from an enabled-group list (the wizard's answer): only
    /// deviations from the all-on default are written, so a default setup
    /// keeps the config quiet about a choice it didn't make.
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

/// `[formatters]`: opt-in post-edit formatting. absent or `enabled = false`
/// leaves every write untouched (the pre-formatter behavior).
///
/// when enabled, a formatter runs after `write_file` / `apply_files`
/// succeeds. built-in rules cover the common languages; `commands` overrides
/// or adds rules by extension. a command template may contain `{file}`; when
/// it doesn't, the path is appended. commands run through the operator's
/// shell, so the table is trusted config, never model-authored input.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct FormattersSection {
    #[serde(default)]
    pub enabled: bool,
    /// hard cap for one formatter invocation. default 10s.
    #[serde(default = "default_formatter_timeout_secs")]
    pub timeout_secs: u64,
    /// extension -> command template, e.g. `".rs" = "rustfmt {file}"`.
    /// keys may be written with or without the leading dot.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub commands: std::collections::HashMap<String, String>,
}

fn default_formatter_timeout_secs() -> u64 {
    10
}

/// third-party packages the skill library needs (`[skill_deps]` in
/// config.toml). install-time dependencies of pantheon itself: the setup
/// wizard's "Skill dependencies" screen detects each and offers
/// install-or-skip, instead of every skill carrying its own manual `pip
/// install` step.
///
/// only the skips are recorded. a present dependency writes nothing; a
/// skipped one lands in `skipped` so `pantheon doctor` can report the gap
/// later - the same "config records the choice; doctor reports the gap"
/// contract the wizard uses for skipped local provider binaries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SkillDepsSection {
    /// stable ids the user skipped at install-or-skip time (see the
    /// skill-deps registry in `pantheon-tui`). sorted when written so the
    /// file stays diff-stable.
    #[serde(default)]
    pub skipped: Vec<String>,
}

/// TUI chrome (`[tui]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TuiSection {
    /// active theme (`pantheon`, `dark`, `light`). absent = default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    /// modal vim editing for the composer (`/vim`). absent = off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vim: Option<bool>,
    /// context-warning threshold in percent: one threshold the composer's
    /// `ctx NN%` element and the sidebar context meter both warn at. absent
    /// or out of range (0, 100] = 80.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_warn_pct: Option<f64>,
}

impl Default for RetentionSection {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

// ANCHOR(mcp-workstream): `[mcp]` / `[mcp.servers.<name>]` config section.
// the config file is the primary MCP server source; migration declaration
// files (`<data_dir>/mcp/*.json`) fill in names the config doesn't define.

/// MCP servers (`[mcp]` in config.toml). the runtime manager
/// (`pantheon-mcp`) spawns/connects the enabled servers and projects their
/// tools into the `ToolRegistry` as `mcp_<server>_<tool>`.
///
/// ## enablement: which gate wins
///
/// a server launches iff enabled AND its identity is trusted:
///
/// - bundled catalog servers are first-party: the `enabled` flag is the
///   only gate. fresh installs enable zero - enabling is always explicit
///   (this file, the dashboard's Tools & MCPs page, the mobile app, or the
///   agent proposing through the approval flow).
/// - custom servers additionally need operator consent in the unified
///   approval store (`<data_dir>/mcp/.approvals.json`,
///   [`crate::approval`]): an unapproved third-party server never connects
///   no matter what the flag says.
///
/// disabling only flips the switch - a recorded approval persists, so
/// re-enabling the same binary/URL resumes without re-consent. the approval
/// binds to the server's content hash: if the binary or URL changes, the
/// approval lapses and consent is asked again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct McpSection {
    /// master switch. default true; false means no MCP server is launched
    /// at all, whatever the per-server flags say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// one table per server: `[mcp.servers.<name>]`.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub servers: std::collections::HashMap<String, McpServerEntry>,
}

/// one MCP server (`[mcp.servers.<name>]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServerEntry {
    /// `stdio`, `sse`, or `http` (streamable HTTP). default `stdio`.
    #[serde(default = "default_mcp_transport")]
    pub transport: String,
    /// program to spawn (stdio only). PATH or a path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// program arguments (stdio only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// extra env for the server process (stdio only). a value starting
    /// with `env:` resolves from the operator's environment at spawn time
    /// (e.g. `GITHUB_TOKEN = "env:GITHUB_TOKEN"` reads `$GITHUB_TOKEN`);
    /// anything else is literal. never log these values: the manager
    /// prints variable *names* only.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub env: std::collections::HashMap<String, String>,
    /// endpoint URL (sse/http): the SSE stream URL or streamable-HTTP
    /// endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// per-server switch. default false: a fresh install enables zero,
    /// and enabling is always explicit (config file, dashboard Tools &
    /// MCPs page, mobile app, or the agent proposing through approval).
    /// see [`McpSection`] for how this interacts with the approval store.
    ///
    /// false for every server, bundled or custom: serde defaults can't see
    /// the table's key name, so a two-tier default isn't expressible at
    /// parse time. every enablement path (`set_bundled_enabled`, the
    /// dashboard and app writes, the agent's `enable_mcp` tool)
    /// materializes `enabled` explicitly, so this default only governs
    /// hand-written tables. no automatic migration rewrites a config to
    /// `enabled = true`: switching on process-spawning integrations the
    /// operator never approved would defeat the gate.
    #[serde(default = "mcp_server_enabled_default")]
    pub enabled: bool,
    /// per-request timeout in seconds. absent = 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// extra http headers for sse/http, sent with every request. values
    /// starting with `env:` resolve from the operator's environment at
    /// connect time (same convention as `env`); anything else is literal.
    /// this is how a remote MCP endpoint expecting `Authorization:
    /// Bearer <token>` gets it. header names are logged, values never.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub headers: std::collections::HashMap<String, String>,
    /// remote auth mode. `"oauth"` runs the MCP OAuth 2.1 flow (metadata
    /// discovery, dynamic client registration, PKCE, loopback redirect,
    /// encrypted token storage). absent/`"none"` keeps header-only
    /// behavior. OAuth is only valid for `sse`/`http`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// OAuth scopes to request. empty = the ones advertised by the
    /// protected-resource metadata, falling back to the auth server's
    /// `scopes_supported`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// pre-registered OAuth client id. absent -> pantheon registers a
    /// public client dynamically if the auth server supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

fn default_mcp_transport() -> String {
    "stdio".to_string()
}

fn mcp_server_enabled_default() -> bool {
    false
}

impl McpServerEntry {
    /// config problems for one server table, as `validate()` strings
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
        match self.auth.as_deref().map(str::trim) {
            None | Some("") | Some("none") => {}
            Some("oauth") => {
                if self.transport == "stdio" {
                    out.push(format!(
                        "mcp.servers.{name}: auth = \"oauth\" requires sse or http transport"
                    ));
                }
            }
            Some(other) => out.push(format!(
                "mcp.servers.{name}: unknown auth {other:?} (none|oauth)"
            )),
        }
        out
    }
}

/// one bundled plugin's enablement (`[plugins.<name>]` in config.toml).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PluginEntry {
    /// master switch. when the entry exists, this beats the plugin manifest's
    /// own `enabled` - the config file is the single enablement state (config
    /// file, dashboard, mobile app, or the agent proposing through approval).
    /// when the entry is absent, the bundled manifest's `enabled` is the
    /// default (false for every bundled plugin except noisegate).
    #[serde(default)]
    pub enabled: bool,
    /// catalog kind (`tool` / `hook`), stamped when written through a pantheon
    /// surface so the table is self-describing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// catalog version the entry was written for. informational only; a
    /// catalog bump doesn't lapse enablement the way a third-party code change
    /// lapses an approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// `[mcp_synthesis]`: model that bounds large MCP tool results into a short
/// note before they enter context (compression's pattern, scoped to MCP
/// results). absent = `auto` -> run's default model summarizes.
pub type McpSynthesisSection = AuxSection;

/// `[extraction]`: structured-extraction model. pulls fields and records out
/// of prose and tool outputs into typed values the runtime can act on. absent
/// = `auto` -> run's default model extracts.
pub type ExtractionSection = AuxSection;

/// `[rerank]`: rerank model. scores and orders search + memory-retrieval
/// candidates before they enter context. absent = `auto` -> run's default
/// model reranks.
pub type RerankSection = AuxSection;

/// `[planner]`: planner model for a future planner/worker split, where
/// planning and execution run on different models. absent = `auto` -> run's
/// default model plans.
pub type PlannerSection = AuxSection;

/// `[repair]`: repair model for the nightly fix loop - the only slot that
/// revises drafts (eval-reject and replay-reject paths) - and for
/// diagnosis/repair of broken MCP servers, scheduled tasks, and tools.
/// absent = OFF: the slot adds no policy entry and fix-loop draft revision is
/// unavailable (the loop falls back to plain retries, then escalation).
///
/// `max_repairs_per_task_per_day` caps how many repair investigations one
/// scheduled task may trigger per UTC day (default 3): without it a flapping
/// task burns API budget on investigator sessions without bound. 0 disables
/// repair investigations entirely.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct RepairSection {
    #[serde(flatten)]
    pub aux: AuxSection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repairs_per_task_per_day: Option<u32>,
}

impl RepairSection {
    /// effective per-task daily cap: configured value, else 3
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

/// `RepairSection` derefs to its flattened aux slot wherever an `&AuxSection`
/// is expected (model resolution, TUI pin lists).
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

/// `[verify]`: adversarial verifier model. after a delegated sub-agent
/// completes, it re-reads the task goal + the child's claimed result and
/// tries to falsify the claim. absent = OFF: no policy entry, no verification.
/// pin a cheap model here to turn post-delegation verification on.
pub type VerifySection = AuxSection;

/// default per-agent spawn cap for `[swarm]`: how many live children one
/// agent may have before further spawns are refused.
fn default_swarm_max_subagents() -> u32 {
    4
}
/// default delegation depth for `[swarm]`: primary = 0, children 1, deeper
/// spawns refused.
fn default_swarm_max_depth() -> u32 {
    2
}
/// default live-agent ceiling for `[swarm]`.
fn default_swarm_max_concurrent() -> u32 {
    4
}

/// What happens to a message that arrives while a turn is running.
///
/// * `steer` - delivered into the running turn at its next boundary (the
///   current mid-turn behaviour).
/// * `followup` (default) - queued; the newest message replaces an earlier
///   queued one, and it runs when the turn ends.
/// * `collect` - queued and *merged* with anything already queued, so a burst
///   of channel messages becomes one turn.
/// * `interrupt` - the running turn is cancelled and this message runs next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QueueMode {
    Steer,
    #[default]
    Followup,
    Collect,
    Interrupt,
}

impl QueueMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Followup => "followup",
            Self::Collect => "collect",
            Self::Interrupt => "interrupt",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "steer" => Some(Self::Steer),
            "followup" | "follow-up" | "queue" => Some(Self::Followup),
            "collect" | "batch" | "merge" => Some(Self::Collect),
            "interrupt" | "cancel" => Some(Self::Interrupt),
            _ => None,
        }
    }

    pub fn all() -> [Self; 4] {
        [Self::Steer, Self::Followup, Self::Collect, Self::Interrupt]
    }
}

/// `[runtime]`: process-wide execution limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuntimeSection {
    /// Live top-level turns this process admits before queueing. Delegated
    /// children inherit their parent's slot, so this bounds provider
    /// concurrency, not the size of a swarm (that is `[swarm] max_concurrent`).
    /// Default 4; `PANTHEON_MAX_CONCURRENT_TURNS` overrides at startup.
    #[serde(default = "default_max_concurrent_turns")]
    pub max_concurrent_turns: u32,
    /// Seconds a queued turn waits for a free slot before failing with
    /// `RT_CAPACITY_TIMEOUT`. Default 120.
    #[serde(default = "default_turn_queue_wait_secs")]
    pub turn_queue_wait_secs: u64,
    /// What a message that arrives mid-turn does. Default `followup`.
    #[serde(default)]
    pub queue_mode: QueueMode,
}

fn default_max_concurrent_turns() -> u32 {
    4
}

fn default_turn_queue_wait_secs() -> u64 {
    120
}

impl Default for RuntimeSection {
    fn default() -> Self {
        Self {
            max_concurrent_turns: default_max_concurrent_turns(),
            turn_queue_wait_secs: default_turn_queue_wait_secs(),
            queue_mode: QueueMode::default(),
        }
    }
}

/// `[swarm]`: caps for sub-agent delegation and swarm runs. every key
/// optional; absent keys (or an absent section) take the documented defaults.
/// config-side spelling of `pantheon_runtime::swarm::Caps` - see
/// `Caps::from_swarm_section` for the mapping (`max_subagents` becomes the
/// per-agent spawn cap).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SwarmSection {
    /// per-agent spawn cap. default 4.
    #[serde(default = "default_swarm_max_subagents")]
    pub max_subagents: u32,
    /// max delegation depth (primary = 0). default 2.
    #[serde(default = "default_swarm_max_depth")]
    pub max_depth: u32,
    /// max live sub-agents swarm-wide. default 4.
    #[serde(default = "default_swarm_max_concurrent")]
    pub max_concurrent: u32,
    /// when false, a child agent (depth >= 1) trying to delegate gets a
    /// structured refusal instead of a grandchild. default true.
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

/// one curated delegation route: a name the model picks, a one-line
/// description it reads, and the agent profile the child runs as.
/// routes are an allowlist: only profiles named here can be picked via a
/// route. a direct `delegate(agent = ...)` call is unaffected by this list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DelegationRoute {
    /// what the model picks. must be unique within the section.
    #[serde(default)]
    pub name: String,
    /// one line the model reads when choosing. never a credential, endpoint,
    /// or model id - names and descriptions only.
    #[serde(default)]
    pub description: String,
    /// the `[agents.<name>]` profile the child runs as.
    #[serde(default)]
    pub profile: String,
    /// credential names this route may grant its child. empty = legacy
    /// unrestricted behavior for back-compat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<String>,
}

/// `[delegation]`: curated routes for the `delegate` tool, plus an optional
/// default. absent section = no routes: `delegate` works exactly as before,
/// and a `route` argument is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DelegationSection {
    /// routes, in the order the model sees them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<DelegationRoute>,
    /// used when a `delegate` call names neither agent nor route. must name
    /// one of `routes`; unknown default is refused at the call, like an
    /// unknown route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl DelegationSection {
    /// route names, in config order. pure, for error messages and the tool
    /// description the model reads.
    pub fn route_names(&self) -> Vec<&str> {
        self.routes.iter().map(|r| r.name.as_str()).collect()
    }

    /// resolve a route name, fail-closed: unknown names and names declared
    /// more than once are both refused, never silently swapped for another
    /// profile. the error names the valid routes so the caller can retry.
    pub fn resolve(&self, name: &str) -> Result<&DelegationRoute, String> {
        let matches: Vec<&DelegationRoute> =
            self.routes.iter().filter(|r| r.name == name).collect();
        match matches.as_slice() {
            [route] => Ok(route),
            [] => Err(format!(
                "unknown delegation route {name:?}; valid routes: {}",
                self.route_names().join(", ")
            )),
            _ => Err(format!(
                "delegation route {name:?} is declared more than once; fix [delegation] so each route name is unique"
            )),
        }
    }

    /// the default route, if configured and resolves. unknown default is an
    /// error at the call, not a fallback.
    pub fn default_route(&self) -> Result<Option<&DelegationRoute>, String> {
        match self.default.as_deref() {
            None => Ok(None),
            Some(name) => self.resolve(name).map(Some),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct MemorySection {
    /// backend name from the catalog: native | http | ...
    ///
    /// authoritative: on session load this wins over
    /// `<data_dir>/memory-backend.toml`, and the resolved selection is synced
    /// back to that file (see `pantheon_memory::sync_selection_from_config`),
    /// so the two never drift silently. a conflict logs a warning naming both.
    /// an absent section, or a blank `backend`, means no opinion: the selection
    /// file wins untouched. to switch backends, edit this section (or use the
    /// memory-backend selection / setup, which writes both files).
    pub backend: String,
    #[serde(default)]
    pub options: std::collections::HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ServerSection {
    /// AG-UI HTTP port. 0 ("auto") resolves to the 18789 default in `serve`
    /// and remotes: a literal ephemeral bind was undiscoverable (nothing
    /// reported the bound port, remotes refused 0, GenUI URLs baked in `:0`).
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub host: String,
}

/// `[secrets]`: the run's secrets-boundary policy. both lists empty by default
/// (fail closed).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SecretsSection {
    /// env vars readable via the `env:` secret-name form. exact var names
    /// (`"MY_KEY"`) or `PREFIX_*` wildcards (`"PANTHEON_*"`); `"*"` alone
    /// allows all (explicit opt-out).
    ///
    /// default: empty - `env:` lookups resolve nothing. secrets must come from
    /// `PANTHEON_SECRET_*` or a durable vault, so `env:AWS_SECRET_ACCESS_KEY`
    /// can never exfiltrate an arbitrary host variable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_allowlist: Vec<String>,
    /// manifest-declared env vars the plugin supervisor may copy from the host
    /// into plugin subprocesses (same entry syntax as above).
    ///
    /// default: empty - plugins get PATH + pantheon-set vars only. a
    /// project-controlled manifest can declare any name, so a declared name
    /// alone never crosses the boundary; only an entry here lets a host var
    /// (including API keys) reach plugin code.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugin_env_allowlist: Vec<String>,
}

/// an `[agents.<name>]` table: durable identity for one persistent agent.
///
/// the durable half: name, persona source files, memory namespace and
/// capability policy live in config. persona files are path references
/// (repo-relative or absolute), never inlined, so secrets that drift into a
/// SOUL.md stay out of config snapshots.
///
/// this re-exports the core type rather than defining a second one. the
/// declaration, inheritance and namespace rules live in
/// `pantheon_api::agent_profile`, so profiles are config-document types the
/// runtime can consume without depending on a client.
///
pub use crate::agent_profile::AgentProfile as AgentIdentity;

/// why an `[agents.<name>]` table isn't usable, in `doctor`'s wording.
///
/// every rule lives in `ProfileRegistry`, where `resolve` and `validate_all`
/// enforce them, including the ones inheritance adds (a child may not claim a
/// parent's namespace, a `profile` naming no declared agent is an error). a
/// second copy here would let `doctor` pass a config the runtime then refuses.
fn agent_table_problems(all: &std::collections::HashMap<String, AgentIdentity>) -> Vec<String> {
    let mut reg = ProfileRegistry::new();
    let mut problems = Vec::new();
    // a malformed table name is reported here rather than through
    // `problems`, because it can't be inserted and would otherwise hide
    // every other problem in the file.
    for (name, agent) in all {
        if let Err(e) = reg.insert(name, agent.clone()) {
            problems.push(e.to_string());
        }
    }
    // `problems` covers registry-wide rules: unknown policies, namespace
    // clashes, inheritance cycles, nonexistent parents.
    problems.extend(reg.problems("coder").into_iter().map(|e| e.to_string()));
    problems
}

static ADMIN: std::sync::RwLock<Option<AdminSection>> = std::sync::RwLock::new(None);

/// install the admin policy at CLI startup
pub fn set_admin_policy(policy: AdminSection) {
    if let Ok(mut guard) = ADMIN.write() {
        *guard = Some(policy);
    }
}

/// whether a provider/model pair is permitted. no policy = allow.
pub fn admin_allows(provider: &str, model: &str) -> bool {
    if let Ok(guard) = ADMIN.read() {
        if let Some(ref policy) = *guard {
            return policy.provider_allowed(provider) && policy.model_allowed(provider, model);
        }
    }
    true
}

/// phase-aware routing: map task types to model pins.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RoutingSection {
    /// task type -> model pin. task types: code, chat, analysis, etc.
    #[serde(default)]
    pub pins: std::collections::HashMap<String, ModelPin>,
}

/// a pinned provider + model pair for a task type.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ModelPin {
    pub provider: String,
    pub model: String,
}

impl RoutingSection {
    pub fn resolve(&self, task_type: &str) -> Option<&ModelPin> {
        self.pins.get(task_type)
    }
}

/// admin policy section. deny wins over allow.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdminSection {
    #[serde(default)]
    pub deny_providers: Vec<String>,
    #[serde(default)]
    pub deny_models: Vec<String>,
    #[serde(default)]
    pub allow_providers: Vec<String>,
    #[serde(default)]
    pub allow_models: Vec<String>,
}

impl AdminSection {
    pub fn provider_allowed(&self, provider: &str) -> bool {
        if self
            .deny_providers
            .iter()
            .any(|p| p.eq_ignore_ascii_case(provider))
        {
            return false;
        }
        if self.allow_providers.is_empty() {
            return true;
        }
        self.allow_providers
            .iter()
            .any(|p| p.eq_ignore_ascii_case(provider))
    }

    pub fn model_allowed(&self, provider: &str, model: &str) -> bool {
        if self
            .deny_models
            .iter()
            .any(|m| m.eq_ignore_ascii_case(model))
        {
            return false;
        }
        if self.allow_models.is_empty() {
            return true;
        }
        self.allow_models
            .iter()
            .any(|m| m.eq_ignore_ascii_case(model))
            || self
                .allow_models
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&format!("{provider}/{model}")))
    }
}

/// egress policy section from config.toml.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EgressSection {
    /// `off` = unrestricted, `allowlist` = only listed hosts, `offline` = deny all.
    #[serde(default)]
    pub mode: String,
    /// host suffixes allowed when mode is `allowlist`. empty = deny all.
    #[serde(default)]
    pub allowlist: Vec<String>,
}

/// `[limits]`: user-adjustable safety caps. every field optional; absent =
/// built-in default in parens. env overrides (`PANTHEON_LIMIT_<FIELD>`) beat
/// config, so deployments can tune without editing it.
/// `[auth]`: credential pool policy. keys themselves live once per provider in
/// the dotenv under `PANTHEON_KEY_<PROVIDER>`; this section only tunes how
/// stacked keys are picked.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AuthSection {
    /// key rotation across stacked keys: `fill_first` (default, try in order,
    /// rotate on 401/403/429), `round_robin` (spread attempts across keys),
    /// `random` (random key per attempt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rotation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LimitsSection {
    /// Max decoded voice audio bytes per transcribe request (25 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_audio_bytes: Option<usize>,
    /// Max speak text chars per request (64000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_text_chars: Option<usize>,
    /// Max image bytes per attachment (25 MiB).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_bytes: Option<usize>,
    /// Max images per message (10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images_per_message: Option<usize>,
    /// Max videos per message (5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub videos_per_message: Option<usize>,
    /// Video frames sampled per video (16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_frames: Option<usize>,
    /// Search synthesis brief cap chars (4000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_synthesis_chars: Option<usize>,
    /// MCP synthesis note cap chars (3000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_synthesis_chars: Option<usize>,
    /// Vision description cap chars (4000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_chars: Option<usize>,
    /// Video summary cap chars (6000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video_chars: Option<usize>,
    /// Planner max steps (20).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner_steps: Option<usize>,
}

/// Resolve one limit: env override, then config, then the default.
pub fn limit_or_env(field: &str, configured: Option<usize>, default: usize) -> usize {
    let env_name = format!("PANTHEON_LIMIT_{}", field.to_ascii_uppercase());
    if let Ok(v) = std::env::var(&env_name) {
        if let Ok(n) = v.trim().parse::<usize>() {
            if n > 0 {
                return n;
            }
        }
    }
    match configured {
        Some(n) if n > 0 => n,
        _ => default,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    /// the agent this install runs as (`agent = "zeus"`). must name a
    /// declared `[agents.<name>]` table. this is the key that selects an
    /// agent - a legacy free-form `profile` label key was removed; old
    /// configs still carrying it load fine, the key is ignored.
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
    /// `[image]`: image-generation backend (see [`ImageSection`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<ImageSection>,
    /// `[video]` pin. absent = `auto` -> run's default model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<ScheduledSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_synthesis: Option<McpSynthesisSection>,
    /// `[extraction]` pin. absent = `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extraction: Option<ExtractionSection>,
    /// `[rerank]` pin. absent = `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank: Option<RerankSection>,
    /// `[planner]` pin for a future planner/worker split. absent = `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner: Option<PlannerSection>,
    /// `[repair]` pin. absent = OFF: no policy entry, fix-loop draft revision
    /// unavailable - the loop falls back to plain retries, then escalation.
    /// the nightly fix loop's draft revision (eval-reject / replay-reject)
    /// resolves only through this slot, never reflection. broken-MCP/schedule/
    /// tool diagnosis (workstream 3) will resolve it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairSection>,
    /// `[verify]`: adversarial verifier pin. absent = OFF: no policy entry,
    /// post-delegation verification never runs. pin a cheap model to turn it on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<VerifySection>,
    /// `[swarm]`: sub-agent delegation caps. absent = all defaults
    /// ([`SwarmSection::default`]). partial tables fine; each key defaults
    /// independently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm: Option<SwarmSection>,
    /// `[runtime]`: concurrency limits for live turns (see [`RuntimeSection`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeSection>,
    /// `[delegation]`: curated routes for `delegate` (see [`DelegationSection`]).
    /// absent = no routes: `delegate` works as before, a `route` argument refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<CompressionSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_gen: Option<TitleGenSection>,
    /// `[reflect]`: behavior knobs + aux model pin for reflection.
    /// DEPRECATED for the knobs (moved to `[nightly]`, which is authoritative);
    /// still honored field-by-field when `[nightly]` is absent. the model pin
    /// (`provider`/`model`/`timeout`) stays here. absent = LLM steps off
    /// (`enabled = false`), and if enabled later, `auto` = run's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reflect: Option<ReflectSection>,
    /// `[consolidation]`: behavior knobs + aux model pin for consolidation.
    /// DEPRECATED for the knobs (moved to `[nightly]`); still honored
    /// field-by-field when `[nightly]` is absent. the model pin stays here.
    /// absent = LLM steps off (`enabled = false`); if enabled later, `auto` =
    /// run's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidation: Option<ConsolidationSection>,
    /// `[nightly]`: unified nightly self-improvement pass (`pantheon nightly`).
    /// the single authoritative section. when absent, legacy sections are
    /// honored field-by-field as a deprecated migration fallback (see
    /// [`nightly_config`]); when present, legacy sections are ignored entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nightly: Option<NightlySection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt: Option<VoiceSection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tts: Option<VoiceSection>,
    /// `[voice]`: live voice-mode knobs. absent = live mode disabled with
    /// default limits (see [`LiveVoiceSection`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice: Option<LiveVoiceSection>,
    /// `[limits]`: user-adjustable safety caps. absent = built-in defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<LimitsSection>,
    /// `[auth]`: credential rotation policy. absent = fill_first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthSection>,
    pub policy: Option<PolicyPreset>,
    /// `permission_mode`: how much scrutiny an approval-gated tool call gets
    /// (`ask` / `smart` / `allow_all`). absent = `ask`, the conservative
    /// default: a run parks exactly where the deterministic policy says.
    /// `smart` consults the `[judge]` auxiliary; `allow_all` clears approval
    /// parks but never overrides a policy `deny`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    pub memory: Option<MemorySection>,
    pub server: Option<ServerSection>,
    /// secrets-boundary policy (`env:` lookups, plugin subprocess env).
    /// absent = both allowlists empty (fail closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<SecretsSection>,
    /// event-ledger retention (`[retention]`). absent = 90-day default;
    /// `keep_days = 0` disables pruning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionSection>,
    /// tacit temporal awareness (`[temporal]`). absent = enabled with defaults
    /// (2h gap, date-rollover hints, system local timezone).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporal: Option<TemporalSection>,
    /// run budgets (`[budget]`). absent = runtime defaults (16 turns, 32
    /// tool calls, depth 2, uncapped tokens, 3 pipeline iterations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetSection>,
    /// `/goal` behavior (`[goal]`). absent = 10-iteration default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalSection>,
    /// browser automation (`[browser]`). absent = enabled with defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<BrowserSection>,
    /// `[gateway]`: channel options (`[gateway.channels.<name>]`).
    /// absent = defaults (text replies everywhere, no per-channel voice).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewaySection>,
    /// desktop control through a driver (`[computer_use]`). absent = the
    /// ComputerUse group toggle decides; driver used when its binary is on
    /// PATH. `driver` picks the driver (`cua-driver` default, or `tapp`),
    /// `binary` pins its path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer_use: Option<ComputerUseSection>,
    /// egress policy for outbound provider requests. absent = no restriction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressSection>,
    /// admin policy: deny/allow lists for providers and models.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admin: Option<AdminSection>,
    /// phase-aware routing: map task types to model pins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingSection>,

    /// web search (`[websearch]`). absent = enabled with defaults; the tool
    /// only registers when an API key resolves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub websearch: Option<WebsearchSection>,
    /// tool-group enablement (`[tools]`). absent = every group enabled; the
    /// runtime only registers enabled groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsSection>,
    /// opt-in post-edit formatting (`[formatters]`). absent = disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatters: Option<FormattersSection>,
    /// third-party skill dependencies (`[skill_deps]`). absent = the wizard's
    /// skill-deps screen never ran; present with empty `skipped` = everything
    /// it checked was found or installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_deps: Option<SkillDepsSection>,
    /// MCP servers (`[mcp]` / `[mcp.servers.<name>]`). absent = launcher off.
    /// ANCHOR(mcp-workstream): config half of MCP wiring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<McpSection>,
    /// bundled plugin enablement (`[plugins]` / `[plugins.<name>]`).
    /// single source of truth for bundled-plugin enablement, shared by config,
    /// dashboard, mobile app, and the agent's `enable_plugin` tool. a bundled
    /// plugin not named here (or named without `enabled`) is disabled: bundled
    /// plugins ship off. third-party plugins are unaffected - their gate is
    /// the approval store (`pantheon_api::approval`), not this table. entries
    /// under names the bundled catalog doesn't know are inert.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub plugins: std::collections::HashMap<String, PluginEntry>,
    /// TUI chrome (`[tui]`). absent = defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tui: Option<TuiSection>,
    /// user-defined providers (`pantheon model` -> Custom provider). empty when unset.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub custom_providers: std::collections::HashMap<String, CustomProviderSection>,
    /// durable per-agent identities (§4). empty = all runs anonymous, the
    /// pre-identity behavior (back-compat).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub agents: std::collections::HashMap<String, AgentIdentity>,
}

/// process-lifetime agent-profile override, set once from CLI flags
/// (`pantheon --profile <name>` / `-p` / `--agent`) before any session exists.
/// slots into [`Config::resolve_profile`] between an explicit call-site
/// override and the `agent = "..."` config value, so the TUI launch path -
/// which resolves with `None` - picks it up without session construction
/// needing a new parameter. set-once: second call wins, never read before
/// the CLI has parsed.
static PROFILE_OVERRIDE: std::sync::OnceLock<std::sync::Mutex<Option<String>>> =
    std::sync::OnceLock::new();

/// set the process-lifetime profile override (the `--profile` flag)
pub fn set_profile_override(name: Option<String>) {
    let slot = PROFILE_OVERRIDE.get_or_init(|| std::sync::Mutex::new(None));
    *slot.lock().unwrap_or_else(|e| e.into_inner()) = name;
}

/// current process-lifetime profile override, if the CLI set one
pub fn profile_override() -> Option<String> {
    PROFILE_OVERRIDE
        .get()
        .and_then(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).clone())
}

/// top-level config keys the loader understands.
///
/// load policy for unknown keys: warn on stderr, non-fatal. a typo'd section
/// must not nuke a working install, but must not pass silently either.
/// `pantheon config set` keeps its loud rejection; this is the load path's
/// quieter twin, applied uniformly in [`Config::load`].
///
/// only top-level keys are checked: tables like `[custom_providers.*]` and
/// `[agents.*]` take arbitrary names by design, and per-section key lists
/// would rot. a typo *inside* a section (e.g. `model.providr`) is still
/// silent; catching that needs `deny_unknown_fields` on the section structs.
///
/// kept in sync with the `Config` struct by
/// `known_config_keys_match_struct_fields` below: the test builds an
/// exhaustive `Config` literal (no `..Default::default()`), so adding a
/// field breaks compilation until this list grows with it.
const KNOWN_CONFIG_KEYS: &[&str] = &[
    "agent",
    "model",
    "judge",
    "limits",
    "auth",
    "embeddings",
    "search_synthesis",
    "vision",
    "image",
    "video",
    "scheduled",
    "mcp_synthesis",
    "extraction",
    "rerank",
    "planner",
    "repair",
    "verify",
    "swarm",
    "delegation",
    "compression",
    "title_gen",
    "reflect",
    "consolidation",
    "nightly",
    "stt",
    "tts",
    "voice",
    "policy",
    "permission_mode",
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
    "formatters",
    "skill_deps",
    "mcp",
    "plugins",
    "tui",
    "custom_providers",
    "agents",
    "egress",
    "admin",
    "routing",
];

/// unknown keys anywhere in a config document, as dotted paths, sorted. pure
/// so the load policy is unit-testable without touching the filesystem.
///
/// serde silently drops unknown fields on deserialize, so the trick is the
/// one `config set` already uses for one key, generalized: parse the
/// document, round-trip through [`Config`], diff the input table against the
/// re-serialized one. a key in the input but not the output names no field
/// of the document type - `model.providr`, a typo inside a section, or a
/// typo'd section itself. this is a lint, not a load gate: loads still
/// succeed (the unknown key is inert), but doctor surfaces these so a hand
/// edit that "did nothing" gets explained instead of failing open.
///
/// two diff quirks tolerated, because the alternative is false positives on
/// valid configs:
/// - empty tables/arrays serialize away under `skip_serializing_if`
///   (`Vec::is_empty`, empty option maps), so an empty-valued unknown key
///   isn't flagged. a typo with a real value always is.
/// - map sections (`[agents.<name>]`, `[custom_providers.<name>]`,
///   `[gateway.channels.<name>]`) recurse by name: the dynamic name itself
///   is never "unknown", but unknown fields *inside* are flagged, because
///   the entry type re-serializes only its fields.
pub fn unknown_inner_keys(text: &str) -> Vec<String> {
    let input: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let cfg: Config = match input.clone().try_into() {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let output = match toml::Value::try_from(&cfg) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    fn is_empty_value(v: &toml::Value) -> bool {
        matches!(v, toml::Value::Table(t) if t.is_empty())
            || matches!(v, toml::Value::Array(a) if a.is_empty())
    }
    fn walk(prefix: &str, input: &toml::Value, output: &toml::Value, out: &mut Vec<String>) {
        let (Some(in_table), Some(out_table)) = (input.as_table(), output.as_table()) else {
            return;
        };
        for (key, in_value) in in_table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match out_table.get(key) {
                None => {
                    if !is_empty_value(in_value) {
                        out.push(path);
                    }
                }
                Some(out_value) => walk(&path, in_value, out_value, out),
            }
        }
    }
    let mut found = Vec::new();
    walk("", &input, &output, &mut found);
    found.sort();
    found
}

/// unknown top-level keys in a config document, sorted. pure so the load
/// policy is unit-testable without touching the filesystem.
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
    /// effective event-retention window in days. absent `[retention]` =
    /// 90-day default; `keep_days = 0` disables pruning.
    pub fn retention_days(&self) -> u32 {
        self.retention
            .as_ref()
            .map(|r| r.keep_days)
            .unwrap_or(DEFAULT_RETENTION_DAYS)
    }
    /// max `/goal` iterations. absent `[goal]` = 10.
    pub fn goal_iterations(&self) -> u32 {
        self.goal
            .as_ref()
            .and_then(|g| g.max_iterations)
            .filter(|&v| v > 0)
            .unwrap_or(10)
    }
    /// effective live voice-mode settings. absent `[voice]` = defaults
    /// (live mode disabled; see [`LiveVoiceSection`]).
    pub fn live_voice(&self) -> LiveVoiceSection {
        self.voice.clone().unwrap_or_default()
    }
    /// effective swarm caps. absent `[swarm]` = [`SwarmSection::default`];
    /// a partial table fills missing keys with defaults.
    pub fn swarm_section(&self) -> SwarmSection {
        self.swarm.clone().unwrap_or_default()
    }
    /// max pipeline iterations (`pantheon pipeline`). absent `[budget]` = 3.
    pub fn pipeline_iterations(&self) -> u32 {
        self.budget
            .as_ref()
            .map(BudgetSection::pipeline_iterations)
            .unwrap_or(3)
    }
    /// is bundled plugin `name` enabled? absent entry (or no `enabled`) =
    /// disabled: bundled plugins ship off. unknown names disable too - entries
    /// outside the bundled catalog are inert.
    pub fn plugin_enabled(&self, name: &str) -> bool {
        self.plugins.get(name).map(|e| e.enabled).unwrap_or(false)
    }
    /// is MCP server `name` enabled? absent entry (or no `enabled`) =
    /// disabled: bundled servers ship off. unknown names disable too (custom
    /// servers still come from their declaration files in the launcher; this
    /// helper only answers the config-section question).
    pub fn mcp_server_enabled(&self, name: &str) -> bool {
        self.mcp
            .as_ref()
            .and_then(|m| m.servers.get(name))
            .map(|e| e.enabled)
            .unwrap_or(false)
    }
    /// load the config, or explain why it couldn't be read.
    ///
    /// `load(...).ok()` at a dozen call sites was throwing away a
    /// `CONFIG_PARSE` error that names the file, line, column. one typo then
    /// looked like an install that couldn't reach a provider, with no mention
    /// of the broken file.
    ///
    /// a missing config isn't an error, so this returns `None` for that case
    /// and prints for a config that exists but doesn't parse.
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
            // unknown keys warn on stderr, non-fatal (see
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
    /// load for a read-modify-write save: a missing file reads as a default
    /// document (the save creates it), but a file that exists and doesn't
    /// parse is an error, never a default. `load(...).unwrap_or_default()` at
    /// a save site would replace a corrupt `config.toml` with defaults + one
    /// field: `/theme`, `/vim`, `/model`, `/nightly` and CLI `model` would all
    /// clobber the user's config on a typo. fail closed.
    pub fn load_for_save(data_dir: &Path) -> Result<Self, PantheonError> {
        match Self::load(data_dir) {
            Ok(c) => Ok(c),
            Err(e) if e.code == "CONFIG_OPEN" => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }
    /// agent profiles declared in this config, as a resolvable registry.
    ///
    /// bridge from config text to the runtime's profile layer. building it
    /// here (instead of at each call site) means every entry point - terminal,
    /// scheduler, AG-UI - sees the same declarations, and an inheritance chain
    /// broken by a typo is reported identically everywhere.
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

    /// policy preset an agent gets when neither it nor an ancestor names one.
    /// global config wins, so a profile inherits the install's baseline
    /// rather than a hardcoded runtime default.
    fn default_policy_preset(&self) -> &'static str {
        self.policy
            .map(crate::config_schema::PolicyPreset::as_str)
            .unwrap_or("coder")
    }

    /// resolve the agent profile this config selects.
    ///
    /// order: explicit `--agent`, else `agent = "..."`, else `default`.
    /// an undeclared name is an error, never a silent fallback - if someone
    /// asked for `zeus` and got the default agent's memory and persona, they'd
    /// have no way to notice.
    ///
    /// a config with no `[agents]` at all is fine: that's every install from
    /// before profiles, and those runs stay anonymous until the user declares
    /// one. only a *named* agent must exist.
    pub fn resolve_profile(
        &self,
        override_name: Option<&str>,
    ) -> Result<Option<EffectiveProfile>, ProfileError> {
        // order: explicit call-site override, CLI `--profile` flag,
        // `agent = "..."`, `default`.
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
            // nothing declared, nothing asked for: anonymous, as before.
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
        // atomic write: tmp then rename, matching the rest of the codebase.
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
    /// validate the config for doctor. one error per problem.
    ///
    /// pure: reads only the config document, never the data dir, secrets
    /// broker, or network. that's why channel token EXISTENCE isn't checked
    /// here: `token_secret` can only be resolved through the broker (process
    /// env, then `<data_dir>/gateway.env`, then vault), which validate has no
    /// access to. shape is checked (empty vs absent); existence is a doctor
    /// probe (`check_gateway_channels` in pantheon-doctor).
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
                if let Some(k) = f.key_env.as_deref() {
                    if k.trim().is_empty() {
                        problems.push(format!(
                            "model.fallbacks[{i}].key_env is empty; omit it to resolve via the provider's catalog/env credential"
                        ));
                    }
                }
                // a fallback that can't help at runtime is rejected here:
                // identical to the default or a duplicate of an earlier
                // entry - both burn 3 retries per turn. unknown providers
                // can't be checked here (pantheon-api doesn't depend on the
                // catalog), so `pantheon fallback add` warns about those.
                if f.provider == m.provider && f.model == m.model {
                    problems.push(format!(
                        "model.fallbacks[{i}] is identical to the default model; it adds retries, not resilience"
                    ));
                }
                if m.fallbacks[..i]
                    .iter()
                    .any(|g| g.provider == f.provider && g.model == f.model)
                {
                    problems.push(format!(
                        "model.fallbacks[{i}] duplicates an earlier fallback entry"
                    ));
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
        // `permission_mode` spelling: the runtime parses it with
        // `PermissionMode::parse` and falls back to `Ask` on anything
        // unparseable (fail-safe but silent). a typo like `smrt` would read
        // as `ask` with no complaint, and a rewrite would drop
        // `smart`/`allow_all`. flag it here so doctor and `config set` see it.
        if let Some(mode) = self
            .permission_mode
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            if crate::permission_mode::PermissionMode::parse(mode).is_none() {
                problems.push(format!(
                    "permission_mode {mode:?} is not ask|smart|allow_all (unparseable values fall back to ask)"
                ));
            }
        }
        // every aux section validates identically: resolvable api_key_env
        // when named, non-zero timeout. empty provider/model is fine - it
        // inherits the `[model]` target, so there's no "empty field" error
        // anymore.
        fn aux_problem(name: &str, timeout: &Option<u64>, problems: &mut Vec<String>) {
            if *timeout == Some(0) {
                problems.push(format!(
                    "{name}.timeout is 0: aux requests would instantly time out"
                ));
            }
        }
        for slot in AUX_SLOTS {
            if let Some(s) = cfg_section(self, slot) {
                aux_problem(slot.name, &s.timeout, &mut problems);
            }
        }
        // `[reflect]` carries its own pin; each field inherits independently
        // now, so a half-set pin is valid.
        if let Some(r) = &self.reflect {
            aux_problem("reflect", &r.timeout, &mut problems);
        }
        // `[consolidation]` likewise.
        if let Some(c) = &self.consolidation {
            aux_problem("consolidation", &c.timeout, &mut problems);
        }
        if let Some(mem) = &self.memory {
            if mem.backend.trim().is_empty() {
                problems.push("memory.backend is empty".into());
            }
        }
        if let Some(v) = &self.voice {
            if let Some(backend) = v.realtime_backend.as_deref().map(str::trim) {
                if !backend.is_empty() && backend != "openai" {
                    problems.push(format!(
                        "voice.realtime_backend {backend:?} is not openai; omit it to use the local STT/agent/TTS path"
                    ));
                }
            }
        }
        for (section, v) in [("stt", &self.stt), ("tts", &self.tts)] {
            if let Some(v) = v {
                // accepted backends are the ones the runtime can construct
                // (`open_stt` / `open_tts`, mirrored in STT/TTS_BACKENDS).
                // a command|openai-only check rejects configs the wizard
                // itself writes (groq, piper-local, ...), sending doctor into
                // a "rerun setup" loop that rewrites identical values.
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
                // per-backend required options, mirroring the runtime's
                // from_options constructors: shell-out backends need their
                // command, piper-local needs its voice model id, the legacy
                // "openai" shape needs the provider it aliases.
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
        // agent identity tables validate at load: slugs, known policies, no
        // namespace clashes. an invalid [agents.*] table fails doctor loudly
        // instead of silently running anonymous.
        // one registry-wide check: the rules are per-registry (namespace
        // clashes, inheritance chains), so validating table-by-table would
        // report the same fault once per profile. every problem is
        // reported, not just the first.
        problems.extend(agent_table_problems(&self.agents));
        // a selected `agent` naming no declared profile is a config error.
        // note: this is `agent`, not `profile` - `profile` is a free-form
        // label `setup` writes as "default" with no `[agents]` table at all,
        // so treating it as a selector would fail every pre-agent-profiles
        // install.
        if let Some(selected) = &self.agent {
            if !self.agents.contains_key(selected) {
                problems.push(format!(
                    "agent {selected:?} is not declared; add [agents.{selected}] or \
                     remove the agent setting"
                ));
            }
        }
        if let Some(server) = &self.server {
            // Only reject hosts that can't be a bind address at all. a LAN
            // address (192.168.1.10) binds fine, and the loopback set isn't
            // a bindability rule - the dashboard warns rather than refuses
            // on non-loopback binds. empty is allowed ("default");
            // whitespace or an unparseable value is not.
            let host = server.host.trim();
            if !host.is_empty() && host.contains(char::is_whitespace) {
                problems.push(format!(
                    "server.host {:?} contains whitespace and cannot be a bind address",
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
        // `[custom_providers.*]`: the runtime lowercases api_mode with a
        // fallthrough to OpenAI, so "anthroic" silently runs the wrong wire
        // protocol. reject anything but openai|anthropic here.
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
                    // an endpoint with no URL is skipped at registration, so a
                    // declared-but-unusable provider would look configured
                    // while doing nothing.
                    if sec.base_url.trim().is_empty() {
                        problems.push(format!(
                            "custom_providers.{name}.base_url is empty: the endpoint is skipped at registration"
                        ));
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
                    // a channel profile must name a declared agent profile
                    // (same rule as top-level `agent`); an undeclared one fails
                    // only when the channel starts, with no config-time hint.
                    // an explicitly-set-but-empty token secret is also not the
                    // same as absent (absent falls back to the platform's
                    // standard token).
                    if let Some(p) = ch
                        .profile
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        if !self.agents.contains_key(p) {
                            problems.push(format!(
                                "gateway.channels.{name}.profile {p:?} is not declared; add [agents.{p}] or remove the profile setting"
                            ));
                        }
                    }
                    if let Some(t) = ch.token_secret.as_deref() {
                        if t.trim().is_empty() {
                            problems.push(format!(
                                "gateway.channels.{name}.token_secret is empty; omit it to use the platform's standard token"
                            ));
                        }
                        // existence NOT checked here: validate is pure (no
                        // data dir, no broker). a named but unresolvable
                        // secret is a doctor probe (check_gateway_channels),
                        // not a config error.
                    }
                }
            }
        }
        // `[browser]`: an unknown backend warns and falls back to gsd at
        // registration, so the typo only shows as "why is gsd running". flag
        // it here like the `[stt]`/`[tts]` checks above.
        if let Some(b) = &self.browser {
            if let Some(backend) = b
                .backend
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if !BROWSER_BACKENDS.contains(&backend.to_ascii_lowercase().as_str()) {
                    problems.push(format!(
                        "browser.backend {backend:?} is not {} (unknown ids fall back to gsd)",
                        BROWSER_BACKENDS.join("|")
                    ));
                }
            }
        }
        // `[websearch]`: an unknown provider leaves `web_search` unregistered
        // with a session-start warning. flag it here so doctor names the typo.
        if let Some(w) = &self.websearch {
            if let Some(provider) = w
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if !WEBSEARCH_PROVIDERS.contains(&provider.to_ascii_lowercase().as_str()) {
                    problems.push(format!(
                        "websearch.provider {provider:?} is not {}",
                        WEBSEARCH_PROVIDERS.join("|")
                    ));
                }
            }
            // the legacy global base_url is ignored: it leaks one provider's
            // endpoint into another's requests. loud here so someone who set
            // it for SearXNG moves it to [websearch.base_urls.searxng] instead
            // of wondering why their self-hosted instance stopped seeing
            // traffic.
            if w.base_url
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some()
            {
                problems.push(
                    "websearch.base_url is deprecated; move it to [websearch.base_urls.<provider>] for the provider it belongs to".to_string(),
                );
            }
        }
        // `[temporal]`: an unparseable timezone falls back to UTC at runtime,
        // so a typo silently shifts every hint. parse with the same IANA
        // table the runtime uses.
        if let Some(t) = &self.temporal {
            if let Some(tz) = t
                .timezone
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if tz.parse::<chrono_tz::Tz>().is_err() {
                    problems.push(format!(
                        "temporal.timezone {tz:?} is not a parseable IANA timezone name (e.g. \"Africa/Lagos\")"
                    ));
                }
            }
        }
        // `[swarm]`: 0 does NOT mean "default" here (unlike
        // `[budget].max_delegations`) - it's passed through verbatim, so 0
        // silently disables delegation for that dimension. to disable
        // delegation use `allow_child_spawn = false`, not a 0 cap.
        if let Some(s) = &self.swarm {
            if s.max_subagents == 0 {
                problems.push(
                    "swarm.max_subagents is 0: no sub-agent can ever spawn; use allow_child_spawn = false to disable delegation instead"
                        .into(),
                );
            }
            if s.max_depth == 0 {
                problems.push(
                    "swarm.max_depth is 0: no child can run (depth 0 is the primary itself); use allow_child_spawn = false to disable delegation instead"
                        .into(),
                );
            }
            if s.max_concurrent == 0 {
                problems.push(
                    "swarm.max_concurrent is 0: no sub-agent can run live; use allow_child_spawn = false to disable delegation instead"
                        .into(),
                );
            }
        }
        // `[nightly]`: repair thresholds of 0 invert their meaning - every
        // schedule (0 consecutive failures) and every unused tool (0 calls,
        // 0 errors) counts as broken. negative `max_age_days` can never
        // match a recency window, so memory promotion silently never runs.
        if let Some(n) = &self.nightly {
            if n.repair_mcp_max_failures == Some(0) {
                problems.push(
                    "nightly.repair_mcp_max_failures is 0: every failed/backoff MCP server counts as broken"
                        .into(),
                );
            }
            if n.repair_schedule_max_failures == Some(0) {
                problems.push(
                    "nightly.repair_schedule_max_failures is 0: every scheduled job counts as broken"
                        .into(),
                );
            }
            if n.repair_tool_min_calls == Some(0) {
                problems.push(
                    "nightly.repair_tool_min_calls is 0: every unused tool (0 calls, 0 errors) counts as broken"
                        .into(),
                );
            }
            if n.max_age_days < 0 {
                problems.push(format!(
                    "nightly.max_age_days is {}: a negative recency window can never match, so memory promotion never runs",
                    n.max_age_days
                ));
            }
        }
        // `[formatters]`: the runtime drops empty commands and extension
        // keys silently, so a typo'd rule looks enabled while formatting
        // nothing.
        if let Some(f) = &self.formatters {
            let mut keys: Vec<&String> = f.commands.keys().collect();
            keys.sort();
            for key in keys {
                let normalized = key.trim().trim_start_matches('.');
                if normalized.is_empty() {
                    problems.push(
                        "formatters.commands has an empty extension key (dropped at runtime)"
                            .into(),
                    );
                    continue;
                }
                if let Some(cmd) = f.commands.get(key) {
                    if cmd.trim().is_empty() {
                        problems.push(format!(
                            "formatters.commands.{key:?} is empty (dropped at runtime)"
                        ));
                    }
                }
            }
        }
        // `[delegation]`: routes resolve at call time, so a route naming an
        // undeclared profile, a duplicate name, or a default naming no route
        // fails only when the model picks it.
        if let Some(d) = &self.delegation {
            let mut seen: Vec<&str> = Vec::new();
            for route in &d.routes {
                if route.name.trim().is_empty() {
                    problems.push("delegation.routes has a route with an empty name".into());
                } else if seen.contains(&route.name.as_str()) {
                    problems.push(format!(
                        "delegation.routes.{:?} is declared more than once",
                        route.name
                    ));
                } else {
                    seen.push(route.name.as_str());
                }
                if route.profile.trim().is_empty() {
                    problems.push(format!(
                        "delegation.routes.{:?}.profile is empty: the route names no agent profile",
                        route.name
                    ));
                } else if !self.agents.contains_key(route.profile.trim()) {
                    problems.push(format!(
                        "delegation.routes.{:?}.profile {:?} is not declared; add [agents.{:?}] or fix the route",
                        route.name,
                        route.profile,
                        route.profile.trim()
                    ));
                }
            }
            if let Some(default) = d
                .default
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                if !d.routes.iter().any(|r| r.name == default) {
                    problems.push(format!(
                        "delegation.default {default:?} names no declared route"
                    ));
                }
            }
        }
        // `[tui]`: an unknown theme falls back to the default silently at
        // startup, so a typo reads as "my theme didn't apply" with no hint.
        if let Some(t) = &self.tui {
            if let Some(theme) = t.theme.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                if !TUI_THEMES.contains(&theme.to_ascii_lowercase().as_str()) {
                    problems.push(format!(
                        "tui.theme {theme:?} is not {} (unknown names fall back to the default theme)",
                        TUI_THEMES.join("|")
                    ));
                }
            }
        }
        problems
    }
}

/// one aux slot: everything that varies per capability. this table drives
/// target resolution, key seeding, validation, and the `auxiliaries()`
/// fan-out - adding a capability means adding one row.
pub struct AuxSlot {
    pub kind: crate::model::AuxiliaryKind,
    /// config section name (for diagnostics)
    pub name: &'static str,
    /// `PANTHEON_<PREFIX>_PROVIDER` / `PANTHEON_<PREFIX>_MODEL`
    pub env_prefix: &'static str,
    /// vault entry the section's key env seeds.
    /// absent section falls back to `auto` (the default model).
    /// false only for embeddings (absent = local embedder, never chat).
    pub auto: bool,
    pub section: for<'a> fn(&'a Config) -> Option<&'a AuxSection>,
}

pub const AUX_SLOTS: &[AuxSlot] = &[
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Judge,
        name: "judge",
        env_prefix: "JUDGE",
        auto: true,
        section: |c| c.judge.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Compression,
        name: "compression",
        env_prefix: "COMPRESSION",
        auto: true,
        section: |c| c.compression.as_ref().map(|s| &s.aux),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::TitleGen,
        name: "title_gen",
        env_prefix: "TITLEGEN",
        auto: true,
        section: |c| c.title_gen.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Embeddings,
        name: "embeddings",
        env_prefix: "EMBEDDINGS",
        auto: false,
        section: |c| c.embeddings.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::SearchSynthesis,
        name: "search_synthesis",
        env_prefix: "SEARCH_SYNTHESIS",
        auto: true,
        section: |c| c.search_synthesis.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Vision,
        name: "vision",
        env_prefix: "VISION",
        auto: true,
        section: |c| c.vision.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Video,
        name: "video",
        env_prefix: "VIDEO",
        auto: true,
        section: |c| c.video.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Scheduled,
        name: "scheduled",
        env_prefix: "SCHEDULED",
        auto: true,
        section: |c| c.scheduled.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::McpSynthesis,
        name: "mcp_synthesis",
        env_prefix: "MCP_SYNTHESIS",
        auto: true,
        section: |c| c.mcp_synthesis.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Extraction,
        name: "extraction",
        env_prefix: "EXTRACTION",
        auto: true,
        section: |c| c.extraction.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Rerank,
        name: "rerank",
        env_prefix: "RERANK",
        auto: true,
        section: |c| c.rerank.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Planner,
        name: "planner",
        env_prefix: "PLANNER",
        auto: true,
        section: |c| c.planner.as_ref(),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Repair,
        name: "repair",
        env_prefix: "REPAIR",
        auto: false,
        section: |c| c.repair.as_ref().map(|r| &r.aux),
    },
    AuxSlot {
        kind: crate::model::AuxiliaryKind::Verify,
        name: "verify",
        env_prefix: "VERIFY",
        // the other documented exception alongside embeddings: absent
        // section = slot OFF, never `auto`. verification only runs when
        // someone pins a model here - an unverified delegation must be a
        // choice, not an accident of defaults.
        auto: false,
        section: |c| c.verify.as_ref(),
    },
];

/// borrow one slot's section for validation
pub fn cfg_section<'a>(cfg: &'a Config, slot: &AuxSlot) -> Option<&'a AuxSection> {
    (slot.section)(cfg)
}

#[cfg(test)]
mod config_max_tokens_tests {
    use super::*;

    #[test]
    fn limit_or_env_prefers_env_then_config_then_default() {
        std::env::set_var("PANTHEON_LIMIT_TEST_FIELD", "42");
        assert_eq!(super::limit_or_env("test_field", Some(7), 99), 42);
        std::env::remove_var("PANTHEON_LIMIT_TEST_FIELD");
        assert_eq!(super::limit_or_env("test_field", Some(7), 99), 7);
        assert_eq!(super::limit_or_env("test_field", None, 99), 99);
        assert_eq!(super::limit_or_env("test_field", Some(0), 99), 99);
    }

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

    /// `[model] timeout_ms` parses when present, is absent when the
    /// key is missing, and a non-numeric value fails parsing (it is
    /// never silently read as a different timeout).
    #[test]
    fn model_timeout_ms_parses() {
        let cfg: Config = toml::from_str(
            "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\ntimeout_ms = 45000\n",
        )
        .expect("config parses");
        assert_eq!(cfg.model.as_ref().unwrap().timeout_ms, Some(45_000));
        let cfg: Config = toml::from_str("[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n")
            .expect("config parses");
        assert_eq!(cfg.model.as_ref().unwrap().timeout_ms, None);
        assert!(toml::from_str::<Config>(
            "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\ntimeout_ms = \"fast\"\n"
        )
        .is_err());
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
mod delegation_section_tests {
    use super::*;

    fn section() -> DelegationSection {
        DelegationSection {
            routes: vec![
                DelegationRoute {
                    name: "fast".into(),
                    description: "Cheap and quick: lookups, summaries, simple edits.".into(),
                    profile: "coder".into(),
                    credentials: vec![],
                },
                DelegationRoute {
                    name: "deep".into(),
                    description: "Slow and careful: hard debugging and design work.".into(),
                    profile: "researcher".into(),
                    credentials: vec![],
                },
            ],
            default: Some("fast".into()),
        }
    }

    #[test]
    fn routes_parse_from_toml_and_round_trip() {
        let cfg: Config = toml::from_str(
            "[delegation]\ndefault = \"fast\"\n\
             [[delegation.routes]]\nname = \"fast\"\ndescription = \"Cheap and quick.\"\nprofile = \"coder\"\n\
             [[delegation.routes]]\nname = \"deep\"\ndescription = \"Slow and careful.\"\nprofile = \"researcher\"\n",
        )
        .expect("config parses");
        let section = cfg.delegation.as_ref().expect("[delegation] present");
        assert_eq!(section.routes.len(), 2);
        assert_eq!(
            section.resolve("deep").expect("deep resolves").profile,
            "researcher"
        );
        assert_eq!(
            section
                .default_route()
                .expect("default resolves")
                .map(|r| r.name.as_str()),
            Some("fast")
        );
        let back = toml::to_string(&cfg).expect("config serializes");
        assert!(back.contains("[delegation]"));
        assert!(back.contains("profile = \"coder\""));
    }

    #[test]
    fn unknown_route_names_the_valid_routes() {
        let err = section()
            .resolve("local")
            .expect_err("local is not configured");
        assert!(err.contains("unknown delegation route"));
        assert!(err.contains("fast"));
        assert!(err.contains("deep"));
    }

    #[test]
    fn duplicate_route_name_is_refused_not_silently_picked() {
        let mut section = section();
        section.routes.push(DelegationRoute {
            name: "fast".into(),
            description: "A second fast.".into(),
            profile: "other".into(),
            credentials: vec![],
        });
        let err = section.resolve("fast").expect_err("duplicate fast");
        assert!(err.contains("more than once"));
    }

    #[test]
    fn unknown_default_is_an_error_and_no_default_is_none() {
        let mut section = section();
        section.default = Some("missing".into());
        assert!(section.default_route().is_err());
        section.default = None;
        assert!(section.default_route().expect("no default").is_none());
    }

    #[test]
    fn absent_section_serializes_to_nothing() {
        let cfg = Config::default();
        assert!(cfg.delegation.is_none());
        let back = toml::to_string(&cfg).expect("config serializes");
        assert!(!back.contains("delegation"));
    }
}

#[cfg(test)]
mod gateway_multi_agent_tests {
    use super::*;

    /// panic-safe guard: the profile override is process-global, so every
    /// test that sets it must clear it even on failure - a leak silently
    /// changes what other tests resolve.
    ///
    /// any test touching it must hold this lock for its whole body: two
    /// parallel tests can otherwise interleave and one reads the other's
    /// override (`cli_override_beats_the_agent_setting` failed on CI with
    /// `UnknownProfile { zeus }` because a concurrent guard set zeus between
    /// the set and the read).
    static OVERRIDES_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    /// holds the lock for the whole test, so two override tests never hold
    /// the global at once.
    struct OverrideGuard {
        // named so the lock lives to the end of the test body; `dead_code`
        // doesn't count a destructor as a read, hence the allow.
        #[allow(dead_code)]
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl OverrideGuard {
        fn set(name: &str) -> Self {
            let lock = OVERRIDES_LOCK.get_or_init(|| std::sync::Mutex::new(()));
            let _lock = lock.lock().unwrap_or_else(|e| e.into_inner());
            set_profile_override(Some(name.to_string()));
            OverrideGuard { _lock }
        }
        fn clear() -> Self {
            let lock = OVERRIDES_LOCK.get_or_init(|| std::sync::Mutex::new(()));
            let _lock = lock.lock().unwrap_or_else(|e| e.into_inner());
            set_profile_override(None);
            OverrideGuard { _lock }
        }
    }
    impl Drop for OverrideGuard {
        fn drop(&mut self) {
            set_profile_override(None);
            // self._lock (the MutexGuard) gets dropped by the compiler after
            // this, releasing OVERRIDES_LOCK for the next override test.
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
        // pre-profiles behavior untouched: no [agents] table and no
        // override = anonymous, not an error.
        let _guard = OverrideGuard::clear();
        let cfg = Config::default();
        assert!(cfg.resolve_profile(None).unwrap().is_none());
    }

    #[test]
    fn cli_override_with_unknown_name_is_an_error_not_anonymous() {
        // asking for a profile that doesn't exist must fail loudly -
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
mod runtime_section_tests {
    use super::*;

    #[test]
    fn runtime_section_parses_limits_and_queue_mode() {
        let cfg: Config = toml::from_str(
            "[runtime]\nmax_concurrent_turns = 2\nturn_queue_wait_secs = 30\nqueue_mode = \"collect\"\n",
        )
        .expect("parse");
        let rt = cfg.runtime.expect("runtime section");
        assert_eq!(rt.max_concurrent_turns, 2);
        assert_eq!(rt.turn_queue_wait_secs, 30);
        assert_eq!(rt.queue_mode, QueueMode::Collect);
        // defaults when the section is absent
        let empty: Config = toml::from_str("").expect("parse empty");
        assert!(empty.runtime.is_none());
        let d = RuntimeSection::default();
        assert_eq!(d.max_concurrent_turns, 4);
        assert_eq!(d.queue_mode, QueueMode::Followup);
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

    #[test]
    fn validate_flags_browser_websearch_temporal_swarm_nightly_formatter() {
        let mut cfg = cfg_with_model();
        cfg.browser = Some(BrowserSection {
            backend: Some("chrme".to_string()),
            ..Default::default()
        });
        cfg.websearch = Some(WebsearchSection {
            provider: Some("googol".to_string()),
            ..Default::default()
        });
        cfg.temporal = Some(TemporalSection {
            timezone: Some("Mars/Olympus".to_string()),
            ..Default::default()
        });
        cfg.swarm = Some(SwarmSection {
            max_subagents: 0,
            max_depth: 0,
            max_concurrent: 0,
            ..Default::default()
        });
        cfg.nightly = Some(NightlySection {
            repair_mcp_max_failures: Some(0),
            repair_schedule_max_failures: Some(0),
            repair_tool_min_calls: Some(0),
            max_age_days: -1,
            ..Default::default()
        });
        cfg.formatters = Some(FormattersSection {
            enabled: true,
            commands: std::collections::HashMap::from([
                ("rs".to_string(), String::new()),
                (String::new(), "rustfmt {file}".to_string()),
            ]),
            ..Default::default()
        });
        let problems = cfg.validate();
        for needle in [
            "browser.backend",
            "websearch.provider",
            "temporal.timezone",
            "swarm.max_subagents",
            "swarm.max_depth",
            "swarm.max_concurrent",
            "nightly.repair_mcp_max_failures",
            "nightly.repair_schedule_max_failures",
            "nightly.repair_tool_min_calls",
            "nightly.max_age_days",
            "formatters.commands",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "expected a {needle} problem, got: {problems:?}"
            );
        }
        // valid spellings pass: backend aliases, a real provider, a real IANA
        // zone, non-zero caps, a non-empty command.
        cfg.browser = Some(BrowserSection {
            backend: Some("camoufox".to_string()),
            ..Default::default()
        });
        cfg.websearch = Some(WebsearchSection {
            provider: Some("tinyfish".to_string()),
            ..Default::default()
        });
        cfg.temporal = Some(TemporalSection {
            timezone: Some("Africa/Lagos".to_string()),
            ..Default::default()
        });
        cfg.swarm = Some(SwarmSection::default());
        cfg.nightly = Some(NightlySection::default());
        cfg.formatters = Some(FormattersSection {
            enabled: true,
            commands: std::collections::HashMap::from([(
                "rs".to_string(),
                "rustfmt {file}".to_string(),
            )]),
            ..Default::default()
        });
        let problems = cfg.validate();
        assert!(
            problems.is_empty(),
            "valid sections must not be flagged: {problems:?}"
        );
    }

    #[test]
    fn websearch_base_url_for_is_per_provider_never_global() {
        // regression for the base_url leak: an exa override must never
        // resolve for tavily, and the legacy global resolves for nobody.
        let section = WebsearchSection {
            provider: Some("exa".to_string()),
            base_url: Some("https://global.example.test".to_string()),
            base_urls: std::collections::HashMap::from([
                (
                    "exa".to_string(),
                    "https://exa-mirror.example.test".to_string(),
                ),
                (
                    "  ".to_string(),
                    "https://blank-key.example.test".to_string(),
                ),
                ("searxng".to_string(), "   ".to_string()),
            ]),
            ..Default::default()
        };
        assert_eq!(
            section.base_url_for("exa"),
            Some("https://exa-mirror.example.test")
        );
        assert_eq!(
            section.base_url_for("EXA"),
            Some("https://exa-mirror.example.test")
        );
        assert_eq!(section.base_url_for("tavily"), None);
        assert_eq!(section.base_url_for("searxng"), None);
        // validate() also flags the legacy global, so it can't sit there
        // looking effective.
        let mut cfg = cfg_with_model();
        cfg.websearch = Some(section);
        let problems = cfg.validate();
        assert!(
            problems.iter().any(|p| p.contains("websearch.base_url")),
            "expected a websearch.base_url problem, got: {problems:?}"
        );
    }

    #[test]
    fn validate_flags_empty_custom_base_url_and_channel_and_delegation_profiles() {
        let mut cfg = cfg_with_model();
        cfg.custom_providers.insert(
            "mine".to_string(),
            CustomProviderSection {
                base_url: String::new(),
                api_mode: "openai".to_string(),
                ..Default::default()
            },
        );
        let mut gw = GatewaySection::default();
        gw.channels.insert(
            "bot-a".to_string(),
            GatewayChannelSection {
                platform: Some("telegram".to_string()),
                token_secret: Some(String::new()),
                profile: Some("ghost".to_string()),
                ..Default::default()
            },
        );
        cfg.gateway = Some(gw);
        cfg.delegation = Some(DelegationSection {
            routes: vec![
                DelegationRoute {
                    name: "research".to_string(),
                    profile: "ghost".to_string(),
                    ..Default::default()
                },
                DelegationRoute {
                    name: "research".to_string(),
                    profile: String::new(),
                    ..Default::default()
                },
            ],
            default: Some("nope".to_string()),
        });
        let problems = cfg.validate();
        for needle in [
            "custom_providers.mine.base_url",
            "gateway.channels.bot-a.profile",
            "gateway.channels.bot-a.token_secret",
            "delegation.routes.\"research\".profile",
            "declared more than once",
            "delegation.default",
        ] {
            assert!(
                problems.iter().any(|p| p.contains(needle)),
                "expected a {needle} problem, got: {problems:?}"
            );
        }
        // declared profiles pass: add the agent the channel and route name,
        // point the default at a real route, give the custom provider a URL.
        if let Some(sec) = cfg.custom_providers.get_mut("mine") {
            sec.base_url = "https://example.com/v1".to_string();
        }
        cfg.agents
            .insert("support".to_string(), AgentIdentity::default());
        if let Some(gw) = cfg.gateway.as_mut() {
            if let Some(ch) = gw.channels.get_mut("bot-a") {
                ch.profile = Some("support".to_string());
                ch.token_secret = None;
            }
        }
        cfg.delegation = Some(DelegationSection {
            routes: vec![DelegationRoute {
                name: "research".to_string(),
                profile: "support".to_string(),
                ..Default::default()
            }],
            default: Some("research".to_string()),
        });
        let problems = cfg.validate();
        assert!(
            !problems
                .iter()
                .any(|p| p.contains("custom_providers.mine.base_url")
                    || p.contains("gateway.channels")
                    || p.contains("delegation")),
            "declared profiles and routes must not be flagged: {problems:?}"
        );
    }

    #[test]
    fn validate_flags_unknown_tui_theme_and_accepts_known_ones() {
        let mut cfg = cfg_with_model();
        cfg.tui = Some(TuiSection {
            theme: Some("neon".to_string()),
            ..Default::default()
        });
        let problems = cfg.validate();
        assert!(
            problems.iter().any(|p| p.contains("tui.theme")),
            "unknown theme must be flagged: {problems:?}"
        );
        for theme in ["pantheon", "dark", "light", "Dark"] {
            cfg.tui = Some(TuiSection {
                theme: Some(theme.to_string()),
                ..Default::default()
            });
            let problems = cfg.validate();
            assert!(
                !problems.iter().any(|p| p.contains("tui.theme")),
                "{theme:?} must validate: {problems:?}"
            );
        }
    }

    #[test]
    fn unknown_inner_keys_flags_section_typos() {
        let text = "[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\nprovidr = \"openai\"\n[budget]\nmax_turns = 16\n";
        assert_eq!(unknown_inner_keys(text), vec!["model.providr".to_string()]);
        // a valid config flags nothing, including map sections and half-set
        // [model] tables.
        assert!(unknown_inner_keys("[model]\nprovider = \"openai\"\n").is_empty());
        assert!(unknown_inner_keys("[agents.coder]\nmodel = \"openai/gpt-4o\"\n").is_empty());
        // unparseable documents report nothing here - the parse-error path
        // owns that case.
        assert!(unknown_inner_keys("[model\n").is_empty());
    }

    #[test]
    fn validate_flags_bad_permission_mode_and_accepts_lan_host() {
        let mut cfg = Config {
            permission_mode: Some("smrt".to_string()),
            server: Some(ServerSection {
                host: "192.168.1.10".to_string(),
                port: 18789,
            }),
            ..Config::default()
        };
        let problems = cfg.validate();
        assert!(
            problems.iter().any(|p| p.contains("permission_mode")),
            "bad permission_mode must be flagged: {problems:?}"
        );
        assert!(
            !problems.iter().any(|p| p.contains("server.host")),
            "a LAN host is bindable and must not be flagged: {problems:?}"
        );
        cfg.permission_mode = Some("smart".to_string());
        let problems = cfg.validate();
        assert!(
            !problems.iter().any(|p| p.contains("permission_mode")),
            "smart must validate: {problems:?}"
        );
    }

    /// item 7: unknown top-level keys reported (pure function)
    #[test]
    fn unknown_config_keys_flags_typos() {
        let text =
            "[modle]\nprovider = \"openai\"\n[model]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\n";
        assert_eq!(unknown_config_keys(text), vec!["modle".to_string()]);
        assert!(unknown_config_keys("[model]\nprovider = \"openai\"\n").is_empty());
        // unparseable documents report nothing here - the parse-error path
        // owns that case.
        assert!(unknown_config_keys("[model\n").is_empty());
    }

    /// item 7: KNOWN_CONFIG_KEYS tracks the Config struct. the literal is
    /// exhaustive (no `..Default::default()`), so adding a field breaks
    /// compilation until the list grows with it.
    #[test]
    fn known_config_keys_match_struct_fields() {
        let full = Config {
            runtime: None,
            agent: Some(String::new()),
            model: Some(ModelSection::default()),
            judge: Some(AuxSection::default()),
            embeddings: Some(AuxSection::default()),
            search_synthesis: Some(AuxSection::default()),
            vision: Some(AuxSection::default()),
            image: Some(ImageSection::default()),
            video: Some(AuxSection::default()),
            scheduled: Some(AuxSection::default()),
            mcp_synthesis: Some(AuxSection::default()),
            extraction: Some(AuxSection::default()),
            rerank: Some(AuxSection::default()),
            planner: Some(AuxSection::default()),
            repair: Some(RepairSection::default()),
            verify: Some(AuxSection::default()),
            swarm: Some(SwarmSection::default()),
            delegation: Some(DelegationSection::default()),
            compression: Some(CompressionSection::default()),
            title_gen: Some(TitleGenSection::default()),
            reflect: Some(ReflectSection::default()),
            consolidation: Some(ConsolidationSection::default()),
            nightly: Some(NightlySection::default()),
            stt: Some(VoiceSection::default()),
            tts: Some(VoiceSection::default()),
            voice: Some(LiveVoiceSection::default()),
            limits: Some(LimitsSection::default()),
            auth: Some(AuthSection::default()),
            policy: Some(PolicyPreset::default()),
            // non-None on purpose: the guard test serializes this literal and
            // compares its keys against KNOWN_CONFIG_KEYS, and a None
            // `permission_mode` is skipped on serialization, hiding the key
            // from the list (every load then warns it's unknown/ignored).
            permission_mode: Some("smart".to_string()),
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
            egress: Some(EgressSection::default()),
            admin: Some(AdminSection::default()),
            routing: Some(RoutingSection::default()),
            websearch: Some(WebsearchSection::default()),
            tools: Some(ToolsSection::default()),
            formatters: Some(FormattersSection::default()),
            skill_deps: Some(SkillDepsSection::default()),
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

    /// a `[repair]` section carrying only the model pin keys still parses;
    /// the cap key defaults to 3.
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

    /// the new cap key parses, is honored, serializes back out.
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

    /// `RepairSection` still derefs to its aux slot: model-policy resolution
    /// sees the same provider/model as before the struct split.
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
    #[test]
    fn validate_flags_duplicate_and_self_fallbacks() {
        let cfg = Config {
            model: Some(ModelSection {
                provider: "openai".into(),
                model: "gpt-4o".into(),
                fallbacks: vec![
                    FallbackEntry {
                        provider: "openai".into(),
                        model: "gpt-4o".into(),
                        key_env: None,
                    },
                    FallbackEntry {
                        provider: "anthropic".into(),
                        model: "claude-sonnet-4-6".into(),
                        key_env: None,
                    },
                    FallbackEntry {
                        provider: "anthropic".into(),
                        model: "claude-sonnet-4-6".into(),
                        key_env: None,
                    },
                ],
                ..ModelSection::default()
            }),
            ..Config::default()
        };
        let problems = cfg.validate();
        assert!(
            problems
                .iter()
                .any(|p| p.contains("identical to the default")),
            "self-fallback must be flagged: {problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("duplicates an earlier")),
            "duplicate fallback must be flagged: {problems:?}"
        );
    }

    #[test]
    fn custom_model_capability_defaults_match_model_meta() {
        // a row declaring nothing keeps vision off and tools/streaming on; a
        // row declaring vision round-trips.
        let plain: CustomModel = toml::from_str("id = \"m\"").expect("parses");
        assert!(!plain.vision && plain.tools && plain.streaming);
        let seeing: CustomModel =
            toml::from_str("id = \"m\"\nvision = true\nstreaming = false").expect("parses");
        assert!(seeing.vision && !seeing.streaming && seeing.tools);
    }
}
