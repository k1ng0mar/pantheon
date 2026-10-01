//! Plugin import pipeline: source validation, tarball fetch, format
//! detection, static security scan, and quarantine staging.
//!
//! Sources: `https://github.com/<owner>/<repo>` (optional `#<branch|tag>`
//! fragment or `ref` field) and `clawhub:<slug>`.
//!
//! Honest design notes — read before extending:
//!
//! - The static scanner below is **heuristic pattern matching**, not a
//!   sandbox and not a guarantee. It catches low-effort malicious
//!   patterns (miners, `curl|sh`, credential grabs) and flags suspicious
//!   ones for a human to review. Defense in depth, in order: (1) the
//!   quarantine dir is never on any plugin load path, (2) the scan gates
//!   approval, (3) the existing approval store still gates loading.
//!   A clever attacker can evade static heuristics; treat `clean` as
//!   "nothing obviously bad found", not "safe".
//! - ClawHub has a real packages API (verified live 2026-09-30):
//!   `GET /api/v1/packages/{name}` returns `{"package": {family,
//!   latestVersion, ...}}` and
//!   `GET /api/v1/packages/{name}/download?version={v}` returns the
//!   package zip. So `clawhub:<slug>` (either `<name>` or
//!   `<owner>/<name>`) resolves via the packages API and downloads the
//!   zip through the same quarantine → scan → format-detect pipeline as
//!   GitHub tarballs. A package whose `family` is `skill` still ends in
//!   422 NOT_A_PLUGIN pointing at the skills importer, and a skill slug
//!   that 404s on the packages endpoint is checked against the skills
//!   endpoint before giving up, so the hint survives.
//! - GitHub tarballs come from
//!   `codeload.github.com/<owner>/<repo>/tar.gz/<ref>`. A repo's default
//!   branch is unknowable without the GitHub API, so `ref` defaults to
//!   `main` and we retry `master` once on 404. Pass an explicit `ref`
//!   (or `#fragment`) for anything else.
//! - codeload.github.com (Fastly) closes the TLS session without
//!   `close_notify`; rustls reports that as a transport error even when
//!   every byte already arrived. Body reads go through the shared
//!   `pantheon_exec::http` helper, which accepts a body the framing says
//!   is whole (declared length fully received, or close-delimited with
//!   bytes received) and keeps failing hard on genuine truncation;
//!   unclean-shutdown errors are rewritten in plain English.
//! - Tar entries are parsed by hand (no `tar` crate) with flate2 for
//!   gzip; entries with `..`, absolute paths, or symlinks/hardlinks are
//!   skipped and counted, never materialized. Zip entries (ClawHub
//!   packages) are parsed by hand the same way — central directory,
//!   stored/deflated via flate2 — with the same safety rules.
//! - Imported plugins land UNAPPROVED in
//!   `<data_dir>/plugins/.quarantine/<name>/` with `.scan-report.json`
//!   and `.import-meta.json` beside them. Approval (via the existing
//!   `plugins::approve` gate, extended with the scan verdict) promotes
//!   the tree into the live dir and only then records approval.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Cap on downloaded bundle bytes (task spec).
pub const MAX_DOWNLOAD_BYTES: u64 = 25 * 1024 * 1024;
/// Cap on decompressed tar bytes (zip-bomb guard).
pub const MAX_DECOMPRESSED_BYTES: u64 = 100 * 1024 * 1024;
/// Files bigger than this are skipped by the scanner.
pub const MAX_SCANNED_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Network timeout for bundle fetches (task spec: 30s).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// ClawHub public API base.
pub const CLAWHUB_API: &str = "https://clawhub.ai/api/v1";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Pipeline failure. The dashboard maps these to HTTP responses; the
/// `code()` values are the stable `error.code` strings clients match on.
#[derive(Debug)]
pub enum ImportError {
    BadRequest(String),
    NotFound(String),
    Conflict(String),
    TooLarge(String),
    FetchFailed(String),
    UnsupportedLayout {
        message: String,
        found: Vec<String>,
    },
    NotAPlugin {
        message: String,
        skill: serde_json::Value,
        hint: String,
    },
    Io(String),
}

impl ImportError {
    pub fn code(&self) -> &'static str {
        match self {
            ImportError::BadRequest(_) => "BAD_REQUEST",
            ImportError::NotFound(_) => "NOT_FOUND",
            ImportError::Conflict(_) => "PLUGIN_IMPORT_EXISTS",
            ImportError::TooLarge(_) => "PLUGIN_IMPORT_TOO_LARGE",
            ImportError::FetchFailed(_) => "PLUGIN_IMPORT_FETCH",
            ImportError::UnsupportedLayout { .. } => "UNSUPPORTED_LAYOUT",
            ImportError::NotAPlugin { .. } => "NOT_A_PLUGIN",
            ImportError::Io(_) => "PLUGIN_IMPORT_IO",
        }
    }

    pub fn message(&self) -> String {
        match self {
            ImportError::BadRequest(m)
            | ImportError::NotFound(m)
            | ImportError::Conflict(m)
            | ImportError::TooLarge(m)
            | ImportError::FetchFailed(m)
            | ImportError::Io(m) => m.clone(),
            ImportError::UnsupportedLayout { message, .. } => message.clone(),
            ImportError::NotAPlugin { message, .. } => message.clone(),
        }
    }
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.message())
    }
}

/// Injectable HTTP GET. Production passes [`fetch_url`]; tests pass a stub
/// so no test ever touches the network.
#[derive(Debug)]
pub enum FetchError {
    Status(u16, String),
    Transport(String),
    TooLarge,
}

pub type FetchFn = dyn Fn(&str) -> Result<Vec<u8>, FetchError>;

// ---------------------------------------------------------------------------
// Source specs
// ---------------------------------------------------------------------------

/// Where the bundle comes from.
#[derive(Debug, Clone)]
pub enum SourceSpec {
    Github {
        owner: String,
        repo: String,
        gitref: String,
    },
    ClawHub {
        slug: String,
    },
}

/// Parse the `url` field of the import request: either
/// `https://github.com/<owner>/<repo>[#<ref>]` or `clawhub:<slug>`.
/// `ref_field` is the optional `ref` JSON field and wins over the URL fragment.
pub fn parse_source_spec(raw: &str, ref_field: Option<&str>) -> Result<SourceSpec, ImportError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(ImportError::BadRequest("url is empty".into()));
    }
    if let Some(slug) = raw.strip_prefix("clawhub:") {
        return parse_clawhub_slug(slug).map(|slug| SourceSpec::ClawHub { slug });
    }
    parse_github_url(raw, ref_field)
}

/// Parse a `clawhub:` slug: `<name>` or `<owner>/<name>`. Each segment
/// must be path-safe (ascii letters/digits plus `.-_`, no leading dot or
/// dash, not `.`/`..`); the packages API is addressed by the final
/// segment.
fn parse_clawhub_slug(slug: &str) -> Result<String, ImportError> {
    let bad = || {
        ImportError::BadRequest(
            "bad clawhub slug (want clawhub:<name> or clawhub:<owner>/<name>, ascii letters/digits/.-_)".into(),
        )
    };
    let s = slug.trim();
    if s.is_empty() || s.len() > 128 {
        return Err(bad());
    }
    let mut parts = s.split('/');
    let mut count = 0;
    for part in &mut parts {
        count += 1;
        if count > 2
            || part.is_empty()
            || part == "."
            || part == ".."
            || part.starts_with('.')
            || part.starts_with('-')
            || !part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(bad());
        }
    }
    if count == 0 {
        return Err(bad());
    }
    Ok(s.to_string())
}

/// URL whitelist: exactly `https://github.com/<owner>/<repo>`. Everything
/// else — private IPs, loopback, other hosts, credentials, ports, deep
/// paths — is rejected with 400.
fn parse_github_url(raw: &str, ref_field: Option<&str>) -> Result<SourceSpec, ImportError> {
    let bad = |m: &str| ImportError::BadRequest(m.to_string());
    let u = url::Url::parse(raw).map_err(|_| bad("url is not a valid URL"))?;
    if u.scheme() != "https" {
        return Err(bad("only https GitHub URLs are accepted"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(bad("URL must not embed credentials"));
    }
    let host = u.host_str().unwrap_or("");
    // Explicit SSRF-flavored rejections with a clear message. (The exact
    // host match below already excludes these; this is the clearer error.)
    if host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok() {
        return Err(bad(
            "private IPs, loopback, and non-GitHub hosts are rejected",
        ));
    }
    if !host.eq_ignore_ascii_case("github.com") {
        return Err(bad("only github.com URLs are accepted"));
    }
    if u.port().is_some() {
        return Err(bad("URL must not specify a port"));
    }
    // `path_segments` percent-decodes; the strict charset check below then
    // also defeats %2e%2e-style tricks.
    let segs: Vec<String> = u
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default();
    if segs.len() != 2 {
        return Err(bad("URL must be https://github.com/<owner>/<repo>"));
    }
    let owner = segs[0].clone();
    let mut repo = segs[1].clone();
    if let Some(stripped) = repo.strip_suffix(".git") {
        repo = stripped.to_string();
    }
    for part in [&owner, &repo] {
        if part.is_empty()
            || part.len() > 100
            || part.contains("..")
            || !part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(bad("invalid owner/repo in URL"));
        }
    }
    let gitref = ref_field
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| u.fragment().map(str::trim).filter(|s| !s.is_empty()))
        .unwrap_or("main")
        .to_string();
    validate_gitref(&gitref)?;
    Ok(SourceSpec::Github {
        owner,
        repo,
        gitref,
    })
}

fn validate_gitref(r: &str) -> Result<(), ImportError> {
    let ok = !r.is_empty()
        && r.len() <= 128
        && !r.contains("..")
        && !r.starts_with('/')
        && !r.starts_with('.')
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'));
    if ok {
        Ok(())
    } else {
        Err(ImportError::BadRequest(
            "bad ref (ascii letters/digits/._-/, no leading dot or slash, no ..)".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

/// Plain HTTPS GET with a timeout and a byte cap, via the shared
/// [`pantheon_exec::http::get`] helper: ureq first, one curl-CLI retry
/// on transport-level failure only. HTTP error statuses and oversize
/// bodies never trigger the fallback.
///
/// Proxy env is honored for *transport*, the same posture as curl and
/// git. That does not weaken the source whitelist: the URL is validated
/// against the whitelist before any fetch happens, and TLS still
/// verifies the origin server's certificate through the proxy tunnel —
/// a proxy cannot make a non-whitelisted host present a valid cert for
/// a whitelisted name.
///
/// TLS teardown tolerance lives in the shared [`pantheon_exec::http`]
/// helper: some servers (codeload.github.com via Fastly, the Claude
/// marketplace) close the TLS session without `close_notify`, which
/// rustls surfaces as a transport error even when the framing says the
/// body is already whole. Genuine truncation still errors, and the
/// failure message is plain English (no rustls doc link).
pub fn fetch_url(url: &str, timeout: Duration, max_bytes: u64) -> Result<Vec<u8>, FetchError> {
    match pantheon_exec::http::get(url, timeout, max_bytes, "pantheon-plugin-import") {
        Ok(fetched) => Ok(fetched.body),
        Err(pantheon_exec::http::GetError::Status(code)) => {
            Err(FetchError::Status(code, String::new()))
        }
        Err(pantheon_exec::http::GetError::Transport(msg)) => Err(FetchError::Transport(msg)),
        Err(pantheon_exec::http::GetError::TooLarge) => Err(FetchError::TooLarge),
    }
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Tarball extraction (hand-rolled tar over flate2 gzip)
// ---------------------------------------------------------------------------

pub struct ExtractStats {
    pub files: usize,
    pub skipped_unsafe: usize,
}

fn read_octal(field: &[u8]) -> Option<u64> {
    let s = std::str::from_utf8(field).ok()?;
    let s = s.trim_matches(|c| c == '\0' || c == ' ');
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s.trim(), 8).ok()
}

/// ustar name: 100-byte name field plus the 155-byte prefix field.
fn tar_entry_name(hdr: &[u8]) -> String {
    let raw = &hdr[0..100];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let mut name = String::from_utf8_lossy(&raw[..end]).into_owned();
    let praw = &hdr[345..500];
    let pend = praw.iter().position(|&b| b == 0).unwrap_or(praw.len());
    if pend > 0 {
        let prefix = String::from_utf8_lossy(&praw[..pend]);
        name = format!("{prefix}/{name}");
    }
    name
}

/// Map a tar entry name to a staging-relative path. Rejects absolute
/// paths and any `..`/`.`/empty components; strips the tarball's single
/// top-level directory (the GitHub codeload `<repo>-<ref>/` layout).
fn safe_stage_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() {
        return None;
    }
    let p = Path::new(name);
    if p.is_absolute() {
        return None;
    }
    let mut rel = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::Normal(c) => rel.push(c),
            _ => return None,
        }
    }
    let mut it = rel.components();
    it.next()?; // strip top-level dir
    let stripped: PathBuf = it.collect();
    if stripped.as_os_str().is_empty() {
        return None;
    }
    Some(stripped)
}

pub fn extract_tar_gz(data: &[u8], dest: &Path) -> Result<ExtractStats, ImportError> {
    if data.len() < 2 || data[0] != 0x1f || data[1] != 0x8b {
        return Err(ImportError::BadRequest(
            "downloaded bytes are not gzip (magic mismatch)".into(),
        ));
    }
    let dec = flate2::read::GzDecoder::new(data);
    let mut tar = Vec::new();
    dec.take(MAX_DECOMPRESSED_BYTES + 1)
        .read_to_end(&mut tar)
        .map_err(|e| ImportError::BadRequest(format!("gzip decode failed: {e}")))?;
    if tar.len() as u64 > MAX_DECOMPRESSED_BYTES {
        return Err(ImportError::TooLarge(format!(
            "decompressed tarball exceeds {} MiB",
            MAX_DECOMPRESSED_BYTES / 1024 / 1024
        )));
    }
    extract_tar(&tar, dest)
}

fn extract_tar(tar: &[u8], dest: &Path) -> Result<ExtractStats, ImportError> {
    let mut stats = ExtractStats {
        files: 0,
        skipped_unsafe: 0,
    };
    let mut off = 0usize;
    let mut long_name: Option<String> = None;
    while off + 512 <= tar.len() {
        let hdr = &tar[off..off + 512];
        if hdr.iter().all(|&b| b == 0) {
            break; // end-of-archive marker
        }
        let size = read_octal(&hdr[124..136])
            .ok_or_else(|| ImportError::BadRequest("tar: bad size field".into()))?
            as usize;
        let typeflag = hdr[156];
        off += 512;
        let data_end = off
            .checked_add(size)
            .ok_or_else(|| ImportError::BadRequest("tar: size overflow".into()))?;
        if data_end > tar.len() {
            return Err(ImportError::BadRequest("tar: truncated entry".into()));
        }
        if typeflag == b'L' {
            // GNU long-name entry: its payload names the *next* entry.
            let raw = &tar[off..data_end];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            long_name = String::from_utf8(raw[..end].to_vec()).ok();
            off = data_end.next_multiple_of(512);
            continue;
        }
        let name = long_name.take().unwrap_or_else(|| tar_entry_name(hdr));
        let rel = safe_stage_path(&name);
        match typeflag {
            b'0' | 0 => {
                match rel {
                    Some(rel) => {
                        let out = dest.join(&rel);
                        // Belt and braces: the component filter above makes
                        // escape impossible, but never trust a tarball.
                        if !out.starts_with(dest) {
                            stats.skipped_unsafe += 1;
                        } else {
                            if let Some(parent) = out.parent() {
                                std::fs::create_dir_all(parent)
                                    .map_err(|e| ImportError::Io(format!("staging: {e}")))?;
                            }
                            std::fs::write(&out, &tar[off..data_end])
                                .map_err(|e| ImportError::Io(format!("staging: {e}")))?;
                            stats.files += 1;
                        }
                    }
                    None => stats.skipped_unsafe += 1,
                }
            }
            b'5' => match rel {
                Some(rel) => {
                    let out = dest.join(&rel);
                    if out.starts_with(dest) {
                        std::fs::create_dir_all(&out)
                            .map_err(|e| ImportError::Io(format!("staging: {e}")))?;
                    } else {
                        stats.skipped_unsafe += 1;
                    }
                }
                None => stats.skipped_unsafe += 1,
            },
            // Symlinks (b'2'), hardlinks (b'1'), and anything exotic are
            // never materialized — a symlink pointing at /etc/passwd must
            // not survive extraction. Counted, not an error.
            _ => stats.skipped_unsafe += 1,
        }
        off = data_end.next_multiple_of(512);
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Zip extraction (hand-rolled, for ClawHub package zips)
// ---------------------------------------------------------------------------
//
// Parses the End of Central Directory + Central Directory (robust against
// data-descriptor local headers), supports stored (method 0) and deflated
// (method 8, via flate2) entries, and applies the same safety rules as
// the tar path: no absolute paths, no `..`/`.` components, symlinks and
// anything exotic are skipped and counted, never materialized.
//
// Unlike codeload tarballs, ClawHub zips have no guaranteed top-level
// directory, so the single top-level dir is stripped only when every
// entry lives under one common first component.

struct ZipEntry {
    name: String,
    method: u16,
    comp_size: u64,
    uncomp_size: u64,
    local_offset: u64,
    is_symlink: bool,
    encrypted: bool,
}

fn u16le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn u32le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Locate the End of Central Directory record by scanning backwards over
/// the last 64 KiB + 22 bytes (the EOCD comment can be up to 64 KiB).
fn find_eocd(data: &[u8]) -> Option<usize> {
    if data.len() < 22 {
        return None;
    }
    let floor = data.len().saturating_sub(65_557 + 22);
    let mut i = data.len() - 22;
    loop {
        if data[i..].starts_with(b"PK\x05\x06") {
            return Some(i);
        }
        if i == floor {
            return None;
        }
        i -= 1;
    }
}

fn parse_central_dir(data: &[u8]) -> Result<Vec<ZipEntry>, ImportError> {
    let bad = |m: &str| ImportError::BadRequest(format!("zip: {m}"));
    let eocd = find_eocd(data).ok_or_else(|| bad("no end-of-central-directory record"))?;
    let e = &data[eocd..];
    if e.len() < 22 {
        return Err(bad("truncated end-of-central-directory"));
    }
    let count = u16le(&e[10..12]);
    let cd_size = u32le(&e[12..16]);
    let cd_off = u32le(&e[16..20]);
    if count == 0xffff || cd_size == 0xffff_ffff || cd_off == 0xffff_ffff {
        return Err(bad("zip64 is not supported"));
    }
    let count = count as usize;
    let cd_off = cd_off as usize;
    let cd_end = cd_off
        .checked_add(cd_size as usize)
        .ok_or_else(|| bad("central directory size overflow"))?;
    if cd_end > data.len() || cd_off > data.len() {
        return Err(bad("central directory outside the file"));
    }
    let mut entries = Vec::with_capacity(count.min(100_000));
    let mut off = cd_off;
    for _ in 0..count {
        if off + 46 > data.len() || !data[off..].starts_with(b"PK\x01\x02") {
            return Err(bad("bad central directory entry"));
        }
        let h = &data[off..];
        let made_by = u16le(&h[4..6]);
        let method = u16le(&h[10..12]);
        let flags = u16le(&h[8..10]);
        let comp_size = u32le(&h[20..24]) as u64;
        let uncomp_size = u32le(&h[24..28]) as u64;
        let name_len = u16le(&h[28..30]) as usize;
        let extra_len = u16le(&h[30..32]) as usize;
        let comment_len = u16le(&h[32..34]) as usize;
        let ext_attrs = u32le(&h[38..42]);
        let local_off = u32le(&h[42..46]) as u64;
        let name_start = off + 46;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or_else(|| bad("entry name overflow"))?;
        let entry_end = name_end
            .checked_add(extra_len)
            .and_then(|x| x.checked_add(comment_len))
            .ok_or_else(|| bad("entry size overflow"))?;
        if entry_end > data.len() {
            return Err(bad("entry runs past the central directory"));
        }
        let name = String::from_utf8_lossy(&data[name_start..name_end]).into_owned();
        // Unix symlink: made-by OS 3 and S_IFLNK in the mode bits.
        let is_symlink = (made_by >> 8) == 3 && ((ext_attrs >> 16) & 0o170_000) == 0o120_000;
        let encrypted = flags & 0x1 != 0;
        entries.push(ZipEntry {
            name,
            method,
            comp_size,
            uncomp_size,
            local_offset: local_off,
            is_symlink,
            encrypted,
        });
        off = entry_end;
    }
    Ok(entries)
}

/// Map a zip entry name to a staging-relative path. Same component rules
/// as the tar path; top-level stripping (when decided) is applied by the
/// caller on the already-safe path.
fn zip_stage_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('/') {
        return None;
    }
    let mut rel = PathBuf::new();
    for comp in Path::new(name).components() {
        match comp {
            Component::Normal(c) => rel.push(c),
            _ => return None,
        }
    }
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(rel)
}

pub fn extract_zip(data: &[u8], dest: &Path) -> Result<ExtractStats, ImportError> {
    let bad = |m: &str| ImportError::BadRequest(format!("zip: {m}"));
    if data.len() < 4 || !data.starts_with(b"PK\x03\x04") && find_eocd(data).is_none() {
        return Err(bad("not a zip file (magic mismatch)"));
    }
    let entries = parse_central_dir(data)?;
    let mut stats = ExtractStats {
        files: 0,
        skipped_unsafe: 0,
    };
    // Phase 1: keep only entries with safe relative paths.
    let mut safe: Vec<(&ZipEntry, PathBuf)> = Vec::new();
    for e in &entries {
        match zip_stage_path(&e.name) {
            Some(rel) => safe.push((e, rel)),
            None => stats.skipped_unsafe += 1,
        }
    }
    // Phase 2: strip the single top-level dir, but only when every
    // surviving *file* entry lives under one common first component
    // (the codeload-tarball shape). Unsafe entries never influence this.
    let strip_top = {
        let files: Vec<&PathBuf> = safe
            .iter()
            .filter(|(e, _)| !e.name.ends_with('/'))
            .map(|(_, rel)| rel)
            .collect();
        !files.is_empty() && files.iter().all(|rel| rel.components().count() >= 2) && {
            let first: PathBuf = files[0].components().take(1).collect();
            files
                .iter()
                .all(|rel| rel.components().take(1).collect::<PathBuf>() == first)
        }
    };
    let mut total_uncomp: u64 = 0;
    for (e, rel) in &safe {
        let rel = if strip_top {
            // Re-strip: safe paths were built without stripping.
            let mut it = rel.components();
            it.next();
            let stripped: PathBuf = it.collect();
            if stripped.as_os_str().is_empty() {
                continue;
            }
            stripped
        } else {
            rel.clone()
        };
        let is_dir = e.name.ends_with('/');
        let out = dest.join(&rel);
        // Belt and braces, same as the tar path.
        if !out.starts_with(dest) {
            stats.skipped_unsafe += 1;
            continue;
        }
        if is_dir {
            std::fs::create_dir_all(&out).map_err(|e| ImportError::Io(e.to_string()))?;
            continue;
        }
        if e.is_symlink || e.encrypted || !matches!(e.method, 0 | 8) {
            stats.skipped_unsafe += 1;
            continue;
        }
        total_uncomp = total_uncomp
            .checked_add(e.uncomp_size)
            .ok_or_else(|| bad("uncompressed size overflow"))?;
        if total_uncomp > MAX_DECOMPRESSED_BYTES {
            return Err(ImportError::TooLarge(format!(
                "decompressed zip exceeds {} MiB",
                MAX_DECOMPRESSED_BYTES / 1024 / 1024
            )));
        }
        // Payload bounds come from the central directory (authoritative
        // even when the local header uses a data descriptor).
        let lo = e.local_offset as usize;
        if lo + 30 > data.len() || !data[lo..].starts_with(b"PK\x03\x04") {
            return Err(bad("bad local file header"));
        }
        let fn_len = u16le(&data[lo + 26..lo + 28]) as usize;
        let ex_len = u16le(&data[lo + 28..lo + 30]) as usize;
        let data_off = lo
            .checked_add(30)
            .and_then(|x| x.checked_add(fn_len))
            .and_then(|x| x.checked_add(ex_len))
            .ok_or_else(|| bad("local header size overflow"))?;
        let data_end = (e.comp_size as usize)
            .checked_add(data_off)
            .ok_or_else(|| bad("entry data overflow"))?;
        if data_end > data.len() {
            return Err(bad("entry data runs past the file"));
        }
        let raw = &data[data_off..data_end];
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ImportError::Io(e.to_string()))?;
        }
        match e.method {
            0 => {
                if raw.len() as u64 != e.uncomp_size {
                    return Err(bad("stored entry size mismatch"));
                }
                std::fs::write(&out, raw).map_err(|e| ImportError::Io(e.to_string()))?;
            }
            8 => {
                let dec = flate2::read::DeflateDecoder::new(raw);
                let mut inflated = Vec::new();
                dec.take(MAX_DECOMPRESSED_BYTES + 1)
                    .read_to_end(&mut inflated)
                    .map_err(|e| ImportError::BadRequest(format!("zip: deflate failed: {e}")))?;
                if inflated.len() as u64 > MAX_DECOMPRESSED_BYTES {
                    return Err(ImportError::TooLarge(format!(
                        "decompressed zip exceeds {} MiB",
                        MAX_DECOMPRESSED_BYTES / 1024 / 1024
                    )));
                }
                std::fs::write(&out, &inflated).map_err(|e| ImportError::Io(e.to_string()))?;
            }
            _ => unreachable!("method filtered above"),
        }
        stats.files += 1;
    }
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Format detection
// ---------------------------------------------------------------------------

/// What the staged tree turned out to be.
#[derive(Debug, Clone)]
pub struct DetectedPlugin {
    pub kind: &'static str, // "tool" | "hook"
    pub name: String,
    pub version: String,
    pub description: String,
    /// Human-useful capability strings for the import report.
    pub capabilities: Vec<String>,
    /// Env vars declared by a native tool manifest (for the scanner's
    /// manifest-mismatch rule).
    pub declared_env: Vec<String>,
    /// Capability names declared by a native tool manifest.
    pub capability_names: Vec<String>,
    pub notes: Vec<String>,
}

/// Plugin dir names must be path-safe: they become a directory under
/// `plugins/` or `extensions/`, and a URL path segment.
fn check_plugin_name(name: &str) -> Result<(), ImportError> {
    let bad = name.is_empty()
        || name.len() > 64
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..");
    if bad {
        Err(ImportError::BadRequest(format!(
            "manifest name {name:?} is not a safe plugin name"
        )))
    } else {
        Ok(())
    }
}

/// Detect the plugin layout at the staged root:
/// - `manifest.yaml` → Pantheon native tool plugin
/// - `plugin.yaml` → Pantheon native hook plugin (also accepts the Hermes
///   shapes the in-tree loader accepts, since Pantheon's format derives
///   from them)
/// - `openclaw.plugin.json` → OpenClaw plugin, converted
/// - `.claude-plugin/plugin.json` → Claude Code plugin, converted
/// - anything else → 422 listing what was actually found.
pub fn detect_format(root: &Path) -> Result<DetectedPlugin, ImportError> {
    let manifest_path = root.join("manifest.yaml");
    if manifest_path.exists() {
        let text =
            std::fs::read_to_string(&manifest_path).map_err(|e| ImportError::Io(e.to_string()))?;
        let m: pantheon_exec::plugins::PluginManifest = serde_yaml::from_str(&text)
            .map_err(|e| ImportError::BadRequest(format!("manifest.yaml: {e}")))?;
        check_plugin_name(&m.name)?;
        let capabilities = m
            .capabilities
            .iter()
            .map(|c| format!("tool:{}", c.name))
            .chain(m.env_vars.iter().map(|v| format!("env:{}", v.name)))
            .collect::<Vec<_>>();
        return Ok(DetectedPlugin {
            kind: "tool",
            name: m.name.clone(),
            version: m.version.clone(),
            description: m.description.clone(),
            capabilities,
            declared_env: m.env_vars.iter().map(|v| v.name.clone()).collect(),
            capability_names: m.capabilities.iter().map(|c| c.name.clone()).collect(),
            notes: Vec::new(),
        });
    }
    let plugin_yaml = root.join("plugin.yaml");
    if plugin_yaml.exists() {
        let m = pantheon_extensions::manifest::PluginManifest::load(&plugin_yaml)
            .map_err(|e| ImportError::BadRequest(format!("plugin.yaml: {e}")))?;
        check_plugin_name(&m.name)?;
        let (known, unknown) = m.hook_list();
        let mut capabilities: Vec<String> = known
            .iter()
            .map(|h| {
                // Hook serializes snake_case (serde rename_all); fall back to
                // Debug if serialization ever fails.
                let name = serde_json::to_value(h)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_else(|| format!("{h:?}"));
                format!("hook:{name}")
            })
            .collect();
        let mut notes = Vec::new();
        if !unknown.is_empty() {
            notes.push(format!(
                "unknown hook names (kept, non-fatal): {}",
                unknown.join(", ")
            ));
            capabilities.extend(unknown.iter().map(|h| format!("hook?:{h}")));
        }
        return Ok(DetectedPlugin {
            kind: "hook",
            name: m.name.clone(),
            version: m.version.clone(),
            description: m.description.clone(),
            capabilities,
            declared_env: Vec::new(),
            capability_names: Vec::new(),
            notes,
        });
    }
    if root.join("openclaw.plugin.json").exists() {
        return convert_openclaw_plugin(root);
    }
    if root.join(".claude-plugin").join("plugin.json").exists() {
        return convert_claude_plugin(root);
    }
    let mut found = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten().take(20) {
            found.push(e.file_name().to_string_lossy().into_owned());
        }
    }
    found.sort();
    Err(ImportError::UnsupportedLayout {
        message: "unsupported plugin layout: expected manifest.yaml (tool), plugin.yaml (hook), openclaw.plugin.json (OpenClaw), or .claude-plugin/plugin.json (Claude Code) at the repo root".into(),
        found,
    })
}

/// Claude Code event → Pantheon hook mapping. Events with no Pantheon
/// analog are kept verbatim in the manifest's non-validated `hooks` list.
const CLAUDE_EVENT_MAP: &[(&str, &str)] = &[
    ("SessionStart", "on_session_start"),
    ("SessionEnd", "on_session_end"),
    ("PreToolUse", "pre_tool_call"),
    ("PostToolUse", "post_tool_call"),
    ("UserPromptSubmit", "pre_llm_call"),
    ("PreCompact", "on_compaction"),
    ("Stop", "subagent_stop"),
    ("SubagentStop", "subagent_stop"),
];

/// Convert a Claude Code plugin (`.claude-plugin/plugin.json`) into a
/// Pantheon hook plugin.
///
/// Field mapping:
/// - `name` → plugin.yaml `name` (sanitized to lowercase `[a-z0-9-_]`, ≤64 chars; required)
/// - `description` → `description`
/// - `version` → `version` (default `"0.0.0"`)
/// - `author` (string or `{name}`) → `author`
/// - `license` → `license`
/// - `hooks` (map of Claude event → `[{matcher, hooks: [{type, command}]}]`)
///   → Pantheon `provides_hooks` for mapped events; unmapped event names
///   are kept verbatim in the non-validated `hooks` list. Hook *bodies*
///   (shell commands) are preserved verbatim in `claude-hooks.json` —
///   Pantheon's runner executes `__init__.py`, not shell, so they need
///   manual porting before the plugin can do anything.
/// - `commands` / `agents` / `skills` (dirs) and `mcpServers` are NOT
///   converted; they surface in `detected_capabilities` and the notes.
/// - The original `plugin.json` is preserved as `imported-plugin.json`.
fn convert_claude_plugin(root: &Path) -> Result<DetectedPlugin, ImportError> {
    let raw = std::fs::read(root.join(".claude-plugin").join("plugin.json"))
        .map_err(|e| ImportError::Io(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|e| ImportError::BadRequest(format!("plugin.json: {e}")))?;

    let name_raw = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
    let name = sanitize_plugin_name(name_raw)?;
    let description = manifest_str(&v, "description", "");
    let version = manifest_str(&v, "version", "0.0.0");
    let author = manifest_author(&v);
    let license = manifest_str(&v, "license", "");

    let mut provides_hooks: Vec<String> = Vec::new();
    let mut raw_hooks: Vec<String> = Vec::new();
    let mut capabilities: Vec<String> = Vec::new();
    if let Some(hooks) = v.get("hooks").and_then(|h| h.as_object()) {
        let mut events: Vec<&String> = hooks.keys().collect();
        events.sort();
        for event in events {
            capabilities.push(format!("claude-hook:{event}"));
            match CLAUDE_EVENT_MAP.iter().find(|(k, _)| k == event) {
                Some((_, pantheon)) => {
                    let p = pantheon.to_string();
                    if !provides_hooks.contains(&p) {
                        provides_hooks.push(p);
                    }
                }
                None => raw_hooks.push(event.clone()),
            }
        }
    }
    for (field, label) in [
        ("commands", "claude-commands"),
        ("agents", "claude-agents"),
        ("skills", "claude-skills"),
    ] {
        match v.get(field) {
            Some(serde_json::Value::Array(a)) => capabilities.push(format!("{label}:{}", a.len())),
            Some(_) => capabilities.push(label.to_string()),
            None => {}
        }
    }
    if let Some(mcp) = v.get("mcpServers").and_then(|x| x.as_object()) {
        capabilities.push(format!("claude-mcp-servers:{}", mcp.len()));
    }

    write_converted_manifest(
        root,
        &name,
        &version,
        &description,
        &author,
        &license,
        &provides_hooks,
        &raw_hooks,
        &raw,
    )?;
    if let Some(hooks) = v.get("hooks") {
        let _ = std::fs::write(
            root.join("claude-hooks.json"),
            serde_json::to_string_pretty(hooks).unwrap_or_default(),
        );
    }

    let mut notes = vec![
        "Converted from a Claude Code plugin. Hook bodies are preserved in claude-hooks.json / imported-plugin.json; Pantheon hook plugins run __init__.py via the Python runner, so port the shell commands manually before approving."
            .to_string(),
    ];
    if name_raw != name {
        notes.push(format!(
            "plugin name sanitized from {name_raw:?} to {name:?}"
        ));
    }
    if capabilities
        .iter()
        .any(|c| c.starts_with("claude-mcp-servers"))
    {
        notes.push(
            "mcpServers are not converted; configure them as Pantheon MCP servers instead."
                .to_string(),
        );
    }

    Ok(DetectedPlugin {
        kind: "hook",
        name,
        version,
        description,
        capabilities,
        declared_env: Vec::new(),
        capability_names: Vec::new(),
        notes,
    })
}

/// Shared string-field extraction for the foreign-manifest converters.
fn manifest_str(v: &serde_json::Value, key: &str, default: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or(default)
        .to_string()
}

fn manifest_author(v: &serde_json::Value) -> String {
    match v.get("author") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(a) => a
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        None => String::new(),
    }
}

/// Write the Pantheon `plugin.yaml` for a converted foreign manifest and
/// preserve the original as `imported-plugin.json` for manual porting.
fn write_converted_manifest(
    root: &Path,
    name: &str,
    version: &str,
    description: &str,
    author: &str,
    license: &str,
    provides_hooks: &[String],
    raw_hooks: &[String],
    original_raw: &[u8],
) -> Result<(), ImportError> {
    let yaml = serde_yaml::to_string(&serde_json::json!({
        "name": name,
        "version": version,
        "description": description,
        "author": author,
        "license": license,
        "provides_hooks": provides_hooks,
        "hooks": raw_hooks,
    }))
    .map_err(|e| ImportError::Io(e.to_string()))?;
    std::fs::write(root.join("plugin.yaml"), yaml).map_err(|e| ImportError::Io(e.to_string()))?;
    let _ = std::fs::write(root.join("imported-plugin.json"), original_raw);
    Ok(())
}

/// Convert an OpenClaw plugin (`openclaw.plugin.json`, `{id, name,
/// description, version, ...}`) into a Pantheon hook plugin.
///
/// Field mapping mirrors the Claude Code converter:
/// - `name` (fallback: `id`) → plugin.yaml `name` (sanitized to
///   lowercase `[a-z0-9-_]`, ≤64 chars; required)
/// - `description` → `description`
/// - `version` → `version` (default `"0.0.0"`)
/// - `author` (string or `{name}`) → `author`
/// - `license` → `license`
/// - `commands` / `agents` / `skills` (arrays) → `openclaw-*`
///   capability counters
/// - `hooks` (object of event → handler, or array) → event names kept
///   verbatim in the non-validated `hooks` list (no Pantheon event
///   mapping exists for OpenClaw events) and surfaced as
///   `openclaw-hook:<event>` capabilities
/// - `mcpServers` → capability counter; not converted (note points at
///   Pantheon MCP servers instead)
/// - The original manifest is preserved as `imported-plugin.json`.
fn convert_openclaw_plugin(root: &Path) -> Result<DetectedPlugin, ImportError> {
    let raw = std::fs::read(root.join("openclaw.plugin.json"))
        .map_err(|e| ImportError::Io(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_slice(&raw)
        .map_err(|e| ImportError::BadRequest(format!("openclaw.plugin.json: {e}")))?;

    let name_raw = v
        .get("name")
        .and_then(|x| x.as_str())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| v.get("id").and_then(|x| x.as_str()))
        .unwrap_or("");
    let name = sanitize_plugin_name(name_raw)?;
    let description = manifest_str(&v, "description", "");
    let version = manifest_str(&v, "version", "0.0.0");
    let author = manifest_author(&v);
    let license = manifest_str(&v, "license", "");

    let mut capabilities: Vec<String> = Vec::new();
    let mut raw_hooks: Vec<String> = Vec::new();
    for (field, label) in [
        ("commands", "openclaw-commands"),
        ("agents", "openclaw-agents"),
        ("skills", "openclaw-skills"),
    ] {
        match v.get(field) {
            Some(serde_json::Value::Array(a)) => capabilities.push(format!("{label}:{}", a.len())),
            Some(_) => capabilities.push(label.to_string()),
            None => {}
        }
    }
    match v.get("hooks") {
        Some(serde_json::Value::Object(map)) => {
            let mut events: Vec<&String> = map.keys().collect();
            events.sort();
            for event in events {
                capabilities.push(format!("openclaw-hook:{event}"));
                raw_hooks.push(event.clone());
            }
        }
        Some(serde_json::Value::Array(a)) => {
            capabilities.push(format!("openclaw-hooks:{}", a.len()));
        }
        Some(_) => capabilities.push("openclaw-hooks".to_string()),
        None => {}
    }
    if let Some(mcp) = v.get("mcpServers").and_then(|x| x.as_object()) {
        capabilities.push(format!("openclaw-mcp-servers:{}", mcp.len()));
    }

    write_converted_manifest(
        root,
        &name,
        &version,
        &description,
        &author,
        &license,
        &[],
        &raw_hooks,
        &raw,
    )?;

    let mut notes = vec![
        "Converted from an OpenClaw plugin (openclaw.plugin.json). Command/agent/skill bodies are preserved in imported-plugin.json; Pantheon hook plugins run __init__.py via the Python runner, so port them manually before approving."
            .to_string(),
    ];
    if name_raw != name {
        notes.push(format!(
            "plugin name sanitized from {name_raw:?} to {name:?}"
        ));
    }
    if capabilities
        .iter()
        .any(|c| c.starts_with("openclaw-mcp-servers"))
    {
        notes.push(
            "mcpServers are not converted; configure them as Pantheon MCP servers instead."
                .to_string(),
        );
    }

    Ok(DetectedPlugin {
        kind: "hook",
        name,
        version,
        description,
        capabilities,
        declared_env: Vec::new(),
        capability_names: Vec::new(),
        notes,
    })
}

fn sanitize_plugin_name(raw: &str) -> Result<String, ImportError> {
    let clean: String = raw
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean.is_empty() || clean.len() > 64 {
        Err(ImportError::BadRequest(
            "plugin.json 'name' is missing or unusable as a plugin name".into(),
        ))
    } else {
        Ok(clean)
    }
}

// ---------------------------------------------------------------------------
// Static security scanner
// ---------------------------------------------------------------------------
//
// Heuristic static analysis over the staged files — NOT a sandbox, NOT a
// guarantee. It catches low-effort malicious patterns and flags
// suspicious ones for human review. A clean verdict means "nothing
// obviously bad found", not "safe". Clever attackers evade static
// heuristics; the quarantine dir (never on a load path) and the
// approval gate are the real enforcement.

/// One scanner hit.
#[derive(Debug, Clone)]
pub struct ScanFinding {
    pub severity: &'static str, // "critical" | "high" | "medium" | "low"
    pub file: String,           // staged-relative path
    pub line: Option<u64>,      // 1-based, None for aggregate rules
    pub rule: &'static str,
    pub description: String,
}

/// Whole-tree verdict: "clean" | "suspicious" | "malicious".
/// critical → malicious; high/medium → suspicious; low-only → clean.
#[derive(Debug, Clone)]
pub struct ScanReport {
    pub verdict: &'static str,
    pub findings: Vec<ScanFinding>,
}

impl ScanReport {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "verdict": self.verdict,
            "findings": self.findings.iter().map(|f| serde_json::json!({
                "severity": f.severity,
                "file": f.file,
                "line": f.line,
                "rule": f.rule,
                "description": f.description,
            })).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &serde_json::Value) -> Option<ScanReport> {
        let verdict = v.get("verdict")?.as_str()?;
        if !matches!(verdict, "clean" | "suspicious" | "malicious") {
            return None;
        }
        // 'static coercion: only the three known verdicts pass the check above.
        let verdict: &'static str = match verdict {
            "clean" => "clean",
            "suspicious" => "suspicious",
            _ => "malicious",
        };
        let mut findings = Vec::new();
        for f in v.get("findings")?.as_array()? {
            let severity = f.get("severity")?.as_str()?;
            let severity: &'static str = match severity {
                "critical" => "critical",
                "high" => "high",
                "medium" => "medium",
                _ => "low",
            };
            let rule = f.get("rule")?.as_str()?;
            let rule: &'static str = match rule {
                "miner" => "miner",
                "download_execute" => "download_execute",
                "obfuscated_exec" => "obfuscated_exec",
                "credential_harvest" => "credential_harvest",
                "exfiltration" => "exfiltration",
                "persistence" => "persistence",
                "base64_blob" => "base64_blob",
                "manifest_mismatch" => "manifest_mismatch",
                _ => "unknown",
            };
            findings.push(ScanFinding {
                severity,
                file: f.get("file")?.as_str()?.to_string(),
                line: f.get("line").and_then(|x| x.as_u64()),
                rule,
                description: f.get("description")?.as_str()?.to_string(),
            });
        }
        Some(ScanReport { verdict, findings })
    }
}

/// Manifest context for the manifest-mismatch rule. Only native tool
/// plugins declare env vars / capabilities, so the rule only applies to them.
pub struct ScanContext {
    pub tool_manifest: bool,
    pub declared_env: Vec<String>,
    pub capability_names: Vec<String>,
}

#[derive(Default)]
struct ScanState {
    env_reads: HashMap<String, String>, // var -> first file that read it
    network_files: Vec<String>,         // files with network use (capped)
}

const MAX_FINDINGS: usize = 200;

/// Scan every text file under `root`.
pub fn scan_tree(root: &Path, ctx: &ScanContext) -> ScanReport {
    let mut findings: Vec<ScanFinding> = Vec::new();
    let mut state = ScanState::default();
    let mut files_scanned = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if findings.len() >= MAX_FINDINGS {
                break;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if rel == ".scan-report.json" || rel == ".import-meta.json" {
                continue;
            }
            let meta = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.len() > MAX_SCANNED_FILE_BYTES {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            // Binary sniff: NUL in the first 4 KiB.
            let head = &bytes[..bytes.len().min(4096)];
            if head.contains(&0) {
                continue;
            }
            files_scanned += 1;
            if files_scanned > 5000 {
                break;
            }
            let text = String::from_utf8_lossy(&bytes);
            scan_text(&rel, &text, &mut state, &mut findings);
        }
    }

    // Aggregate rule: manifest-vs-code mismatch (tool plugins only).
    if ctx.tool_manifest {
        for (var, file) in &state.env_reads {
            if findings.len() >= MAX_FINDINGS {
                break;
            }
            if !ctx.declared_env.iter().any(|d| d == var) {
                findings.push(ScanFinding {
                    severity: "medium",
                    file: file.clone(),
                    line: None,
                    rule: "manifest_mismatch",
                    description: format!(
                        "code reads env var {var:?} which the manifest does not declare in env_vars"
                    ),
                });
            }
        }
        let declares_network = ctx.capability_names.iter().any(|c| {
            let cl = c.to_lowercase();
            cl.contains("net")
                || cl.contains("http")
                || cl.contains("fetch")
                || cl.contains("socket")
        });
        if !declares_network {
            for file in state.network_files.iter().take(5) {
                if findings.len() >= MAX_FINDINGS {
                    break;
                }
                findings.push(ScanFinding {
                    severity: "medium",
                    file: file.clone(),
                    line: None,
                    rule: "manifest_mismatch",
                    description:
                        "code uses the network but the manifest declares no network capability"
                            .to_string(),
                });
            }
        }
    }

    let verdict = if findings.iter().any(|f| f.severity == "critical") {
        "malicious"
    } else if findings
        .iter()
        .any(|f| f.severity == "high" || f.severity == "medium")
    {
        "suspicious"
    } else {
        "clean"
    };
    ScanReport { verdict, findings }
}

fn scan_text(rel: &str, text: &str, state: &mut ScanState, findings: &mut Vec<ScanFinding>) {
    for (idx, line) in text.lines().enumerate() {
        if findings.len() >= MAX_FINDINGS {
            return;
        }
        let line_no = (idx + 1) as u64;
        let l = line.to_lowercase();
        macro_rules! hit {
            ($rule:expr, $sev:expr, $desc:expr) => {
                findings.push(ScanFinding {
                    severity: $sev,
                    file: rel.to_string(),
                    line: Some(line_no),
                    rule: $rule,
                    description: $desc.to_string(),
                })
            };
        }
        // --- miners (critical) ---
        for pat in [
            "xmrig",
            "minerd",
            "cryptonight",
            "stratum+tcp",
            "nicehash",
            "ethminer",
            "cpuminer",
            "kawpow",
            "randomx",
        ] {
            if l.contains(pat) {
                hit!("miner", "critical", format!("known miner string {pat:?}"));
                break;
            }
        }
        // --- download-and-execute (critical) ---
        if (l.contains("curl") || l.contains("wget"))
            && (l.contains("| sh")
                || l.contains("|sh")
                || l.contains("| bash")
                || l.contains("|bash")
                || l.contains("| zsh"))
        {
            hit!(
                "download_execute",
                "critical",
                "download piped directly into a shell"
            );
        }
        if l.contains("iex(") || l.contains("invoke-expression") {
            hit!(
                "download_execute",
                "critical",
                "PowerShell Invoke-Expression"
            );
        }
        if l.contains("eval(")
            && (l.contains("curl")
                || l.contains("wget")
                || l.contains("requests.get")
                || l.contains("urlopen")
                || l.contains("fetch("))
        {
            hit!(
                "download_execute",
                "critical",
                "eval() applied to fetched content"
            );
        }
        // --- obfuscated execution (critical) ---
        if (l.contains("eval(") || l.contains("exec("))
            && (l.contains("base64") || l.contains("b64decode") || l.contains("atob("))
        {
            hit!(
                "obfuscated_exec",
                "critical",
                "eval/exec of a base64-decoded payload"
            );
        }
        if l.contains("new function(") && l.contains("fromcharcode") {
            hit!(
                "obfuscated_exec",
                "critical",
                "Function constructor with char-code obfuscation"
            );
        }
        // --- credential harvesting (high) ---
        for pat in [
            ".ssh/",
            ".aws/credentials",
            ".gnupg/",
            "keychain",
            "login data",
            "cookies",
            ".mozilla/",
            "application support/google/chrome",
            "application support/firefox",
            "security find",
            "secret-tool",
            "lsass",
            "ntds.dit",
            "/etc/shadow",
        ] {
            if l.contains(pat) {
                hit!(
                    "credential_harvest",
                    "high",
                    format!("touches credential store {pat:?}")
                );
                break;
            }
        }
        if is_env_read(&l) && contains_secret_word(&l) {
            hit!(
                "credential_harvest",
                "high",
                "reads a secret-looking environment variable"
            );
        }
        // --- exfiltration (high): network send carrying sensitive data ---
        if is_network_send(&l)
            && (contains_secret_word(&l) || l.contains(".ssh") || l.contains("keychain"))
        {
            hit!(
                "exfiltration",
                "high",
                "sends sensitive data over the network"
            );
        }
        // --- persistence (medium) ---
        for pat in [
            "crontab",
            "/etc/cron",
            "systemctl enable",
            "launchagents",
            "launchdaemons",
            "currentversion\\run",
            "shell:startup",
            ".config/autostart",
            "schtasks",
            "update-rc.d",
        ] {
            if l.contains(pat) {
                hit!(
                    "persistence",
                    "medium",
                    format!("persistence mechanism {pat:?}")
                );
                break;
            }
        }
        // --- large base64 blobs (medium): possible packed payloads ---
        if has_long_base64_token(line) {
            hit!(
                "base64_blob",
                "medium",
                "large base64 blob (possible packed payload)"
            );
        }
        // --- aggregate-rule inputs ---
        for var in extract_env_reads(line) {
            state
                .env_reads
                .entry(var)
                .or_insert_with(|| rel.to_string());
        }
        if is_network_use(&l) && !state.network_files.contains(&rel.to_string()) {
            state.network_files.push(rel.to_string());
        }
    }
}

fn is_env_read(l: &str) -> bool {
    l.contains("os.environ")
        || l.contains("os.getenv")
        || l.contains("process.env")
        || l.contains("getenv(")
        || l.contains("env::var")
}

fn contains_secret_word(l: &str) -> bool {
    [
        "api_key",
        "apikey",
        "secret",
        "passwd",
        "password",
        "token",
        "private_key",
        "privatekey",
        "aws_",
        "github_token",
        "openai",
        "bearer",
        "credentials",
    ]
    .iter()
    .any(|w| l.contains(w))
}

fn is_network_send(l: &str) -> bool {
    [
        "requests.post",
        "requests.put",
        "requests.patch",
        "urllib.request",
        "http.client",
        "fetch(",
        "axios.post",
        "axios.put",
        "curl ",
        "--data",
        "wget --post-data",
        "invoke-webrequest",
        "invoke-restmethod",
    ]
    .iter()
    .any(|w| l.contains(w))
}

fn is_network_use(l: &str) -> bool {
    is_network_send(l)
        || [
            "requests.get",
            "requests.",
            "urllib",
            "http.get",
            "axios",
            "curl",
            "wget",
            "socket.",
            "reqwest",
            "net/http",
            "urlopen",
        ]
        .iter()
        .any(|w| l.contains(w))
}

/// A run of ≥200 base64-alphabet characters — rare in honest code,
///
/// common in packed payloads.
fn has_long_base64_token(line: &str) -> bool {
    let mut run = 0usize;
    for c in line.chars() {
        if c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=' {
            run += 1;
            if run >= 200 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// Extract env var names read in one source line. Heuristic patterns for
/// Python / Node / C / Rust / Ruby.
fn extract_env_reads(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    for marker in [
        "os.environ[",
        "os.environ.get(",
        "os.getenv(",
        "getenv(",
        "env::var(",
        "ENV[",
    ] {
        let mut rest = line;
        while let Some(i) = rest.find(marker) {
            rest = &rest[i + marker.len()..];
            let t = rest.trim_start();
            let mut chars = t.chars();
            let quote = chars.next();
            if quote == Some('"') || quote == Some('\'') {
                let q = quote.unwrap();
                if let Some(end) = t[1..].find(q) {
                    let var: String = t[1..1 + end]
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                        .collect();
                    if !var.is_empty() {
                        out.push(var);
                    }
                }
            }
        }
    }
    let mut rest = line;
    while let Some(i) = rest.find("process.env.") {
        rest = &rest[i + 13..];
        let var: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !var.is_empty() {
            out.push(var);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Quarantine
// ---------------------------------------------------------------------------

/// Staging/quarantine live here — never on any plugin load path.
/// (The tool loader scans `<data_dir>/plugins` immediate subdirs for
/// `manifest.yaml`; `.quarantine` contains none directly, and the
/// loader additionally skips dot-directories. The hook loader only
/// scans `<data_dir>/extensions`.)
pub fn quarantine_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins").join(".quarantine")
}

fn staging_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("plugins").join(".staging")
}

/// Metadata persisted beside a quarantined plugin.
#[derive(Debug, Clone)]
pub struct ImportMeta {
    pub name: String,
    pub kind: String, // "tool" | "hook"
    pub version: String,
    pub description: String,
    pub source: String, // "github"
    pub source_ref: String,
    pub imported_at: u64,
}

impl ImportMeta {
    fn file(dir: &Path) -> PathBuf {
        dir.join(".import-meta.json")
    }

    pub fn load(data_dir: &Path, name: &str) -> Option<ImportMeta> {
        Self::load_from(&quarantine_dir(data_dir).join(name))
    }

    pub fn load_from(dir: &Path) -> Option<ImportMeta> {
        let text = std::fs::read_to_string(Self::file(dir)).ok()?;
        let v: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some(ImportMeta {
            name: v.get("name")?.as_str()?.to_string(),
            kind: v.get("kind")?.as_str()?.to_string(),
            version: v.get("version")?.as_str().unwrap_or("").to_string(),
            description: v.get("description")?.as_str().unwrap_or("").to_string(),
            source: v.get("source")?.as_str().unwrap_or("").to_string(),
            source_ref: v.get("source_ref")?.as_str().unwrap_or("").to_string(),
            imported_at: v.get("imported_at")?.as_u64().unwrap_or(0),
        })
    }

    fn write_to(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::write(
            Self::file(dir),
            serde_json::to_string_pretty(&serde_json::json!({
                "name": self.name,
                "kind": self.kind,
                "version": self.version,
                "description": self.description,
                "source": self.source,
                "source_ref": self.source_ref,
                "imported_at": self.imported_at,
            }))
            .unwrap_or_default(),
        )
    }
}

pub fn is_quarantined(data_dir: &Path, name: &str) -> bool {
    quarantine_dir(data_dir).join(name).is_dir()
}

/// The persisted scan verdict for a quarantined plugin, if any.
pub fn read_scan_verdict(data_dir: &Path, name: &str) -> Option<String> {
    let text = std::fs::read_to_string(
        quarantine_dir(data_dir)
            .join(name)
            .join(".scan-report.json"),
    )
    .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(v.get("verdict")?.as_str()?.to_string())
}

/// The full persisted scan report for a quarantined plugin, if any.
pub fn read_scan_report(data_dir: &Path, name: &str) -> Option<ScanReport> {
    let text = std::fs::read_to_string(
        quarantine_dir(data_dir)
            .join(name)
            .join(".scan-report.json"),
    )
    .ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    ScanReport::from_json(&v)
}

/// Promote a quarantined plugin into its live dir on approval. The scan
/// report and import meta travel with it as an audit trail.
pub fn promote_from_quarantine(data_dir: &Path, meta: &ImportMeta) -> Result<(), String> {
    let src = quarantine_dir(data_dir).join(&meta.name);
    if !src.is_dir() {
        return Err("quarantined plugin disappeared".into());
    }
    let dst = match meta.kind.as_str() {
        "tool" => data_dir.join("plugins").join(&meta.name),
        "hook" => data_dir.join("extensions").join(&meta.name),
        k => return Err(format!("unknown plugin kind {k:?}")),
    };
    if dst.exists() {
        return Err(format!(
            "a live plugin named {:?} already exists",
            meta.name
        ));
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    if let Err(rename_err) = std::fs::rename(&src, &dst) {
        // Cross-device fallback: copy then remove.
        copy_dir_recursive(&src, &dst)
            .and_then(|_| std::fs::remove_dir_all(&src).map_err(|e| e.to_string()))
            .map_err(|e| format!("promote failed (rename: {rename_err}): {e}"))?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    let entries = std::fs::read_dir(src).map_err(|e| e.to_string())?;
    for e in entries.flatten() {
        let p = e.path();
        let rel = p.strip_prefix(src).map_err(|e| e.to_string())?;
        let target = dst.join(rel);
        if p.is_dir() {
            copy_dir_recursive(&p, &target)?;
        } else {
            std::fs::copy(&p, &target).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// ClawHub
// ---------------------------------------------------------------------------

/// ClawHub → registry entry field mapping (verified against the live API
/// 2026-09-30):
/// - `slug` ← item.slug
/// - `name` ← item.displayName
/// - `description` ← item.summary (fallback: item.description, often null)
/// - `version` ← item.tags.latest (fallback: item.latestVersion.version)
/// - `kind` ← "skill" (constant: ClawHub lists skills only; there is no
///   plugin family, so every result is a skill, never a plugin)
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub kind: &'static str,
}

impl RegistryEntry {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "slug": self.slug,
            "name": self.name,
            "description": self.description,
            "version": self.version,
            "kind": self.kind,
        })
    }
}

fn registry_entry_from(item: &serde_json::Value) -> RegistryEntry {
    let slug = item
        .get("slug")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let name = item
        .get("displayName")
        .and_then(|x| x.as_str())
        .unwrap_or(&slug)
        .to_string();
    let description = item
        .get("summary")
        .and_then(|x| x.as_str())
        .or_else(|| item.get("description").and_then(|x| x.as_str()))
        .unwrap_or("")
        .to_string();
    let version = item
        .get("tags")
        .and_then(|t| t.get("latest"))
        .and_then(|x| x.as_str())
        .or_else(|| {
            item.get("latestVersion")
                .and_then(|x| x.get("version"))
                .and_then(|x| x.as_str())
        })
        .unwrap_or("")
        .to_string();
    RegistryEntry {
        slug,
        name,
        description,
        version,
        kind: "skill",
    }
}

/// Proxy ClawHub skill search for the registry picker UI.
pub fn clawhub_search(fetch: &FetchFn, query: &str) -> Result<Vec<RegistryEntry>, ImportError> {
    let q = query.trim();
    if q.is_empty() {
        return Err(ImportError::BadRequest("q is required".into()));
    }
    if q.len() > 200 {
        return Err(ImportError::BadRequest("q is too long".into()));
    }
    let url = format!("{CLAWHUB_API}/skills?q={}&limit=25", percent_encode(q));
    let body = fetch(&url).map_err(|e| match e {
        FetchError::Status(code, _) => {
            ImportError::FetchFailed(format!("ClawHub search failed: HTTP {code}"))
        }
        other => ImportError::FetchFailed(format!("ClawHub search failed: {other:?}")),
    })?;
    let v: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ImportError::FetchFailed(format!("ClawHub returned bad JSON: {e}")))?;
    let items = v
        .get("items")
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items.iter().map(registry_entry_from).collect())
}

/// A ClawHub package resolved to downloadable bytes.
struct ClawHubPackage {
    bytes: Vec<u8>,
    version: String,
}

/// Resolve `clawhub:<slug>` via the packages API and download the package
/// zip (verified live 2026-09-30: `GET /api/v1/packages/{name}` →
/// `{"package": {family, latestVersion, ...}}`,
/// `GET /api/v1/packages/{name}/download?version={v}` → zip).
///
/// The slug may be `<name>` or `<owner>/<name>`; the API is addressed by
/// the final segment. A package whose `family` is `skill` still ends in
/// 422 NOT_A_PLUGIN pointing at the skills importer; a slug that 404s on
/// the packages endpoint is checked against the skills endpoint before
/// giving up, so the hint survives for skill slugs.
fn fetch_clawhub_package(fetch: &FetchFn, slug: &str) -> Result<ClawHubPackage, ImportError> {
    let api_name = slug.rsplit('/').next().unwrap_or(slug);
    let meta_url = format!("{CLAWHUB_API}/packages/{}", percent_encode(api_name));
    let body = match fetch(&meta_url) {
        Ok(b) => b,
        Err(FetchError::Status(404, _)) => return Err(clawhub_skill_hint(fetch, slug)),
        Err(e) => {
            return Err(ImportError::FetchFailed(format!(
                "ClawHub package lookup failed: {e:?}"
            )))
        }
    };
    let v: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ImportError::FetchFailed(format!("ClawHub returned bad JSON: {e}")))?;
    let pkg = v.get("package").unwrap_or(&v);
    if pkg
        .get("family")
        .and_then(|x| x.as_str())
        .map(|f| f.eq_ignore_ascii_case("skill"))
        .unwrap_or(false)
    {
        return Err(not_a_plugin_error(pkg, slug));
    }
    // `latestVersion` is a plain string on the packages endpoint
    // ("2.5.0"); accept the object shape too, then the other known
    // shapes, defensively.
    let version = pkg
        .get("latestVersion")
        .and_then(|x| {
            x.as_str().map(str::to_string).or_else(|| {
                x.get("version")
                    .and_then(|y| y.as_str())
                    .map(str::to_string)
            })
        })
        .or_else(|| {
            pkg.get("tags")
                .and_then(|t| t.get("latest"))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            let s = manifest_str(pkg, "version", "");
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
        .ok_or_else(|| {
            ImportError::FetchFailed(format!("ClawHub package {slug:?} has no published version"))
        })?;
    let dl_url = format!(
        "{CLAWHUB_API}/packages/{}/download?version={}",
        percent_encode(api_name),
        percent_encode(&version)
    );
    let bytes = fetch(&dl_url).map_err(|e| match e {
        FetchError::Status(404, _) => ImportError::NotFound(format!(
            "ClawHub package {slug:?} has no download for version {version:?}"
        )),
        other => ImportError::FetchFailed(format!("ClawHub download failed: {other:?}")),
    })?;
    Ok(ClawHubPackage { bytes, version })
}

/// A slug that 404s on the packages endpoint might be a ClawHub skill —
/// check the skills endpoint so the operator still gets the helpful
/// NOT_A_PLUGIN hint instead of a bare 404.
fn clawhub_skill_hint(fetch: &FetchFn, slug: &str) -> ImportError {
    let api_name = slug.rsplit('/').next().unwrap_or(slug);
    let url = format!("{CLAWHUB_API}/skills/{}", percent_encode(api_name));
    match fetch(&url) {
        Ok(body) => {
            let v: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let skill = v.get("skill").unwrap_or(&v);
            not_a_plugin_error(skill, slug)
        }
        Err(FetchError::Status(404, _)) => {
            ImportError::NotFound(format!("no ClawHub package or skill for slug {slug:?}"))
        }
        Err(e) => ImportError::FetchFailed(format!("ClawHub lookup failed: {e:?}")),
    }
}

fn not_a_plugin_error(entry_json: &serde_json::Value, slug: &str) -> ImportError {
    let mut entry = registry_entry_from(entry_json);
    // The packages-metadata shape has no `slug` field — the slug the
    // operator typed is authoritative.
    if entry.slug.is_empty() {
        entry.slug = slug.to_string();
    }
    if entry.name.is_empty() {
        entry.name = entry_json
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or(slug)
            .to_string();
    }
    ImportError::NotAPlugin {
        message: format!(
            "clawhub:{slug} is a ClawHub skill (SKILL.md bundle), not a Pantheon plugin — refusing to install it as a plugin"
        ),
        skill: entry.to_json(),
        hint: "POST /api/skills/import".into(),
    }
}

// ---------------------------------------------------------------------------
// Import orchestration
// ---------------------------------------------------------------------------

/// The successful import report, serialized as the endpoint response.
#[derive(Debug)]
pub struct ImportReport {
    pub name: String,
    pub kind: &'static str,
    pub version: String,
    pub source: &'static str,
    pub source_ref: String,
    pub capabilities: Vec<String>,
    pub notes: Vec<String>,
    pub scan: ScanReport,
}

impl ImportReport {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "name": self.name,
            "kind": self.kind,
            "version": self.version,
            "source": self.source,
            "source_ref": self.source_ref,
            "detected_capabilities": self.capabilities,
            "notes": self.notes,
            "approval_required": true,
            "quarantined": true,
            "scan_report": self.scan.to_json(),
        })
    }
}

fn map_fetch_error(e: FetchError, url: &str) -> ImportError {
    match e {
        FetchError::Status(404, _) => ImportError::NotFound(format!(
            "GitHub returned 404 for {url} (bad owner/repo/ref?)"
        )),
        FetchError::Status(code, text) => {
            let text = text.trim();
            let detail = if text.is_empty() {
                format!("HTTP {code}")
            } else {
                format!("HTTP {code} ({text})")
            };
            ImportError::FetchFailed(format!("download failed: {detail}"))
        }
        FetchError::Transport(msg) => ImportError::FetchFailed(format!(
            "download failed: {}",
            pantheon_exec::http::fetch_error_message(url, &msg)
        )),
        FetchError::TooLarge => ImportError::TooLarge(format!(
            "download exceeds {} MiB",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        )),
    }
}

/// Which archive layout the downloaded bundle uses.
#[derive(Debug, Clone, Copy)]
enum ArchiveKind {
    TarGz,
    Zip,
}

/// Full import: validate → fetch → extract → detect → scan → quarantine.
/// `fetch` is injectable so tests never touch the network.
pub fn run_import(
    data_dir: &Path,
    url: &str,
    ref_field: Option<&str>,
    fetch: &FetchFn,
) -> Result<ImportReport, ImportError> {
    let spec = parse_source_spec(url, ref_field)?;
    // (bundle bytes, archive layout, source label, source ref)
    let (bytes, kind, source, source_ref): (Vec<u8>, ArchiveKind, &'static str, String) = match spec
    {
        SourceSpec::ClawHub { slug } => {
            let pkg = fetch_clawhub_package(fetch, &slug)?;
            (
                pkg.bytes,
                ArchiveKind::Zip,
                "clawhub",
                format!("clawhub:{slug}@{}", pkg.version),
            )
        }
        SourceSpec::Github {
            owner,
            repo,
            gitref,
        } => {
            // Download. Default ref `main`; one retry with `master` on 404.
            let tarball_url = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/{gitref}");
            let (gitref_used, bytes) = match fetch(&tarball_url) {
                Ok(b) => (gitref.clone(), b),
                Err(FetchError::Status(404, _)) if gitref == "main" => {
                    let alt = format!("https://codeload.github.com/{owner}/{repo}/tar.gz/master");
                    match fetch(&alt) {
                        Ok(b) => ("master".to_string(), b),
                        Err(e) => return Err(map_fetch_error(e, &alt)),
                    }
                }
                Err(e) => return Err(map_fetch_error(e, &tarball_url)),
            };
            (
                bytes,
                ArchiveKind::TarGz,
                "github",
                format!("{owner}/{repo}@{gitref_used}"),
            )
        }
    };

    // The cap holds no matter which fetch impl produced the bytes.
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(ImportError::TooLarge(format!(
            "download exceeds {} MiB",
            MAX_DOWNLOAD_BYTES / 1024 / 1024
        )));
    }

    // Stage under .staging/; always cleaned up (rename moves it away on success).
    let stage_root = staging_dir(data_dir);
    std::fs::create_dir_all(&stage_root).map_err(|e| ImportError::Io(e.to_string()))?;
    let stage = stage_root.join(format!("import-{}-{}", now_unix(), std::process::id()));
    std::fs::create_dir_all(&stage).map_err(|e| ImportError::Io(e.to_string()))?;

    let result = run_import_staged(data_dir, &stage, &bytes, kind, source, &source_ref);
    let _ = std::fs::remove_dir_all(&stage);
    result
}

fn run_import_staged(
    data_dir: &Path,
    stage: &Path,
    bytes: &[u8],
    kind: ArchiveKind,
    source: &'static str,
    source_ref: &str,
) -> Result<ImportReport, ImportError> {
    let stats = match kind {
        ArchiveKind::TarGz => extract_tar_gz(bytes, stage)?,
        ArchiveKind::Zip => extract_zip(bytes, stage)?,
    };
    if stats.files == 0 {
        return Err(ImportError::BadRequest(
            "bundle contained no usable files".into(),
        ));
    }
    let detected = detect_format(stage)?;

    let ctx = ScanContext {
        tool_manifest: detected.kind == "tool",
        declared_env: detected.declared_env.clone(),
        capability_names: detected.capability_names.clone(),
    };
    let scan = scan_tree(stage, &ctx);

    // Quarantine: never on a load path, never approved by default.
    let qdir = quarantine_dir(data_dir);
    std::fs::create_dir_all(&qdir).map_err(|e| ImportError::Io(e.to_string()))?;
    let target = qdir.join(&detected.name);
    if target.exists() {
        return Err(ImportError::Conflict(format!(
            "a quarantined plugin named {:?} already exists",
            detected.name
        )));
    }
    let live_clash = data_dir.join("plugins").join(&detected.name).exists()
        || data_dir.join("extensions").join(&detected.name).exists();
    if live_clash {
        return Err(ImportError::Conflict(format!(
            "a live plugin named {:?} already exists",
            detected.name
        )));
    }
    std::fs::rename(stage, &target).map_err(|e| ImportError::Io(format!("quarantine: {e}")))?;

    std::fs::write(
        target.join(".scan-report.json"),
        serde_json::to_string_pretty(&scan.to_json()).unwrap_or_default(),
    )
    .map_err(|e| ImportError::Io(e.to_string()))?;
    ImportMeta {
        name: detected.name.clone(),
        kind: detected.kind.to_string(),
        version: detected.version.clone(),
        description: detected.description.clone(),
        source: source.to_string(),
        source_ref: source_ref.to_string(),
        imported_at: now_unix(),
    }
    .write_to(&target)
    .map_err(|e| ImportError::Io(e.to_string()))?;

    let mut notes = detected.notes;
    if stats.skipped_unsafe > 0 {
        notes.push(format!(
            "skipped {} unsafe tarball entries (parent-dir, absolute, or symlink)",
            stats.skipped_unsafe
        ));
    }
    match scan.verdict {
        "malicious" => notes.push(
            "scan verdict MALICIOUS: approval is blocked; remove it from quarantine manually if this is a false positive."
                .to_string(),
        ),
        "suspicious" => notes.push(
            "scan verdict SUSPICIOUS: approving requires {\"acknowledge_risk\": true}."
                .to_string(),
        ),
        _ => {}
    }

    Ok(ImportReport {
        name: detected.name,
        kind: detected.kind,
        version: detected.version,
        source,
        source_ref: source_ref.to_string(),
        capabilities: detected.capabilities,
        notes,
        scan,
    })
}

// ---------------------------------------------------------------------------
// Tests (no network: every fetch is a stub behind the FetchFn trait object)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_data_dir() -> std::path::PathBuf {
        let n = TEST_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-plugin-import-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        dir
    }

    /// Minimal plugin bundle: a root `manifest.yaml` (native tool plugin).
    fn fixture_zip() -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("manifest.yaml", zip::write::SimpleFileOptions::default())
                .expect("start manifest.yaml");
            zip.write_all(b"name: my-plugin\ndescription: fixture\nversion: 1.2.3\n")
                .expect("write manifest.yaml");
            zip.finish().expect("finish zip");
        }
        buf.into_inner()
    }

    /// P1 #3 (server side of the contract): the value the TUI now sends as
    /// `{url}` — `clawhub:<slug>` — must resolve through the ClawHub
    /// packages API instead of 400ing with "url is required". Fetch is
    /// stubbed; no network.
    #[test]
    fn clawhub_slug_in_url_field_resolves() {
        let zip_bytes = fixture_zip();
        let fetch = move |url: &str| -> Result<Vec<u8>, FetchError> {
            match url {
                "https://clawhub.ai/api/v1/packages/my-plugin" => {
                    Ok(br#"{"package":{"latestVersion":"1.2.3"}}"#.to_vec())
                }
                "https://clawhub.ai/api/v1/packages/my-plugin/download?version=1.2.3" => {
                    Ok(zip_bytes.clone())
                }
                other => Err(FetchError::Status(404, other.to_string())),
            }
        };
        let dir = temp_data_dir();
        let report = run_import(&dir, "clawhub:owner/my-plugin", None, &fetch)
            .expect("clawhub:<slug> in the url field must resolve");
        assert_eq!(report.source, "clawhub");
        assert_eq!(report.name, "my-plugin");
        assert_eq!(report.source_ref, "clawhub:owner/my-plugin@1.2.3");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
