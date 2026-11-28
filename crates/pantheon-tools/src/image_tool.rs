//! Agent-facing image generation tool.
//!
//! The runtime could read images and render SVG it wrote itself, but had no
//! way to *produce* a bitmap: "make me an app icon" ended with a model
//! hand-writing paths in a `<svg>`. This exposes the provider's
//! `/images/generations` endpoint under its own capability
//! ([`Capability::MediaGenerate`]), which default policies mark **approval** —
//! the prompt and size are visible before anything is billed.
//!
//! The bytes land in `<data_dir>/images/` rather than in the transcript: a
//! 2 MiB PNG inline would blow up every later request, and the ledger stores
//! rows, not blobs. The tool result names the path, the mime and the size so a
//! later turn can act on the file (read it, attach it, copy it).

use crate::tools::{ToolRegistry, tool_err};
use pantheon_api::capability::Capability;
use pantheon_api::config::Config;
use pantheon_api::error::Layer;
use pantheon_providers::image::{ImageRequest, generate_image, resolve_spec};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub const NAME: &str = "image";

fn perr(code: &str, cause: String) -> pantheon_api::error::PantheonError {
    tool_err(
        code,
        Layer::Execution,
        false,
        cause,
        "check the prompt and try again",
    )
}

/// Register the tool. `data_dir` is the install whose `images/` directory
/// receives the bytes, and whose `config.toml` says which provider to call.
///
/// The schema is built from that config: a service that cannot edit does not
/// advertise `source`, one that cannot batch does not advertise `n`, one
/// without an upscale route does not advertise `scale`. A model that never
/// sees a knob it cannot use stops asking for it.
pub fn register(reg: &mut ToolRegistry, data_dir: PathBuf) {
    let schema = schema_for(&data_dir);
    reg.register_with(
        schema,
        Capability::MediaGenerate,
        move |args| run(args, &data_dir),
        None,
    );
}

/// The tool schema for whatever service is configured right now.
fn schema_for(data_dir: &Path) -> pantheon_api::message::ToolSchema {
    let cfg = Config::load(data_dir).ok();
    let spec = cfg.as_ref().and_then(|c| {
        resolve_spec(
            c,
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
        .ok()
    });

    let mut props = serde_json::json!({
        "prompt": {
            "type": "string",
            "description": "what to draw, or what change to make when editing"
        },
        "size": {
            "type": "string",
            "description": "aspect (`landscape`, `square`, `portrait`) or pixels (`1024x1024`)"
        },
        "model": {
            "type": "string",
            "description": "image model id; defaults to the service's first declared model"
        },
        "name": {
            "type": "string",
            "description": "file stem for the saved image (optional)"
        }
    });
    let mut notes = Vec::new();
    if let Some(spec) = &spec {
        notes.push(format!(
            "Active service: {} ({}).",
            spec.provider,
            spec.backend.as_str()
        ));
        if spec.max_reference_images > 0 {
            props["source"] = serde_json::json!({
                "type": "string",
                "description": "image to edit: an absolute path, an http(s) URL, or a data URI"
            });
            props["references"] = serde_json::json!({
                "type": "array",
                "items": {"type": "string"},
                // The cap is real: the service clamps (or rejects) beyond it.
                "maxItems": spec.max_reference_images,
                "description": format!(
                    "up to {} style/composition reference images",
                    spec.max_reference_images
                )
            });
            notes.push(format!("It can edit and takes up to {} reference image(s).", spec.max_reference_images));
        } else {
            notes.push("It cannot take a source image, so text-to-image only.".to_string());
        }
        let batches = !matches!(
            spec.backend,
            pantheon_providers::image::Backend::UrlTemplate
                | pantheon_providers::image::Backend::ChatImage
        );
        if batches {
            props["n"] = serde_json::json!({
                "type": "integer",
                "minimum": 1,
                "maximum": 8,
                "description": "how many images to generate; each one is billed"
            });
        }
        if !matches!(
            spec.backend,
            pantheon_providers::image::Backend::ChatImage
        ) {
            props["seed"] = serde_json::json!({
                "type": "integer",
                "description": "deterministic seed: reuse one to explore the same composition"
            });
        }
        if spec.upscale_endpoint.is_some()
            || matches!(spec.backend, pantheon_providers::image::Backend::A1111)
        {
            notes.push("A separate `upscale` tool can enlarge a returned image.".to_string());
        }
    } else {
        // No readable config: advertise the full surface and let the call fail
        // with the configuration error, which is more useful than a schema that
        // silently hides every knob.
        props["source"] = serde_json::json!({"type": "string", "description": "image to edit"});
        props["references"] = serde_json::json!({
            "type": "array", "items": {"type": "string"}
        });
        props["n"] = serde_json::json!({"type": "integer", "minimum": 1, "maximum": 8});
        props["seed"] = serde_json::json!({"type": "integer"});
    }
    let description = format!(
        "Generate an image from a text prompt (or edit one) and save it to disk. Returns the \
         absolute paths, mime type, byte size and the backend that drew it. {}",
        notes.join(" ")
    );
    pantheon_api::message::ToolSchema {
        name: NAME.into(),
        description,
        parameters: serde_json::json!({
            "type": "object",
            "properties": props,
            "required": ["prompt"]
        }),
    }
}

fn slug(prompt: &str, fallback: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in prompt.chars().take(120) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= 32 {
            break;
        }
    }
    let trimmed = out.trim_end_matches('-');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn stamp() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    ms.to_string()
}

/// Only a bare file stem is accepted: a caller-supplied `name` must not be
/// able to steer the write outside `images/`.
fn safe_stem(name: &str) -> Option<String> {
    let n = name.trim();
    if n.is_empty() || n.len() > 64 {
        return None;
    }
    let ok = n
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok { Some(n.to_string()) } else { None }
}

fn run(args: &str, data_dir: &Path) -> Result<String, pantheon_api::error::PantheonError> {
    let v: Value = serde_json::from_str(args).map_err(|e| perr("IMAGE_ARGS", e.to_string()))?;
    let prompt = v
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .ok_or_else(|| perr("IMAGE_NO_PROMPT", "`prompt` is required".into()))?
        .to_string();
    let size = v
        .get("size")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let model = v
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let n = v
        .get("n")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(1);
    let seed = v.get("seed").and_then(Value::as_u64);
    let cfg = Config::load(data_dir).map_err(|e| perr("IMAGE_CONFIG", e.to_string()))?;
    // Source images: a local path becomes a data URI (a remote service cannot
    // read our disk), an http(s)/data value passes through after a scheme
    // check, and anything else is refused so a prompt cannot smuggle a
    // `file://` read into a request body. The size budget depends on the
    // backend: fal's gateway refuses bodies above ~4.5 MB.
    let budget = resolve_spec(
        &cfg,
        &ImageRequest {
            prompt: prompt.clone(),
            size: None,
            model: None,
            provider: None,
            source_images: Vec::new(),
            n: 1,
            seed: None,
        },
    )
    .map(|s| match s.backend {
        pantheon_providers::image::Backend::Fal => INLINE_BUDGET_FAL,
        _ => INLINE_BUDGET_ENCODED,
    })
    .unwrap_or(INLINE_BUDGET_ENCODED);
    let mut source_images = Vec::new();
    if let Some(src) = v.get("source").and_then(Value::as_str) {
        source_images.push(inline_source_with(src, budget)?);
    }
    if let Some(refs) = v.get("references").and_then(Value::as_array) {
        for r in refs.iter().filter_map(Value::as_str) {
            source_images.push(inline_source_with(r, budget)?);
        }
    }
    let started = std::time::Instant::now();
    let out = generate_image(
        &cfg,
        &ImageRequest {
            prompt: prompt.clone(),
            size: size.clone(),
            model: model.clone(),
            provider: None,
            source_images,
            n,
            seed,
        },
    );
    let (out, cost) = match out {
        Ok(out) => {
            let cost = out
                .price
                .map(|p| p * out.images.len() as f64);
            (out, cost)
        }
        Err(e) => {
            record_generation(data_dir, &cfg, &model, 0, false, None, started.elapsed().as_millis() as i64, Some(e.code.clone()));
            return Err(e);
        }
    };

    let dir = data_dir.join("images");
    std::fs::create_dir_all(&dir).map_err(|e| perr("IMAGE_DIR", e.to_string()))?;
    let stem = v
        .get("name")
        .and_then(Value::as_str)
        .and_then(safe_stem)
        .unwrap_or_else(|| format!("{}-{}", slug(&prompt, "image"), stamp()));
    let mut paths = Vec::new();
    for (i, blob) in out.images.iter().enumerate() {
        let name = if out.images.len() == 1 {
            stem.clone()
        } else {
            format!("{stem}-{}", i + 1)
        };
        let path: PathBuf = dir.join(format!("{name}.{}", blob.extension()));
        if let Err(e) = std::fs::write(&path, &blob.bytes) {
            return Err(perr("IMAGE_WRITE", e.to_string()));
        }
        paths.push(serde_json::json!({
            "path": path.display().to_string(),
            "mime": blob.mime,
            "bytes": blob.bytes.len(),
        }));
    }
    sweep_old_images(data_dir, cfg.image.as_ref().and_then(|i| i.keep_days));
    record_generation(
        data_dir,
        &cfg,
        &model,
        out.images.len(),
        true,
        cost,
        started.elapsed().as_millis() as i64,
        None,
    );
    Ok(serde_json::json!({
        "edited": out.edited,
        "count": paths.len(),
        "images": paths,
        // First image as a convenience for a caller that wants "the picture".
        "path": paths.first().and_then(|p| p.get("path")).cloned(),
        "mime": out.primary().map(|b| b.mime.clone()),
        "bytes": out.bytes(),
        "model": out.model,
        "provider": out.provider,
        "backend": out.backend,
        "seed": out.seed,
        "cost_usd": cost,
        "size": size,
        "revised_prompt": out.revised_prompt,
    })
    .to_string())
}

/// Book one generation in the ledger: what drew it, how many frames, what it
/// cost, how it ended. Best-effort - the picture already exists (and was already
/// billed), so bookkeeping must never turn success into failure.
#[allow(clippy::too_many_arguments)]
pub fn record_generation(
    data_dir: &Path,
    cfg: &Config,
    model: &Option<String>,
    images: usize,
    ok: bool,
    cost_usd: Option<f64>,
    duration_ms: i64,
    code: Option<String>,
) {
    use pantheon_storage::{ImageGeneration, Ledger};
    let run_id = crate::tools::current_call_context()
        .map(|c| c.run_id)
        .unwrap_or_else(|| "tool:calls".to_string());
    // `Ledger::open` wants the database file, not the install directory.
    let Ok(ledger) = Ledger::open(&data_dir.join("ledger.db")) else {
        return;
    };
    let spec = resolve_spec(
        cfg,
        &ImageRequest {
            prompt: "x".into(),
            size: None,
            model: None,
            provider: None,
            source_images: Vec::new(),
            n: 1,
            seed: None,
        },
    );
    let (provider, backend) = match &spec {
        Ok(s) => (s.provider.clone(), s.backend.as_str().to_string()),
        Err(_) => (String::new(), String::new()),
    };
    let _ = ledger.record_image_generation(&ImageGeneration {
        run_id,
        provider,
        backend,
        model: model.clone().unwrap_or_default(),
        images,
        ok,
        cost_usd,
        duration_ms,
        code,
    });
}

/// Turn one source reference into something a remote service can consume: a
/// local path is read, sniffed and inlined as a `data:` URI; http(s) and
/// existing data URIs pass through; anything else is refused.
#[cfg(test)]
fn inline_source(value: &str) -> Result<String, pantheon_api::error::PantheonError> {
    inline_source_with(value, INLINE_BUDGET_ENCODED)
}

/// Base64 bytes we are willing to put in one request body. fal's gateway
/// rejects bodies above ~4.5 MB (measured by the service's own docs and
/// reproduced elsewhere), so an inline source there gets a smaller budget than
/// a general-purpose endpoint.
const INLINE_BUDGET_ENCODED: usize = 6 * 1024 * 1024;
const INLINE_BUDGET_FAL: usize = 3 * 1024 * 1024;

fn inline_source_with(
    value: &str,
    encoded_budget: usize,
) -> Result<String, pantheon_api::error::PantheonError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(perr("IMAGE_BAD_SOURCE", "empty source image".into()));
    }
    let lower = v.to_ascii_lowercase();
    if lower.starts_with("data:image/") {
        if v.len() > encoded_budget {
            return Err(perr(
                "IMAGE_SOURCE_TOO_LARGE",
                format!("inline source is {} bytes of base64", v.len()),
            ));
        }
        return Ok(v.to_string());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Ok(v.to_string());
    }
    if lower.starts_with("file:") || v.contains("://") {
        return Err(perr(
            "IMAGE_BAD_SOURCE",
            format!("unsupported source reference: {v}"),
        ));
    }
    let path = Path::new(v);
    let bytes = std::fs::read(path).map_err(|e| perr("IMAGE_BAD_SOURCE", format!("{v}: {e}")))?;
    // base64 grows by 4/3: keep the encoded size inside the budget.
    let encoded = bytes.len() / 3 * 4 + 4;
    if encoded > encoded_budget {
        return Err(perr(
            "IMAGE_SOURCE_TOO_LARGE",
            format!(
                "{v} encodes to {encoded} bytes, over the {} budget for this service",
                encoded_budget
            ),
        ));
    }
    let mime = pantheon_providers::image::sniff_mime(&bytes).ok_or_else(|| {
        perr(
            "IMAGE_BAD_SOURCE",
            format!("{v} is not a png/jpeg/webp/gif image"),
        )
    })?;
    Ok(format!(
        "data:{mime};base64,{}",
        pantheon_api::message::b64encode(&bytes)
    ))
}

/// Drop generated images older than `keep_days` (0/absent = keep forever).
/// Best-effort: a sweep failure must never fail a generation that worked.
fn sweep_old_images(data_dir: &Path, keep_days: Option<u64>) {
    let Some(days) = keep_days.filter(|d| *d > 0) else {
        return;
    };
    let dir = data_dir.join("images");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(days * 24 * 60 * 60);
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let modified = meta
            .modified()
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        if modified < cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Register the `upscale` tool. A separate action, not a flag on generation:
/// enlarging an image is a different intent from making one, and the pass is a
/// creative redraw that can change fine detail.
pub fn register_upscale(reg: &mut ToolRegistry, data_dir: PathBuf) {
    let cfg = Config::load(&data_dir).ok();
    let can_fal = cfg.as_ref().is_some_and(|c| {
        c.image_providers
            .values()
            .any(|r| r.upscale_endpoint.is_some())
    });
    let can_sd = cfg
        .as_ref()
        .is_some_and(|c| c.image_providers.values().any(|r| r.backend == "a1111"));
    let mut description = String::from(
        "Enlarge an existing image (a high-resolution pass over a file this session produced or \
         the user pointed at). Returns the saved path, mime and pixel budget note.",
    );
    if !(can_fal || can_sd) {
        description.push_str(
            " No service is configured for this: add `upscale_endpoint` to a fal row or select \
             an a1111 service.",
        );
    }
    reg.register_with(
        pantheon_api::message::ToolSchema {
            name: UPSCALE_NAME.into(),
            description,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "image to enlarge: an absolute path, an http(s) URL, or a data URI"},
                    "scale": {"type": "number", "description": "factor between 1.5 and 4 (default 2)"},
                    "prompt": {"type": "string", "description": "optional guidance for the redraw"},
                    "name": {"type": "string", "description": "file stem for the saved image (optional)"}
                },
                "required": ["source"]
            }),
        },
        Capability::MediaGenerate,
        move |args| run_upscale(args, &data_dir),
        None,
    );
}

pub const UPSCALE_NAME: &str = "upscale";

fn run_upscale(args: &str, data_dir: &Path) -> Result<String, pantheon_api::error::PantheonError> {
    let v: Value = serde_json::from_str(args).map_err(|e| perr("IMAGE_ARGS", e.to_string()))?;
    let source = v
        .get("source")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| perr("IMAGE_NO_SOURCE", "`source` is required".into()))?
        .to_string();
    let scale = v.get("scale").and_then(Value::as_f64).unwrap_or(2.0);
    let prompt = v
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let cfg = Config::load(data_dir).map_err(|e| perr("IMAGE_CONFIG", e.to_string()))?;
    let started = std::time::Instant::now();
    let local_source = source.clone();
    let req = pantheon_providers::image::UpscaleRequest {
        source,
        scale,
        prompt,
        provider: None,
    };
    let out = match pantheon_providers::image::upscale_image(&cfg, &req) {
        Ok(out) => out,
        Err(e) => {
            record_upscale(data_dir, &cfg, false, started.elapsed().as_millis() as i64, Some(e.code.clone()));
            return Err(e);
        }
    };
    let dir = data_dir.join("images");
    std::fs::create_dir_all(&dir).map_err(|e| perr("IMAGE_DIR", e.to_string()))?;
    let stem = v
        .get("name")
        .and_then(Value::as_str)
        .and_then(safe_stem)
        .unwrap_or_else(|| format!("{}-upscaled-{}", slug(&local_source, "image"), stamp()));
    let blob = out
        .primary()
        .ok_or_else(|| perr("IMAGE_EMPTY", "the upscaler returned no image".into()))?;
    let path: PathBuf = dir.join(format!("{stem}.{}", blob.extension()));
    std::fs::write(&path, &blob.bytes).map_err(|e| perr("IMAGE_WRITE", e.to_string()))?;
    record_upscale(data_dir, &cfg, true, started.elapsed().as_millis() as i64, None);
    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "mime": blob.mime,
        "bytes": blob.bytes.len(),
        "model": out.model,
        "provider": out.provider,
        "backend": out.backend,
        "scale": scale,
    })
    .to_string())
}

fn record_upscale(
    data_dir: &Path,
    cfg: &Config,
    ok: bool,
    duration_ms: i64,
    code: Option<String>,
) {
    record_generation(data_dir, cfg, &None, usize::from(ok), ok, None, duration_ms, code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_prompt_is_refused_before_any_io() {
        let dir = std::env::temp_dir().join(format!("pantheon-image-tool-{}", std::process::id()));
        let e = run("{}", &dir).unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_PROMPT");
        let e = run(r#"{"prompt":"   "}"#, &dir).unwrap_err();
        assert_eq!(e.code, "IMAGE_NO_PROMPT");
        let e = run("not json", &dir).unwrap_err();
        assert_eq!(e.code, "IMAGE_ARGS");
    }

    #[test]
    fn slugs_stay_short_ascii_and_never_empty() {
        assert_eq!(slug("A Heron, at dusk!", "image"), "a-heron-at-dusk");
        assert_eq!(slug("!!!", "image"), "image");
        assert!(slug(&"word ".repeat(40), "image").len() <= 32);
    }

    #[test]
    fn a_caller_supplied_name_cannot_escape_the_images_dir() {
        assert_eq!(safe_stem("icon-32").as_deref(), Some("icon-32"));
        assert!(safe_stem("../../etc/passwd").is_none());
        assert!(safe_stem("a/b").is_none());
        assert!(safe_stem("").is_none());
        assert!(safe_stem(&"x".repeat(80)).is_none());
    }

    #[test]
    fn generates_through_a_stub_endpoint_and_saves_the_file() {
        use std::io::{Read, Write};
        let png = [0x89u8, b'P', b'N', b'G', 0x0d, 0x0a];
        let b64 = pantheon_api::message::b64encode(&png);
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let body = format!(r#"{{"data":[{{"b64_json":"{b64}"}}]}}"#);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).unwrap();
        });
        let dir = std::env::temp_dir().join(format!("pantheon-image-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!(
                "[model]\nprovider = \"stub\"\nmodel = \"m\"\n\n[custom_providers.stub]\nbase_url = \"http://127.0.0.1:{port}/v1\"\napi_mode = \"openai\"\n\n[[custom_providers.stub.models]]\nid = \"stub-image\"\n"
            ),
        )
        .unwrap();
        let out = run(
            r#"{"prompt":"a heron at dusk","size":"1024x1024","name":"heron"}"#,
            &dir,
        )
        .expect("stub generation");
        handle.join().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        let path = v["path"].as_str().unwrap();
        assert!(path.ends_with("images/heron.png"), "{path}");
        assert_eq!(std::fs::read(path).unwrap(), png);
        assert_eq!(v["mime"], "image/png");
        assert_eq!(v["bytes"], png.len());
        assert_eq!(v["model"], "stub-image", "the declared image model is picked");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Real generation against a live keyless endpoint. Ignored by default:
    /// run with `cargo test -p pantheon-tools --lib -- --ignored live_generation`
    /// (needs network). Proves the template backend end to end: settings ->
    /// HTTP -> bytes -> sniffed mime -> file on disk.
    #[test]
    #[ignore = "network: runs a real generation against image.pollinations.ai"]
    fn live_generation_through_a_template_backend() {
        let dir = std::env::temp_dir().join(format!("pantheon-image-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            "[model]\nprovider = \"llm-router\"\nmodel = \"chat\"\n\n[image]\nbackend = \"url_template\"\nendpoint = \"https://image.pollinations.ai/prompt/{prompt}?width={width}&height={height}&nologo=true&seed={seed}\"\ntimeout_secs = 90\n",
        )
        .unwrap();
        let out = run(
            r#"{"prompt":"a single red cube on a white background, studio product photo","size":"512x512"}"#,
            &dir,
        )
        .expect("live generation");
        let v: Value = serde_json::from_str(&out).unwrap();
        let path = v["path"].as_str().unwrap();
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() > 2000, "a real image, not an error page");
        assert_eq!(v["backend"], "url_template");
        assert!(
            v["mime"].as_str().unwrap_or("").starts_with("image/"),
            "sniffed mime: {}",
            v["mime"]
        );
        eprintln!(
            "live image: {} ({} bytes, {})",
            path,
            bytes.len(),
            v["mime"].as_str().unwrap_or("?")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_source_becomes_a_data_uri_and_paths_cannot_escape() {
        let dir = std::env::temp_dir().join(format!("pantheon-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("in.png");
        std::fs::write(&png, [0x89u8, b'P', b'N', b'G', 0x0d]).unwrap();
        let out = inline_source(png.to_str().unwrap()).unwrap();
        assert!(out.starts_with("data:image/png;base64,"), "{out}");
        // Scheme handling: pass-through, and refusals.
        assert!(inline_source("https://x.test/a.png").is_ok());
        assert!(inline_source("data:image/png;base64,AAA").is_ok());
        let e = inline_source("file:///etc/passwd").unwrap_err();
        assert_eq!(e.code, "IMAGE_BAD_SOURCE");
        let notimg = dir.join("notes.txt");
        std::fs::write(&notimg, b"hello").unwrap();
        let e = inline_source(notimg.to_str().unwrap()).unwrap_err();
        assert_eq!(e.code, "IMAGE_BAD_SOURCE");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_retention_sweep_removes_only_images_past_the_cutoff() {
        let dir = std::env::temp_dir().join(format!("pantheon-sweep-{}", std::process::id()));
        let images = dir.join("images");
        std::fs::create_dir_all(&images).unwrap();
        let fresh = images.join("fresh.png");
        let stale = images.join("stale.png");
        std::fs::write(&fresh, b"x").unwrap();
        std::fs::write(&stale, b"x").unwrap();
        // Age the stale file beyond the cutoff without touching the fresh one.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(40 * 24 * 60 * 60);
        let f = std::fs::File::options().write(true).open(&stale).unwrap();
        f.set_modified(old).unwrap();
        sweep_old_images(&dir, Some(30));
        assert!(fresh.exists(), "recent images survive");
        assert!(!stale.exists(), "images older than keep_days are swept");
        // 0 / absent means keep forever.
        std::fs::write(&stale, b"x").unwrap();
        let f = std::fs::File::options().write(true).open(&stale).unwrap();
        f.set_modified(old).unwrap();
        sweep_old_images(&dir, Some(0));
        sweep_old_images(&dir, None);
        assert!(stale.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_schema_only_advertises_what_the_service_can_do() {
        let dir = std::env::temp_dir().join(format!("pantheon-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A template service: no edit, no batch, but a seed.
        std::fs::write(
            dir.join("config.toml"),
            "[model]\nprovider = \"stub\"\nmodel = \"m\"\n\n[image]\nprovider = \"tpl\"\n\n[image_providers.tpl]\nbackend = \"url_template\"\nbase_url = \"https://img.test/{prompt}?w={width}\"\n",
        )
        .unwrap();
        let schema = schema_for(&dir);
        let props = &schema.parameters["properties"];
        assert!(props.get("prompt").is_some());
        assert!(props.get("source").is_none(), "no edit route -> no source knob");
        assert!(props.get("references").is_none());
        assert!(props.get("n").is_none(), "single-image wire -> no batch knob");
        assert!(props.get("seed").is_some(), "templates take a seed");
        assert!(schema.description.contains("cannot take a source image"));

        // A fal row with an edit route and an upscaler: the knobs appear.
        std::fs::write(
            dir.join("config.toml"),
            "[model]\nprovider = \"stub\"\nmodel = \"m\"\n\n[image]\nprovider = \"f\"\n\n[image_providers.f]\nbackend = \"fal\"\nbase_url = \"https://fal.run\"\nedit_endpoint = \"fal-ai/flux/schnell/edit\"\nupscale_endpoint = \"fal-ai/clarity-upscaler\"\n[[image_providers.f.models]]\nid = \"fal-ai/flux/schnell\"\n",
        )
        .unwrap();
        let schema = schema_for(&dir);
        let props = &schema.parameters["properties"];
        assert!(props.get("source").is_some(), "edit route -> source knob");
        assert_eq!(props["references"]["maxItems"], 1);
        assert!(props.get("n").is_some());
        assert!(props.get("seed").is_some());
        assert!(schema.description.contains("upscale"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_upscale_tool_is_registered_separately() {
        let mut reg = ToolRegistry::new();
        register_upscale(&mut reg, std::env::temp_dir());
        assert!(reg.get(UPSCALE_NAME).is_some());
        assert_eq!(
            reg.capability_of(UPSCALE_NAME),
            Some(Capability::MediaGenerate),
            "enlarging spends money like generating"
        );
    }

    #[test]
    fn the_tool_is_registered_under_the_media_capability() {
        let mut reg = ToolRegistry::new();
        register(&mut reg, std::env::temp_dir());
        assert!(reg.get(NAME).is_some());
        assert_eq!(
            reg.capability_of(NAME),
            Some(Capability::MediaGenerate),
            "an untrusted origin must see this tool as a generation request, not a read"
        );
    }
}
