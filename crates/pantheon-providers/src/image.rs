//! Image generation against an OpenAI-compatible `/images/generations`.
//!
//! Pantheon could read images (vision) and files but could not make one: the
//! model had no way to answer "draw me a diagram/mockup/icon" except by
//! writing SVG by hand. This is the smallest honest seam for the rest of the
//! features (mockups, app icons, social assets): one prompt out, bytes back,
//! resolved through the *same* provider the chat turn uses.
//!
//! Deliberate limits:
//! - the endpoint is the provider's base URL + `/images/generations`; no
//!   hardcoded vendor host, so a local router or a proxy works the same way;
//! - both wire shapes providers actually return are accepted (`b64_json`, or
//!   a `url` that is fetched once and never cached by us);
//! - `url` downloads are restricted to http(s): a generated "image URL" must
//!   not be able to read `file:///etc/passwd` through us;
//! - the bytes are size-capped, because the response is attacker-influenced
//!   JSON like any other provider payload.

use pantheon_api::config::Config;
use pantheon_api::error::{Layer, PantheonError};

use crate::catalog::{self, ApiMode};

/// A generation call may legitimately take a while; a hung one must not.
pub const IMAGE_TIMEOUT_SECS: u64 = 120;
/// Hard cap on one generated image. Providers return PNG/JPEG/WebP in the
/// low single-digit MiB; anything far past that is not an image we can use.
pub const MAX_IMAGE_BYTES: usize = 24 * 1024 * 1024;

/// What to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRequest {
    pub prompt: String,
    /// `1024x1024`, or a semantic `landscape|square|portrait` (`16:9`, `1:1`,
    /// `9:16` also accepted). Backends differ in what they accept, so the
    /// semantic form is translated per backend instead of forwarded blindly.
    pub size: Option<String>,
    /// Model id; the provider's first declared model when omitted.
    pub model: Option<String>,
    /// Provider id override; the `[model] provider` otherwise.
    pub provider: Option<String>,
    /// Source images for an edit or a reference-grounded generation: absolute
    /// paths, http(s) URLs, or `data:` URIs. Empty = text-to-image.
    pub source_images: Vec<String>,
    /// How many images to ask for. Backends that cannot batch are refused
    /// rather than silently returning one; every image is billed.
    pub n: usize,
    /// Deterministic seed where the backend accepts one - the knob for "same
    /// composition, another try".
    pub seed: Option<u64>,
}

impl ImageRequest {
    /// Convenience constructor for a plain text-to-image call.
    pub fn text(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            size: None,
            model: None,
            provider: None,
            source_images: Vec::new(),
            n: 1,
            seed: None,
        }
    }

    /// Effective batch size: at least one, capped so a typo cannot order fifty
    /// paid images.
    pub fn batch(&self) -> usize {
        self.n.clamp(1, MAX_IMAGES_PER_CALL)
    }
}

/// Hard ceiling on one call's image count. The upper bound exists because the
/// caller is a model: `n: 40` is a mistake, not a request.
pub const MAX_IMAGES_PER_CALL: usize = 8;

/// One produced image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBlob {
    pub bytes: Vec<u8>,
    pub mime: String,
}

impl ImageBlob {
    /// File extension for the mime we detected.
    pub fn extension(&self) -> &'static str {
        mime_extension(&self.mime)
    }
}

/// What came back. A batch keeps every image; `primary()` is the first one,
/// which is what a caller that only wants "the picture" uses.
#[derive(Debug, Clone)]
pub struct ImageOutput {
    pub images: Vec<ImageBlob>,
    pub model: String,
    pub provider: String,
    /// Some models rewrite the prompt; surfaced so the caller can see what the
    /// asset actually depicts.
    pub revised_prompt: Option<String>,
    /// Which backend produced it (`openai_images`, `chat_image`, `a1111`,
    /// `url_template`, `fal`), so a surprising result can be traced to a wire.
    pub backend: String,
    /// True when a source image was supplied and the result is an edit.
    pub edited: bool,
    /// The seed used, when one was given or drawn - lets a caller reproduce
    /// or re-roll a composition deliberately.
    pub seed: Option<u64>,
    /// Declared USD per image for the service that produced this, if the
    /// operator configured one. `None` = price unknown, recorded as unknown.
    pub price: Option<f64>,
}

impl ImageOutput {
    pub fn primary(&self) -> Option<&ImageBlob> {
        self.images.first()
    }
    pub fn bytes(&self) -> usize {
        self.images.iter().map(|i| i.bytes.len()).sum()
    }
}

impl ImageOutput {
    /// File extension of the first image, for a caller that saves only one.
    pub fn extension(&self) -> &'static str {
        self.primary().map(ImageBlob::extension).unwrap_or("bin")
    }
}

fn mime_extension(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    }
}

fn ierr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Provider,
        false,
        cause,
        "check the image prompt, size and the provider's image endpoint",
        String::new(),
    )
}

/// Magic-byte sniffing: a provider that returns `image/png` on the wire but
/// JPEG bytes must not produce a file that every viewer refuses to open.
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.len() > 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else {
        None
    }
}

/// Standard-alphabet base64 decoder. Small on purpose: the repo has an
/// encoder ([`pantheon_api::message::b64encode`]) and no decoder, and pulling
/// a crate in for one call is not worth the supply-chain surface.
pub fn b64decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut nbits = 0u32;
    for &c in s.as_bytes() {
        if c == b'=' {
            break;
        }
        if c.is_ascii_whitespace() {
            continue;
        }
        let v = val(c)?;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Resolve where a generation request goes: the custom-provider table first
/// (that is where a router lives), then the built-in catalog.
fn endpoint(cfg: &Config, provider: &str) -> Option<(String, Option<String>)> {
    let id = catalog::canonical_provider_id(provider);
    if let Some(c) = cfg.custom_providers.get(&id) {
        if !c.base_url.trim().is_empty() {
            return Some((c.base_url.trim_end_matches('/').to_string(), c.key_env.clone()));
        }
    }
    let built_in = catalog::provider(&id)?;
    let base = catalog::base_url_for(&id);
    if base.is_empty() {
        return None;
    }
    Some((
        base,
        if built_in.key_env.is_empty() {
            None
        } else {
            Some(built_in.key_env.clone())
        },
    ))
}

/// The model to generate with: an explicit choice, else the provider's first
/// declared model (the router pattern: `auto`/`chat`/… lists an image model
/// only if the operator names one).
fn model_for(cfg: &Config, provider: &str, requested: Option<&str>) -> Option<String> {
    if let Some(m) = requested.map(str::trim).filter(|m| !m.is_empty()) {
        return Some(m.to_string());
    }
    let id = catalog::canonical_provider_id(provider);
    if let Some(c) = cfg.custom_providers.get(&id) {
        if let Some(m) = c.models.iter().find(|m| {
            let id = m.id.to_ascii_lowercase();
            id.contains("image") || id.contains("dall") || id.contains("flux")
        }) {
            return Some(m.id.clone());
        }
    }
    None
}

fn key_for(cfg: &Config, provider: &str, key_env: Option<&str>) -> String {
    if let Some(env) = key_env {
        if let Ok(k) = std::env::var(env) {
            if !k.trim().is_empty() {
                return k;
            }
        }
    }
    // Built-ins keep their normal env name; customs without a declared key_env
    // fall back to `PANTHEON_KEY_<ID>` so a loopback router works keyless.
    let _ = cfg;
    catalog::key_for(provider, "")
}

/// Build the JSON body for a generation call. Split out so the wire shape can
/// be asserted without a network.
pub fn request_body(model: &str, req: &ImageRequest) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "prompt": req.prompt,
        "n": 1,
        // Ask for bytes inline: a pre-signed URL expires and leaks a fetch of
        // arbitrary content into a second request we would have to trust.
        "response_format": "b64_json",
    });
    body["n"] = serde_json::Value::from(req.batch());
    if let Some(size) = openai_size(req.size.as_deref()) {
        body["size"] = serde_json::Value::String(size);
    }
    body
}

/// One decoded provider answer: bytes (empty when the image is remote), the
/// mime we can prove, the model's rewritten prompt, and a URL to fetch.
pub type ParsedImage = (Vec<u8>, String, Option<String>, Option<String>);

/// Pull the image out of a provider response.
pub fn parse_response(v: &serde_json::Value) -> Result<ParsedImage, PantheonError> {
    let first = v
        .get("data")
        .and_then(|d| d.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("provider returned no data array: {v}")))?;
    let revised = first
        .get("revised_prompt")
        .and_then(|p| p.as_str())
        .map(str::to_string);
    let declared = first
        .get("mime_type")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    if let Some(b64) = first.get("b64_json").and_then(|b| b.as_str()) {
        let bytes = b64decode(b64)
            .ok_or_else(|| ierr("IMAGE_BAD_BASE64", "provider sent unreadable base64".into()))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ierr(
                "IMAGE_TOO_LARGE",
                format!("{} bytes exceeds the {MAX_IMAGE_BYTES} cap", bytes.len()),
            ));
        }
        let mime = sniff_mime(&bytes)
            .or(declared.as_deref())
            .unwrap_or("application/octet-stream")
            .to_string();
        return Ok((bytes, mime, revised, None));
    }
    if let Some(url) = first.get("url").and_then(|u| u.as_str()) {
        return Ok((Vec::new(), String::new(), revised, Some(url.to_string())));
    }
    Err(ierr(
        "IMAGE_NO_DATA",
        "provider returned neither b64_json nor url".into(),
    ))
}

/// Which wire a generation call uses. The tool never names a vendor: it picks
/// a backend, and a backend is a protocol, not a company.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// `POST {base}/images/generations` (OpenAI-compatible services, routers).
    OpenAiImages,
    /// `POST {base}/chat/completions` with `modalities: ["image","text"]`;
    /// images ride back inside the message (OpenRouter, Gemini-via-chat,
    /// several gateways).
    ChatImage,
    /// A local Stable-Diffusion WebUI: `POST {base}/sdapi/v1/txt2img`.
    A1111,
    /// `GET <endpoint>` where the URL itself carries the prompt
    /// (`{prompt}`, `{width}`, `{height}`, `{seed}`, `{steps}`). This is how a
    /// small self-hosted server or a keyless image service plugs in.
    UrlTemplate,
    /// fal-style: `POST {base}/{model}` with `{"prompt": ...}` answers
    /// synchronously with `{"images":[{"url": ...}]}`. The async queue API is
    /// not used: one image per call does not need a job id.
    Fal,
}

impl Backend {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => None,
            "openai_images" | "openai" | "images" => Some(Self::OpenAiImages),
            "chat_image" | "chat" => Some(Self::ChatImage),
            "a1111" | "automatic1111" | "sdwebui" | "sd" => Some(Self::A1111),
            "url_template" | "template" | "url" => Some(Self::UrlTemplate),
            "fal" | "fal_ai" => Some(Self::Fal),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiImages => "openai_images",
            Self::ChatImage => "chat_image",
            Self::A1111 => "a1111",
            Self::UrlTemplate => "url_template",
            Self::Fal => "fal",
        }
    }
}

/// Everything one generation call needs, resolved from settings. Kept as a
/// value so a caller can show the user *which* service a prompt would reach
/// before anything is billed.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageSpec {
    /// id used in logs and tool results (a catalog row name, a provider id, or
    /// `inline` for an endpoint configured directly in `[image]`).
    pub provider: String,
    pub label: String,
    pub backend: Backend,
    pub base: String,
    pub model: Option<String>,
    pub key_env: Option<String>,
    pub key_header: Option<String>,
    pub steps: Option<u32>,
    pub timeout_secs: u64,
    /// Route for image-to-image; None = this service cannot edit.
    pub edit_endpoint: Option<String>,
    /// How many reference images the service takes (0 = no edit support).
    pub max_reference_images: usize,
    /// Route for a high-resolution pass; None = this service cannot upscale.
    pub upscale_endpoint: Option<String>,
    /// Declared USD per generated image, when the operator told us.
    pub price_per_image: Option<f64>,
}

/// Which provider id a call should use: the request's, else `[image].provider`,
/// else the chat provider (a router that also serves images).
fn provider_id(cfg: &Config, explicit: Option<&str>) -> String {
    explicit
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| {
            cfg.image
                .as_ref()
                .and_then(|s| s.provider.as_deref())
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
        })
        .or_else(|| cfg.model.as_ref().map(|m| m.provider.clone()))
        .unwrap_or_default()
}

fn wire_of(cfg: &Config, provider: &str) -> String {
    cfg.custom_providers
        .get(&catalog::canonical_provider_id(provider))
        .map(|c| c.api_mode.trim().to_ascii_lowercase())
        .or_else(|| {
            catalog::provider(provider).map(|p| match p.api_mode {
                ApiMode::OpenAi => "openai".to_string(),
                ApiMode::Anthropic => "anthropic".to_string(),
                ApiMode::Gemini => "gemini".to_string(),
            })
        })
        .unwrap_or_else(|| "openai".to_string())
}

/// Resolve the service a prompt would go to.
///
/// Order: an `[image_providers.<id>]` row (the image catalogue), then a chat
/// provider that happens to serve images (a router), then an endpoint written
/// straight into `[image]`. Refuses with a message naming the options rather
/// than guessing - a wrong guess bills the wrong service.
pub fn resolve_spec(cfg: &Config, req: &ImageRequest) -> Result<ImageSpec, PantheonError> {
    let section = cfg.image.clone().unwrap_or_default();
    let want = provider_id(cfg, req.provider.as_deref());
    let explicit_backend = section.backend.as_deref().map(str::trim);
    let endpoint_override = section
        .endpoint
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty());

    // 1. The image catalogue.
    if let Some(row) = cfg.image_providers.get(&want) {
        let backend = Backend::parse(&row.backend).ok_or_else(|| {
            ierr(
                "IMAGE_BAD_BACKEND",
                format!(
                    "image_providers.{want}.backend {:?} is not one of \
                     openai_images|chat_image|a1111|url_template|fal",
                    row.backend
                ),
            )
        })?;
        let base = endpoint_override
            .map(str::to_string)
            .unwrap_or_else(|| row.base_url.trim_end_matches('/').to_string());
        if base.is_empty() {
            return Err(ierr(
                "IMAGE_NO_ENDPOINT",
                format!("image_providers.{want}.base_url is empty"),
            ));
        }
        let model = req
            .model
            .clone()
            .or_else(|| row.models.first().map(|m| m.id.clone()));
        let tabs = base.contains("{image}");
        return Ok(ImageSpec {
            provider: want,
            label: row.label.clone().unwrap_or_else(|| "image service".into()),
            backend,
            base: base.clone(),
            model,
            key_env: row.key_env.clone(),
            key_header: row.key_header.clone(),
            steps: row.steps.or(section.steps),
            timeout_secs: row
                .timeout_secs
                .or(section.timeout_secs)
                .filter(|s| *s > 0)
                .unwrap_or(IMAGE_TIMEOUT_SECS),
            edit_endpoint: row.edit_endpoint.clone(),
            upscale_endpoint: row.upscale_endpoint.clone(),
            price_per_image: row.price_per_image,
            max_reference_images: row.max_reference_images.unwrap_or_else(|| {
                // A row that does not say: the wire decides. A chat model takes
                // images inline (a handful), an SD server takes one init image,
                // fal needs an explicit edit route, the rest cannot edit.
                match backend {
                    Backend::ChatImage => 4,
                    Backend::A1111 => 1,
                    Backend::Fal => usize::from(row.edit_endpoint.is_some()),
                    // A template that names `{image}` takes a source image;
                    // one that does not cannot.
                    Backend::UrlTemplate => usize::from(tabs),
                    _ => 0,
                }
            }),
        });
    }

    // 2. A chat provider used for images (routers, OpenAI-wire endpoints).
    let chat_endpoint = endpoint(cfg, &want);
    let backend = match explicit_backend.and_then(Backend::parse) {
        Some(b) => b,
        None => {
            if let Some(name) = explicit_backend.filter(|n| {
                !n.is_empty() && !n.eq_ignore_ascii_case("auto")
            }) {
                return Err(ierr(
                    "IMAGE_BAD_BACKEND",
                    format!(
                        "unknown image backend {name:?} \
                         (openai_images|chat_image|a1111|url_template|fal)"
                    ),
                ));
            }
            let ep = endpoint_override.unwrap_or("");
            if ep.contains("{prompt}") {
                Backend::UrlTemplate
            } else if ep.contains("sdapi") {
                Backend::A1111
            } else {
                match wire_of(cfg, &want).as_str() {
                    "openai" => Backend::OpenAiImages,
                    other => {
                        return Err(ierr(
                            "IMAGE_UNSUPPORTED_WIRE",
                            format!(
                                "provider '{want}' speaks '{other}' and has no image endpoint; \
                                 add an [image_providers.<id>] row and point [image].provider at it"
                            ),
                        ))
                    }
                }
            }
        }
    };

    // 3. An endpoint written straight into [image] (no catalog row).
    let base = match endpoint_override {
        Some(e) => e.trim_end_matches('/').to_string(),
        None => match &chat_endpoint {
            Some((b, _)) => b.clone(),
            None => {
                return Err(ierr(
                    "IMAGE_NO_ENDPOINT",
                    format!(
                        "no image service configured: add [image_providers.<id>] with \
                         backend/base_url and set [image].provider, or put endpoint directly \
                         in [image] (provider '{want}' has no base URL)"
                    ),
                ))
            }
        },
    };
    let model = req
        .model
        .clone()
        .or_else(|| section.model.clone())
        .or_else(|| model_for(cfg, &want, None));
    Ok(ImageSpec {
        provider: want.clone(),
        label: cfg
            .custom_providers
            .get(&catalog::canonical_provider_id(&want))
            .and_then(|c| c.label.clone())
            .unwrap_or_else(|| want.clone()),
        backend,
        base,
        model,
        key_env: section.key_env.clone().or_else(|| chat_endpoint.map(|(_, k)| k).unwrap_or(None)),
        key_header: None,
        steps: section.steps,
        timeout_secs: section
            .timeout_secs
            .filter(|s| *s > 0)
            .unwrap_or(IMAGE_TIMEOUT_SECS),
        // A service reachable only through [image].endpoint/its chat provider
        // has no declared edit route: editing and upscaling are opt-in per
        // catalogue row.
        edit_endpoint: None,
        upscale_endpoint: None,
        price_per_image: None,
        max_reference_images: 0,
    })
}

/// Backwards-compatible helper: the backend a prompt would use.
pub fn resolve_backend(cfg: &Config) -> Result<Backend, PantheonError> {
    let req = ImageRequest::text("x");
    resolve_spec(cfg, &req).map(|s| s.backend)
}

/// One row of `pantheon image list`: a configured service, or a preset worth
/// copying into the config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageProviderInfo {
    pub id: String,
    pub label: String,
    pub backend: String,
    pub base: String,
    pub models: Vec<String>,
    pub key_env: Option<String>,
    pub configured: bool,
    /// Selected by `[image].provider` (or the chat provider when that is
    /// absent), i.e. what a prompt would use right now.
    pub selected: bool,
}

/// Presets shown by `pantheon image list` when the catalogue is empty. They are
/// suggestions with the right wire, not enabled services: nothing here is
/// used until the user writes a row and points `[image].provider` at it.
pub fn presets() -> Vec<ImageProviderInfo> {
    vec![
        ImageProviderInfo {
            id: "pollinations".into(),
            label: "Pollinations (keyless)".into(),
            backend: "url_template".into(),
            base: "https://image.pollinations.ai/prompt/{prompt}?width={width}&height={height}&nologo=true&seed={seed}".into(),
            models: vec![],
            key_env: None,
            configured: false,
            selected: false,
        },
        ImageProviderInfo {
            id: "fal".into(),
            label: "fal.ai".into(),
            backend: "fal".into(),
            base: "https://fal.run".into(),
            models: vec!["fal-ai/flux/schnell".into()],
            key_env: Some("FAL_KEY".into()),
            configured: false,
            selected: false,
        },
        ImageProviderInfo {
            id: "openai-images".into(),
            label: "OpenAI-compatible images".into(),
            backend: "openai_images".into(),
            base: "https://api.openai.com/v1".into(),
            models: vec!["gpt-image-1".into()],
            key_env: Some("OPENAI_API_KEY".into()),
            configured: false,
            selected: false,
        },
        ImageProviderInfo {
            id: "local-sd".into(),
            label: "Local Stable-Diffusion (A1111/Forge)".into(),
            backend: "a1111".into(),
            base: "http://127.0.0.1:7860".into(),
            models: vec![],
            key_env: None,
            configured: false,
            selected: false,
        },
    ]
}

/// What `pantheon image list` prints: configured rows first, then presets the
/// user has not set up, with the active one marked.
pub fn catalog(cfg: &Config) -> Vec<ImageProviderInfo> {
    let active = cfg
        .image
        .as_ref()
        .and_then(|s| s.provider.as_deref())
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| cfg.model.as_ref().map(|m| m.provider.clone()));
    let mut out: Vec<ImageProviderInfo> = cfg
        .image_providers
        .iter()
        .map(|(id, row)| ImageProviderInfo {
            id: id.clone(),
            label: row.label.clone().unwrap_or_else(|| id.clone()),
            backend: row.backend.clone(),
            base: row.base_url.clone(),
            models: row.models.iter().map(|m| m.id.clone()).collect(),
            key_env: row.key_env.clone(),
            configured: true,
            selected: active.as_deref() == Some(id.as_str()),
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    for mut preset in presets() {
        if cfg.image_providers.contains_key(&preset.id) {
            continue;
        }
        preset.selected = active.as_deref() == Some(preset.id.as_str());
        out.push(preset);
    }
    out
}

/// What the caller asked for, before it is translated per backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeHint {
    /// `landscape` | `square` | `portrait` - the model's vocabulary, mapped to
    /// whatever the backend accepts.
    Semantic(&'static str),
    Pixels(u32, u32),
    /// Nothing usable was given: the backend's own default.
    Unset,
}

/// Parse `size`: a literal `WxH`, or a semantic aspect (`landscape`, `square`,
/// `portrait`, plus the `16:9`/`1:1`/`9:16` spellings). An unparsable value is
/// `Unset` rather than an error, so a model passing `"wide"` still gets a
/// picture instead of a rejection it will repeat.
pub fn size_hint(size: Option<&str>) -> SizeHint {
    let Some(raw) = size.map(str::trim).filter(|s| !s.is_empty()) else {
        return SizeHint::Unset;
    };
    let lowered = raw.to_ascii_lowercase().replace(' ', "");
    let semantic = match lowered.as_str() {
        "landscape" | "wide" | "16:9" | "3:2" | "4:3" | "horizontal" => Some("landscape"),
        "square" | "1:1" => Some("square"),
        "portrait" | "tall" | "9:16" | "2:3" | "3:4" | "vertical" => Some("portrait"),
        _ => None,
    };
    if let Some(s) = semantic {
        return SizeHint::Semantic(s);
    }
    match size_parts(Some(raw)) {
        Some((w, h)) => SizeHint::Pixels(w, h),
        None => SizeHint::Unset,
    }
}

/// OpenAI-wire `size` string. A literal spelling returns as given; a semantic
/// aspect maps to the established trio.
pub fn openai_size(size: Option<&str>) -> Option<String> {
    match size_hint(size) {
        SizeHint::Pixels(w, h) => Some(format!("{w}x{h}")),
        SizeHint::Semantic("square") => Some("1024x1024".into()),
        SizeHint::Semantic("portrait") => Some("1024x1536".into()),
        SizeHint::Semantic(_) => Some("1536x1024".into()),
        SizeHint::Unset => None,
    }
}

/// fal `image_size`: a preset enum for a semantic aspect, an object for pixels.
pub fn fal_image_size(size: Option<&str>) -> Option<serde_json::Value> {
    match size_hint(size) {
        SizeHint::Pixels(w, h) => Some(serde_json::json!({"width": w, "height": h})),
        SizeHint::Semantic("square") => Some(serde_json::json!("square_hd")),
        SizeHint::Semantic("portrait") => Some(serde_json::json!("portrait_16_9")),
        SizeHint::Semantic(_) => Some(serde_json::json!("landscape_16_9")),
        SizeHint::Unset => None,
    }
}

/// Width/height for servers that take pixels (A1111, templates).
pub fn pixel_dims(size: Option<&str>) -> Option<(u32, u32)> {
    match size_hint(size) {
        SizeHint::Pixels(w, h) => Some((w, h)),
        SizeHint::Semantic("square") => Some((1024, 1024)),
        SizeHint::Semantic("portrait") => Some((832, 1216)),
        SizeHint::Semantic(_) => Some((1216, 832)),
        SizeHint::Unset => None,
    }
}

/// Split `1024x1024` into width/height. Absent or unparsable = the backend's
/// own default (each server has a different natural size).
pub fn size_parts(size: Option<&str>) -> Option<(u32, u32)> {
    let s = size?.trim().to_ascii_lowercase().replace('*', "x");
    let (w, h) = s.split_once('x')?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 || w > 8192 || h > 8192 {
        return None;
    }
    Some((w, h))
}

/// Percent-encode a string for a URL path segment or query value (RFC 3986
/// unreserved set kept). Small and local: the prompt is untrusted text and
/// must not be able to inject query parameters into a backend URL.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Fill a `url_template` endpoint. Unknown placeholders are left alone so a
/// template can carry the server's own literal braces.
pub fn fill_template(
    endpoint: &str,
    prompt: &str,
    size: Option<&str>,
    seed: u64,
    steps: Option<u32>,
) -> String {
    // Semantic aspects become pixels here: a template takes numbers, and a
    // portrait request must not silently render square.
    let (w, h) = pixel_dims(size).unwrap_or((0, 0));
    let aspect = match size_hint(size) {
        SizeHint::Semantic(s) => s.to_string(),
        SizeHint::Pixels(w, h) => format!("{w}:{h}"),
        SizeHint::Unset => String::new(),
    };
    endpoint
        .replace("{prompt}", &url_encode(prompt))
        .replace("{width}", &w.to_string())
        .replace("{height}", &h.to_string())
        .replace("{aspect}", &aspect)
        .replace("{seed}", &seed.to_string())
        .replace("{steps}", &steps.unwrap_or(0).to_string())
}

/// One HTTP round trip with the image timeout applied. `headers` are
/// `(name, value)` pairs; the agent is otherwise the shared one.
fn http_call(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, PantheonError> {
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let applied = |mut req: ureq::RequestBuilder<ureq::typestate::WithoutBody>| {
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req
    };
    let resp = if method == "GET" {
        applied(agent.get(url))
            .call()
            .map_err(|e| ierr("IMAGE_NETWORK", format!("{url}: {e}")))?
    } else {
        let mut req = agent.post(url);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        req.send(body.unwrap_or(""))
            .map_err(|e| ierr("IMAGE_NETWORK", format!("{url}: {e}")))?
    };
    let status = resp.status().as_u16();
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_body()
        .into_reader()
        .take((MAX_IMAGE_BYTES * 2 + 4096) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| ierr("IMAGE_READ", format!("{url}: {e}")))?;
    if status >= 400 {
        // A non-2xx with a real image body is still a usable answer: some
        // servers answer 402/429 while still serving the picture (queue
        // limits, ad-funded endpoints). Bytes we can verify beat a status
        // code we cannot.
        if sniff_mime(&buf).is_some() {
            return Ok(buf);
        }
        let snippet: String = String::from_utf8_lossy(&buf).chars().take(400).collect();
        return Err(ierr(
            &format!("IMAGE_HTTP_{status}"),
            format!("{url} answered {status}: {snippet}"),
        ));
    }
    Ok(buf)
}

/// Final checks every backend's bytes pass before they become a file.
///
/// A batch fails as a whole if any image is unusable: handing back "2 of 3
/// images" hides a truncated or error payload, and the caller cannot tell
/// which frame is missing.
fn finish(
    images: Vec<(Vec<u8>, Option<String>)>,
    model: String,
    provider: String,
    backend: Backend,
    revised: Option<String>,
    price: Option<f64>,
) -> Result<ImageOutput, PantheonError> {
    if images.is_empty() {
        return Err(ierr("IMAGE_EMPTY", "the backend returned no images".into()));
    }
    let mut out = Vec::with_capacity(images.len());
    for (bytes, claimed) in images {
        if bytes.is_empty() {
            return Err(ierr("IMAGE_EMPTY", "the backend returned an empty image".into()));
        }
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ierr(
                "IMAGE_TOO_LARGE",
                format!("{} bytes exceeds the {MAX_IMAGE_BYTES} cap", bytes.len()),
            ));
        }
        let mime = sniff_mime(&bytes)
            .or(claimed.as_deref().filter(|m| !m.is_empty()))
            .unwrap_or("application/octet-stream")
            .to_string();
        out.push(ImageBlob { bytes, mime });
    }
    Ok(ImageOutput {
        images: out,
        model,
        provider,
        revised_prompt: revised,
        backend: backend.as_str().to_string(),
        edited: false,
        seed: None,
        price,
    })
}

/// The refusal an edit gets when the selected service cannot take an image in.
/// Its remediation is different from a plain generation failure, so it says what
/// to configure instead of "check the prompt".
fn iedit_err(cause: String) -> PantheonError {
    PantheonError::new(
        "IMAGE_NO_EDIT",
        Layer::Provider,
        false,
        cause,
        "select an edit-capable service: a fal row with edit_endpoint, a chat_image or a1111 \n         backend, or a url_template whose URL contains {image}; `pantheon image list` shows them",
        String::new(),
    )
}

/// Ask the configured backend for one image.
pub fn generate_image(cfg: &Config, req: &ImageRequest) -> Result<ImageOutput, PantheonError> {
    if req.prompt.trim().is_empty() {
        return Err(ierr("IMAGE_NO_PROMPT", "the prompt is empty".into()));
    }
    let spec = resolve_spec(cfg, req)?;
    let timeout = std::time::Duration::from_secs(spec.timeout_secs);
    let editing = !req.source_images.is_empty();
    if editing && spec.max_reference_images == 0 {
        return Err(iedit_err(format!(
            "service '{}' ({}) cannot take a source image",
            spec.provider,
            spec.backend.as_str()
        )));
    }
    // More references than the service takes would be rejected by the far end;
    // clamp like every other provider adapter does.
    let sources: Vec<String> = req
        .source_images
        .iter()
        .take(spec.max_reference_images.max(1))
        .cloned()
        .collect();
    let section = cfg.image.clone().unwrap_or_default();
    let key = spec
        .key_env
        .as_deref()
        .filter(|e| !e.trim().is_empty())
        .and_then(|e| std::env::var(e).ok())
        .or_else(|| {
            let k = key_for(cfg, &spec.provider, spec.key_env.as_deref());
            (!k.is_empty()).then_some(k)
        });

    // Batch size is a wire capability, not a wish: a single-image endpoint that
    // is asked for three would silently return one and still be billed as one.
    let batch = req.batch();
    if batch > 1 && matches!(spec.backend, Backend::UrlTemplate | Backend::ChatImage) {
        return Err(ierr(
            "IMAGE_NO_BATCH",
            format!(
                "backend {} returns one image per call; use a fal/a1111/openai_images service \
                 for n > 1, or make {batch} calls",
                spec.backend.as_str()
            ),
        ));
    }

    match spec.backend {
        Backend::UrlTemplate => {
            let lower = spec.base.to_ascii_lowercase();
            if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                return Err(ierr(
                    "IMAGE_BAD_URL",
                    format!("url_template must be an http(s) URL: {}", spec.base),
                ));
            }
            if !spec.base.contains("{prompt}") {
                return Err(ierr(
                    "IMAGE_BAD_URL",
                    format!(
                        "backend url_template needs {{prompt}} in the endpoint: {}",
                        spec.base
                    ),
                ));
            }
            // An explicit seed makes the render reproducible; otherwise a fresh
            // one, reported back so the caller can ask for the same thing again.
            let seed = req.seed.unwrap_or_else(random_seed);
            let mut url =
                fill_template(&spec.base, &req.prompt, req.size.as_deref(), seed, spec.steps);
            if editing {
                // An inline data URI would be a multi-megabyte query string and
                // most template services fetch the URL themselves, so only a
                // fetchable reference can be used here.
                let first = sources.first().cloned().unwrap_or_default();
                let lower = first.to_ascii_lowercase();
                if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                    return Err(iedit_err(
                        "a url_template edit needs the source as an http(s) URL the service can fetch"
                            .to_string(),
                    ));
                }
                if !spec.base.contains("{image}") {
                    return Err(iedit_err(format!(
                        "service '{}' template has no {{image}} placeholder",
                        spec.provider
                    )));
                }
                url = url.replace("{image}", &url_encode(&first));
            }
            let mut headers = vec![("accept".to_string(), "image/*".to_string())];
            if let Some(k) = key.clone() {
                let (n, v) = key_pair(&spec, &k);
                headers.push((n, v));
            }
            let bytes = http_call("GET", &url, &headers, None, timeout)?;
            let mut out = finish(
                vec![(bytes, None)],
                spec.model.clone().unwrap_or_default(),
                spec.provider,
                spec.backend,
                None,
                spec.price_per_image,
            )?;
            out.edited = editing;
            out.seed = Some(seed);
            Ok(out)
        }
        Backend::A1111 => {
            let (url, body) = if editing {
                a1111_img2img_request(&spec.base, req, &sources, spec.steps)
            } else {
                a1111_request(&spec.base, req, spec.steps)
            };
            let json = http_call(
                "POST",
                &url,
                &[("content-type".to_string(), "application/json".to_string())],
                Some(&body),
                timeout,
            )?;
            let v: serde_json::Value = serde_json::from_slice(&json)
                .map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
            let mut images = Vec::new();
            for bytes in parse_a1111_all(&v)? {
                images.push((bytes, Some("image/png".to_string())));
            }
            let mut out = finish(
                images,
                spec.model.clone().unwrap_or_else(|| "sd".into()),
                spec.provider,
                spec.backend,
                None,
                spec.price_per_image,
            )?;
            out.edited = editing;
            out.seed = req.seed;
            Ok(out)
        }
        Backend::Fal => {
            let model = spec.model.clone().ok_or_else(|| {
                ierr(
                    "IMAGE_NO_MODEL",
                    format!(
                        "image service '{}' has no model; name one (e.g. fal-ai/flux/schnell)",
                        spec.provider
                    ),
                )
            })?;
            let (url, body) = if editing {
                let endpoint = spec.edit_endpoint.clone().ok_or_else(|| {
                    iedit_err(format!(
                        "fal service '{}' has no edit_endpoint; add one (e.g. \
                         fal-ai/flux-2-pro/edit)",
                        spec.provider
                    ))
                })?;
                (
                    format!("{}/{}", spec.base.trim_end_matches('/'), endpoint.trim_matches('/')),
                    fal_edit_body(req, &sources).to_string(),
                )
            } else {
                fal_request(&spec.base, &model, req)
            };
            let mut headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ];
            if let Some(k) = key.clone() {
                headers.push(("authorization".to_string(), format!("Key {k}")));
            }
            let json = http_call("POST", &url, &headers, Some(&body), timeout)?;
            let v: serde_json::Value = serde_json::from_slice(&json)
                .map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
            let mut images = Vec::new();
            for (bytes, claimed, remote) in parse_fal_all(&v)? {
                let bytes = match remote {
                    None => bytes,
                    Some(link) => http_call("GET", &link, &[], None, timeout)?,
                };
                images.push((bytes, Some(claimed)));
            }
            let mut out = finish(images, model, spec.provider, spec.backend, None, spec.price_per_image)?;
            out.edited = editing;
            out.seed = req.seed;
            Ok(out)
        }
        Backend::ChatImage | Backend::OpenAiImages => {
            if editing && spec.backend == Backend::OpenAiImages {
                return Err(iedit_err(format!(
                    "backend openai_images has no edit route here (service '{}')",
                    spec.provider
                )));
            }
            let model = spec.model.clone().ok_or_else(|| {
                ierr(
                    "IMAGE_NO_MODEL",
                    format!(
                        "no image model named; set [image].model or the provider row's models \
                         (service '{}')",
                        spec.provider
                    ),
                )
            })?;
            let mut headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ];
            if let Some(k) = key.clone() {
                let (n, v) = key_pair(&spec, &k);
                headers.push((n, v));
            }
            let (url, body) = match spec.backend {
                Backend::OpenAiImages => (
                    format!("{}/images/generations", spec.base),
                    request_body(&model, req).to_string(),
                ),
                _ => (
                    format!("{}/chat/completions", spec.base),
                    chat_image_body(&model, &req.prompt, &sources).to_string(),
                ),
            };
            let json = http_call("POST", &url, &headers, Some(&body), timeout)?;
            let v: serde_json::Value = serde_json::from_slice(&json)
                .map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
            let (items, revised) = match spec.backend {
                Backend::OpenAiImages => parse_response_all(&v)?,
                _ => {
                    let one = parse_chat_image(&v)?;
                    (vec![one], None)
                }
            };
            let mut images = Vec::new();
            for (bytes, claimed, _rev, remote) in items {
                let bytes = match remote {
                    None => bytes,
                    Some(link) => http_call("GET", &link, &[], None, timeout)?,
                };
                images.push((bytes, Some(claimed)));
            }
            let _ = &section;
            let mut out = finish(images, model, spec.provider, spec.backend, revised, spec.price_per_image)?;
            out.edited = editing;
            out.seed = req.seed;
            Ok(out)
        }
    }
}

/// Ask for a high-resolution pass over an existing image.
///
/// A separate call rather than a flag on generation: "make it bigger" and "make
/// it" are different intents, and an upscaler is a creative redraw that can
/// change fine detail (rendered text, faces). The caller opts in explicitly.
pub struct UpscaleRequest {
    /// Source image: absolute path, http(s) URL or `data:` URI.
    pub source: String,
    /// Requested factor (1.5-4.0). Backends clamp.
    pub scale: f64,
    /// Optional guidance for the redraw.
    pub prompt: Option<String>,
    /// Service override; `[image].provider` otherwise.
    pub provider: Option<String>,
}

pub fn upscale_image(cfg: &Config, req: &UpscaleRequest) -> Result<ImageOutput, PantheonError> {
    let spec = resolve_spec(
        cfg,
        &ImageRequest {
            prompt: req.prompt.clone().unwrap_or_default(),
            size: None,
            model: None,
            provider: req.provider.clone(),
            source_images: vec![req.source.clone()],
            n: 1,
            seed: None,
        },
    )?;
    let scale = req.scale.clamp(1.5, 4.0);
    let timeout = std::time::Duration::from_secs(spec.timeout_secs);
    let key = spec
        .key_env
        .as_deref()
        .filter(|e| !e.trim().is_empty())
        .and_then(|e| std::env::var(e).ok())
        .or_else(|| {
            let k = key_for(cfg, &spec.provider, spec.key_env.as_deref());
            (!k.is_empty()).then_some(k)
        });

    // A1111 exposes upscaling on its own route and takes the image as base64.
    if spec.backend == Backend::A1111 {
        let bare = match req.source.strip_prefix("data:") {
            Some(rest) => rest
                .split_once(',')
                .map(|(_, p)| p.to_string())
                .unwrap_or_else(|| req.source.clone()),
            None => {
                let path = std::path::Path::new(&req.source);
                let bytes = std::fs::read(path).map_err(|e| {
                    ierr(
                        "IMAGE_BAD_SOURCE",
                        format!("{}: {e} (an SD server needs a local file or data URI)", req.source),
                    )
                })?;
                pantheon_api::message::b64encode(&bytes)
            }
        };
        let body = serde_json::json!({
            "image": bare,
            "upscaling_resize": scale,
            "upscaler_1": "R-ESRGAN 4x+",
        })
        .to_string();
        let url = format!("{}/sdapi/v1/extra-single-image", spec.base.trim_end_matches('/'));
        let json = http_call(
            "POST",
            &url,
            &[("content-type".to_string(), "application/json".to_string())],
            Some(&body),
            timeout,
        )?;
        let v: serde_json::Value = serde_json::from_slice(&json)
            .map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
        let b64 = v
            .get("image")
            .and_then(|i| i.as_str())
            .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("no image in the upscale answer: {v}")))?;
        let bytes = b64decode(b64)
            .ok_or_else(|| ierr("IMAGE_BAD_BASE64", "unreadable base64 from the SD server".into()))?;
        return finish(
            vec![(bytes, Some("image/png".to_string()))],
            "sd-upscale".to_string(),
            spec.provider,
            spec.backend,
            None,
            spec.price_per_image,
        );
    }

    let endpoint = spec.upscale_endpoint.clone().ok_or_else(|| {
        PantheonError::new(
            "IMAGE_NO_UPSCALE",
            Layer::Provider,
            false,
            format!(
                "service '{}' ({}) has no upscale route",
                spec.provider,
                spec.backend.as_str()
            ),
            "add `upscale_endpoint = \"fal-ai/clarity-upscaler\"` to a fal [image_providers] row \
             (or select an a1111 service), then `pantheon image upscale`",
            String::new(),
        )
    })?;
    // fal's upscaler fetches the image itself; a data URI would be a huge body.
    let lower = req.source.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(PantheonError::new(
            "IMAGE_BAD_SOURCE",
            Layer::Provider,
            false,
            "the upscaler fetches the source itself, so it needs an http(s) URL".to_string(),
            "pass a URL, or use an a1111 service which accepts a local file",
            String::new(),
        ));
    }
    let mut body = serde_json::json!({
        "image_url": req.source,
        "upscale_factor": scale,
    });
    if let Some(p) = req.prompt.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        body["prompt"] = serde_json::Value::String(p.to_string());
    }
    let url = format!(
        "{}/{}",
        spec.base.trim_end_matches('/'),
        endpoint.trim_matches('/')
    );
    let mut headers = vec![
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
    ];
    if let Some(k) = key {
        headers.push(("authorization".to_string(), format!("Key {k}")));
    }
    let json = http_call("POST", &url, &headers, Some(body.to_string().as_str()), timeout)?;
    let v: serde_json::Value = serde_json::from_slice(&json)
        .map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
    let mut images = Vec::new();
    for (bytes, claimed, remote) in parse_fal_all(&v)? {
        let bytes = match remote {
            None => bytes,
            Some(link) => http_call("GET", &link, &[], None, timeout)?,
        };
        images.push((bytes, Some(claimed)));
    }
    finish(images, endpoint, spec.provider, spec.backend, None, spec.price_per_image)
}

/// A seed when the caller gave none. Small, local, and good enough for "another
/// take" - not a cryptographic draw.
fn random_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 % 1_000_000_000)
        .unwrap_or(0)
}

/// Auth header for a service: the row's header when it names one, otherwise
/// Bearer via the shared helper.
fn key_pair(spec: &ImageSpec, key: &str) -> (String, String) {
    match spec.key_header.as_deref().map(str::trim).filter(|h| !h.is_empty()) {
        Some(header) => {
            let name = header.to_string();
            let value = if name.eq_ignore_ascii_case("authorization") {
                format!("Bearer {key}")
            } else {
                key.to_string()
            };
            (name, value)
        }
        None => crate::http::auth_header_pair(&catalog::key_header_for(&spec.provider), key),
    }
}

/// fal: the model id is the path, the prompt is the body.
pub fn fal_request(base: &str, model: &str, req: &ImageRequest) -> (String, String) {
    let mut body = serde_json::json!({
        "prompt": req.prompt,
        "num_images": req.batch(),
    });
    if let Some(size) = fal_image_size(req.size.as_deref()) {
        body["image_size"] = size;
    }
    if let Some(seed) = req.seed {
        body["seed"] = serde_json::Value::from(seed);
    }
    (
        format!("{}/{}", base.trim_end_matches('/'), model.trim_matches('/')),
        body.to_string(),
    )
}

/// `{"images":[{"url": "...", "content_type": "image/png"}]}` - or inline
/// base64 for endpoints that return bytes.
pub fn parse_fal(
    v: &serde_json::Value,
) -> Result<(Vec<u8>, String, Option<String>), PantheonError> {
    let first = v
        .get("images")
        .and_then(|i| i.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("no images[0] in the answer: {v}")))?;
    let mime = first
        .get("content_type")
        .and_then(|m| m.as_str())
        .unwrap_or("application/octet-stream")
        .to_string();
    if let Some(url) = first.get("url").and_then(|u| u.as_str()) {
        return Ok((Vec::new(), mime, Some(url.to_string())));
    }
    if let Some(b64) = first.get("b64_json").and_then(|b| b.as_str()) {
        let bytes = b64decode(b64)
            .ok_or_else(|| ierr("IMAGE_BAD_BASE64", "unreadable base64 from fal".into()))?;
        return Ok((bytes, mime, None));
    }
    Err(ierr(
        "IMAGE_NO_DATA",
        "the fal answer had neither url nor b64_json".into(),
    ))
}

/// Every image in an OpenAI-wire answer (a batch arrives as `data[]`).
pub fn parse_response_all(
    v: &serde_json::Value,
) -> Result<(Vec<ParsedImage>, Option<String>), PantheonError> {
    let arr = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("provider returned no data array: {v}")))?;
    if arr.is_empty() {
        return Err(ierr("IMAGE_NO_DATA", "the data array was empty".into()));
    }
    let revised = arr
        .first()
        .and_then(|f| f.get("revised_prompt"))
        .and_then(|p| p.as_str())
        .map(str::to_string);
    let mut items = Vec::with_capacity(arr.len());
    let mut last_err = None;
    for entry in arr {
        // Reuse the single-item reader by wrapping one entry.
        let single = serde_json::json!({"data": [entry]});
        match parse_response(&single) {
            Ok(item) => items.push(item),
            Err(e) => last_err = Some(e),
        }
    }
    if items.is_empty() {
        return Err(last_err
            .unwrap_or_else(|| ierr("IMAGE_NO_DATA", "no usable image in the answer".into())));
    }
    Ok((items, revised))
}

/// Every image in a fal answer.
pub type FalFrame = (Vec<u8>, String, Option<String>);

pub fn parse_fal_all(v: &serde_json::Value) -> Result<Vec<FalFrame>, PantheonError> {
    let arr = v
        .get("images")
        .and_then(|i| i.as_array())
        .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("no images[] in the answer: {v}")))?;
    if arr.is_empty() {
        return Err(ierr("IMAGE_NO_DATA", "the images array was empty".into()));
    }
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let single = serde_json::json!({"images": [entry]});
        out.push(parse_fal(&single)?);
    }
    Ok(out)
}

/// Every frame an SD server returned.
pub fn parse_a1111_all(v: &serde_json::Value) -> Result<Vec<Vec<u8>>, PantheonError> {
    let arr = v
        .get("images")
        .and_then(|i| i.as_array())
        .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("no images[] in the answer: {v}")))?;
    let mut out = Vec::new();
    for entry in arr {
        if let Some(b64) = entry.as_str() {
            if let Some(bytes) = b64decode(b64) {
                out.push(bytes);
            }
        }
    }
    if out.is_empty() {
        return Err(ierr(
            "IMAGE_NO_DATA",
            "the SD server returned no usable frames".into(),
        ));
    }
    Ok(out)
}

/// Body for a chat model that emits images (`modalities: ["image","text"]`).
/// With source images the content becomes a parts array, which is how a chat
/// model takes an image in to edit or to ground a new one.
pub fn chat_image_body(model: &str, prompt: &str, sources: &[String]) -> serde_json::Value {
    let content = if sources.is_empty() {
        serde_json::Value::String(prompt.to_string())
    } else {
        let mut parts = vec![serde_json::json!({"type": "text", "text": prompt})];
        for src in sources {
            parts.push(serde_json::json!({
                "type": "image_url",
                "image_url": {"url": src},
            }));
        }
        serde_json::Value::Array(parts)
    };
    serde_json::json!({
        "model": model,
        "modalities": ["image", "text"],
        "messages": [{"role": "user", "content": content}],
    })
}

/// fal edit request: the edit endpoint takes `image_urls` (data URIs and http
/// URLs both work) plus the prompt.
pub fn fal_edit_body(req: &ImageRequest, sources: &[String]) -> serde_json::Value {
    let mut body = serde_json::json!({
        "prompt": req.prompt,
        "image_urls": sources,
        "num_images": req.batch(),
    });
    if let Some(seed) = req.seed {
        body["seed"] = serde_json::Value::from(seed);
    }
    if let Some(size) = fal_image_size(req.size.as_deref()) {
        body["image_size"] = size;
    }
    body
}

/// A1111 image-to-image: `init_images` are bare base64 (no `data:` prefix).
pub fn a1111_img2img_request(
    base: &str,
    req: &ImageRequest,
    sources: &[String],
    steps: Option<u32>,
) -> (String, String) {
    let (w, h) = pixel_dims(req.size.as_deref()).unwrap_or((512, 512));
    let init: Vec<String> = sources
        .iter()
        .map(|s| match s.strip_prefix("data:") {
            Some(rest) => rest
                .split_once(',')
                .map(|(_, payload)| payload.to_string())
                .unwrap_or_else(|| s.clone()),
            None => s.clone(),
        })
        .collect();
    let mut body = serde_json::json!({
        "prompt": req.prompt,
        "init_images": init,
        "width": w,
        "height": h,
        "batch_size": req.batch(),
        "n_iter": 1,
    });
    if let Some(steps) = steps.filter(|s| *s > 0) {
        body["steps"] = serde_json::Value::from(steps);
    }
    if let Some(seed) = req.seed {
        body["seed"] = serde_json::Value::from(seed);
    }
    if let Some(model) = req.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        body["override_settings"] = serde_json::json!({"sd_model_checkpoint": model});
    }
    (
        format!("{}/sdapi/v1/img2img", base.trim_end_matches('/')),
        body.to_string(),
    )
}

/// A chat-image answer carries the picture in `message.images[].image_url.url`
/// as a data URI (OpenRouter), or as `inline_data` parts (Gemini-shaped
/// gateways). `content` may be a plain string or a parts array.
pub fn parse_chat_image(v: &serde_json::Value) -> Result<ParsedImage, PantheonError> {
    let message = v
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
        .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("no choices[0].message in the answer: {v}")))?;
    if let Some(images) = message.get("images").and_then(|i| i.as_array()) {
        for img in images {
            let url = img
                .get("image_url")
                .and_then(|u| u.get("url"))
                .or_else(|| img.get("url"))
                .and_then(|u| u.as_str());
            if let Some(url) = url {
                if let Some(rest) = url.strip_prefix("data:") {
                    let (meta, payload) = rest.split_once(',').ok_or_else(|| {
                        ierr("IMAGE_BAD_DATA_URI", "malformed data URI".into())
                    })?;
                    let mime = meta.split(';').next().unwrap_or("").to_string();
                    let bytes = b64decode(payload).ok_or_else(|| {
                        ierr("IMAGE_BAD_BASE64", "unreadable base64 in the data URI".into())
                    })?;
                    return Ok((bytes, mime, None, None));
                }
                return Ok((Vec::new(), String::new(), None, Some(url.to_string())));
            }
        }
    }
    // Gemini-shaped parts.
    if let Some(parts) = message.get("content").and_then(|c| c.as_array()) {
        for part in parts {
            let inline = part
                .get("inline_data")
                .or_else(|| part.get("inlineData"))
                .filter(|v| !v.is_null());
            if let Some(inline) = inline {
                let mime = inline
                    .get("mime_type")
                    .or_else(|| inline.get("mimeType"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("image/png")
                    .to_string();
                if let Some(data) = inline.get("data").and_then(|d| d.as_str()) {
                    let bytes = b64decode(data).ok_or_else(|| {
                        ierr("IMAGE_BAD_BASE64", "unreadable inline image data".into())
                    })?;
                    return Ok((bytes, mime, None, None));
                }
            }
        }
    }
    let text = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .chars()
        .take(160)
        .collect::<String>();
    Err(ierr(
        "IMAGE_NO_DATA",
        format!("the chat model returned text, not an image: {text:?}"),
    ))
}

/// Stable-Diffusion WebUI (A1111) request: same fields the `/sdapi/v1/txt2img`
/// endpoint documents, minus the ones a text prompt cannot supply.
pub fn a1111_request(base: &str, req: &ImageRequest, steps: Option<u32>) -> (String, String) {
    let (w, h) = pixel_dims(req.size.as_deref()).unwrap_or((512, 512));
    let mut body = serde_json::json!({
        "prompt": req.prompt,
        "width": w,
        "height": h,
        "batch_size": req.batch(),
        "n_iter": 1,
    });
    if let Some(steps) = steps.filter(|s| *s > 0) {
        body["steps"] = serde_json::Value::from(steps);
    }
    if let Some(seed) = req.seed {
        body["seed"] = serde_json::Value::from(seed);
    }
    if let Some(model) = req.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        // In A1111 an "image model" is a checkpoint override.
        body["override_settings"] = serde_json::json!({"sd_model_checkpoint": model});
    }
    (
        format!("{}/sdapi/v1/txt2img", base.trim_end_matches('/')),
        body.to_string(),
    )
}

/// `{"images": ["<base64>", ...]}` — the first frame is the picture.
pub fn parse_a1111(v: &serde_json::Value) -> Result<Vec<u8>, PantheonError> {
    let first = v
        .get("images")
        .and_then(|i| i.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.as_str())
        .ok_or_else(|| ierr("IMAGE_NO_DATA", format!("no images[0] in the answer: {v}")))?;
    b64decode(first)
        .ok_or_else(|| ierr("IMAGE_BAD_BASE64", "unreadable base64 from the SD server".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        let mut c = Config {
            model: Some(pantheon_api::config::ModelSection {
                provider: "llm-router".to_string(),
                model: "router".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        };
        c.custom_providers.insert(
            "llm-router".to_string(),
            pantheon_api::config::CustomProviderSection {
                label: Some("LLM Router".into()),
                base_url: "http://127.0.0.1:8015/v1".into(),
                api_mode: "openai".into(),
                key_env: Some("PANTHEON_KEY_ROUTER".into()),
                models: vec![pantheon_api::config::CustomModel {
                    id: "router-image".into(),
                    ..Default::default()
                }],
            },
        );
        c
    }

    #[test]
    fn decodes_padded_and_unpadded_base64() {
        assert_eq!(b64decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(b64decode("aGVsbG8").unwrap(), b"hello");
        assert!(b64decode("").is_none());
        assert!(b64decode("****").is_none());
    }

    #[test]
    fn sniffs_the_real_format_not_the_claimed_one() {
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 1]), Some("image/png"));
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 1]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"RIFF....WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff_mime(b"not an image"), None);
    }

    #[test]
    fn request_body_omits_absent_size_and_asks_for_inline_bytes() {
        let req = ImageRequest {
            prompt: "a heron".into(),
            size: None,
            model: None,
            provider: None,
            source_images: Vec::new(),
            n: 1,
            seed: None,
        };
        let body = request_body("dall-e-3", &req);
        assert_eq!(body["model"], "dall-e-3");
        assert_eq!(body["response_format"], "b64_json");
        assert!(body.get("size").is_none(), "absent size must not be sent");
        let sized = request_body(
            "dall-e-3",
            &ImageRequest {
                size: Some("1536x1024".into()),
                ..req
            },
        );
        assert_eq!(sized["size"], "1536x1024");
    }

    #[test]
    fn parses_b64_json_and_reports_the_sniffed_mime() {
        let png = [0x89u8, b'P', b'N', b'G', 0x0d];
        let b64 = pantheon_api::message::b64encode(&png);
        let v = serde_json::json!({"data":[{"b64_json": b64, "mime_type": "image/jpeg"}]});
        let (bytes, mime, revised, remote) = parse_response(&v).unwrap();
        assert_eq!(bytes, png);
        assert_eq!(mime, "image/png", "the bytes win over the claimed mime");
        assert!(revised.is_none());
        assert!(remote.is_none());
    }

    #[test]
    fn parses_a_url_response_without_fetching_it() {
        let v = serde_json::json!({"data":[{"url":"https://example.test/a.png"}]});
        let (bytes, _mime, _revised, remote) = parse_response(&v).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(remote.as_deref(), Some("https://example.test/a.png"));
    }

    #[test]
    fn an_anthropic_shaped_provider_is_refused_by_name() {
        let mut c = cfg();
        c.custom_providers.get_mut("llm-router").unwrap().api_mode = "anthropic".into();
        let e = generate_image(
            &c,
            &ImageRequest {
                prompt: "x".into(),
                size: None,
                model: Some("m".into()),
                provider: None,
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_UNSUPPORTED_WIRE");
    }

    #[test]
    fn an_empty_prompt_never_reaches_the_network() {
        let e = generate_image(
            &cfg(),
            &ImageRequest {
                prompt: "   ".into(),
                size: None,
                model: Some("m".into()),
                provider: None,
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_PROMPT");
    }

    #[test]
    fn a_provider_without_endpoint_or_model_fails_loudly() {
        let c = Config {
            model: Some(pantheon_api::config::ModelSection {
                provider: "nope".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let e = generate_image(
            &c,
            &ImageRequest {
                prompt: "x".into(),
                size: None,
                model: None,
                provider: None,
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_ENDPOINT");

        let mut c = cfg();
        c.custom_providers.get_mut("llm-router").unwrap().models.clear();
        let e = generate_image(
            &c,
            &ImageRequest {
                prompt: "x".into(),
                size: None,
                model: None,
                provider: None,
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_MODEL", "no image model declared anywhere");
    }

    #[test]
    fn size_accepts_a_semantic_aspect_or_pixels_and_translates_per_backend() {
        assert_eq!(size_hint(Some("landscape")), SizeHint::Semantic("landscape"));
        assert_eq!(size_hint(Some("16:9")), SizeHint::Semantic("landscape"));
        assert_eq!(size_hint(Some("Portrait")), SizeHint::Semantic("portrait"));
        assert_eq!(size_hint(Some("1:1")), SizeHint::Semantic("square"));
        assert_eq!(size_hint(Some("768x512")), SizeHint::Pixels(768, 512));
        assert_eq!(size_hint(Some("wide")), SizeHint::Semantic("landscape"));
        assert_eq!(size_hint(Some("nonsense")), SizeHint::Unset);
        assert_eq!(size_hint(None), SizeHint::Unset);

        // openai literal, fal preset, sd pixels: same request, three wires.
        assert_eq!(openai_size(Some("square")).as_deref(), Some("1024x1024"));
        assert_eq!(openai_size(Some("landscape")).as_deref(), Some("1536x1024"));
        assert_eq!(openai_size(Some("640x480")).as_deref(), Some("640x480"));
        assert_eq!(fal_image_size(Some("square")), Some(serde_json::json!("square_hd")));
        assert_eq!(
            fal_image_size(Some("800x600")),
            Some(serde_json::json!({"width": 800, "height": 600}))
        );
        assert_eq!(pixel_dims(Some("portrait")), Some((832, 1216)));
        assert_eq!(pixel_dims(Some("512x512")), Some((512, 512)));
    }

    fn editable_cfg(backend: &str, edit: Option<&str>, refs: Option<usize>) -> Config {
        let mut c = cfg();
        c.image_providers.insert(
            "svc".into(),
            pantheon_api::config::ImageProviderSection {
                label: Some("Svc".into()),
                backend: backend.into(),
                base_url: "https://svc.test/v1".into(),
                models: vec![pantheon_api::config::CustomModel {
                    id: "m".into(),
                    ..Default::default()
                }],
                edit_endpoint: edit.map(str::to_string),
                max_reference_images: refs,
                ..Default::default()
            },
        );
        c.image = Some(pantheon_api::config::ImageSection {
            provider: Some("svc".into()),
            ..Default::default()
        });
        c
    }

    #[test]
    fn a_service_that_cannot_edit_refuses_a_source_image_by_name() {
        // url_template has no edit route.
        let c = editable_cfg("url_template", None, None);
        let mut req = ImageRequest::text("edit this");
        req.source_images.push("data:image/png;base64,AAAA".into());
        let e = generate_image(&c, &req).unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_EDIT");
        assert!(e.cause.contains("svc"), "{}", e.cause);
    }

    #[test]
    fn the_wire_decides_how_many_reference_images_a_row_takes() {
        // No declaration: chat_image takes a few, a1111 one, fal only with a route.
        let chat = resolve_spec(&editable_cfg("chat_image", None, None), &ImageRequest::text("x")).unwrap();
        assert_eq!(chat.max_reference_images, 4);
        let sd = resolve_spec(&editable_cfg("a1111", None, None), &ImageRequest::text("x")).unwrap();
        assert_eq!(sd.max_reference_images, 1);
        let fal_plain =
            resolve_spec(&editable_cfg("fal", None, None), &ImageRequest::text("x")).unwrap();
        assert_eq!(fal_plain.max_reference_images, 0);
        let fal_edit = resolve_spec(
            &editable_cfg("fal", Some("fal-ai/flux-2-pro/edit"), None),
            &ImageRequest::text("x"),
        )
        .unwrap();
        assert_eq!(fal_edit.max_reference_images, 1);
        assert_eq!(fal_edit.edit_endpoint.as_deref(), Some("fal-ai/flux-2-pro/edit"));
    }

    #[test]
    fn a_fal_edit_body_carries_the_source_images_and_the_size() {
        let body = fal_edit_body(
            &ImageRequest {
                prompt: "make it blue".into(),
                size: Some("square".into()),
                model: None,
                provider: None,
                source_images: vec![],
                n: 1,
                seed: None,
            },
            &["data:image/png;base64,AAA".to_string()],
        );
        assert_eq!(body["image_urls"][0], "data:image/png;base64,AAA");
        assert_eq!(body["image_size"], "square_hd");
        assert_eq!(body["num_images"], 1);
    }

    #[test]
    fn an_sd_edit_request_strips_the_data_uri_prefix() {
        let (url, body) = a1111_img2img_request(
            "http://127.0.0.1:7860/",
            &ImageRequest {
                prompt: "add snow".into(),
                size: Some("portrait".into()),
                model: None,
                provider: None,
                source_images: vec![],
                n: 1,
                seed: None,
            },
            &[
                "data:image/png;base64,QUJD".to_string(),
                "http://x.test/b.png".to_string(),
            ],
            Some(20),
        );
        assert_eq!(url, "http://127.0.0.1:7860/sdapi/v1/img2img");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["init_images"][0], "QUJD");
        assert_eq!(v["init_images"][1], "http://x.test/b.png");
        assert_eq!(v["width"], 832);
        assert_eq!(v["steps"], 20);
    }

    #[test]
    fn a_chat_image_body_switches_to_parts_when_sources_are_given() {
        let plain = chat_image_body("m", "a heron", &[]);
        assert!(plain["messages"][0]["content"].is_string());
        let edit = chat_image_body("m", "add snow", &["data:image/png;base64,AAA".into()]);
        let parts = edit["messages"][0]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[1]["type"], "image_url");
    }

    #[test]
    fn a_template_with_image_can_edit_and_one_without_cannot() {
        let mut c = cfg();
        c.image_providers.insert(
            "kontext".into(),
            pantheon_api::config::ImageProviderSection {
                backend: "url_template".into(),
                base_url: "https://img.test/p/{prompt}?image={image}".into(),
                ..Default::default()
            },
        );
        c.image = Some(pantheon_api::config::ImageSection {
            provider: Some("kontext".into()),
            ..Default::default()
        });
        let spec = resolve_spec(&c, &ImageRequest::text("x")).unwrap();
        assert_eq!(
            spec.max_reference_images, 1,
            "an {{image}} placeholder in the template means the service is editable"
        );

        // A data URI cannot ride a query string: refused with the edit error.
        let mut req = ImageRequest::text("make it blue");
        req.source_images.push("data:image/png;base64,AAAA".into());
        let e = generate_image(&c, &req).unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_EDIT");
        assert!(e.remediation.contains("edit-capable"), "{}", e.remediation);
    }

    #[test]
    fn an_openai_batch_answer_yields_every_frame() {
        let a = [0x89u8, b'P', b'N', b'G', 1];
        let b = [0x89u8, b'P', b'N', b'G', 2];
        let v = serde_json::json!({"data": [
            {"b64_json": pantheon_api::message::b64encode(&a), "revised_prompt": "one"},
            {"b64_json": pantheon_api::message::b64encode(&b)}
        ]});
        let (items, revised) = parse_response_all(&v).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, a);
        assert_eq!(items[1].0, b);
        assert_eq!(revised.as_deref(), Some("one"));

        // A batch fails as a whole when any frame is unusable.
        let bad = serde_json::json!({"data": [{"b64_json": "!!!!"}]});
        assert_eq!(parse_response_all(&bad).unwrap_err().code, "IMAGE_BAD_BASE64");
    }

    #[test]
    fn a_batch_request_carries_the_count_and_the_seed() {
        let req = ImageRequest {
            prompt: "a heron".into(),
            size: Some("square".into()),
            model: None,
            provider: None,
            source_images: vec![],
            n: 3,
            seed: Some(77),
        };
        let v = request_body("m", &req);
        assert_eq!(v["n"], 3);
        let (_, fal) = fal_request("https://fal.run", "fal-ai/flux/schnell", &req);
        let fal: serde_json::Value = serde_json::from_str(&fal).unwrap();
        assert_eq!(fal["num_images"], 3);
        assert_eq!(fal["seed"], 77);
        let (_, sd) = a1111_request("http://127.0.0.1:7860", &req, None);
        let sd: serde_json::Value = serde_json::from_str(&sd).unwrap();
        assert_eq!(sd["batch_size"], 3);
        assert_eq!(sd["seed"], 77);
        // A typo cannot order fifty paid images.
        assert_eq!(ImageRequest { n: 40, ..req.clone() }.batch(), MAX_IMAGES_PER_CALL);
    }

    #[test]
    fn a_single_image_wire_refuses_a_batch_instead_of_returning_one() {
        let mut c = cfg();
        c.image = Some(pantheon_api::config::ImageSection {
            endpoint: Some("https://img.test/{prompt}".into()),
            ..Default::default()
        });
        let mut req = ImageRequest::text("x");
        req.n = 3;
        let e = generate_image(&c, &req).unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_BATCH");
        assert!(e.cause.contains("url_template"), "{}", e.cause);
    }

    #[test]
    fn a_service_without_an_upscale_route_refuses_the_pass() {
        let c = editable_cfg("openai_images", None, None);
        let e = upscale_image(
            &c,
            &UpscaleRequest {
                source: "https://x.test/a.png".into(),
                scale: 2.0,
                prompt: None,
                provider: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_UPSCALE");
        assert!(e.remediation.contains("upscale_endpoint"), "{}", e.remediation);
    }

    #[test]
    fn an_upscaler_that_fetches_the_source_refuses_a_data_uri() {
        let mut c = editable_cfg("fal", None, None);
        c.image_providers.get_mut("svc").unwrap().upscale_endpoint =
            Some("fal-ai/clarity-upscaler".into());
        let e = upscale_image(
            &c,
            &UpscaleRequest {
                source: "data:image/png;base64,AAAA".into(),
                scale: 2.0,
                prompt: None,
                provider: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_BAD_SOURCE");
    }

    #[test]
    fn a_template_endpoint_carries_the_prompt_without_injecting_query_params() {
        let url = fill_template(
            "https://img.test/p/{prompt}?w={width}&h={height}&seed={seed}&steps={steps}",
            "a heron & a fox? #1",
            Some("1024x768"),
            42,
            Some(20),
        );
        assert!(url.contains("a%20heron%20%26%20a%20fox%3F%20%231"), "{url}");
        assert!(url.contains("w=1024&h=768"), "{url}");
        assert!(url.contains("seed=42"), "{url}");
        assert!(url.contains("steps=20"), "{url}");
        let fallback = fill_template("https://x/{prompt}?w={width}", "p", Some("huge"), 1, None);
        assert!(fallback.ends_with("w=0"), "{fallback}");
        // A semantic aspect fills pixels, not zeros: portrait must not render square.
        let portrait = fill_template("https://x/{prompt}?w={width}&h={height}", "p", Some("portrait"), 1, None);
        assert!(portrait.ends_with("w=832&h=1216"), "{portrait}");
        let aspect = fill_template("https://x/{prompt}?a={aspect}", "p", Some("landscape"), 1, None);
        assert!(aspect.ends_with("a=landscape"), "{aspect}");
    }

    #[test]
    fn size_parsing_rejects_nonsense() {
        assert_eq!(size_parts(Some("1024x1024")), Some((1024, 1024)));
        assert_eq!(size_parts(Some("512*512")), Some((512, 512)));
        assert_eq!(size_parts(Some("0x512")), None);
        assert_eq!(size_parts(Some("x")), None);
        assert_eq!(size_parts(None), None);
        assert_eq!(size_parts(Some("99999x1024")), None);
    }

    #[test]
    fn a_chat_image_answer_is_read_from_a_data_uri() {
        let png = [0x89u8, b'P', b'N', b'G', 1];
        let data = format!(
            "data:image/png;base64,{}",
            pantheon_api::message::b64encode(&png)
        );
        let v = serde_json::json!({
            "choices": [{"message": {"images": [{"image_url": {"url": data}}]}}]
        });
        let (bytes, mime, _r, remote) = parse_chat_image(&v).unwrap();
        assert_eq!(bytes, png);
        assert_eq!(mime, "image/png");
        assert!(remote.is_none());
    }

    #[test]
    fn a_chat_image_answer_is_read_from_gemini_inline_parts() {
        let jpg = [0xFFu8, 0xD8, 0xFF, 2];
        let v = serde_json::json!({
            "choices": [{"message": {"content": [
                {"type": "text", "text": "here"},
                {"inline_data": {"mime_type": "image/jpeg", "data": pantheon_api::message::b64encode(&jpg)}}
            ]}}]
        });
        let (bytes, mime, _r, _remote) = parse_chat_image(&v).unwrap();
        assert_eq!(bytes, jpg);
        assert_eq!(mime, "image/jpeg");
    }

    #[test]
    fn a_text_only_chat_answer_names_the_model_refusal() {
        let v = serde_json::json!({
            "choices": [{"message": {"content": "I can't create images"}}]
        });
        let e = parse_chat_image(&v).unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_DATA");
        assert!(e.cause.contains("I can't create images"), "{}", e.cause);
    }

    #[test]
    fn an_a1111_answer_is_decoded_and_its_request_maps_size_and_checkpoint() {
        let png = [0x89u8, b'P', b'N', b'G', 3];
        let v = serde_json::json!({"images": [pantheon_api::message::b64encode(&png)]});
        assert_eq!(parse_a1111(&v).unwrap(), png);

        let req = ImageRequest {
            prompt: "a heron".into(),
            size: Some("768x512".into()),
            model: Some("sdxl".into()),
            provider: None,
            source_images: Vec::new(),
            n: 1,
            seed: None,
        };
        let (url, body) = a1111_request("http://127.0.0.1:7860/", &req, Some(25));
        assert_eq!(url, "http://127.0.0.1:7860/sdapi/v1/txt2img");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["width"], 768);
        assert_eq!(v["height"], 512);
        assert_eq!(v["steps"], 25);
        assert_eq!(v["override_settings"]["sd_model_checkpoint"], "sdxl");
    }

    fn with_pollinations_row(cfg: &mut Config) {
        cfg.image_providers.insert(
            "pollinations".into(),
            pantheon_api::config::ImageProviderSection {
                label: Some("Pollinations".into()),
                backend: "url_template".into(),
                base_url: "https://image.pollinations.ai/prompt/{prompt}".into(),
                key_env: None,
                key_header: None,
                models: vec![],
                steps: Some(4),
                timeout_secs: Some(30),
                edit_endpoint: None,
                max_reference_images: None,
                upscale_endpoint: None,
                price_per_image: None,
            },
        );
        cfg.image = Some(pantheon_api::config::ImageSection {
            provider: Some("pollinations".into()),
            ..Default::default()
        });
    }

    #[test]
    fn a_catalog_row_supplies_backend_endpoint_and_timeout() {
        let mut c = cfg();
        with_pollinations_row(&mut c);
        let spec = resolve_spec(
            &c,
            &ImageRequest {
                prompt: "x".into(),
                size: None,
                model: None,
                provider: None,
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        )
        .unwrap();
        assert_eq!(spec.provider, "pollinations");
        assert_eq!(spec.backend, Backend::UrlTemplate);
        assert_eq!(spec.base, "https://image.pollinations.ai/prompt/{prompt}");
        assert_eq!(spec.steps, Some(4));
        assert_eq!(spec.timeout_secs, 30);
        assert_eq!(spec.label, "Pollinations");
    }

    #[test]
    fn a_request_level_provider_beats_the_configured_one() {
        // [image].provider says pollinations; the call names the chat provider,
        // which is what a router-backed image endpoint looks like.
        let mut c = cfg();
        with_pollinations_row(&mut c);
        let spec = resolve_spec(
            &c,
            &ImageRequest {
                prompt: "x".into(),
                size: None,
                model: Some("m".into()),
                provider: Some("llm-router".into()),
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        )
        .unwrap();
        assert_eq!(spec.provider, "llm-router");
        assert_eq!(spec.backend, Backend::OpenAiImages, "openai-wire chat provider");
        assert_eq!(spec.base, "http://127.0.0.1:8015/v1");
        // A provider that exists nowhere is an error, not a silent fallback.
        let e = resolve_spec(
            &c,
            &ImageRequest {
                prompt: "x".into(),
                size: None,
                model: None,
                provider: Some("nowhere".into()),
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_ENDPOINT");
    }

    #[test]
    fn a_catalog_row_with_a_bad_backend_or_no_url_is_refused_by_name() {
        let mut c = cfg();
        with_pollinations_row(&mut c);
        c.image_providers
            .get_mut("pollinations")
            .unwrap()
            .backend = "dalle".into();
        let e = resolve_spec(&c, &ImageRequest::text("x"))
            .unwrap_err();
        assert_eq!(e.code, "IMAGE_BAD_BACKEND");
        assert!(e.cause.contains("image_providers.pollinations"), "{}", e.cause);

        let mut c = cfg();
        with_pollinations_row(&mut c);
        c.image_providers.get_mut("pollinations").unwrap().base_url = String::new();
        let e = resolve_spec(&c, &ImageRequest::text("x"))
            .unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_ENDPOINT");
    }

    #[test]
    fn the_catalog_lists_configured_rows_then_presets_and_marks_the_active_one() {
        let mut c = cfg();
        with_pollinations_row(&mut c);
        let rows = catalog(&c);
        assert_eq!(rows[0].id, "pollinations");
        assert!(rows[0].configured);
        assert!(rows[0].selected);
        assert!(rows.iter().any(|r| r.id == "fal" && !r.configured));
        assert!(rows.iter().filter(|r| r.selected).count() == 1);
        assert!(presets().iter().any(|p| p.id == "local-sd"));
    }

    #[test]
    fn a_fal_answer_is_read_from_url_or_inline_bytes() {
        let v = serde_json::json!({"images":[{"url":"https://fal.test/a.png","content_type":"image/png"}]});
        let (bytes, mime, remote) = parse_fal(&v).unwrap();
        assert!(bytes.is_empty());
        assert_eq!(mime, "image/png");
        assert_eq!(remote.as_deref(), Some("https://fal.test/a.png"));
        let png = [0x89u8, b'P', b'N', b'G'];
        let v = serde_json::json!({"images":[{"b64_json": pantheon_api::message::b64encode(&png)}]});
        let (bytes, _mime, remote) = parse_fal(&v).unwrap();
        assert_eq!(bytes, png);
        assert!(remote.is_none());
    }

    #[test]
    fn a_fal_request_puts_the_model_in_the_path_and_the_prompt_in_the_body() {
        let (url, body) = fal_request(
            "https://fal.run/",
            "fal-ai/flux/schnell",
            &ImageRequest {
                prompt: "a heron".into(),
                size: Some("768x1024".into()),
                model: None,
                provider: None,
                source_images: Vec::new(),
                n: 1,
                seed: None,
            },
        );
        assert_eq!(url, "https://fal.run/fal-ai/flux/schnell");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["prompt"], "a heron");
        assert_eq!(v["image_size"]["width"], 768);
        assert_eq!(v["image_size"]["height"], 1024);
    }

    #[test]
    fn the_backend_is_inferred_from_the_settings_and_never_guessed_wrong() {
        let mut c = cfg();
        c.image = Some(pantheon_api::config::ImageSection {
            endpoint: Some("https://img.test/{prompt}".into()),
            ..Default::default()
        });
        assert_eq!(resolve_backend(&c).unwrap(), Backend::UrlTemplate);
        let mut c = cfg();
        c.image = Some(pantheon_api::config::ImageSection {
            endpoint: Some("http://127.0.0.1:7860/sdapi".into()),
            ..Default::default()
        });
        assert_eq!(resolve_backend(&c).unwrap(), Backend::A1111);
        let mut c = cfg();
        c.image = Some(pantheon_api::config::ImageSection {
            backend: Some("chat_image".into()),
            ..Default::default()
        });
        assert_eq!(resolve_backend(&c).unwrap(), Backend::ChatImage);
        assert_eq!(resolve_backend(&cfg()).unwrap(), Backend::OpenAiImages);
        let mut c = cfg();
        c.image = Some(pantheon_api::config::ImageSection {
            backend: Some("dalle".into()),
            ..Default::default()
        });
        assert_eq!(resolve_backend(&c).unwrap_err().code, "IMAGE_BAD_BACKEND");
    }

    #[test]
    fn a_declared_image_model_is_picked_automatically() {
        let c = cfg();
        assert_eq!(model_for(&c, "llm-router", None).as_deref(), Some("router-image"));
        assert_eq!(model_for(&c, "llm-router", Some("x")).as_deref(), Some("x"));
    }
}
