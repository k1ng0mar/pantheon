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
            description: "Generate an image from a text prompt and save it to disk. Returns the \
                          absolute path, mime type and byte size; the file can then be read, \
                          attached or copied like any other image. Use `size` for an aspect ratio \
                          the provider understands (e.g. 1024x1024, 1536x1024)."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "what to draw, in words"},
                    "size": {"type": "string", "description": "e.g. 1024x1024 or 1536x1024"},
                    "model": {"type": "string", "description": "image model id; defaults to the provider's declared image model"},
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
    let cfg = Config::load(data_dir).map_err(|e| perr("IMAGE_CONFIG", e.to_string()))?;
    let out = generate_image(
        &cfg,
        &ImageRequest {
            prompt: prompt.clone(),
            size: size.clone(),
            model,
            provider: None,
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
    Ok(serde_json::json!({
        "path": path.display().to_string(),
        "mime": out.mime,
        "bytes": out.bytes.len(),
        "model": out.model,
        "provider": out.provider,
        "size": size,
        "revised_prompt": out.revised_prompt,
    })
    .to_string())
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
