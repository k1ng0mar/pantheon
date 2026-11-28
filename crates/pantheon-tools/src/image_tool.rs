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
use pantheon_providers::image::{ImageRequest, generate_image};
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
pub fn register(reg: &mut ToolRegistry, data_dir: PathBuf) {
    reg.register_with(
        pantheon_api::message::ToolSchema {
            name: NAME.into(),
            description: "Generate an image from a text prompt, or edit an existing image, and save \
                          the result to disk. Returns the absolute path, mime type, byte size and \
                          which backend drew it; the file can then be read, attached or copied like \
                          any other image. With `source` (and optional `references`) it becomes an \
                          edit when the configured service supports image input, and an error \
                          naming the alternatives when it does not."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "what to draw, or what change to make when editing"},
                    "size": {"type": "string", "description": "aspect (`landscape`, `square`, `portrait`) or pixels (`1024x1024`)"},
                    "model": {"type": "string", "description": "image model id; defaults to the provider's declared image model"},
                    "source": {"type": "string", "description": "image to edit: an absolute path, an http(s) URL, or a data URI"},
                    "references": {"type": "array", "items": {"type": "string"}, "description": "extra style/composition images (clamped to what the service takes)"},
                    "name": {"type": "string", "description": "file stem for the saved image (optional)"}
                },
                "required": ["prompt"]
            }),
        },
        Capability::MediaGenerate,
        move |args| run(args, &data_dir),
        None,
    );
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
    // Source images: a local path becomes a data URI (a remote service cannot
    // read our disk), an http(s)/data value passes through after a scheme
    // check, and anything else is refused so a prompt cannot smuggle a
    // `file://` read into a request body.
    let mut source_images = Vec::new();
    if let Some(src) = v.get("source").and_then(Value::as_str) {
        source_images.push(inline_source(src)?);
    }
    if let Some(refs) = v.get("references").and_then(Value::as_array) {
        for r in refs.iter().filter_map(Value::as_str) {
            source_images.push(inline_source(r)?);
        }
    }
    let cfg = Config::load(data_dir).map_err(|e| perr("IMAGE_CONFIG", e.to_string()))?;
    let out = generate_image(
        &cfg,
        &ImageRequest {
            prompt: prompt.clone(),
            size: size.clone(),
            model,
            provider: None,
            source_images,
        },
    )?;

    let dir = data_dir.join("images");
    std::fs::create_dir_all(&dir).map_err(|e| perr("IMAGE_DIR", e.to_string()))?;
    let stem = v
        .get("name")
        .and_then(Value::as_str)
        .and_then(safe_stem)
        .unwrap_or_else(|| format!("{}-{}", slug(&prompt, "image"), stamp()));
    let path: PathBuf = dir.join(format!("{stem}.{}", out.extension()));
    if let Err(e) = std::fs::write(&path, &out.bytes) {
        return Err(perr("IMAGE_WRITE", e.to_string()));
    }
    sweep_old_images(data_dir, cfg.image.as_ref().and_then(|i| i.keep_days));
    Ok(serde_json::json!({
        "edited": out.edited,
        "path": path.display().to_string(),
        "mime": out.mime,
        "bytes": out.bytes.len(),
        "model": out.model,
        "provider": out.provider,
        "backend": out.backend,
        "size": size,
        "revised_prompt": out.revised_prompt,
    })
    .to_string())
}

/// Turn one source reference into something a remote service can consume: a
/// local path is read, sniffed and inlined as a `data:` URI; http(s) and
/// existing data URIs pass through; anything else is refused.
fn inline_source(value: &str) -> Result<String, pantheon_api::error::PantheonError> {
    let v = value.trim();
    if v.is_empty() {
        return Err(perr("IMAGE_BAD_SOURCE", "empty source image".into()));
    }
    let lower = v.to_ascii_lowercase();
    if lower.starts_with("data:image/") {
        // Cap inline payloads: a huge base64 blob is rejected by every service
        // and inflates the request body. ~6 MB of encoded data.
        if v.len() > 6 * 1024 * 1024 {
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
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(perr(
            "IMAGE_SOURCE_TOO_LARGE",
            format!("{v} is {} bytes (4 MiB cap for an inline source)", bytes.len()),
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
