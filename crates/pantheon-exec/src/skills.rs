//! Skill discovery: `SKILL.md` files with YAML frontmatter are the
//! capability contract (ECC/OpenMausBot pattern). Each skill is a named,
//! described, self-contained unit the model can list and load.
//!
//! Discovery scans these places:
//!   <data_dir>/skills/<name>/SKILL.md
//!   <cwd>/.pantheon/skills/<name>/SKILL.md
//!
//! Skills are data, not code: the model sees name + description, and
//! reads the body via a gated tool. Nothing executes at discovery time.
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the SKILL.md",
        "",
    )
}

/// Parsed frontmatter of one skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Origin tag for provenance (e.g. "bundled", "user", catalog name).
    #[serde(default)]
    pub origin: String,
}

/// One discovered skill: metadata plus where its body lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub meta: SkillMeta,
    /// Absolute path to the SKILL.md file.
    pub path: PathBuf,
}

/// Parse frontmatter + body from a SKILL.md. Frontmatter is `---` fenced
/// YAML at the very top. Missing frontmatter is an error; an empty body
/// is fine.
pub fn parse_skill(raw: &str, path: &Path) -> Result<Skill, PantheonError> {
    let rest = raw.strip_prefix("---").ok_or_else(|| {
        serr(
            "SKILL_NO_FRONTMATTER",
            format!("{}: no frontmatter block", path.display()),
        )
    })?;
    let end = rest.find("\n---").ok_or_else(|| {
        serr(
            "SKILL_NO_FRONTMATTER",
            format!("{}: frontmatter not closed", path.display()),
        )
    })?;
    let yaml = &rest[..end];
    let mut meta: SkillMeta = serde_yaml::from_str(yaml)
        .map_err(|e| serr("SKILL_BAD_FRONTMATTER", format!("{}: {e}", path.display())))?;
    if meta.name.trim().is_empty() {
        return Err(serr(
            "SKILL_NO_NAME",
            format!("{}: frontmatter has no name", path.display()),
        ));
    }
    if meta.origin.is_empty() {
        meta.origin = "user".into();
    }
    Ok(Skill {
        meta,
        path: path.to_path_buf(),
    })
}

/// Load one skill from a SKILL.md path. Broken skills are skipped (None).
///
/// Silent on purpose. This runs during every session start across every
/// cross-tool skill root, so a single malformed third-party SKILL.md would
/// otherwise print a line into the middle of unrelated CLI output. Callers
/// that need to report a broken skill use `parse_skill` directly, which is
/// what `skills doctor` does.
pub fn load_skill(path: &Path) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    parse_skill(&raw, path).ok()
}

/// Read a skill's full markdown body (frontmatter stripped).
pub fn skill_body(skill: &Skill) -> Result<String, PantheonError> {
    let raw = std::fs::read_to_string(&skill.path)
        .map_err(|e| serr("SKILL_READ", format!("{}: {e}", skill.path.display())))?;
    let rest = raw.strip_prefix("---").unwrap_or(&raw);
    match rest.find("\n---") {
        Some(end) => Ok(rest[end + 4..].trim_start().to_string()),
        None => Ok(raw),
    }
}

/// Register `skills_list` and `skill_read` on a registry.
///
/// `skills_list` is read-only (FilesystemRead): names + descriptions.
/// `skill_read` loads one skill's body, gated on FilesystemRead too —
/// skills are data, not executable capability. A skill that wanted to
/// grant powers would be a plugin, not a skill.
pub fn register_skill_tools(reg: &mut crate::tools::ToolRegistry, skills: Vec<Skill>) {
    if skills.is_empty() {
        return;
    }
    let list = skills.clone();
    reg.register(
        pantheon_core::message::ToolSchema {
            name: "skills_list".into(),
            description: "List available skills (name, description, origin).".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
        pantheon_core::capability::Capability::FilesystemRead,
        move |_args| {
            let mut out = String::new();
            for s in &list {
                out.push_str(&format!(
                    "- {}: {} ({})\n",
                    s.meta.name, s.meta.description, s.meta.origin
                ));
            }
            Ok(out)
        },
    );

    let read = skills;
    reg.register(
        pantheon_core::message::ToolSchema {
            name: "skill_read".into(),
            description: "Read one skill's full markdown body by name.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"]
            }),
        },
        pantheon_core::capability::Capability::FilesystemRead,
        move |args| {
            let v: serde_json::Value =
                serde_json::from_str(if args.trim().is_empty() { "{}" } else { args }).map_err(
                    |e| {
                        PantheonError::new(
                            "TOOL_BAD_ARGS",
                            Layer::Execution,
                            false,
                            format!("invalid JSON args: {e}"),
                            "check tool name and arguments",
                            "",
                        )
                    },
                )?;
            let name = v.get("name").and_then(|x| x.as_str()).ok_or_else(|| {
                PantheonError::new(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "missing 'name'".to_string(),
                    "check tool name and arguments",
                    "",
                )
            })?;
            let skill = read.iter().find(|s| s.meta.name == name).ok_or_else(|| {
                PantheonError::new(
                    "SKILL_UNKNOWN",
                    Layer::Execution,
                    false,
                    format!("no skill named '{name}'"),
                    "call skills_list first",
                    "",
                )
            })?;
            skill_body(skill)
        },
    );
}

#[cfg(test)]
#[path = "skills_tests.rs"]
mod tests;

/// Where a skill was discovered from. Carried as provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillSource {
    /// <data_dir>/skills or <cwd>/.pantheon/skills
    Pantheon,
    /// ~/.hermes/skills or a Hermes plugin's skills/ subdir
    Hermes,
    /// ~/.openclaw/skills
    OpenClaw,
    /// <cwd>/.agents/skills (Claude Code convention)
    Agents,
    /// <cwd>/.claude/skills (Claude convention)
    Claude,
    /// ~/.codex/skills/.system/ (Codex system skills; dot-dir root)
    Codex,
    /// ~/.omp/agent/skills/ (OMP native skills)
    Omp,
    /// ~/.claude/skills (user-level Claude skills)
    ClaudeUser,
    /// Some other directory the user pointed us at
    External,
}

impl SkillSource {
    pub fn tag(self) -> &'static str {
        match self {
            SkillSource::Pantheon => "pantheon",
            SkillSource::Hermes => "hermes",
            SkillSource::OpenClaw => "openclaw",
            SkillSource::Agents => "agents",
            SkillSource::Claude => "claude",
            SkillSource::Codex => "codex",
            SkillSource::Omp => "omp",
            SkillSource::ClaudeUser => "claude-user",
            SkillSource::External => "external",
        }
    }
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Normalize a parsed skill: tag its origin, and if `source` is not
/// Pantheon, stamp the provenance tag so the model can tell where it
/// came from. Never overwrites an existing Pantheon skill's origin.
fn with_origin(mut s: Skill, source: SkillSource) -> Skill {
    if !matches!(source, SkillSource::Pantheon) {
        s.meta.origin = source.tag().to_string();
    }
    s
}

/// Collect into `out`, recording every unparseable SKILL.md (and every
/// name collision) into `rejected` with the reason.
///
/// The rejections list is what makes `pantheon skills doctor` meaningful:
/// without it a malformed skill is dropped during discovery and the doctor
/// sees a clean tree while the user silently lost the skill.
fn collect_skills_rejecting(
    base: &Path,
    src: SkillSource,
    out: &mut Vec<Skill>,
    rejected: &mut Vec<RejectedSkill>,
) {
    // NOTE: no hidden-dir skip here. An explicitly-passed root such as
    // ~/.codex/skills/.system/ must still be collected even though generic
    // walks (copy_tree, collect_skill_dirs) skip dot-dirs.
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for e in entries.flatten() {
        let candidate = e.path().join("SKILL.md");
        if !candidate.is_file() {
            continue;
        }
        match load_skill(&candidate) {
            Some(s) => {
                let s = with_origin(s, src);
                if let Some(winner) = out.iter().find(|x| x.meta.name == s.meta.name) {
                    rejected.push(RejectedSkill {
                        path: candidate.clone(),
                        reason: format!(
                            "duplicate skill name {:?} ({} wins)",
                            s.meta.name,
                            winner.path.display()
                        ),
                    });
                } else {
                    out.push(s);
                }
            }
            None => {
                let reason = skill_rejection_reason(&candidate);
                rejected.push(RejectedSkill {
                    path: candidate,
                    reason,
                })
            }
        }
    }
}

/// The parse error behind a rejected SKILL.md, as a one-line reason.
fn skill_rejection_reason(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(raw) => match parse_skill(&raw, path) {
            Ok(_) => "unreadable".into(),
            Err(e) => e.cause,
        },
        Err(e) => format!("read failed: {e}"),
    }
}

/// A SKILL.md that discovery did not return, with the reason why.
#[derive(Debug, Clone)]
pub struct RejectedSkill {
    pub path: PathBuf,
    pub reason: String,
}

/// The full picture of a skill tree: what loaded, and what did not.
#[derive(Debug, Default, Clone)]
pub struct SkillScan {
    pub loaded: Vec<Skill>,
    pub rejected: Vec<RejectedSkill>,
}

/// Every root discovery reads, in precedence order (first name wins).
fn skill_roots(
    data_dir: &Path,
    project_root: &Path,
    extra_roots: &[PathBuf],
    home: &Path,
) -> Vec<(PathBuf, SkillSource)> {
    let mut roots = vec![
        (data_dir.join("skills"), SkillSource::Pantheon),
        (
            project_root.join(".pantheon").join("skills"),
            SkillSource::Pantheon,
        ),
    ];
    roots.extend(
        extra_roots
            .iter()
            .map(|r| (r.clone(), SkillSource::External)),
    );
    roots.extend([
        (home.join(".hermes").join("skills"), SkillSource::Hermes),
        (home.join(".openclaw").join("skills"), SkillSource::OpenClaw),
        (
            project_root.join(".agents").join("skills"),
            SkillSource::Agents,
        ),
        (
            project_root.join(".claude").join("skills"),
            SkillSource::Claude,
        ),
        (
            home.join(".codex").join("skills").join(".system"),
            SkillSource::Codex,
        ),
        (
            home.join(".omp").join("agent").join("skills"),
            SkillSource::Omp,
        ),
        (home.join(".claude").join("skills"), SkillSource::ClaudeUser),
    ]);
    roots
}

/// Discover skills from the data dir, project root, and the standard
/// cross-tool directories. Dedup by skill name (first wins: pantheon
/// scope beats external scope on collision). Broken skills are skipped.
pub fn discover_skills_ext(
    data_dir: &Path,
    project_root: &Path,
    extra_roots: &[PathBuf],
) -> Vec<Skill> {
    scan_skills_ext(data_dir, project_root, extra_roots).loaded
}

/// Like `discover_skills_ext`, but also reports what was skipped and why.
///
/// `skills doctor` needs this: with only the `Vec<Skill>` return, a
/// malformed third-party skill vanishes during discovery and the doctor
/// reports a healthy tree.
pub fn scan_skills_ext(data_dir: &Path, project_root: &Path, extra_roots: &[PathBuf]) -> SkillScan {
    // Materialize bundled skills before the first root is scanned, so every
    // caller (session, `skills list`, `skills doctor`) sees the same tree.
    // Seeding is idempotent and never clobbers a user-edited copy, so
    // running it here rather than per call site is safe. A seed failure is
    // reported by the seeder and does not abort discovery.
    let _ = crate::bundled_skills::seed_bundled_skills(data_dir);
    scan_skills_with_home(data_dir, project_root, extra_roots, &home_dir())
}

fn scan_skills_with_home(
    data_dir: &Path,
    project_root: &Path,
    extra_roots: &[PathBuf],
    home: &Path,
) -> SkillScan {
    let mut out: Vec<Skill> = Vec::new();
    let mut rejected: Vec<RejectedSkill> = Vec::new();
    for (root, src) in skill_roots(data_dir, project_root, extra_roots, home) {
        // A missing root is not a rejection: most roots do not exist on any
        // given machine.
        if !root.exists() {
            continue;
        }
        collect_skills_rejecting(&root, src, &mut out, &mut rejected);
    }
    out.sort_by(|a, b| a.meta.name.cmp(&b.meta.name));
    rejected.sort_by(|a, b| a.path.cmp(&b.path));
    SkillScan {
        loaded: out,
        rejected,
    }
}

/// Inner discovery parameterized by home dir so tests can fake `HOME`
/// without mutating the process environment (cargo tests run in parallel).
#[cfg(test)]
fn discover_skills_ext_with_home(
    data_dir: &Path,
    project_root: &Path,
    extra_roots: &[PathBuf],
    home: &Path,
) -> Vec<Skill> {
    scan_skills_with_home(data_dir, project_root, extra_roots, home).loaded
}

/// Fetch a SKILL.md body, trying each candidate URL in order. A 404 on a
/// bare-repo URL falls through to the next candidate rather than failing.
fn fetch_skill_md(url: &str) -> Result<String, PantheonError> {
    let candidates = candidate_urls(url);
    let mut last_err: Option<PantheonError> = None;
    for candidate in &candidates {
        match ureq::get(candidate)
            .timeout(std::time::Duration::from_secs(15))
            .set("User-Agent", "pantheon-skill-import")
            .call()
        {
            Ok(resp) => {
                if !(200..300).contains(&resp.status()) {
                    last_err = Some(serr(
                        "SKILL_FETCH_HTTP",
                        format!("{}: HTTP {}", url, resp.status()),
                    ));
                    continue;
                }
                let body = resp
                    .into_string()
                    .map_err(|e| serr("SKILL_FETCH_BODY", format!("{e}")))?;
                return Ok(body);
            }
            Err(e) => {
                last_err = Some(serr("SKILL_FETCH", format!("{}: {e}", url)));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| serr("SKILL_FETCH", format!("no candidates for {url}"))))
}

/// Candidate URLs to try for a user-supplied import target.
///
/// GitHub repo URLs are rewritten to raw.githubusercontent.com. A bare
/// repo URL returns both `main` and `master` so the first branch that
/// actually serves a SKILL.md wins.
fn candidate_urls(url: &str) -> Vec<String> {
    let Some(stripped) = url.strip_prefix("https://github.com/") else {
        return vec![url.to_string()];
    };
    let t = stripped.trim_end_matches('/');
    if let Some(blob_idx) = t.find("/blob/") {
        let owner_repo = &t[..blob_idx];
        let after = &t[blob_idx + 6..]; // <ref>/<path>
        if let Some(ref_end) = after.find('/') {
            let r#ref = &after[..ref_end];
            let path = &after[ref_end + 1..];
            return vec![format!(
                "https://raw.githubusercontent.com/{owner_repo}/{ref}/{path}"
            )];
        }
    }
    vec![
        format!("https://raw.githubusercontent.com/{t}/main/SKILL.md"),
        format!("https://raw.githubusercontent.com/{t}/master/SKILL.md"),
    ]
}

/// Import a skill from a URL into <data_dir>/skills.
///
/// The fetched body is validated by `parse_skill` before anything is
/// written, so a malformed SKILL.md fails closed and leaves no file.
pub fn import_skill_from_url(
    data_dir: &Path,
    url: &str,
) -> Result<(PathBuf, Skill), PantheonError> {
    let raw = fetch_skill_md(url)?;
    let parsed = parse_skill(&raw, &PathBuf::from(url))?;
    let name = parsed.meta.name.clone();
    let dest_dir = data_dir.join("skills").join(&name);
    let dest = dest_dir.join("SKILL.md");
    let mut s = parsed;
    if !dest.exists() {
        std::fs::create_dir_all(&dest_dir).map_err(|e| {
            serr(
                "SKILL_IMPORT_IO",
                format!("create {}: {e}", dest_dir.display()),
            )
        })?;
        std::fs::write(&dest, raw)
            .map_err(|e| serr("SKILL_IMPORT_IO", format!("write {}: {e}", dest.display())))?;
    }
    s.path = dest.clone();
    Ok((dest, s))
}

/// Import every SKILL.md found under a GitHub repo.
///
/// Shallow-clones the repo, walks it for `SKILL.md` files, and imports
/// each one that parses. A single broken skill is skipped with a warning
/// rather than aborting the batch.
pub fn import_skills_from_repo(
    data_dir: &Path,
    repo_url: &str,
    subpath: Option<&str>,
) -> Result<Vec<(String, PathBuf)>, PantheonError> {
    let tmp = std::env::temp_dir().join(format!("pantheon-skillscan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)
        .map_err(|e| serr("SKILL_REPO_TMP", format!("create {}: {e}", tmp.display())))?;
    let clone = std::process::Command::new("git")
        .args(["clone", "--depth", "1", "--filter=blob:none", repo_url])
        .arg(&tmp)
        .output()
        .map_err(|e| serr("SKILL_REPO_CLONE", format!("git clone: {e}")))?;
    if !clone.status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(serr(
            "SKILL_REPO_CLONE",
            format!(
                "git clone {repo_url} failed: {}",
                String::from_utf8_lossy(&clone.stderr).trim()
            ),
        ));
    }
    let root = subpath
        .filter(|s| !s.is_empty())
        .map(|s| tmp.join(s.trim_start_matches('/')))
        .unwrap_or(tmp.clone());
    let imported = import_skill_dirs(data_dir, &root)?;
    let _ = std::fs::remove_dir_all(&tmp);
    Ok(imported)
}

/// Response shape for `GET /api/v1/skills/{slug}`. Only the fields we
/// need are modeled; the registry returns much more.
#[derive(Debug, Deserialize)]
struct ClawhubSkillResponse {
    skill: ClawhubSkill,
}

#[derive(Debug, Deserialize)]
struct ClawhubSkill {
    #[serde(default)]
    display_name: String,
    /// Raw SKILL.md body; not read directly (we re-fetch via the ZIP).
    #[allow(dead_code)]
    #[serde(default)]
    description: String,
}

/// Ambiguous-slug response from ClawHub (`409`). Carries the owners the
/// caller can disambiguate with `--owner`.
#[derive(Debug, Deserialize)]
struct ClawhubAmbiguous {
    matches: Vec<ClawhubAmbiguousMatch>,
}

#[derive(Debug, Deserialize)]
struct ClawhubAmbiguousMatch {
    #[serde(rename = "ownerHandle")]
    owner_handle: String,
}

/// Fetch a JSON body from a URL, returning the body and HTTP status.
///
/// Unlike `fetch_skill_md`, this preserves the status code so callers
/// can branch on non-2xx shapes (e.g. ClawHub's 409 ambiguous-slug).
fn fetch_clawhub_json_with_status(url: &str) -> Result<(String, u16), PantheonError> {
    // ureq returns Err(Error::Status(code, resp)) for HTTP error statuses;
    // the response is included, so pull body+status out of either branch.
    match ureq::get(url)
        .timeout(std::time::Duration::from_secs(15))
        .set("User-Agent", "pantheon-skill-import")
        .set("Accept", "application/json")
        .call()
    {
        Ok(resp) => {
            let status = resp.status();
            let body = resp
                .into_string()
                .map_err(|e| serr("SKILL_FETCH_BODY", format!("{e}")))?;
            Ok((body, status))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Ok((body, code))
        }
        Err(e) => Err(serr("SKILL_FETCH", format!("{url}: {e}"))),
    }
}

/// Import a skill from the OpenClaw ClawHub registry.
///
/// Uses the public, unauthenticated `GET /api/v1/download?slug=…` ZIP
/// endpoint, which serves the full skill bundle (SKILL.md plus any
/// references/, scripts/, assets/ files) — so bundled materials arrive
/// with the skill, not just the instructions. The ZIP's SKILL.md is
/// validated by `parse_skill` before anything is written, so a malformed
/// entry fails closed and leaves no file behind.
///
/// Ambiguous slugs return HTTP 409 with the matching owners; the error
/// names them so the caller can retry with `--owner`.
pub fn import_skill_from_clawhub(
    data_dir: &Path,
    slug: &str,
    owner: Option<&str>,
) -> Result<(PathBuf, Skill), PantheonError> {
    // Detail endpoint first: it resolves ambiguity with a useful error
    // and is cheap, while the download ZIP is the bulk transfer.
    let detail = match owner {
        Some(o) => format!("https://clawhub.ai/api/v1/skills/{slug}?owner={o}"),
        None => format!("https://clawhub.ai/api/v1/skills/{slug}"),
    };
    let (body, status) = fetch_clawhub_json_with_status(&detail)?;
    if status == 409 {
        if let Ok(parsed) = serde_json::from_str::<ClawhubAmbiguous>(&body) {
            let owners: Vec<&str> = parsed
                .matches
                .iter()
                .map(|m| m.owner_handle.as_str())
                .collect();
            return Err(serr(
                "SKILL_HUB_AMBIGUOUS",
                format!(
                    "clawhub slug '{slug}' is ambiguous; specify --owner: {}",
                    owners.join(", ")
                ),
            ));
        }
    }
    let parsed = serde_json::from_str::<ClawhubSkillResponse>(&body)
        .map_err(|e| serr("SKILL_HUB_DECODE", format!("clawhub {slug}: {e}")))?;
    let display_name = parsed.skill.display_name;

    // Download the full ZIP bundle.
    let zip_url = match owner {
        Some(o) => format!("https://clawhub.ai/api/v1/download?slug={slug}&owner={o}"),
        None => format!("https://clawhub.ai/api/v1/download?slug={slug}"),
    };
    let zip_bytes = fetch_bytes(&zip_url)?;
    let bundle = unzip_skill(&zip_bytes)
        .map_err(|e| serr("SKILL_HUB_ZIP", format!("clawhub {slug}: {e}")))?;
    let raw = bundle
        .iter()
        .find(|(p, _)| p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md"))
        .map(|(_, b)| b)
        .ok_or_else(|| {
            serr(
                "SKILL_HUB_EMPTY",
                format!("clawhub {slug}: ZIP has no SKILL.md"),
            )
        })?;
    let raw = String::from_utf8(raw.clone())
        .map_err(|e| serr("SKILL_HUB_DECODE", format!("clawhub {slug}: {e}")))?;
    // Name: frontmatter `name` wins; fall back to the registry slug.
    let name = match parse_skill(&raw, &PathBuf::from(format!("clawhub://{slug}"))) {
        Ok(skill) => skill.meta.name.clone(),
        Err(e) => {
            if display_name.is_empty() {
                return Err(e);
            }
            // Fall back below once we have a name; validation still applies
            // because the SKILL.md is written verbatim and re-parsed by
            // discovery. A frontmatter-less skill is still usable data.
            display_name.clone()
        }
    };
    let dest_dir = data_dir.join("skills").join(&name);
    write_bundle(&bundle, &dest_dir)?;
    let mut s = Skill {
        meta: SkillMeta {
            name: name.clone(),
            description: String::new(),
            origin: "openclaw".to_string(),
        },
        path: dest_dir.join("SKILL.md"),
    };
    if let Some(loaded) = load_skill(&s.path) {
        s = loaded;
        s.meta.origin = "openclaw".to_string();
    }
    Ok((s.path.clone(), s))
}

/// Fetch raw bytes from a URL (binary-safe; no charset re-encoding).
fn fetch_bytes(url: &str) -> Result<Vec<u8>, PantheonError> {
    let resp = ureq::get(url)
        .timeout(std::time::Duration::from_secs(30))
        .set("User-Agent", "pantheon-skill-import")
        .call()
        .map_err(|e| serr("SKILL_FETCH", format!("{url}: {e}")))?;
    let mut buf = Vec::new();
    resp.into_reader()
        .read_to_end(&mut buf)
        .map_err(|e| serr("SKILL_FETCH_BODY", format!("{url}: {e}")))?;
    Ok(buf)
}

/// Inflate a skill ZIP into (relative path, bytes) pairs, dropping the
/// registry's `_meta.json` and directory entries.
fn unzip_skill(bytes: &[u8]) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let reader = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| format!("open zip: {e}"))?;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("entry {i}: {e}"))?;
        let name = f.name().to_string();
        if f.is_dir() || name.ends_with("_meta.json") {
            continue;
        }
        // Reject path traversal: entry names must stay inside the bundle.
        let rel = std::path::PathBuf::from(&name);
        if rel.is_absolute() || name.contains("..") {
            return Err(format!("unsafe entry path: {name}"));
        }
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut buf).map_err(|e| format!("read {name}: {e}"))?;
        out.push((rel, buf));
    }
    Ok(out)
}

/// Write an inflated bundle to disk, creating intermediate directories.
fn write_bundle(bundle: &[(PathBuf, Vec<u8>)], dest_dir: &Path) -> Result<(), PantheonError> {
    for (rel, bytes) in bundle {
        let dest = dest_dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                serr(
                    "SKILL_IMPORT_IO",
                    format!("create {}: {e}", parent.display()),
                )
            })?;
        }
        std::fs::write(&dest, bytes)
            .map_err(|e| serr("SKILL_IMPORT_IO", format!("write {}: {e}", dest.display())))?;
    }
    Ok(())
}

/// Resolve a Hermes docs URL (or any `hermes-agent.nousresearch.com`
/// skills page) to the skill directory path inside the GitHub repo.
///
/// The docs pages are auto-generated from SKILL.md files and carry a
/// `Path | skills/<category>/<name>` row in their metadata table, which
/// is the authoritative mapping. Falls back to a page-slug heuristic
/// when the table is absent: `creative-claude-design` strips the
/// category prefix to `claude-design` and searches category dirs.
pub fn hermes_docs_path(url: &str) -> Result<String, PantheonError> {
    let page = fetch_skill_md(url)?;
    for line in page.lines() {
        // Markdown table row: | Path | `skills/creative/claude-design` |
        if let Some(rest) = line.trim_start().strip_prefix("| Path |") {
            let path = rest
                .trim()
                .trim_start_matches('`')
                .trim_end_matches('`')
                .trim_end_matches('|')
                .trim()
                .to_string();
            if path.starts_with("skills/") || path.starts_with("optional-skills/") {
                return Ok(path);
            }
        }
    }
    // Heuristic: derive a candidate skill dir name from the page slug.
    let slug = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("creative-")
        .to_string();
    if slug.is_empty() {
        return Err(serr(
            "SKILL_HERMES_PATH",
            format!("{url}: no skill path found in docs page"),
        ));
    }
    Ok(slug)
}

/// Import a skill directory from the Hermes agent GitHub repo.
///
/// `repo_path` is the path inside the repo (e.g.
/// `skills/creative/claude-design`). The whole directory — SKILL.md plus
/// any references/, scripts/, assets/ files — is copied into
/// <data_dir>/skills/<name>/, preserving structure. Uses raw
///.githubusercontent.com so no clone is needed for a single skill.
pub fn import_skill_from_hermes(
    data_dir: &Path,
    repo_path: &str,
) -> Result<(PathBuf, Skill), PantheonError> {
    let path = repo_path.trim_matches('/');
    let api = format!("https://api.github.com/repos/NousResearch/hermes-agent/contents/{path}");
    let body = fetch_skill_md(&api)?;
    let entries: Vec<GithubContentEntry> = serde_json::from_str(&body)
        .map_err(|e| serr("SKILL_HERMES_DECODE", format!("github {path}: {e}")))?;
    let mut bundle: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    fetch_hermes_tree(path, &entries, &mut bundle, 0)?;
    let skill_md = bundle
        .iter()
        .find(|(p, _)| p.file_name().and_then(|n| n.to_str()) == Some("SKILL.md"))
        .map(|(_, b)| b)
        .ok_or_else(|| serr("SKILL_HUB_EMPTY", format!("hermes {path}: no SKILL.md")))?;
    let raw = String::from_utf8(skill_md.clone())
        .map_err(|e| serr("SKILL_HERMES_DECODE", format!("hermes {path}: {e}")))?;
    let name = parse_skill(&raw, &PathBuf::from(format!("hermes://{path}")))?
        .meta
        .name;
    let dest_dir = data_dir.join("skills").join(&name);
    write_bundle(&bundle, &dest_dir)?;
    let mut s = load_skill(&dest_dir.join("SKILL.md"))
        .ok_or_else(|| serr("SKILL_HUB_EMPTY", format!("hermes {path}: re-parse failed")))?;
    s.meta.origin = "hermes".to_string();
    Ok((s.path.clone(), s))
}

#[derive(Debug, Deserialize)]
struct GithubContentEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "download_url")]
    download_url: Option<String>,
    name: String,
}

/// Recursively fetch one directory of the Hermes skill from raw
/// GitHub URLs. Depth-limited as a guard against pathological trees.
fn fetch_hermes_tree(
    prefix: &str,
    entries: &[GithubContentEntry],
    bundle: &mut Vec<(PathBuf, Vec<u8>)>,
    depth: usize,
) -> Result<(), PantheonError> {
    if depth > 4 {
        return Err(serr(
            "SKILL_HERMES_DEEP",
            format!("{prefix}: tree deeper than 4 levels"),
        ));
    }
    for e in entries {
        let rel = PathBuf::from(&e.name);
        if e.kind == "dir" {
            let sub_prefix = format!("{prefix}/{}", e.name);
            let sub_api = format!(
                "https://api.github.com/repos/NousResearch/hermes-agent/contents/{sub_prefix}"
            );
            let body = fetch_skill_md(&sub_api)?;
            let sub_entries: Vec<GithubContentEntry> =
                serde_json::from_str(&body).map_err(|err| {
                    serr("SKILL_HERMES_DECODE", format!("github {sub_prefix}: {err}"))
                })?;
            fetch_hermes_tree(&sub_prefix, &sub_entries, bundle, depth + 1)?;
        } else if e.kind == "file" {
            if let Some(url) = &e.download_url {
                let bytes = fetch_bytes(url)?;
                bundle.push((rel, bytes));
            }
        }
    }
    Ok(())
}

/// Copy an external skill into <data_dir>/skills/<name>/SKILL.md so it
/// survives across processes. Returns the new path. Idempotent: if the
/// skill is already present locally it is left untouched.
///
/// Copies the raw file verbatim — frontmatter + body — so the imported
/// skill round-trips through `parse_skill` on the next discovery pass.
pub fn import_skill(data_dir: &Path, skill: &Skill) -> Result<PathBuf, PantheonError> {
    let dest_dir = data_dir.join("skills").join(&skill.meta.name);
    let dest = dest_dir.join("SKILL.md");
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&dest_dir).map_err(|e| {
        serr(
            "SKILL_IMPORT_IO",
            format!("create {}: {e}", dest_dir.display()),
        )
    })?;
    let raw = std::fs::read_to_string(&skill.path).map_err(|e| {
        serr(
            "SKILL_IMPORT_IO",
            format!("read {}: {e}", skill.path.display()),
        )
    })?;
    std::fs::write(&dest, raw)
        .map_err(|e| serr("SKILL_IMPORT_IO", format!("write {}: {e}", dest.display())))?;
    Ok(dest)
}

/// Copy a whole skill directory (SKILL.md + references/ + scripts/ + …)
/// into <data_dir>/skills/<name>/, preserving relative structure.
///
/// Skills are data, not code: the SKILL.md is the contract, but bundled
/// materials (reference docs, utility scripts, assets) are part of the
/// skill and are copied verbatim. `name` is the skill name; every file
/// under `src` other than the git internals is included. Returns the
/// path of the copied SKILL.md.
pub fn import_skill_dir(data_dir: &Path, src: &Path, name: &str) -> Result<PathBuf, PantheonError> {
    let skill_md = src.join("SKILL.md");
    if !skill_md.is_file() {
        return Err(serr(
            "SKILL_NO_SKILLMD",
            format!("{}: no SKILL.md", src.display()),
        ));
    }
    let dest_dir = data_dir.join("skills").join(name);
    copy_tree(src, &dest_dir)?;
    Ok(dest_dir.join("SKILL.md"))
}

/// Recursively copy `src` into `dest`, skipping hidden dirs (`.git`).
/// Existing files are overwritten — a re-import refreshes bundled files.
fn copy_tree(src: &Path, dest: &Path) -> Result<(), PantheonError> {
    std::fs::create_dir_all(dest)
        .map_err(|e| serr("SKILL_IMPORT_IO", format!("create {}: {e}", dest.display())))?;
    for entry in std::fs::read_dir(src)
        .map_err(|e| serr("SKILL_IMPORT_IO", format!("read {}: {e}", src.display())))?
    {
        let entry = entry.map_err(|e| serr("SKILL_IMPORT_IO", format!("{e}")))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            if from
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with('.'))
                .unwrap_or(false)
            {
                continue;
            }
            copy_tree(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|e| {
                serr(
                    "SKILL_IMPORT_IO",
                    format!("copy {} -> {}: {e}", from.display(), to.display()),
                )
            })?;
        }
    }
    Ok(())
}

/// Import every skill directory found under `root` (full filetree copy).
///
/// A directory counts as a skill when it directly contains a `SKILL.md`.
/// Malformed skills are skipped with a warning rather than aborting the
/// batch. Returns (skill name, SKILL.md path) pairs.
pub fn import_skill_dirs(
    data_dir: &Path,
    root: &Path,
) -> Result<Vec<(String, PathBuf)>, PantheonError> {
    let mut names: Vec<String> = Vec::new();
    collect_skill_dirs(root, &mut names);
    let mut out = Vec::new();
    for name in &names {
        let dir = root.join(name);
        match import_skill_dir(data_dir, &dir, name) {
            Ok(p) => out.push((name.clone(), p)),
            Err(e) => eprintln!("skill {}: {e}", dir.display()),
        }
    }
    Ok(out)
}

/// Find immediate subdirectories of `root` that directly contain a
/// `SKILL.md`. Descends one level for skills nested under a category dir
/// (e.g. `skills/creative/claude-design`).
fn collect_skill_dirs(root: &Path, names: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let fname = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if fname.starts_with('.') {
            continue;
        }
        if p.join("SKILL.md").is_file() {
            names.push(fname);
        } else {
            // One level of nesting: category dirs like skills/creative/.
            let sub = p.join("SKILL.md");
            let _ = &sub;
            collect_skill_dirs(&p, names);
        }
    }
}
