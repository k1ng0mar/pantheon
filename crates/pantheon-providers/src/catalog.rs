//! Provider + model catalog: endpoint, wire format, model capabilities + prices.
//! YAML at runtime, embedded `catalog.yaml` by default,
//! `PANTHEON_CATALOG=/path/to.yaml` to override. Read-only at runtime; agents
//! don't pick from it.

use serde::{Deserialize, Serialize};
use std::sync::{OnceLock, RwLock};

/// Wire format a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiMode {
    /// OpenAI chat-completions shape (`/chat/completions`).
    #[serde(rename = "openai")]
    OpenAi,
    /// Anthropic Messages shape (`/messages`).
    #[serde(rename = "anthropic")]
    Anthropic,
    /// Native Gemini generateContent shape.
    #[serde(rename = "gemini")]
    Gemini,
}

/// List prices in USD per million tokens. `None` = unknown / free / local.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelCost {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_per_mtok_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_per_mtok_usd: Option<f64>,
}

impl ModelCost {
    pub fn estimate(&self, input_tokens: u64, output_tokens: u64) -> Option<f64> {
        let i = self.input_per_mtok_usd?;
        let o = self.output_per_mtok_usd?;
        Some(input_tokens as f64 * i / 1_000_000.0 + output_tokens as f64 * o / 1_000_000.0)
    }

    /// Estimate with cache pricing: if `cache` is set, all input tokens get
    /// the cache rate. Conservative in that it assumes every input was cached.
    pub fn estimate_with_cache(
        &self,
        input_tokens: u64,
        output_tokens: u64,
        cache: Option<&ModelCost>,
    ) -> Option<f64> {
        let i = cache
            .and_then(|c| c.input_per_mtok_usd)
            .or(self.input_per_mtok_usd)?;
        let o = self.output_per_mtok_usd?;
        Some(input_tokens as f64 * i / 1_000_000.0 + output_tokens as f64 * o / 1_000_000.0)
    }
}

/// Row in the catalog: where the provider lives, wire format, key env vars.
/// `auto: true` + `models: [...]` = full provider entry.
/// Just a `base_url` and no `auto` = known endpoint, no curated models yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderMeta {
    pub id: String,
    pub label: String,
    pub base_url: String,
    pub api_mode: ApiMode,
    /// Shell command whose stdout is the bearer token when no key is set.
    /// Runs under `sh -c` as the Pantheon user, on the host, unsandboxed.
    /// Only operator-controlled sources may set it (embedded catalog or
    /// `PANTHEON_CATALOG`); `config.toml` custom providers never do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_refresh_ms: Option<u64>,
    #[serde(default)]
    pub reserved: bool,
    /// Params this provider accepts. Absent = send everything; present =
    /// drop anything not listed before the request goes out. Azure and
    /// newer Anthropic 400 on fields they don't recognize.
    #[serde(default)]
    pub allowed_params: Vec<String>,
    /// OAuth apps that can auth this provider. Empty = API key only.
    /// `key_for` tries these in order, first connected app wins.
    #[serde(default)]
    pub oauth_apps: Vec<String>,
    /// Use SigV4 signing instead of Bearer auth (AWS Bedrock).
    #[serde(default)]
    pub sigv4: bool,
    /// Per-provider reasoning effort mapping. Empty = send `reasoning_effort`
    /// verbatim; else rewrite the global level (DeepSeek wants `max`,
    /// Anthropic wants `high`).
    #[serde(default)]
    pub reasoning_map: std::collections::HashMap<String, String>,
    /// Env var that overrides the base URL, e.g. `PANTHEON_BASE_OPENAI`.
    #[serde(default)]
    pub base_env: String,
    /// Env var that overrides the API key, e.g. `PANTHEON_KEY_OPENAI`.
    #[serde(default)]
    pub key_env: String,
    /// Header carrying the key. Default `Authorization` (`Bearer <key>`).
    /// Some vendors want the raw key elsewhere - Xiaomi MiMo uses `api-key`.
    /// Any non-Authorization name sends the raw key.
    #[serde(default = "default_auth_header")]
    pub key_header: String,
    /// Curated models. Empty means "use the generic adapter with whatever
    /// model name you pass at runtime".
    #[serde(default)]
    pub models: Vec<ModelMeta>,
    /// Show in the picker UI. Curated-but-hidden otherwise.
    #[serde(default)]
    pub prominent: bool,
    /// The one provider we recommend by default (at most one row should
    /// set this): setup wizard lists it first, `providers` verb marks it.
    #[serde(default)]
    pub recommended: bool,
    /// Dev endpoint, not a product provider (the local llm-router is one).
    /// The e2e suite needs it, but offering a `127.0.0.1` process that's
    /// usually dead in the setup wizard is a terrible idea. Hidden from
    /// pickers, still resolvable by id so the dev path keeps working.
    #[serde(default)]
    pub dev: bool,
    /// Short blurb for the picker UI ("Chinese lab", "fast inference", ...).
    #[serde(default)]
    pub tag: String,
}

/// Static metadata for one model. `provider` is filled from the parent
/// provider's id at load time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMeta {
    /// Filled from the parent provider's id at load time; skipped in YAML.
    #[serde(skip)]
    pub provider: String,
    pub model: String,
    /// Context window in tokens; `None` = unknown.
    #[serde(default)]
    pub context_limit: Option<u32>,
    /// Provider-imposed max output tokens; `None` = unknown/default.
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default = "default_true")]
    pub tools: bool,
    #[serde(default)]
    pub vision: bool,
    /// Model takes a video file as native input (bytes to the provider,
    /// not frames we extract locally).
    #[serde(default)]
    pub video: bool,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default = "default_true")]
    pub streaming: bool,
    #[serde(default)]
    pub cost: ModelCost,
    /// Cache pricing per million tokens. Absent = no prompt caching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_cost: Option<ModelCost>,
    /// Short aliases for this model (e.g. "opus" for "claude-opus-4-8").
    #[serde(default)]
    pub aliases: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_auth_header() -> String {
    "Authorization".into()
}

/// The full catalog: one slice of providers, each with their model rows.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Catalog {
    pub providers: Vec<ProviderMeta>,
}

/// Embedded default catalog: keeps the binary self-sufficient.
const DEFAULT_CATALOG: &str = include_str!("../catalog.yaml");

static CATALOG: OnceLock<Catalog> = OnceLock::new();

/// Load the catalog once per process. Resolution order:
/// 1. `PANTHEON_CATALOG` env var: a YAML file path (operator-controlled;
///    its rows may carry `auth_command`, which runs as the user).
/// 2. The embedded default (`catalog.yaml`, compile-time).
///
/// No remote refresh, on purpose. We'd be accepting remote rows wholesale -
/// including `auth_command` (runs under `sh -c`) - and a longer remote
/// `base_url` could take over a provider's endpoint. That hands command
/// execution and key redirection to whoever served the URL (audit ZA C2).
///
/// Failures are loud, not silent: a bad `PANTHEON_CATALOG` and a catalog
/// that won't parse both print to stderr with the path, so "every provider
/// vanished" has a visible cause.
fn load_catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let raw = if let Ok(p) = std::env::var("PANTHEON_CATALOG") {
            match std::fs::read_to_string(&p) {
                Ok(text) => Some(text),
                Err(e) => {
                    eprintln!(
                        "pantheon: PANTHEON_CATALOG={p} could not be read ({e}); \
                         using the embedded provider catalog"
                    );
                    None
                }
            }
        } else {
            None
        };
        let text = raw.as_deref().unwrap_or(DEFAULT_CATALOG);
        match serde_yaml::from_str::<Catalog>(text) {
            Ok(mut c) => {
                for p in &mut c.providers {
                    let id = p.id.clone();
                    for m in &mut p.models {
                        m.provider = id.clone();
                    }
                }
                c
            }
            Err(e) => {
                eprintln!(
                    "pantheon: provider catalog does not parse ({e}); no providers are \
                     available until it is fixed{}",
                    std::env::var("PANTHEON_CATALOG")
                        .map(|p| format!(" (PANTHEON_CATALOG={p})"))
                        .unwrap_or_default()
                );
                Catalog::default()
            }
        }
    })
}

/// Catalog accessor. Cheap, cached.
pub fn catalog() -> &'static Catalog {
    load_catalog()
}

/// User-defined providers from `[custom_providers.*]` in config.toml.
/// Registered at CLI startup; every lookup below consults these so custom
/// endpoints behave exactly like cataloged ones.
static CUSTOM: RwLock<Vec<ProviderMeta>> = RwLock::new(Vec::new());

/// Register (or replace, by id) a user-defined provider. Idempotent.
/// Reserved built-in ids are rejected: a custom provider must not silently
/// override a cataloged one.
pub fn register_custom_provider(meta: ProviderMeta) {
    if catalog()
        .providers
        .iter()
        .any(|p| p.id == meta.id && p.reserved)
    {
        return;
    }
    if let Ok(mut customs) = CUSTOM.write() {
        if let Some(slot) = customs.iter_mut().find(|p| p.id == meta.id) {
            *slot = meta;
        } else {
            customs.push(meta);
        }
    }
}

/// Iterate providers.
pub fn providers() -> &'static [ProviderMeta] {
    &catalog().providers
}

/// All providers: cataloged plus user-registered customs. The picker and
/// the `providers` verb use this; the runtime path resolves either way.
pub fn all_providers() -> Vec<ProviderMeta> {
    let mut out: Vec<ProviderMeta> = catalog().providers.clone();
    if let Ok(customs) = CUSTOM.read() {
        out.extend(customs.iter().cloned());
    }
    out
}

/// Providers a user can actually pick: everything minus dev-only ones.
/// Offering `127.0.0.1` in a wizard is offering a process that isn't
/// running; the row can stay in the catalog and be absent from pickers.
pub fn selectable_providers() -> Vec<ProviderMeta> {
    all_providers().into_iter().filter(|p| !p.dev).collect()
}

/// Iterate all model rows across all providers.
pub fn models() -> Vec<&'static ModelMeta> {
    catalog()
        .providers
        .iter()
        .flat_map(|p| p.models.iter())
        .collect()
}

/// Look up a provider by id (cataloged or user-registered custom).
/// Canonical provider id for a user-written reference.
///
/// Accepts the typed namespace Hermes-style configs use: `custom:llm-router`
/// resolves to the custom provider `llm-router`. A bare `custom` recovers the
/// one registered custom provider (the common single-endpoint case); with zero
/// or several it stays `custom`, so the caller's error names what to fix
/// instead of guessing.
pub fn canonical_provider_id(id: &str) -> String {
    let t = id.trim();
    if t.len() > "custom:".len() && t[.."custom:".len()].eq_ignore_ascii_case("custom:") {
        return t["custom:".len()..].trim().to_string();
    }
    if t.eq_ignore_ascii_case("custom") {
        if let Some(only) = single_custom_id() {
            return only;
        }
    }
    t.to_string()
}

fn single_custom_id() -> Option<String> {
    let customs = CUSTOM.read().ok()?;
    match customs.len() {
        1 => Some(customs[0].id.clone()),
        _ => None,
    }
}

/// Endpoint identity for matching two configs to the same server: scheme and
/// host fold case, default ports and a trailing slash drop, the path stays
/// significant (`/v1` is not `/`). Unparseable input falls back to a trimmed,
/// lowercased string rather than matching nothing.
pub fn normalize_endpoint(url: &str) -> String {
    let raw = url.trim();
    let Ok(u) = url::Url::parse(raw) else {
        return raw.trim_end_matches('/').to_ascii_lowercase();
    };
    let scheme = u.scheme().to_ascii_lowercase();
    let host = u.host_str().unwrap_or("").to_ascii_lowercase();
    let default_port = matches!(
        (scheme.as_str(), u.port()),
        ("http", Some(80)) | ("https", Some(443))
    );
    let port = match u.port() {
        Some(p) if !default_port => format!(":{p}"),
        _ => String::new(),
    };
    let path = u.path().trim_end_matches('/');
    format!("{scheme}://{host}{port}{path}")
}

/// The registered provider serving `url`, customs first. Used by
/// `pantheon provider add` so one endpoint doesn't silently become two rows
/// under different names.
pub fn provider_for_endpoint(url: &str) -> Option<ProviderMeta> {
    let want = normalize_endpoint(url);
    if let Ok(customs) = CUSTOM.read() {
        if let Some(p) = customs
            .iter()
            .find(|p| normalize_endpoint(&p.base_url) == want)
        {
            return Some(p.clone());
        }
    }
    providers()
        .iter()
        .find(|p| normalize_endpoint(&p.base_url) == want)
        .cloned()
}

pub fn provider(id: &str) -> Option<ProviderMeta> {
    let id = canonical_provider_id(id);
    if let Ok(customs) = CUSTOM.read() {
        if let Some(p) = customs.iter().find(|p| p.id == id) {
            return Some(p.clone());
        }
    }
    providers().iter().find(|p| p.id == id).cloned()
}

/// Look up a model's static metadata (exact match, then alias match).
pub fn model(provider_id: &str, model_id: &str) -> Option<&'static ModelMeta> {
    let exact = models()
        .into_iter()
        .find(|m| m.provider == provider_id && m.model == model_id);
    if exact.is_some() {
        return exact;
    }
    models().into_iter().find(|m| {
        m.provider == provider_id && m.aliases.iter().any(|a| a.eq_ignore_ascii_case(model_id))
    })
}

/// Canonical wire id: an alias returns the row's real `model` id
/// (`opus` -> `claude-opus-4-8`), anything else passes through. The chain
/// sends the canonical id because Anthropic rejects aliases on the wire.
pub fn canonical_model(provider_id: &str, model_id: &str) -> String {
    model(provider_id, model_id)
        .map(|m| m.model.clone())
        .unwrap_or_else(|| model_id.to_string())
}

/// Does the catalog (or a registered custom) know this model? Distinct from
/// `model_meta`, whose conservative defaults make unknown models look like
/// known non-reasoning ones.
pub fn model_known(provider_id: &str, model_id: &str) -> bool {
    if model(provider_id, model_id).is_some() {
        return true;
    }
    if let Ok(customs) = CUSTOM.read() {
        if customs
            .iter()
            .flat_map(|p| p.models.iter())
            .any(|m| m.provider == provider_id && m.model == model_id)
        {
            return true;
        }
    }
    false
}

/// Model metadata with conservative defaults for unknown models: context
/// unknown, tools on, vision/reasoning off, streaming on, cost unknown.
pub fn model_meta(provider_id: &str, model_id: &str) -> ModelMeta {
    if let Some(m) = model(provider_id, model_id) {
        return m.clone();
    }
    // User-registered custom endpoints: their declared rows (incl.
    // operator-declared capabilities like `video = true`) behave exactly
    // like cataloged ones everywhere this lookup is consulted.
    if let Ok(customs) = CUSTOM.read() {
        if let Some(m) = customs
            .iter()
            .flat_map(|p| p.models.iter())
            .find(|m| m.provider == provider_id && m.model == model_id)
        {
            return m.clone();
        }
    }
    ModelMeta {
        provider: provider_id.to_string(),
        model: model_id.to_string(),
        context_limit: None,
        max_output_tokens: None,
        tools: true,
        vision: false,
        video: false,
        reasoning: false,
        streaming: true,
        cost: ModelCost::default(),
        cache_cost: None,
        aliases: vec![],
    }
}

/// Base URL for a provider: env override → catalog → the provider id
/// itself (treated as a full base URL, legacy passthrough). May still contain
/// `{var}` template placeholders; use [`resolve_base_url`] when building
/// requests.
pub fn base_url_for(provider_id: &str) -> String {
    let provider_id = &canonical_provider_id(provider_id);
    if let Some(p) = provider(provider_id) {
        if !p.base_env.is_empty() {
            if let Ok(u) = std::env::var(&p.base_env) {
                if !u.is_empty() {
                    return u;
                }
            }
        }
        return p.base_url.clone();
    }
    if let Ok(u) = std::env::var(format!("PANTHEON_BASE_{}", provider_id.to_uppercase())) {
        if !u.is_empty() {
            return u;
        }
    }
    // Legacy passthrough: a custom provider's id IS its base URL. That is
    // only true if the id is actually shaped like one. Without this check a
    // typo such as `provider = "http"` became the literal base URL "http",
    // so the request went to the relative path "http/chat/completions",
    // failed as a retryable network error, and the chain reported
    // PROVIDER_EXHAUSTED. The real cause -- an unknown provider -- never
    // reached the user, and `doctor` called the config valid.
    if provider_id.starts_with("http://") || provider_id.starts_with("https://") {
        return provider_id.to_string();
    }
    // Unknown id that is not a URL. resolve_base_url turns this into a
    // PROVIDER_CONFIG error naming the bad id, which is non-retryable and so
    // surfaces directly instead of collapsing into PROVIDER_EXHAUSTED.
    String::new()
}

/// Placeholder vars in a template base URL: `{resource}` → `["resource"]`.
/// Doubled braces are not templates; empty `{}` is ignored.
pub fn template_vars(base_url: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = base_url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = base_url[i..].find('}') {
                let name = base_url[i + 1..i + end].trim();
                if !name.is_empty()
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    && !out.iter().any(|n: &String| n == name)
                {
                    out.push(name.to_string());
                }
                i += end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Env-safe fragment: `my-llm` → `MY_LLM`. Shared by
/// [`config_env_name`] and the CLI's `sanitize_env_suffix` so both build
/// the same `PANTHEON_*` names.
pub fn env_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Env var holding one template value: provider `azure` + var `resource`
/// → `PANTHEON_AZURE_RESOURCE`. Values live in `<data_dir>/.env`
/// (written by `pantheon model`), same as API keys.
pub fn config_env_name(provider_id: &str, var: &str) -> String {
    format!("PANTHEON_{}_{}", env_part(provider_id), env_part(var))
}

/// Required template vars for a provider id (empty = ready to use).
pub fn required_config_vars(provider_id: &str) -> Vec<String> {
    template_vars(&base_url_for(provider_id))
}

/// Resolve a provider's effective base URL, interpolating `{var}`
/// placeholders from `PANTHEON_<PROVIDER>_<VAR>` env values. Errors name
/// every missing var and point at `pantheon model` - never let a raw
/// `{placeholder}` reach the wire.
pub fn resolve_base_url(provider_id: &str) -> Result<String, String> {
    let base = base_url_for(provider_id);
    if base.is_empty() {
        return Err(format!(
            "unknown provider {provider_id:?}: it is not in the catalog and is not a URL. \
             run `pantheon model` to add it, or set PANTHEON_BASE_{} to its base URL",
            provider_id.to_uppercase()
        ));
    }
    resolve_template(provider_id, &base)
}

/// Resolve an explicit base string with the provider's env namespace.
/// Same as [`resolve_base_url`] but for a URL not (yet) in the catalog
/// the `pantheon model` flow uses this before the custom row is saved.
pub fn resolve_template(provider_id: &str, base: &str) -> Result<String, String> {
    let vars = template_vars(base);
    if vars.is_empty() {
        return Ok(base.to_string());
    }
    let mut out = base.to_string();
    let mut missing = Vec::new();
    for v in &vars {
        let env_name = config_env_name(provider_id, v);
        match std::env::var(&env_name) {
            Ok(val) if !val.trim().is_empty() => {
                out = out.replace(&format!("{{{v}}}"), val.trim());
            }
            _ => missing.push(format!("{env_name} (for {{{v}}})")),
        }
    }
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!(
            "provider {provider_id:?} needs {} - run `pantheon model` to fill them (stored in <data_dir>/.env)",
            missing.join(", ")
        ))
    }
}

/// HTTP header carrying the key for a provider (`Authorization` unless
/// the row says otherwise). Consults customs too.
pub fn key_header_for(provider_id: &str) -> String {
    provider(provider_id)
        .map(|p| p.key_header)
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(default_auth_header)
}

/// Resolved credential for a provider, trying in order:
/// 1. `fallback` (broker-resolved key)
/// 2. OAuth apps (if `oauth_apps` is non-empty)
/// 3. Command-backed auth (if `auth_command` is set)
/// 4. Env var (`key_env` or `PANTHEON_KEY_<ID>`)
///
/// Command auth spawns a subprocess (e.g. `gcloud auth print-access-token`)
/// and caches the result for `auth_refresh_ms`. This lets short-lived cloud
/// tokens work without manual refresh.
pub fn credential_for(
    provider_id: &str,
    fallback: &str,
    broker: Option<&pantheon_secrets::SecretsBroker>,
) -> String {
    let provider_id = &canonical_provider_id(provider_id);
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    if let Some(p) = provider(provider_id) {
        if !p.oauth_apps.is_empty() {
            if let Some(b) = broker {
                for app in &p.oauth_apps {
                    if let Ok(token) =
                        pantheon_secrets::oauth::OAuthStore::new(b).ensure_valid_token(app)
                    {
                        return token.expose().to_string();
                    }
                }
            }
        }
        if let Some(ref cmd) = p.auth_command {
            if let Some(token) = command_auth_cached(provider_id, cmd, p.auth_refresh_ms) {
                return token;
            }
        }
    }
    let env_name = provider(provider_id)
        .map(|p| {
            if p.key_env.is_empty() {
                format!("PANTHEON_KEY_{}", p.id.to_uppercase())
            } else {
                p.key_env.clone()
            }
        })
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", provider_id.to_uppercase()));
    std::env::var(env_name).unwrap_or_default()
}

/// API key for a provider: `fallback` (broker-resolved) → env override.
/// The broker-resolved key (from `[model].api_key_env` or the keychain)
/// takes precedence over the raw vendor env var, matching the documented
/// chain: flags > config > env. The env var is a fallback for when the
/// broker has no opinion.
pub fn key_for(provider_id: &str, fallback: &str) -> String {
    let provider_id = &canonical_provider_id(provider_id);
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    let env_name = provider(provider_id)
        .map(|p| {
            if p.key_env.is_empty() {
                format!("PANTHEON_KEY_{}", p.id.to_uppercase())
            } else {
                p.key_env.clone()
            }
        })
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", provider_id.to_uppercase()));
    std::env::var(env_name).unwrap_or_default()
}

/// Command-backed auth cache: provider_id → (token, fetched_at).
static COMMAND_AUTH: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Run `cmd` and return trimmed stdout as a token. Cached for `refresh_ms`.
fn command_auth_cached(provider_id: &str, cmd: &str, refresh_ms: Option<u64>) -> Option<String> {
    let ttl = refresh_ms.unwrap_or(300_000);
    if let Ok(cache) = COMMAND_AUTH.lock() {
        if let Some((token, fetched_at)) = cache.get(provider_id) {
            if fetched_at.elapsed() < std::time::Duration::from_millis(ttl) {
                return Some(token.clone());
            }
        }
    }
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() {
        return None;
    }
    if let Ok(mut cache) = COMMAND_AUTH.lock() {
        cache.insert(
            provider_id.to_string(),
            (token.clone(), std::time::Instant::now()),
        );
    }
    Some(token)
}

#[cfg(test)]
mod tests {
    #[test]
    fn custom_namespace_and_recovery() {
        assert_eq!(super::canonical_provider_id("custom:llm-router"), "llm-router");
        assert_eq!(super::canonical_provider_id("  CUSTOM:My-Router  "), "My-Router");
        // bare `custom` recovery depends on the process-wide registry (exactly
        // one custom provider), so it is not asserted here: parallel tests
        // register their own rows.
        // ordinary ids pass through untouched
        assert_eq!(super::canonical_provider_id("openrouter"), "openrouter");
    }

    #[test]
    fn endpoint_identity_folds_host_case_ports_and_trailing_slash() {
        assert_eq!(
            super::normalize_endpoint("HTTP://127.0.0.1:8015/v1/"),
            "http://127.0.0.1:8015/v1"
        );
        assert_eq!(
            super::normalize_endpoint("https://Example.COM:443/v1"),
            "https://example.com/v1"
        );
        assert_eq!(
            super::normalize_endpoint("http://host:80/"),
            "http://host"
        );
        // the path is significant: /v1 is a different endpoint than /
        assert_ne!(
            super::normalize_endpoint("http://host/v1"),
            super::normalize_endpoint("http://host")
        );
    }

    #[test]
    fn provider_for_endpoint_matches_a_registered_custom() {
        super::register_custom_provider(pantheon_api_meta(
            "endpoint-id-test",
            "http://127.0.0.1:19901/v1/",
        ));
        let hit = super::provider_for_endpoint("HTTP://127.0.0.1:19901/v1").expect("match");
        assert_eq!(hit.id, "endpoint-id-test");
        assert!(super::provider_for_endpoint("http://127.0.0.1:19902/v1").is_none());
    }

    fn pantheon_api_meta(id: &str, base: &str) -> super::ProviderMeta {
        super::ProviderMeta {
            id: id.into(),
            label: id.into(),
            base_url: base.into(),
            api_mode: super::ApiMode::OpenAi,
            base_env: String::new(),
            key_env: String::new(),
            key_header: "Authorization".into(),
            models: Vec::new(),
            prominent: false,
            recommended: false,
            dev: false,
            tag: "custom".into(),
            auth_command: None,
            auth_refresh_ms: None,
            reserved: false,
            allowed_params: vec![],
            oauth_apps: vec![],
            sigv4: false,
            reasoning_map: Default::default(),
        }
    }

    use super::*;

    fn custom_meta(id: &str, base_url: &str) -> ProviderMeta {
        ProviderMeta {
            id: id.to_string(),
            label: id.to_string(),
            base_url: base_url.to_string(),
            api_mode: ApiMode::OpenAi,
            auth_command: None,
            auth_refresh_ms: None,
            reserved: false,
            allowed_params: vec![],
            oauth_apps: vec![],
            sigv4: false,
            reasoning_map: std::collections::HashMap::new(),
            base_env: String::new(),
            key_env: String::new(),
            key_header: "Authorization".to_string(),
            models: vec![],
            prominent: false,
            recommended: false,
            dev: false,
            tag: String::new(),
        }
    }

    #[test]
    fn custom_provider_base_urls_are_scoped_per_provider() {
        // The chat plane already scopes base_url per provider id; lock
        // it in: two custom providers with distinct bases resolve
        // independently, and neither leaks into the other.
        register_custom_provider(custom_meta("dec-rt-custom-a", "https://a.example.test/v1"));
        register_custom_provider(custom_meta("dec-rt-custom-b", "https://b.example.test/v1"));
        assert_eq!(
            resolve_base_url("dec-rt-custom-a").unwrap(),
            "https://a.example.test/v1"
        );
        assert_eq!(
            resolve_base_url("dec-rt-custom-b").unwrap(),
            "https://b.example.test/v1"
        );
    }
}
