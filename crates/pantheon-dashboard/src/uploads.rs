//! File uploads backing chat message attachments (the mobile app's
//! "paperclip" button).
//!
//! `POST /api/uploads` stores a base64 file under
//! `<data_dir>/uploads/<id>_<sanitized-name>` plus a tiny `<id>.json`
//! sidecar (`{name, mime, size_bytes}` - the mime is re-derived from the
//! sidecar, never from the filename extension). `GET /api/uploads/:id`
//! serves the raw bytes back with the stored mime as the content type,
//! for preview/verify in the client.
//!
//! `POST /api/runs/:id/message` then takes an optional
//! `"attachments": ["upl_...", ...]` array; each id is resolved here and
//! its absolute path is appended to the `--say` text so the agent can
//! read it with its file tools. Image attachments are passed as file
//! paths only; they are not interpreted as pictures.

use crate::{bad_json, body_json, created_json, err_json, App};
use pantheon_api::message::ImagePart;
use pantheon_gateway::http::{Request, Response};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// Hard cap on decoded upload size: 25 MiB.
pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

/// Cap on the number of entries inflated from an attached zip.
pub const MAX_ZIP_ENTRIES: usize = 1000;
/// Cap on total inflated bytes from an attached zip (zip-bomb guard,
/// enforced on actual bytes read, never on header claims): 100 MiB.
pub const MAX_ZIP_INFLATED_BYTES: u64 = 100 * 1024 * 1024;
/// Max extracted entries listed inline in the message text; beyond this
/// the listing is summarized so one giant archive can't flood context.
pub const MAX_ZIP_LISTED: usize = 50;

static ID_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Resolved upload: the metadata from the `<id>.json` sidecar plus the
/// absolute path of the stored file.
#[derive(Debug, Clone)]
pub struct UploadInfo {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub path: PathBuf,
}

fn uploads_dir(app: &App) -> PathBuf {
    app.data_dir.join("uploads")
}

/// `upl_<epochms>_<nnnn>`, mirroring `pantheon_runtime::new_scoped_id`
/// (which is private to that crate).
fn new_upload_id() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let sequence = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let n = (sequence as u32).wrapping_add(std::process::id()) % 10_000;
    format!("upl_{ms}_{n:04}")
}

/// Minimal base64 decoder (standard + URL-safe alphabets, padding
/// optional). Copied from `pantheon_providers::voice::b64decode` to avoid
/// a new dependency for this one use.
fn b64decode(input: &str) -> Result<Vec<u8>, String> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' | b'-' => Some(62),
            b'/' | b'_' => Some(63),
            _ => None,
        }
    }
    let clean: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .collect();
    if clean.len() % 4 == 1 {
        return Err("invalid base64 length".to_string());
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut n: u32 = 0;
        for (i, &c) in chunk.iter().enumerate() {
            n |= (val(c).ok_or_else(|| format!("invalid base64 char {c:?}"))? as u32)
                << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Sanitize a client-supplied filename for storage. Path traversal is
/// rejected outright (`/`, `\`, `..`); anything outside
/// `[A-Za-z0-9._-]` collapses to `_`. Returns `None` for empty names,
/// names that sanitize to nothing, or dotfiles.
fn sanitize_name(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return None;
    }
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() || safe.starts_with('.') {
        return None;
    }
    Some(safe)
}

/// Cheap pre-decode size check: the decoded length is at most
/// `clean_len * 3 / 4`, so reject oversized payloads without allocating
/// the decode buffer.
fn decoded_len_within_cap(data: &str) -> bool {
    let clean_len = data
        .bytes()
        .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
        .count();
    clean_len * 3 / 4 <= MAX_UPLOAD_BYTES
}

/// Upload ids have a fixed shape (`upl_<digits>_<digits>`); anything
/// else is not an upload and can never address a file on disk.
fn valid_id_shape(id: &str) -> bool {
    let rest = match id.strip_prefix("upl_") {
        Some(r) => r,
        None => return false,
    };
    let mut parts = rest.split('_');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(b), None)
            if !a.is_empty()
                && !b.is_empty()
                && a.bytes().all(|c| c.is_ascii_digit())
                && b.bytes().all(|c| c.is_ascii_digit()) =>
        {
            true
        }
        _ => false,
    }
}

/// Mime values go out as an HTTP content-type header, so they must be
/// plain token-ish values - no CR/LF smuggling, and a `/` is required.
fn valid_mime(mime: &str) -> bool {
    !mime.is_empty()
        && mime.contains('/')
        && mime
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-*/; =".contains(&b))
}

/// `POST /api/uploads`: `{"name": ..., "mime": ..., "data": "<base64>"}`.
/// 201 `{"id", "name", "mime", "size_bytes"}`.
pub fn create(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let name = match body.get("name").and_then(|v| v.as_str()) {
        Some(n) if !n.trim().is_empty() => n.trim(),
        _ => return bad_json("field \"name\" is required"),
    };
    let safe_name = match sanitize_name(name) {
        Some(n) => n,
        None => return err_json(400, "UPLOAD_BAD_NAME", "file name is not acceptable"),
    };
    let mime = body
        .get("mime")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("application/octet-stream");
    if !valid_mime(mime) {
        return err_json(400, "UPLOAD_BAD_MIME", "mime value is not acceptable");
    }
    let data = match body.get("data").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => return bad_json("field \"data\" is required"),
    };
    if !decoded_len_within_cap(data) {
        return err_json(
            400,
            "UPLOAD_TOO_LARGE",
            &format!(
                "upload exceeds the {} MiB limit",
                MAX_UPLOAD_BYTES / 1024 / 1024
            ),
        );
    }
    let bytes = match b64decode(data) {
        Ok(b) => b,
        Err(e) => return err_json(400, "UPLOAD_BAD_DATA", &format!("invalid base64: {e}")),
    };
    if bytes.len() > MAX_UPLOAD_BYTES {
        return err_json(
            400,
            "UPLOAD_TOO_LARGE",
            &format!(
                "upload exceeds the {} MiB limit",
                MAX_UPLOAD_BYTES / 1024 / 1024
            ),
        );
    }
    let dir = uploads_dir(app);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return err_json(500, "UPLOAD", &format!("create uploads dir: {e}"));
    }
    let id = new_upload_id();
    let file_name = format!("{id}_{safe_name}");
    if let Err(e) = std::fs::write(dir.join(&file_name), &bytes) {
        return err_json(500, "UPLOAD", &format!("write file: {e}"));
    }
    let sidecar = serde_json::json!({
        "name": safe_name,
        "mime": mime,
        "size_bytes": bytes.len(),
    });
    if let Err(e) = std::fs::write(
        dir.join(format!("{id}.json")),
        serde_json::to_string(&sidecar).unwrap_or_default(),
    ) {
        let _ = std::fs::remove_file(dir.join(&file_name));
        return err_json(500, "UPLOAD", &format!("write sidecar: {e}"));
    }
    created_json(serde_json::json!({
        "id": id,
        "name": safe_name,
        "mime": mime,
        "size_bytes": bytes.len(),
    }))
}

/// Resolve an upload id to its metadata + absolute path. Returns `None`
/// for malformed ids, missing sidecars, or missing data files.
pub fn resolve(app: &App, id: &str) -> Option<UploadInfo> {
    resolve_in(&uploads_dir(app), id)
}

/// Same as [`resolve`], but against an explicit uploads directory instead
/// of an `App` - for callers (like the turn child) that have a data dir
/// but no dashboard `App`.
pub fn resolve_in(dir: &Path, id: &str) -> Option<UploadInfo> {
    if !valid_id_shape(id) {
        return None;
    }
    let sidecar_bytes = std::fs::read(dir.join(format!("{id}.json"))).ok()?;
    let sidecar: serde_json::Value = serde_json::from_slice(&sidecar_bytes).ok()?;
    let name = sidecar.get("name")?.as_str()?.to_string();
    let mime = sidecar.get("mime")?.as_str()?.to_string();
    let size_bytes = sidecar.get("size_bytes")?.as_u64()?;
    let prefix = format!("{id}_");
    let data_file = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name())
        .filter_map(|n| n.into_string().ok())
        .find(|n| n.starts_with(&prefix))?;
    let path = std::fs::canonicalize(dir.join(&data_file)).ok()?;
    Some(UploadInfo {
        id: id.to_string(),
        name,
        mime,
        size_bytes,
        path,
    })
}

/// True when `bytes` begins with a zip signature (local file header,
/// empty archive, or spanned archive). Zip detection sniffs magic bytes
/// the stored mime is client-supplied and never trusted.
pub fn is_zip_bytes(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && (bytes.starts_with(b"PK\x03\x04")
            || bytes.starts_with(b"PK\x05\x06")
            || bytes.starts_with(b"PK\x07\x08"))
}

/// True when the stored upload file looks like a zip archive (first four
/// bytes only - the whole file is never read for detection).
pub fn is_zip_upload(info: &UploadInfo) -> bool {
    use std::io::Read;
    let mut head = [0u8; 4];
    match std::fs::File::open(&info.path).and_then(|mut f| f.read(&mut head)) {
        Ok(4) => is_zip_bytes(&head),
        _ => false,
    }
}

/// Directory an attached zip is inflated into:
/// `<uploads_dir>/<id>_unzipped/`.
pub fn zip_extract_dir(app: &App, id: &str) -> PathBuf {
    uploads_dir(app).join(format!("{id}_unzipped"))
}

/// One inflated zip entry: path relative to the extraction root.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExtractedEntry {
    pub rel_path: PathBuf,
    pub size_bytes: u64,
}

/// Inflate an attached zip upload into [`zip_extract_dir`], returning the
/// extracted entries. Idempotent: a completed extraction is recorded in
/// `.extracted.json` and reused on repeat attaches.
///
/// Fail-closed on zip-slip paths (absolute or containing `..`), more
/// than [`MAX_ZIP_ENTRIES`] entries, and total inflated bytes past
/// [`MAX_ZIP_INFLATED_BYTES`] (enforced on actual bytes read, never on
/// header claims). Directory and symlink entries are skipped, and every
/// entry is written as a regular file, so a symlink entry can never
/// materialize as a link on disk.
pub fn extract_zip_upload(app: &App, info: &UploadInfo) -> Result<Vec<ExtractedEntry>, String> {
    let dest_root = zip_extract_dir(app, &info.id);
    let marker = dest_root.join(".extracted.json");
    if marker.is_file() {
        if let Ok(entries) = serde_json::from_str::<Vec<ExtractedEntry>>(
            &std::fs::read_to_string(&marker).unwrap_or_default(),
        ) {
            return Ok(entries);
        }
        // Corrupt marker: fall through and re-extract.
        let _ = std::fs::remove_dir_all(&dest_root);
    }
    let bytes =
        std::fs::read(&info.path).map_err(|e| format!("read upload \"{}\": {e}", info.name))?;
    if !is_zip_bytes(&bytes) {
        return Err(format!("\"{}\" is not a zip archive", info.name));
    }
    let mut zip =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| format!("open zip: {e}"))?;
    if zip.len() > MAX_ZIP_ENTRIES {
        return Err(format!(
            "zip has {} entries (max {MAX_ZIP_ENTRIES})",
            zip.len()
        ));
    }
    // Extract into a temp dir first, then rename into place, so a failed
    // or interrupted extraction never leaves a half-written tree behind.
    let tmp_root = uploads_dir(app).join(format!("{}_unzipped.tmp", info.id));
    let _ = std::fs::remove_dir_all(&tmp_root);
    std::fs::create_dir_all(&tmp_root).map_err(|e| format!("create extract dir: {e}"))?;
    let mut total: u64 = 0;
    let mut entries: Vec<ExtractedEntry> = Vec::new();
    let result: Result<(), String> = (|| {
        use std::io::Read as _;
        for i in 0..zip.len() {
            let mut f = zip.by_index(i).map_err(|e| format!("entry {i}: {e}"))?;
            let name = f.name().to_string();
            if f.is_dir() || f.is_symlink() {
                continue;
            }
            let rel = PathBuf::from(&name);
            if rel.is_absolute()
                || rel
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(format!("unsafe entry path: {name}"));
            }
            let remaining = MAX_ZIP_INFLATED_BYTES.saturating_sub(total);
            let mut buf = Vec::new();
            let n = std::io::Read::take(&mut f, remaining + 1)
                .read_to_end(&mut buf)
                .map_err(|e| format!("read {name}: {e}"))?;
            total = total.saturating_add(n as u64);
            if total > MAX_ZIP_INFLATED_BYTES {
                return Err(format!(
                    "zip inflates past {} MiB cap",
                    MAX_ZIP_INFLATED_BYTES / (1024 * 1024)
                ));
            }
            let dest = tmp_root.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
            }
            std::fs::write(&dest, &buf).map_err(|e| format!("write {name}: {e}"))?;
            entries.push(ExtractedEntry {
                rel_path: rel,
                size_bytes: buf.len() as u64,
            });
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp_root);
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&dest_root);
    std::fs::rename(&tmp_root, &dest_root).map_err(|e| format!("commit extraction: {e}"))?;
    let listing = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(&marker, listing).map_err(|e| format!("write marker: {e}"))?;
    Ok(entries)
}

/// Turn a resolved upload into a vision [`ImagePart`]: reads the stored
/// bytes, sniffs the mime from magic bytes (the stored mime is
/// client-supplied and never trusted), enforces the size cap, and
/// base64-encodes. This is the bridge from "file on disk" to "image part
/// on the outgoing provider request". Loud `Err` on anything that is not
/// actually a sendable image.
pub fn image_part(info: &UploadInfo) -> Result<ImagePart, String> {
    let bytes = std::fs::read(&info.path)
        .map_err(|e| format!("cannot read stored upload \"{}\": {e}", info.name))?;
    ImagePart::from_bytes(&info.name, &bytes).map_err(|e| format!("\"{}\": {e}", info.name))
}

/// `GET /api/uploads/:id`: raw bytes with the stored mime as the
/// content type. 404 `UPLOAD_NOT_FOUND` for unknown ids.
pub fn download(app: &App, id: &str) -> Response {
    let info = match resolve(app, id) {
        Some(i) => i,
        None => {
            return err_json(
                404,
                "UPLOAD_NOT_FOUND",
                &format!("unknown upload id \"{id}\""),
            )
        }
    };
    let bytes = match std::fs::read(&info.path) {
        Ok(b) => b,
        Err(_) => {
            return err_json(
                404,
                "UPLOAD_NOT_FOUND",
                &format!("unknown upload id \"{id}\""),
            )
        }
    };
    // `Response` needs a `&'static str` content type; the mime was
    // validated at upload time and re-checked here, so leaking the tiny
    // string is safe and header-injection-proof. Served as an attachment
    // (never inline): a stored HTML/SVG must not execute in the
    // dashboard's origin.
    let content_type: &'static str = if valid_mime(&info.mime) {
        Box::leak(info.mime.into_boxed_str())
    } else {
        "application/octet-stream"
    };
    let filename = sanitize_name(&info.name).unwrap_or_else(|| "download".to_string());
    Response::download(&filename, content_type, bytes)
}

/// Human-readable byte size: `B`, one-decimal `KB`, one-decimal `MB`.
pub fn human_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < MB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{:.1} MB", b / MB)
    }
}
