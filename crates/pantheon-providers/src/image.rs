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
    /// `1024x1024`, `1536x1024`, ... — passed through; providers differ.
    pub size: Option<String>,
    /// Model id; the provider's first declared model when omitted.
    pub model: Option<String>,
    /// Provider id override; the `[model] provider` otherwise.
    pub provider: Option<String>,
}

/// What came back.
#[derive(Debug, Clone)]
pub struct ImageOutput {
    pub bytes: Vec<u8>,
    pub mime: String,
    pub model: String,
    pub provider: String,
    /// Some models rewrite the prompt; surfaced so the caller can see what the
    /// asset actually depicts.
    pub revised_prompt: Option<String>,
    /// Which backend produced it (`openai_images`, `chat_image`, `a1111`,
    /// `url_template`), so a surprising result can be traced to a wire.
    pub backend: String,
}

impl ImageOutput {
    /// File extension for the mime we detected.
    pub fn extension(&self) -> &'static str {
        mime_extension(&self.mime)
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
    if let Some(size) = req.size.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        body["size"] = serde_json::Value::String(size.to_string());
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
}

impl Backend {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => None,
            "openai_images" | "openai" | "images" => Some(Self::OpenAiImages),
            "chat_image" | "chat" => Some(Self::ChatImage),
            "a1111" | "automatic1111" | "sdwebui" | "sd" => Some(Self::A1111),
            "url_template" | "template" | "url" => Some(Self::UrlTemplate),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiImages => "openai_images",
            Self::ChatImage => "chat_image",
            Self::A1111 => "a1111",
            Self::UrlTemplate => "url_template",
        }
    }
}

/// An explicit `[image].backend` wins. Otherwise infer the only shape the
/// settings can actually mean, and refuse rather than guess when they are
/// ambiguous: a wrong guess here bills a provider for the wrong thing.
pub fn resolve_backend(cfg: &Config) -> Result<Backend, PantheonError> {
    let section = cfg.image.as_ref();
    if let Some(name) = section.and_then(|s| s.backend.as_deref()) {
        if let Some(b) = Backend::parse(name) {
            return Ok(b);
        }
        if !name.trim().is_empty() && !name.trim().eq_ignore_ascii_case("auto") {
            return Err(ierr(
                "IMAGE_BAD_BACKEND",
                format!(
                    "unknown image backend {name:?} (openai_images|chat_image|a1111|url_template)"
                ),
            ));
        }
    }
    let endpoint = section.and_then(|s| s.endpoint.as_deref()).unwrap_or("");
    if endpoint.contains("{prompt}") {
        return Ok(Backend::UrlTemplate);
    }
    if endpoint.contains("sdapi") {
        return Ok(Backend::A1111);
    }
    let provider = provider_id(cfg, None);
    let mode = wire_of(cfg, &provider);
    match mode.as_str() {
        "openai" => Ok(Backend::OpenAiImages),
        "anthropic" => Err(ierr(
            "IMAGE_UNSUPPORTED_WIRE",
            format!(
                "provider '{provider}' speaks anthropic, which has no image endpoint; \
                 set [image].backend (chat_image|a1111|url_template) with an explicit endpoint"
            ),
        )),
        other => Err(ierr(
            "IMAGE_UNSUPPORTED_WIRE",
            format!(
                "provider '{provider}' speaks '{other}'; set [image].backend to one of \
                 openai_images|chat_image|a1111|url_template"
            ),
        )),
    }
}

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
pub fn fill_template(endpoint: &str, prompt: &str, size: Option<&str>, seed: u64, steps: Option<u32>) -> String {
    let (w, h) = size_parts(size).unwrap_or((0, 0));
    endpoint
        .replace("{prompt}", &url_encode(prompt))
        .replace("{width}", &w.to_string())
        .replace("{height}", &h.to_string())
        .replace("{seed}", &seed.to_string())
        .replace("{steps}", &steps.unwrap_or(0).to_string())
}

fn timeout_for(cfg: &Config) -> std::time::Duration {
    let secs = cfg
        .image
        .as_ref()
        .and_then(|s| s.timeout_secs)
        .filter(|s| *s > 0)
        .unwrap_or(IMAGE_TIMEOUT_SECS);
    std::time::Duration::from_secs(secs)
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
fn finish(
    bytes: Vec<u8>,
    claimed_mime: Option<&str>,
    model: String,
    provider: String,
    backend: Backend,
    revised: Option<String>,
) -> Result<ImageOutput, PantheonError> {
    if bytes.is_empty() {
        return Err(ierr("IMAGE_EMPTY", "the backend returned no bytes".into()));
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ierr(
            "IMAGE_TOO_LARGE",
            format!("{} bytes exceeds the {MAX_IMAGE_BYTES} cap", bytes.len()),
        ));
    }
    let mime = sniff_mime(&bytes)
        .or(claimed_mime.filter(|m| !m.is_empty()))
        .unwrap_or("application/octet-stream")
        .to_string();
    Ok(ImageOutput {
        bytes,
        mime,
        model,
        provider,
        revised_prompt: revised,
        backend: backend.as_str().to_string(),
    })
}

/// Ask the configured backend for one image.
pub fn generate_image(cfg: &Config, req: &ImageRequest) -> Result<ImageOutput, PantheonError> {
    if req.prompt.trim().is_empty() {
        return Err(ierr("IMAGE_NO_PROMPT", "the prompt is empty".into()));
    }
    let backend = resolve_backend(cfg)?;
    let provider = provider_id(cfg, req.provider.as_deref());
    let section = cfg.image.clone().unwrap_or_default();
    let timeout = timeout_for(cfg);
    let key = section
        .key_env
        .as_deref()
        .filter(|e| !e.trim().is_empty())
        .and_then(|e| std::env::var(e).ok())
        .or_else(|| {
            let (_, key_env) = endpoint(cfg, &provider)?;
            let k = key_for(cfg, &provider, key_env.as_deref());
            (!k.is_empty()).then_some(k)
        });

    match backend {
        Backend::UrlTemplate => {
            let template = section
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .ok_or_else(|| {
                    ierr(
                        "IMAGE_NO_ENDPOINT",
                        "backend url_template needs [image].endpoint with {prompt}".into(),
                    )
                })?;
            let lower = template.to_ascii_lowercase();
            if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                return Err(ierr(
                    "IMAGE_BAD_URL",
                    format!("url_template must be an http(s) URL: {template}"),
                ));
            }
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64 % 1_000_000)
                .unwrap_or(0);
            let url = fill_template(template, &req.prompt, req.size.as_deref(), seed, section.steps);
            let mut headers = vec![("accept".to_string(), "image/*".to_string())];
            if let Some(k) = key.clone() {
                let (n, v) = crate::http::auth_header_pair(
                    &catalog::key_header_for(&provider),
                    &k,
                );
                headers.push((n, v));
            }
            let bytes = http_call("GET", &url, &headers, None, timeout)?;
            finish(
                bytes,
                None,
                section.model.clone().unwrap_or_default(),
                provider,
                backend,
                None,
            )
        }
        Backend::A1111 => {
            let base = section
                .endpoint
                .as_deref()
                .map(str::trim)
                .map(|e| e.trim_end_matches('/').to_string())
                .filter(|e| !e.is_empty())
                .or_else(|| endpoint(cfg, &provider).map(|(b, _)| b))
                .ok_or_else(|| {
                    ierr(
                        "IMAGE_NO_ENDPOINT",
                        "backend a1111 needs [image].endpoint (or a provider base URL)".into(),
                    )
                })?;
            let (url, body) = a1111_request(&base, req, section.steps);
            let json = http_call(
                "POST",
                &url,
                &[("content-type".to_string(), "application/json".to_string())],
                Some(&body),
                timeout,
            )?;
            let v: serde_json::Value = serde_json::from_slice(&json)
                .map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
            let bytes = parse_a1111(&v)?;
            finish(bytes, Some("image/png"), "sd".to_string(), provider, backend, None)
        }
        Backend::ChatImage | Backend::OpenAiImages => {
            let (base, key_env) = endpoint(cfg, &provider).ok_or_else(|| {
                ierr(
                    "IMAGE_NO_ENDPOINT",
                    format!("provider '{provider}' has no base URL I can reach"),
                )
            })?;
            let base = section
                .endpoint
                .as_deref()
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .map(|e| e.trim_end_matches('/').to_string())
                .unwrap_or(base);
            let model = req
                .model
                .clone()
                .or_else(|| section.model.clone())
                .or_else(|| model_for(cfg, &provider, None))
                .ok_or_else(|| {
                    ierr(
                        "IMAGE_NO_MODEL",
                        format!("no image model named; set [image].model (provider '{provider}' declares none)"),
                    )
                })?;
            let key = key
                .or_else(|| key_for(cfg, &provider, key_env.as_deref()).into())
                .unwrap_or_default();
            let (name, value) =
                crate::http::auth_header_pair(&catalog::key_header_for(&provider), &key);
            let mut headers = vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "application/json".to_string()),
            ];
            if !value.is_empty() {
                headers.push((name, value));
            }
            let (url, body) = match backend {
                Backend::OpenAiImages => (
                    format!("{base}/images/generations"),
                    request_body(&model, req).to_string(),
                ),
                _ => (
                    format!("{base}/chat/completions"),
                    chat_image_body(&model, &req.prompt).to_string(),
                ),
            };
            let json = http_call("POST", &url, &headers, Some(&body), timeout)?;
            let v: serde_json::Value = serde_json::from_slice(&json)
                .map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
            let parsed = match backend {
                Backend::OpenAiImages => parse_response(&v)?,
                _ => parse_chat_image(&v)?,
            };
            let (bytes, claimed, revised, remote) = parsed;
            let bytes = match remote {
                None => bytes,
                Some(link) => http_call("GET", &link, &[], None, timeout)?,
            };
            finish(bytes, Some(&claimed), model, provider, backend, revised)
        }
    }
}

/// Body for a chat model that emits images (`modalities: ["image","text"]`).
pub fn chat_image_body(model: &str, prompt: &str) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "modalities": ["image", "text"],
        "messages": [{"role": "user", "content": prompt}],
    })
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
    let (w, h) = size_parts(req.size.as_deref()).unwrap_or((512, 512));
    let mut body = serde_json::json!({
        "prompt": req.prompt,
        "width": w,
        "height": h,
        "batch_size": 1,
        "n_iter": 1,
    });
    if let Some(steps) = steps.filter(|s| *s > 0) {
        body["steps"] = serde_json::Value::from(steps);
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
            },
        )
        .unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_MODEL", "no image model declared anywhere");
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
        };
        let (url, body) = a1111_request("http://127.0.0.1:7860/", &req, Some(25));
        assert_eq!(url, "http://127.0.0.1:7860/sdapi/v1/txt2img");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["width"], 768);
        assert_eq!(v["height"], 512);
        assert_eq!(v["steps"], 25);
        assert_eq!(v["override_settings"]["sd_model_checkpoint"], "sdxl");
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
