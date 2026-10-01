//! Canonical conversation messages (OpenAI wire shape, per Hermes study).
//!
//! One message type for the whole runtime. Roles: system | user | assistant | tool.
//! Strict alternation is enforced by the loop, not this type. Tool results ride
//! `role: "tool"` rows with their tool_call_id, never a synthetic user message.

use crate::provenance::Provenance;
use serde::{Deserialize, Serialize};

/// One conversation message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Image parts riding alongside `content` (vision). Empty on every row
    /// written before vision existed (`#[serde(default)]`), and skipped on
    /// the wire when empty — so text-only rows serialize byte-identically
    /// to before, and every existing `.content` accessor keeps working.
    /// The base64 payload is stored on the ledger row itself so history
    /// replay re-sends the picture, not a stale disk path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePart>,
    /// Present on assistant rows that requested tools.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCallRef>,
    /// Present on tool rows; matches the assistant tool_call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Structured provenance. `None` on legacy rows (treated as the role's
    /// default tier); set on every row the harness builds from now on.
    /// Skipped on the wire for rows without it, so legacy providers and
    /// fixtures keep working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
    /// Unix millis when the message was created (server time). `None` on
    /// rows written before timestamps existed; omitted on the wire so
    /// legacy consumers never see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A tool request from the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRef {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments string (wire shape; parsed by the executor).
    pub arguments: String,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            provenance: None,
            ts_ms: Some(crate::logging::now_ms() as u64),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            provenance: None,
            ts_ms: Some(crate::logging::now_ms() as u64),
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            provenance: None,
            ts_ms: Some(crate::logging::now_ms() as u64),
        }
    }
    pub fn assistant_tool_calls(calls: Vec<ToolCallRef>) -> Self {
        Self {
            role: Role::Assistant,
            content: String::new(),
            images: vec![],
            tool_calls: calls,
            tool_call_id: None,
            provenance: None,
            ts_ms: Some(crate::logging::now_ms() as u64),
        }
    }
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: Some(tool_call_id.into()),
            provenance: None,
            ts_ms: Some(crate::logging::now_ms() as u64),
        }
    }

    /// Attach provenance to this message. Builder-style so call sites read
    /// as `Message::tool(id, out).with_provenance(...)`.
    pub fn with_provenance(mut self, provenance: Provenance) -> Self {
        self.provenance = Some(provenance);
        self
    }

    /// Recalled memory as its own row type. Memory is context, not
    /// instruction, so it never borrows System role: a System row with no
    /// provenance is authoritative by definition, which would let a record
    /// written from untrusted tool output speak with the harness's voice.
    /// A User row carrying Memory-tier provenance gets the provider's
    /// `[provenance: ...]` envelope and is treated as data.
    pub fn recall(content: impl Into<String>, source: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images: vec![],
            tool_calls: vec![],
            tool_call_id: None,
            provenance: Some(Provenance::memory(source)),
            ts_ms: Some(crate::logging::now_ms() as u64),
        }
    }

    /// Attach image parts to this message. Builder-style:
    /// `Message::user("see this").with_images(parts)`.
    pub fn with_images(mut self, images: Vec<ImagePart>) -> Self {
        self.images = images;
        self
    }

    /// Whether this message carries any image parts.
    pub fn has_images(&self) -> bool {
        !self.images.is_empty()
    }

    /// Text content plus one `[image: <name> (<mime>, <n> bytes)]` note per
    /// image part. For text-only renderers that cannot carry image parts
    /// (compression input, transcript export): the pictures are named, never
    /// silently dropped.
    pub fn text_with_image_notes(&self) -> String {
        if self.images.is_empty() {
            return self.content.clone();
        }
        let mut out = self.content.clone();
        for img in &self.images {
            out.push('\n');
            out.push_str(&img.note());
        }
        out
    }
}

/// Hard cap on one image's raw bytes: 10 MiB. Base64 inflates stored rows
/// by ~4/3 on top of this, and providers cap per-image payloads too — so
/// anything bigger is rejected at the attach site, not mid-request.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;
/// Mimes the wire encoders know how to label. Sniffed from magic bytes,
/// never trusted from the client.
pub const ALLOWED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Max images on one message: bounds the worst-case request
/// (10 images x 10 MiB x 4/3 base64). Enforced by the provider chain.
pub const MAX_IMAGES_PER_MESSAGE: usize = 10;

/// One image riding a message: base64 payload for the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
    /// Original filename (diagnostic; shown in transcripts).
    pub name: String,
    /// Mime type, sniffed from magic bytes at attach time.
    pub mime: String,
    /// Raw bytes, base64 (standard alphabet). Stored on the ledger row so
    /// history replay re-sends the picture, not a stale disk path.
    pub data: String,
}

/// Why an image could not be attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// Raw bytes exceeded [`MAX_IMAGE_BYTES`]; carries the actual size.
    TooLarge(usize),
    /// Magic bytes did not match any of [`ALLOWED_IMAGE_MIMES`].
    UnsupportedMime,
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::TooLarge(n) => write!(
                f,
                "image is {n} bytes, over the {} MiB limit",
                MAX_IMAGE_BYTES / 1024 / 1024
            ),
            ImageError::UnsupportedMime => write!(
                f,
                "not a recognized image (allowed: {})",
                ALLOWED_IMAGE_MIMES.join(", ")
            ),
        }
    }
}

impl ImagePart {
    /// Build from raw bytes: sniffs the mime from magic bytes, enforces
    /// [`MAX_IMAGE_BYTES`], and base64-encodes. The claimed mime is never
    /// trusted — a renamed `.exe` fails the sniff and is rejected loudly.
    pub fn from_bytes(name: &str, bytes: &[u8]) -> Result<Self, ImageError> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(ImageError::TooLarge(bytes.len()));
        }
        let mime = sniff_image_mime(bytes).ok_or(ImageError::UnsupportedMime)?;
        Ok(Self {
            name: name.to_string(),
            mime: mime.to_string(),
            data: b64encode(bytes),
        })
    }

    /// `data:` URL for OpenAI-style `image_url` parts.
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
    }

    /// One-line `[image: <name> (<mime>, <n> bytes)]` note for text-only
    /// renderers (compression input, transcript export, tool rows).
    pub fn note(&self) -> String {
        format!(
            "[image: {} ({}, {} bytes)]",
            self.name,
            self.mime,
            self.data.len() * 3 / 4
        )
    }
}

/// Mime from magic bytes. PNG / JPEG / GIF / WEBP only — anything else is
/// not an image Pantheon will put on the wire.
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Longest edge, in pixels, an image may have before [`downscale_image`]
/// shrinks it. Rationale: providers cap per-image payloads and charge per
/// image token by dimension, so an unbounded phone photo is a cost bug;
/// 1568 keeps plenty of detail for description/QA. Mirrors the Anthropic
/// 5 MB-per-image ceiling with headroom after JPEG re-encode.
pub const VISION_DOWNSCALE_MAX_EDGE: u32 = 1568;
/// JPEG quality for downscaled re-encodes: visually lossless for
/// description work, far smaller than a phone's q95+ original.
const DOWNSCALE_JPEG_QUALITY: u8 = 85;

/// Shrink `raw` image bytes so the longest edge fits
/// [`VISION_DOWNSCALE_MAX_EDGE`], returning the re-encoded bytes.
/// Returns `None` when no work is needed (already small enough) or the
/// bytes cannot be decoded with the available codecs (JPEG/PNG) — the
/// caller passes the original bytes through; magic sniffing in
/// [`ImagePart::from_bytes`] already validated the shape.
///
/// Re-encode is JPEG quality 85, except a PNG source with real
/// transparency (any pixel alpha < 255), which stays PNG so the alpha
/// channel survives. GIF/WebP sources are never decoded here (no codec
/// enabled) and pass through untouched.
pub fn downscale_image(raw: &[u8]) -> Option<Vec<u8>> {
    let fmt = image::guess_format(raw).ok()?;
    let img = image::load_from_memory(raw).ok()?;
    let (w, h) = (img.width(), img.height());
    let longest = w.max(h);
    if longest <= VISION_DOWNSCALE_MAX_EDGE || longest == 0 {
        return None;
    }
    let scale = VISION_DOWNSCALE_MAX_EDGE as f32 / longest as f32;
    let (nw, nh) = (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    );
    let small = img.resize(nw, nh, image::imageops::FilterType::Triangle);
    let keep_png = fmt == image::ImageFormat::Png && small.to_rgba8().pixels().any(|p| p[3] < 255);
    let mut out = Vec::new();
    if keep_png {
        small
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .ok()?;
    } else {
        let rgb = small.to_rgb8();
        let mut enc =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, DOWNSCALE_JPEG_QUALITY);
        enc.encode_image(&rgb).ok()?;
    }
    Some(out)
}

/// Minimal base64 encoder (standard alphabet, padded). Mirrors the decoder
/// in `pantheon-dashboard`'s uploads; std-only, no new dep.
pub fn b64encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Parse a `[attachments]` block (as emitted by the dashboard's
/// `send_message`) for `image/*` entries and build `ImagePart`s from the
/// referenced files, so images attached to a chat turn reach the provider
/// as picture parts instead of dying as file-path text.
///
/// Each entry has the shape `- <name> (<mime>, <size>, id: <upl_...>): <abs path>`.
/// Only entries whose mime starts with `image/` are considered. Every path
/// is canonicalized and must stay inside `uploads_dir` — the block is text
/// the model can influence, so a `../` escape or an absolute path to
/// anywhere else is skipped, never read. Files that fail to read or sniff
/// are skipped too: the block text still names them, so the model sees the
/// filename and can try its file tools — nothing is silently hidden.
/// Capped at [`MAX_IMAGES_PER_MESSAGE`].
pub fn attachment_images(text: &str, uploads_dir: &std::path::Path) -> Vec<ImagePart> {
    let uploads_dir = match uploads_dir.canonicalize() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut in_block = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if !in_block {
            if line.trim() == "[attachments]" {
                in_block = true;
            }
            continue;
        }
        let line = line.trim_end();
        if !line.starts_with("- ") {
            // First non-entry line ends the block (the trailer copy).
            break;
        }
        if out.len() >= MAX_IMAGES_PER_MESSAGE {
            break;
        }
        // Split `- name (mime, size, id: xxx): /abs/path` on the last "): ".
        let Some(sep) = line.rfind("): ") else {
            continue;
        };
        let head = &line[2..sep];
        let path = line[sep + 3..].trim();
        if !head.contains("(image/") {
            continue;
        }
        let name = head.split(" (").next().unwrap_or("image").trim();
        let name = if name.is_empty() { "image" } else { name };
        // Never trust the path: it must resolve inside the uploads dir.
        let Ok(canon) = std::path::Path::new(path).canonicalize() else {
            continue;
        };
        if !canon.starts_with(&uploads_dir) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&canon) else {
            continue;
        };
        // One shared downscale for every path that sends pixels: a 12 MP
        // phone photo becomes a ~1568px JPEG before base64, never raw.
        let bytes = downscale_image(&bytes).unwrap_or(bytes);
        if let Ok(part) = ImagePart::from_bytes(name, &bytes) {
            out.push(part);
        }
    }
    out
}

/// Cap on videos parsed from one `[attachments]` block: videos are
/// expensive (frames × describe calls), so the block beyond two is left
/// for the model to open with its file tools.
pub const MAX_VIDEOS_PER_MESSAGE: usize = 2;

/// One video referenced by an `[attachments]` block: validated path only,
/// never bytes — a 25 MiB upload must not be read fully into memory.
/// Frame extraction (ffmpeg) streams what it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoAttachment {
    /// Original filename (diagnostic; shown in transcripts).
    pub name: String,
    /// Canonicalized absolute path, verified inside `uploads_dir`.
    pub path: std::path::PathBuf,
}

/// Parse the dashboard `[attachments]` block (appended by `send_message`)
/// for `video/*` entries and return their validated paths, so attached
/// videos reach the video-analysis pass instead of dying as file-path
/// text.
///
/// Entry shape and trust rules mirror [`attachment_images`]: `- <name>
/// (<mime>, <size>, id: <upl_...>): <abs path>`, canonicalized, must stay
/// inside `uploads_dir`, capped at [`MAX_VIDEOS_PER_MESSAGE`]. Bytes are
/// never read here — extraction happens downstream via ffmpeg.
pub fn attachment_videos(text: &str, uploads_dir: &std::path::Path) -> Vec<VideoAttachment> {
    let uploads_dir = match uploads_dir.canonicalize() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    let mut in_block = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if !in_block {
            if line.trim() == "[attachments]" {
                in_block = true;
            }
            continue;
        }
        let line = line.trim_end();
        if !line.starts_with("- ") {
            break;
        }
        if out.len() >= MAX_VIDEOS_PER_MESSAGE {
            break;
        }
        let Some(sep) = line.rfind("): ") else {
            continue;
        };
        let head = &line[2..sep];
        let path = line[sep + 3..].trim();
        if !head.contains("(video/") {
            continue;
        }
        let name = head.split(" (").next().unwrap_or("video").trim();
        let name = if name.is_empty() { "video" } else { name };
        let Ok(canon) = std::path::Path::new(path).canonicalize() else {
            continue;
        };
        if !canon.starts_with(&uploads_dir) {
            continue;
        }
        out.push(VideoAttachment {
            name: name.to_string(),
            path: canon,
        });
    }
    out
}

/// Tool schema in OpenAI wire format: {"type":"function","function":{...}}.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema for the parameters object.
    pub parameters: serde_json::Value,
}

impl ToolSchema {
    pub fn to_wire(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }
}
