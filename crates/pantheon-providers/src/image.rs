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

/// Ask the configured provider for one image.
pub fn generate_image(cfg: &Config, req: &ImageRequest) -> Result<ImageOutput, PantheonError> {
    let provider_id = req
        .provider
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .or_else(|| cfg.model.as_ref().map(|m| m.provider.as_str()))
        .unwrap_or_default()
        .to_string();
    if req.prompt.trim().is_empty() {
        return Err(ierr("IMAGE_NO_PROMPT", "the prompt is empty".into()));
    }
    let (base, key_env) = endpoint(cfg, &provider_id).ok_or_else(|| {
        ierr(
            "IMAGE_NO_ENDPOINT",
            format!("provider '{provider_id}' has no base URL I can reach"),
        )
    })?;
    // An anthropic-shaped endpoint has no image API; say so instead of
    // POSTing to a path that will 404 with a confusing body.
    let mode = cfg
        .custom_providers
        .get(&catalog::canonical_provider_id(&provider_id))
        .map(|c| c.api_mode.trim().to_ascii_lowercase())
        .or_else(|| {
            catalog::provider(&provider_id).map(|p| match p.api_mode {
                ApiMode::OpenAi => "openai".to_string(),
                ApiMode::Anthropic => "anthropic".to_string(),
                ApiMode::Gemini => "gemini".to_string(),
            })
        })
        .unwrap_or_else(|| "openai".to_string());
    if mode != "openai" {
        return Err(ierr(
            "IMAGE_UNSUPPORTED_WIRE",
            format!("provider '{provider_id}' speaks '{mode}'; image generation needs an openai-compatible endpoint"),
        ));
    }
    let model = model_for(cfg, &provider_id, req.model.as_deref()).ok_or_else(|| {
        ierr(
            "IMAGE_NO_MODEL",
            format!("no image model named; set `model` (provider '{provider_id}' declares none)"),
        )
    })?;
    let body = request_body(&model, req);
    let key = key_for(cfg, &provider_id, key_env.as_deref());
    let (name, value) =
        crate::http::auth_header_pair(&catalog::key_header_for(&provider_id), &key);
    let url = format!("{base}/images/generations");
    let agent = crate::http::http_agent();
    let mut call = agent
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if !value.is_empty() {
        call = call.header(&name, &value);
    }
    let resp = call
        .send(body.to_string())
        .map_err(|e| ierr("IMAGE_NETWORK", format!("{url}: {e}")))?;
    let status = resp.status().as_u16();
    let text = resp
        .into_body()
        .read_to_string()
        .map_err(|e| ierr("IMAGE_READ", format!("{url}: {e}")))?;
    if status >= 400 {
        let snippet: String = text.chars().take(400).collect();
        return Err(ierr(
            &format!("IMAGE_HTTP_{status}"),
            format!("{url} answered {status}: {snippet}"),
        ));
    }
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| ierr("IMAGE_BAD_JSON", format!("{url}: {e}")))?;
    let (bytes, mut mime, revised, remote) = parse_response(&json)?;
    let bytes = match remote {
        None => bytes,
        Some(link) => download(&link)?,
    };
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(ierr(
            "IMAGE_TOO_LARGE",
            format!("{} bytes exceeds the {MAX_IMAGE_BYTES} cap", bytes.len()),
        ));
    }
    if let Some(sniffed) = sniff_mime(&bytes) {
        mime = sniffed.to_string();
    }
    if mime.is_empty() {
        mime = "application/octet-stream".to_string();
    }
    Ok(ImageOutput {
        bytes,
        mime,
        model,
        provider: provider_id,
        revised_prompt: revised,
    })
}

/// Fetch a generated image the provider returned as a URL. http(s) only.
fn download(link: &str) -> Result<Vec<u8>, PantheonError> {
    let lower = link.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(ierr(
            "IMAGE_BAD_URL",
            format!("refusing to fetch a non-http image URL: {link}"),
        ));
    }
    let agent = crate::http::http_agent();
    let resp = agent
        .get(link)
        .call()
        .map_err(|e| ierr("IMAGE_NETWORK", format!("{link}: {e}")))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(ierr(
            &format!("IMAGE_HTTP_{status}"),
            format!("{link} answered {status}"),
        ));
    }
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_body()
        .into_reader()
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| ierr("IMAGE_READ", format!("{link}: {e}")))?;
    Ok(buf)
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
    fn a_declared_image_model_is_picked_automatically() {
        let c = cfg();
        assert_eq!(model_for(&c, "llm-router", None).as_deref(), Some("router-image"));
        assert_eq!(model_for(&c, "llm-router", Some("x")).as_deref(), Some("x"));
    }
}
