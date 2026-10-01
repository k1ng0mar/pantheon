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
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Read as _;
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

/// A skill or plugin name must be a single safe path segment: ASCII
/// alphanumerics plus `-`/`_`, 1..=64 chars. This rejects `.`, `..`,
/// `/`, absolute paths, and unicode tricks — anything that could make a
/// `root.join(name)` escape the skills/plugins directory.
pub fn valid_slug(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
}

fn bad_name(name: &str, what: &str) -> PantheonError {
    serr(
        "SKILL_BAD_NAME",
        format!("{what} {name:?}: names must match [a-zA-Z0-9_-]{{1,64}}"),
    )
}

/// Join `root/<name>` and prove (via canonicalization) the result is
/// still inside `root`. Fails closed on slug violations, symlinked
/// escapes, and I/O errors. Creates both directories.
fn contained_skill_dir(root: &Path, name: &str) -> Result<PathBuf, PantheonError> {
    if !valid_slug(name) {
        return Err(bad_name(name, "skill name"));
    }
    std::fs::create_dir_all(root)
        .map_err(|e| serr("SKILL_IMPORT_IO", format!("create {}: {e}", root.display())))?;
    let dir = root.join(name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| serr("SKILL_IMPORT_IO", format!("create {}: {e}", dir.display())))?;
    let canon_root = root.canonicalize().map_err(|e| {
        serr(
            "SKILL_IMPORT_IO",
            format!("canonicalize {}: {e}", root.display()),
        )
    })?;
    let canon_dir = dir.canonicalize().map_err(|e| {
        serr(
            "SKILL_IMPORT_IO",
            format!("canonicalize {}: {e}", dir.display()),
        )
    })?;
    if !canon_dir.starts_with(&canon_root) {
        return Err(serr(
            "SKILL_BAD_NAME",
            format!("skill dir escapes the skills root: {}", dir.display()),
        ));
    }
    Ok(dir)
}

/// Cap for any single HTTP response body during skill import.
pub const MAX_HTTP_BYTES: u64 = 10 * 1024 * 1024; // 10 MiB
/// Cap for the total inflated bytes of one imported bundle (ZIP or tree).
pub const MAX_BUNDLE_BYTES: u64 = 50 * 1024 * 1024; // 50 MiB
/// Cap for the number of entries in a skill ZIP (zip-bomb guard).
pub const MAX_ZIP_ENTRIES: usize = 1000;

/// Read a ureq response body with the shared byte cap, tolerating
/// servers that close TLS without `close_notify`
/// (see [`crate::http::read_body_capped`]). Transport failures get a
/// clean message via [`crate::http::fetch_error_message`] — no raw
/// rustls internals leak into API error bodies.
fn read_http_body(resp: ureq::Response, what: &str) -> Result<Vec<u8>, PantheonError> {
    let bytes = crate::http::read_body_capped(resp, MAX_HTTP_BYTES).map_err(|e| {
        serr(
            "SKILL_FETCH_BODY",
            crate::http::fetch_error_message(what, &e.to_string()),
        )
    })?;
    if bytes.len() as u64 > MAX_HTTP_BYTES {
        return Err(serr(
            "SKILL_FETCH_TOO_LARGE",
            format!("{what}: exceeds {} MiB cap", MAX_HTTP_BYTES / (1024 * 1024)),
        ));
    }
    Ok(bytes)
}

/// Clean user-facing message for a ureq call failure (DNS, TLS, refused).
fn fetch_call_error(what: &str, e: &ureq::Error) -> PantheonError {
    serr(
        "SKILL_FETCH",
        crate::http::fetch_error_message(what, &e.to_string()),
    )
}

/// Limits for inflating a skill ZIP. `unzip_skill` uses the defaults;
/// tests pass tighter limits via `unzip_skill_with_limits`.
#[derive(Debug, Clone, Copy)]
pub struct ZipLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
}

const DEFAULT_ZIP_LIMITS: ZipLimits = ZipLimits {
    max_entries: MAX_ZIP_ENTRIES,
    max_total_bytes: MAX_BUNDLE_BYTES,
};

/// Resolve `--sub DIR` under the cloned temp dir. Rejects absolute paths
/// and `..` components, then proves containment via canonicalization.
/// Extracted so the traversal checks are unit-testable without a clone.
fn resolve_repo_subpath(tmp: &Path, subpath: Option<&str>) -> Result<PathBuf, PantheonError> {
    let Some(s) = subpath.filter(|s| !s.is_empty()) else {
        return Ok(tmp.to_path_buf());
    };
    let p = Path::new(s);
    if p.is_absolute()
        || p.components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(serr(
            "SKILL_BAD_SUBPATH",
            format!("subpath {s:?} must be a relative path without '..'"),
        ));
    }
    let root = tmp.join(p);
    let canon_tmp = tmp.canonicalize().map_err(|e| {
        serr(
            "SKILL_REPO_TMP",
            format!("canonicalize {}: {e}", tmp.display()),
        )
    })?;
    let canon_root = root.canonicalize().map_err(|_| {
        serr(
            "SKILL_BAD_SUBPATH",
            format!("subpath {s:?} does not exist inside the cloned repo"),
        )
    })?;
    if !canon_root.starts_with(&canon_tmp) {
        return Err(serr(
            "SKILL_BAD_SUBPATH",
            format!("subpath {s:?} escapes the cloned repo"),
        ));
    }
    Ok(root)
}

/// Validate a Hermes repo path (`skills/creative/claude-design`): relative,
/// no `..` components. The path is interpolated into GitHub API URLs, so a
/// `..` here would walk the API path, and it later seeds bundle structure.
fn check_repo_path(path: &str) -> Result<String, PantheonError> {
    let p = path.trim_matches('/');
    let rel = Path::new(p);
    if p.is_empty()
        || rel.is_absolute()
        || rel
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(serr(
            "SKILL_HERMES_PATH",
            format!("unsafe repo path: {path:?}"),
        ));
    }
    Ok(p.to_string())
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
    /// Executables declared in frontmatter (`exec:`). Empty for skills
    /// that are pure instructions.
    #[serde(default)]
    pub exec: Vec<SkillExec>,
}

/// Name of the agent tool that runs skill-declared executables. Shared
/// with pantheon-runtime so Plan-mode gating and timeline attribution
/// key on one constant instead of a string literal in two crates.
pub const SKILL_EXEC_TOOL_NAME: &str = "skill_exec";

/// Default `timeout_secs` for a declared executable.
pub const SKILL_EXEC_DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Hard ceiling for `timeout_secs`: larger values are a parse error.
pub const SKILL_EXEC_MAX_TIMEOUT_SECS: u64 = 600;

/// Runtime that executes a skill's declared executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillExecRuntime {
    Shell,
    Python3,
    Node,
    Npx,
}

/// Declared side-effect class of a skill executable.
///
/// Defaults to `write` (fail closed): a skill that forgets the field is
/// treated as mutating, so Plan mode blocks it and the capability gate
/// demands `ShellExecute`. `npx` is always forced to `write` — it fetches
/// and runs remote code, so a `read` declaration on it is not honored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillExecSideEffects {
    Read,
    Write,
}

impl SkillExecSideEffects {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillExecSideEffects::Read => "read",
            SkillExecSideEffects::Write => "write",
        }
    }
}

/// One executable a skill declares in frontmatter (`exec:`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillExec {
    /// Kebab-case slug, `[a-z0-9-]{1,64}`.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Script path relative to the skill dir (shell/python3/node), or an
    /// npx package spec when `runtime: npx`.
    pub command: String,
    /// Usage hint shown to the agent (e.g. `"<youtube-url>"`).
    #[serde(default)]
    pub args: String,
    pub runtime: SkillExecRuntime,
    pub side_effects: SkillExecSideEffects,
    /// Wall-clock budget, 1..=600 seconds.
    pub timeout_secs: u64,
}

/// What the agent sees for one declared executable: the invocable
/// surface (`[{name, description, args, side_effects}]`), without the
/// script path (an implementation detail, not a prompt detail).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillExecSummary {
    pub name: String,
    pub description: String,
    pub args: String,
    pub side_effects: SkillExecSideEffects,
}

impl SkillMeta {
    /// Executables the agent can invoke via `skill_exec`, in declaration
    /// order.
    pub fn executables(&self) -> Vec<SkillExecSummary> {
        self.exec
            .iter()
            .map(|e| SkillExecSummary {
                name: e.name.clone(),
                description: e.description.clone(),
                args: e.args.clone(),
                side_effects: e.side_effects,
            })
            .collect()
    }
}

/// Executable slug: `[a-z0-9-]{1,64}`. Deliberately stricter than skill
/// slugs (lowercase only, no `_`): the name becomes part of tool-call
/// routing, so it stays lowercase kebab-case by contract.
pub fn valid_exec_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// Validate an npx package spec: `^[a-z0-9][a-z0-9._/-]*(@[a-z0-9._-]+)?$`.
///
/// Rejects URLs, scopes, and shell metacharacters. The spec is passed to
/// `npx` as a single argv element (never shell-interpolated), but npx
/// itself resolves it over the network, so the accepted shape stays
/// narrow on purpose.
pub fn valid_npx_spec(spec: &str) -> bool {
    fn name_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '/' | '-')
    }
    fn version_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-')
    }
    fn tight_segment(s: &str) -> bool {
        let mut chars = s.chars();
        let Some(first) = chars.next() else {
            return false;
        };
        (first.is_ascii_lowercase() || first.is_ascii_digit()) && chars.all(name_char)
    }
    // Optional scope: @scope/name — the canonical scoped-package form
    // (`npx -y @scope/cli`).
    let rest = match spec.strip_prefix('@') {
        Some(after_at) => match after_at.split_once('/') {
            Some((scope, name)) if tight_segment(scope) => name,
            _ => return false,
        },
        None => spec,
    };
    // Optional @version suffix (split_once on the FIRST @ would break
    // @scope — but the scope was stripped above, so the first @ here is
    // the version separator).
    let (name, version) = match rest.split_once('@') {
        Some((n, v)) => (n, Some(v)),
        None => (rest, None),
    };
    // The name may contain /subpath: every segment tight, none empty.
    if name.is_empty() || !name.split('/').all(tight_segment) {
        return false;
    }
    match version {
        None => true,
        Some(v) => !v.is_empty() && v.chars().all(version_char),
    }
}

/// Validate a script `command` for shell/python3/node: a relative path
/// with no `..`, no absolute path, no empty segments. This is the
/// parse-time string check; invocation re-proves containment via
/// canonicalization because the skill dir may be user-writable.
fn check_script_path(command: &str) -> Result<(), &'static str> {
    if command.is_empty() {
        return Err("empty command");
    }
    if command.contains('\0') {
        return Err("NUL byte in command");
    }
    if command.starts_with('/') {
        return Err("absolute path");
    }
    if command.contains('\\') {
        return Err("backslash in command");
    }
    for seg in command.split('/') {
        if seg.is_empty() {
            return Err("empty path segment");
        }
        if seg == ".." {
            return Err("'..' segment");
        }
        if seg == "." {
            return Err("'.' segment");
        }
    }
    Ok(())
}

/// One raw `exec:` entry straight from YAML. `parse_skill` converts each
/// of these with [`SkillExec::validate`], which names the skill and the
/// entry in every error — a bare serde derive on the final struct would
/// only say "unknown variant", losing which executable of which skill
/// broke.
#[derive(Debug, Deserialize)]
struct SkillExecRaw {
    name: Option<String>,
    #[serde(default)]
    description: String,
    command: Option<String>,
    #[serde(default)]
    args: String,
    #[serde(default)]
    runtime: Option<String>,
    #[serde(default)]
    side_effects: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Frontmatter shape before `exec:` entries are validated.
#[derive(Debug, Deserialize)]
struct SkillMetaRaw {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    origin: String,
    #[serde(default)]
    exec: Vec<SkillExecRaw>,
}

impl SkillExec {
    /// Validate one raw `exec:` entry. Every failure is `SKILL_BAD_EXEC`
    /// naming the skill and the entry index, so `skills doctor` points at
    /// the exact declaration.
    fn validate(skill: &str, index: usize, raw: SkillExecRaw) -> Result<Self, PantheonError> {
        let entry = format!("skill '{skill}', entry {index}");
        let name = raw.name.unwrap_or_default();
        if !valid_exec_name(&name) {
            return Err(serr(
                "SKILL_BAD_EXEC",
                format!("{entry}: name {name:?} must match [a-z0-9-]{{1,64}}"),
            ));
        }
        let at = format!("{entry} '{name}'");
        let runtime = match raw.runtime.as_deref().unwrap_or("shell") {
            "shell" => SkillExecRuntime::Shell,
            "python3" => SkillExecRuntime::Python3,
            "node" => SkillExecRuntime::Node,
            "npx" => SkillExecRuntime::Npx,
            other => {
                return Err(serr(
                    "SKILL_BAD_EXEC",
                    format!("{at}: unknown runtime {other:?} (expected shell|python3|node|npx)"),
                ));
            }
        };
        // npx fetches and runs remote code: always a write side-effect,
        // even if the declaration claims `read`.
        let side_effects = if runtime == SkillExecRuntime::Npx {
            SkillExecSideEffects::Write
        } else {
            match raw.side_effects.as_deref().unwrap_or("write") {
                "read" => SkillExecSideEffects::Read,
                "write" => SkillExecSideEffects::Write,
                other => {
                    return Err(serr(
                        "SKILL_BAD_EXEC",
                        format!("{at}: unknown side_effects {other:?} (expected read|write)"),
                    ));
                }
            }
        };
        let command = raw.command.unwrap_or_default();
        if runtime == SkillExecRuntime::Npx {
            if !valid_npx_spec(&command) {
                return Err(serr(
                    "SKILL_BAD_EXEC",
                    format!(
                        "{at}: npx command {command:?} is not a plain package spec \
                         (no URLs, no scopes, no shell metacharacters)"
                    ),
                ));
            }
        } else if let Err(reason) = check_script_path(&command) {
            return Err(serr(
                "SKILL_BAD_EXEC",
                format!("{at}: unsafe script path {command:?}: {reason}"),
            ));
        }
        let timeout_secs = raw.timeout_secs.unwrap_or(SKILL_EXEC_DEFAULT_TIMEOUT_SECS);
        if timeout_secs == 0 || timeout_secs > SKILL_EXEC_MAX_TIMEOUT_SECS {
            return Err(serr(
                "SKILL_BAD_EXEC",
                format!(
                    "{at}: timeout_secs {timeout_secs} out of range 1..={SKILL_EXEC_MAX_TIMEOUT_SECS}"
                ),
            ));
        }
        Ok(SkillExec {
            name,
            description: raw.description,
            command,
            args: raw.args,
            runtime,
            side_effects,
            timeout_secs,
        })
    }
}

/// Where a resolved executable runs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillExecTarget {
    /// Canonical script path, proven inside the skill dir.
    Script(PathBuf),
    /// Validated npx package spec (re-checked at invocation; no path).
    Npx(String),
}

/// Resolve a skill executable to its target, proving confinement.
///
/// For shell/python3/node the script path is canonicalized and must stay
/// inside the skill dir — this defeats `..` tricks that survived the
/// parse-time string check and symlinks pointing outside the dir (the
/// skill dir may be user-writable, so the check runs at invocation, not
/// just at parse). For npx the package spec is re-validated.
pub fn resolve_skill_exec_target(
    skill: &Skill,
    exec: &SkillExec,
) -> Result<SkillExecTarget, PantheonError> {
    let who = format!("skill '{}', executable '{}'", skill.meta.name, exec.name);
    if exec.runtime == SkillExecRuntime::Npx {
        if !valid_npx_spec(&exec.command) {
            return Err(serr(
                "SKILL_EXEC_NPX",
                format!("{who}: invalid npx package spec {:?}", exec.command),
            ));
        }
        return Ok(SkillExecTarget::Npx(exec.command.clone()));
    }
    // The skill dir comes from discovery metadata (`Skill.dir`), not from
    // re-deriving it here, so there is one source of truth. It is
    // canonicalized below, which also resolves the `dir` itself when the
    // skills root is a symlink.
    let dir = skill_dir(skill);
    if dir.as_os_str().is_empty() {
        return Err(serr(
            "SKILL_EXEC_DIR",
            format!("{who}: skill has no directory"),
        ));
    }
    let canon_dir = dir.canonicalize().map_err(|e| {
        serr(
            "SKILL_EXEC_DIR",
            format!("{who}: canonicalize {}: {e}", dir.display()),
        )
    })?;
    let joined = canon_dir.join(&exec.command);
    // canonicalize resolves symlinks: a symlink inside the skill dir
    // pointing outside fails the containment check below.
    let canon = joined.canonicalize().map_err(|e| {
        serr(
            "SKILL_EXEC_SCRIPT",
            format!("{who}: canonicalize {}: {e}", joined.display()),
        )
    })?;
    if !canon.starts_with(&canon_dir) {
        return Err(serr(
            "SKILL_EXEC_ESCAPE",
            format!("{who}: script escapes the skill dir"),
        ));
    }
    if !canon.is_file() {
        return Err(serr(
            "SKILL_EXEC_SCRIPT",
            format!("{who}: not a file: {}", canon.display()),
        ));
    }
    Ok(SkillExecTarget::Script(canon))
}

/// Find a skill by name and one of its declared executables by name.
pub fn find_skill_exec<'a>(
    skills: &'a [Skill],
    skill: &str,
    name: &str,
) -> Result<(&'a Skill, &'a SkillExec), PantheonError> {
    let s = skills
        .iter()
        .find(|s| s.meta.name == skill)
        .ok_or_else(|| serr("SKILL_UNKNOWN", format!("no skill named '{skill}'")))?;
    let e = s.meta.exec.iter().find(|e| e.name == name).ok_or_else(|| {
        serr(
            "SKILL_UNKNOWN_EXEC",
            format!("skill '{skill}' has no executable named '{name}'"),
        )
    })?;
    Ok((s, e))
}

/// Parse `skill_exec` tool args: `{skill, name, args?}`. `args` defaults
/// to `[]` and must be an array of strings when present.
pub fn parse_skill_exec_args(args: &str) -> Result<(String, String, Vec<String>), PantheonError> {
    let v: serde_json::Value = if args.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(args)
            .map_err(|e| serr("TOOL_BAD_ARGS", format!("invalid JSON args: {e}")))?
    };
    let str_arg = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| serr("TOOL_BAD_ARGS", format!("missing string arg '{key}'")))
    };
    let skill = str_arg("skill")?;
    let name = str_arg("name")?;
    let exec_args = match v.get("args") {
        None => Vec::new(),
        Some(a) => a
            .as_array()
            .ok_or_else(|| {
                serr(
                    "TOOL_BAD_ARGS",
                    "'args' must be an array of strings".to_string(),
                )
            })?
            .iter()
            .map(|x| {
                x.as_str().map(str::to_string).ok_or_else(|| {
                    serr(
                        "TOOL_BAD_ARGS",
                        "'args' must be an array of strings".to_string(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
    };
    Ok((skill, name, exec_args))
}

/// True when a `skill_exec` call must be treated as mutating for
/// Plan-mode gating: the named executable declares `side_effects: write`,
/// or the skill/executable cannot be resolved (fail closed — an
/// unresolvable call errors at execution, but Plan mode must not wave it
/// through as "read-only" first).
pub fn skill_exec_call_is_mutating(skills: &[Skill], args: &str) -> bool {
    let Ok((skill, name, _)) = parse_skill_exec_args(args) else {
        return true;
    };
    match find_skill_exec(skills, &skill, &name) {
        Ok((_, exec)) => exec.side_effects == SkillExecSideEffects::Write,
        Err(_) => true,
    }
}

/// The `skill` argument of a `skill_exec` call, for timeline attribution.
pub fn skill_exec_skill_arg(args: &str) -> Option<String> {
    parse_skill_exec_args(args).ok().map(|(s, _, _)| s)
}

/// One discovered skill: metadata plus where its body lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub meta: SkillMeta,
    /// Absolute path to the SKILL.md file.
    pub path: PathBuf,
    /// Absolute path to the skill's directory (parent of SKILL.md).
    ///
    /// Third-party skills invoke bundled scripts through prose
    /// (`${CLAUDE_SKILL_DIR}/scripts/...`); the agent expands the
    /// placeholder to this dir and runs them via the regular exec tool.
    /// `skill_exec` only covers `exec:`-declared entries.
    #[serde(default)]
    pub dir: PathBuf,
}

/// Absolute directory of a skill: the `dir` discovery field, falling back
/// to the SKILL.md parent for metadata deserialized before the field
/// existed.
pub fn skill_dir(skill: &Skill) -> PathBuf {
    if skill.dir.as_os_str().is_empty() {
        skill
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    } else {
        skill.dir.clone()
    }
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
    let raw_meta: SkillMetaRaw = serde_yaml::from_str(yaml)
        .map_err(|e| serr("SKILL_BAD_FRONTMATTER", format!("{}: {e}", path.display())))?;
    if raw_meta.name.trim().is_empty() {
        return Err(serr(
            "SKILL_NO_NAME",
            format!("{}: frontmatter has no name", path.display()),
        ));
    }
    // Validate every `exec:` entry with the skill name in scope, so each
    // rejection names the skill and the entry. A single bad entry fails
    // the whole skill: a half-declared executable is worse than none.
    let mut exec = Vec::with_capacity(raw_meta.exec.len());
    for (i, e) in raw_meta.exec.into_iter().enumerate() {
        exec.push(SkillExec::validate(&raw_meta.name, i, e)?);
    }
    let mut meta = SkillMeta {
        name: raw_meta.name,
        description: raw_meta.description,
        origin: raw_meta.origin,
        exec,
    };
    if meta.origin.is_empty() {
        meta.origin = "user".into();
    }
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    Ok(Skill {
        meta,
        path: path.to_path_buf(),
        dir,
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

/// Disabled-skill registry: `<data_dir>/skills/disabled.json`, a JSON
/// array of skill names. Shared by the dashboard toggle and the session
/// loader — a disabled skill is not registered as a tool. Reads tolerate
/// a missing or malformed file (nothing disabled) rather than failing
/// discovery; a failed *write* is an error, because a toggle that did
/// not persist would be a lie.
pub fn disabled_skill_names(data_dir: &Path) -> Vec<String> {
    let path = data_dir.join("skills").join("disabled.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<Vec<String>>(&text).unwrap_or_default()
}

/// Set (or clear) a skill's disabled flag. Atomic: temp file + rename in
/// the same directory.
pub fn set_skill_disabled(
    data_dir: &Path,
    name: &str,
    disabled: bool,
) -> Result<(), PantheonError> {
    if name.trim().is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(serr("SKILL_NAME", format!("unsafe skill name: {name:?}")));
    }
    let dir = data_dir.join("skills");
    std::fs::create_dir_all(&dir).map_err(|e| {
        serr(
            "SKILL_DISABLED_IO",
            format!("create {}: {e}", dir.display()),
        )
    })?;
    let mut names = disabled_skill_names(data_dir);
    if disabled {
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    } else {
        names.retain(|n| n != name);
    }
    names.sort();
    let tmp = dir.join("disabled.json.tmp");
    let path = dir.join("disabled.json");
    std::fs::write(
        &tmp,
        serde_json::to_string_pretty(&names).unwrap_or_default(),
    )
    .map_err(|e| serr("SKILL_DISABLED_IO", format!("write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        serr(
            "SKILL_DISABLED_IO",
            format!("rename {}: {e}", path.display()),
        )
    })?;
    Ok(())
}

/// Like `discover_skills_ext`, but skips skills in the disabled registry.
/// Sessions load through this, so the dashboard toggle genuinely removes
/// the skill from the model's toolset.
pub fn discover_skills_enabled(
    data_dir: &Path,
    project_root: &Path,
    extra_roots: &[PathBuf],
) -> Vec<Skill> {
    let disabled = disabled_skill_names(data_dir);
    discover_skills_ext(data_dir, project_root, extra_roots)
        .into_iter()
        .filter(|s| !disabled.iter().any(|d| d == &s.meta.name))
        .collect()
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

/// Fetch a SKILL.md body, trying each candidate URL in order. A 404 on a
/// bare-repo URL falls through to the next candidate rather than failing.
fn fetch_skill_md(url: &str) -> Result<String, PantheonError> {
    let candidates = candidate_urls(url);
    let mut last_err: Option<PantheonError> = None;
    for candidate in &candidates {
        // ureq first, curl fallback on transport failure (see http::get).
        match crate::http::get(
            candidate,
            std::time::Duration::from_secs(15),
            MAX_HTTP_BYTES,
            "pantheon-skill-import",
        ) {
            Ok(fetched) => {
                let body = String::from_utf8(fetched.body)
                    .map_err(|e| serr("SKILL_FETCH_BODY", format!("{url}: {e}")))?;
                return Ok(body);
            }
            Err(crate::http::GetError::Status(code)) => {
                last_err = Some(serr("SKILL_FETCH_HTTP", format!("{url}: HTTP {code}")));
            }
            Err(crate::http::GetError::Transport(msg)) => {
                last_err = Some(serr("SKILL_FETCH", msg));
            }
            Err(crate::http::GetError::TooLarge) => {
                last_err = Some(serr(
                    "SKILL_FETCH_TOO_LARGE",
                    format!("{url}: response exceeds the fetch cap"),
                ));
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
    // Slug-validated + canonical containment: a hostile frontmatter
    // `name: ../../evil` must not escape <data_dir>/skills.
    let dest_dir = contained_skill_dir(&data_dir.join("skills"), &name)?;
    let dest = dest_dir.join("SKILL.md");
    let mut s = parsed;
    if !dest.exists() {
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
    let root = resolve_repo_subpath(&tmp, subpath)?;
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
            let bytes = read_http_body(resp, url)?;
            let body = String::from_utf8(bytes)
                .map_err(|e| serr("SKILL_FETCH_BODY", format!("{url}: {e}")))?;
            Ok((body, status))
        }
        Err(ureq::Error::Status(code, resp)) => {
            let bytes = read_http_body(resp, url).unwrap_or_default();
            let body = String::from_utf8(bytes).unwrap_or_default();
            Ok((body, code))
        }
        Err(e) => Err(fetch_call_error(url, &e)),
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
    // The slug and owner are interpolated into API URLs; validate them as
    // slugs so a crafted value cannot walk the URL path or the dest dir.
    if !valid_slug(slug) {
        return Err(bad_name(slug, "clawhub slug"));
    }
    if let Some(o) = owner {
        if !valid_slug(o) {
            return Err(bad_name(o, "clawhub owner"));
        }
    }
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
    let dest_dir = contained_skill_dir(&data_dir.join("skills"), &name)?;
    write_bundle(&bundle, &dest_dir)?;
    let mut s = Skill {
        meta: SkillMeta {
            name: name.clone(),
            description: String::new(),
            origin: "openclaw".to_string(),
            exec: Vec::new(),
        },
        path: dest_dir.join("SKILL.md"),
        dir: dest_dir.clone(),
    };
    if let Some(loaded) = load_skill(&s.path) {
        s = loaded;
        s.meta.origin = "openclaw".to_string();
    }
    Ok((s.path.clone(), s))
}

/// Fetch raw bytes from a URL (binary-safe; no charset re-encoding).
/// Capped at MAX_HTTP_BYTES so a hostile endpoint cannot OOM the importer.
fn fetch_bytes(url: &str) -> Result<Vec<u8>, PantheonError> {
    match crate::http::get(
        url,
        std::time::Duration::from_secs(30),
        MAX_HTTP_BYTES,
        "pantheon-skill-import",
    ) {
        Ok(fetched) => Ok(fetched.body),
        Err(crate::http::GetError::Status(code)) => {
            Err(serr("SKILL_FETCH_HTTP", format!("{url}: HTTP {code}")))
        }
        Err(crate::http::GetError::Transport(msg)) => Err(serr("SKILL_FETCH", msg)),
        Err(crate::http::GetError::TooLarge) => Err(serr(
            "SKILL_FETCH_TOO_LARGE",
            format!("{url}: response exceeds the fetch cap"),
        )),
    }
}

/// Inflate a skill ZIP into (relative path, bytes) pairs, dropping the
/// registry's `_meta.json` and directory entries.
///
/// Fail-closed on:
/// - entry paths that are absolute or contain `..` (traversal),
/// - more than `limits.max_entries` entries,
/// - total inflated bytes past `limits.max_total_bytes` (zip-bomb guard;
///   the bound is enforced on actual bytes read, not the header's claim).
fn unzip_skill(bytes: &[u8]) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    unzip_skill_with_limits(bytes, &DEFAULT_ZIP_LIMITS)
}

fn unzip_skill_with_limits(
    bytes: &[u8],
    limits: &ZipLimits,
) -> Result<Vec<(PathBuf, Vec<u8>)>, String> {
    let reader = std::io::Cursor::new(bytes);
    let mut zip = zip::ZipArchive::new(reader).map_err(|e| format!("open zip: {e}"))?;
    if zip.len() > limits.max_entries {
        return Err(format!(
            "zip has {} entries (max {})",
            zip.len(),
            limits.max_entries
        ));
    }
    let mut total: u64 = 0;
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).map_err(|e| format!("entry {i}: {e}"))?;
        let name = f.name().to_string();
        if f.is_dir() || name.ends_with("_meta.json") {
            continue;
        }
        // Reject path traversal: entry names must stay inside the bundle.
        let rel = std::path::PathBuf::from(&name);
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("unsafe entry path: {name}"));
        }
        // Zip-bomb guard: cap total inflated bytes on actual bytes read.
        let remaining = limits.max_total_bytes.saturating_sub(total);
        let mut buf = Vec::new();
        let n = std::io::Read::take(&mut f, remaining + 1)
            .read_to_end(&mut buf)
            .map_err(|e| format!("read {name}: {e}"))?;
        total = total.saturating_add(n as u64);
        if total > limits.max_total_bytes {
            return Err(format!(
                "zip inflates past {} MiB cap",
                limits.max_total_bytes / (1024 * 1024)
            ));
        }
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
    // Validate before it is interpolated into API URLs or seeds paths.
    let path = check_repo_path(repo_path)?;
    let api = format!("https://api.github.com/repos/NousResearch/hermes-agent/contents/{path}");
    let body = fetch_skill_md(&api)?;
    let entries: Vec<GithubContentEntry> = serde_json::from_str(&body)
        .map_err(|e| serr("SKILL_HERMES_DECODE", format!("github {path}: {e}")))?;
    let mut bundle: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    fetch_hermes_tree(&path, &entries, &mut bundle, &mut total, 0)?;
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
    let dest_dir = contained_skill_dir(&data_dir.join("skills"), &name)?;
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
/// GitHub URLs. Depth-limited as a guard against pathological trees, and
/// total bytes are capped at MAX_BUNDLE_BYTES.
fn fetch_hermes_tree(
    prefix: &str,
    entries: &[GithubContentEntry],
    bundle: &mut Vec<(PathBuf, Vec<u8>)>,
    total: &mut u64,
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
            fetch_hermes_tree(&sub_prefix, &sub_entries, bundle, total, depth + 1)?;
        } else if e.kind == "file" {
            if let Some(url) = &e.download_url {
                let bytes = fetch_bytes(url)?;
                *total = total.saturating_add(bytes.len() as u64);
                if *total > MAX_BUNDLE_BYTES {
                    return Err(serr(
                        "SKILL_FETCH_TOO_LARGE",
                        format!(
                            "{prefix}: bundle exceeds {} MiB cap",
                            MAX_BUNDLE_BYTES / (1024 * 1024)
                        ),
                    ));
                }
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
    let dest_dir = contained_skill_dir(&data_dir.join("skills"), &skill.meta.name)?;
    let dest = dest_dir.join("SKILL.md");
    if dest.exists() {
        return Ok(dest);
    }
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
    // Slug-validated + canonical containment: `name` comes from directory
    // names (repo clones, user input) and must not escape skills/.
    let dest_dir = contained_skill_dir(&data_dir.join("skills"), name)?;
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
    let found = collect_skill_dirs(root);
    // Group by leaf dir name. On collision the canonical plugin layout
    // (`plugin/skills/<name>`) keeps the bare name; the rest get stable
    // path-derived names so nothing silently overwrites anything else.
    let mut by_leaf: HashMap<String, Vec<&PathBuf>> = HashMap::new();
    for rel in &found {
        let leaf = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        by_leaf.entry(leaf).or_default().push(rel);
    }
    let mut out = Vec::new();
    for rel in &found {
        let leaf = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let group = &by_leaf[&leaf];
        let name = if group.len() == 1 || pick_canonical(group) == rel {
            leaf
        } else {
            path_derived_name(rel)
        };
        let dir = root.join(rel);
        match import_skill_dir(data_dir, &dir, &name) {
            Ok(p) => out.push((name, p)),
            Err(e) => eprintln!("skill {}: {e}", dir.display()),
        }
    }
    Ok(out)
}

/// Pick which colliding skill keeps the bare leaf name: the canonical
/// plugin layout (`plugin/skills/<name>`) wins; ties break by shortest
/// path, then lexicographic. Deterministic across re-imports.
fn pick_canonical<'a>(group: &'a [&'a PathBuf]) -> &'a PathBuf {
    fn score(rel: &PathBuf) -> (bool, usize, String) {
        let comps: Vec<String> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let is_plugin = comps
            .windows(2)
            .any(|w| w[0] == "plugin" && w[1] == "skills");
        (!is_plugin, comps.len(), comps.join("/"))
    }
    group
        .iter()
        .min_by(|a, b| score(a).cmp(&score(b)))
        .expect("non-empty group")
}

/// Max recursion depth when walking a repo for skills. Covers real-world
/// layouts (`plugin/skills/<name>`, `.github/skills/<name>`,
/// `.agents/skills/<name>`) with headroom while bounding pathological
/// trees.
const MAX_SKILL_WALK_DEPTH: usize = 6;

/// Find every directory under `root` that directly contains a `SKILL.md`,
/// as paths relative to `root`, sorted for determinism.
///
/// Recurses (depth-capped) so plugin-style layouts are found. Never
/// follows symlinks — a crafted repo can neither pull the walker outside
/// the clone nor loop it. Skips `.git` (a clone's object store is never a
/// skill) but descends into other dot-dirs (`.github`, `.agents`).
/// `SKILL.md` itself must be a real file, not a symlink.
fn collect_skill_dirs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_skill_dirs_into(root, Path::new(""), &mut out, 0);
    out.sort();
    out
}

fn collect_skill_dirs_into(root: &Path, rel: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > MAX_SKILL_WALK_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(root.join(rel)) else {
        return;
    };
    for e in entries.flatten() {
        // file_type() does not follow symlinks.
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let fname = e.file_name();
        if fname.to_string_lossy() == ".git" {
            continue;
        }
        let child_rel = rel.join(&fname);
        let child_abs = root.join(&child_rel);
        let skill_md_is_file = std::fs::symlink_metadata(child_abs.join("SKILL.md"))
            .map(|m| m.file_type().is_file())
            .unwrap_or(false);
        if skill_md_is_file {
            out.push(child_rel);
        } else {
            collect_skill_dirs_into(root, &child_rel, out, depth + 1);
        }
    }
}

/// Fallback skill name from a relative path (`plugin/skills/impeccable`
/// → `plugin-skills-impeccable`): deterministic across re-imports, so a
/// re-import refreshes instead of duplicating. Components are
/// slug-sanitized to satisfy [`valid_slug`].
fn path_derived_name(rel: &Path) -> String {
    let mut parts = Vec::new();
    for c in rel.components() {
        let s = c.as_os_str().to_string_lossy().to_lowercase();
        let mut clean = String::new();
        for ch in s.chars() {
            if ch.is_ascii_alphanumeric() {
                clean.push(ch);
            } else if !clean.ends_with('-') && !clean.is_empty() {
                clean.push('-');
            }
        }
        let clean = clean.trim_matches('-').to_string();
        if !clean.is_empty() {
            parts.push(clean);
        }
    }
    let mut name = parts.join("-");
    if name.is_empty() {
        name = "skill".to_string();
    }
    while name.len() > 64 {
        name.pop();
    }
    name.trim_matches('-').to_string()
}
