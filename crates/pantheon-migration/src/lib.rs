//! Migration (spec section 23): detect -> analyze -> plan -> approval ->
//! backup -> apply -> validate.
//!
//! The pipeline is end to end. `detect`/`analyze`/`plan` are read-only on the
//! source side and never write anything; `apply` is the only writer, it backs
//! up every target it is about to overwrite first, and `validate` re-reads
//! what landed so a partial import is loud rather than silent.
//!
//! Two invariants hold across every source:
//!
//! 1. **Unmappable content is archived, never dropped.** Every detected item
//!    appears in the plan with either an import target or an archive reason.
//! 2. **Credentials are never copied.** A path that looks like a secret is
//!    classified [`ItemKind::Secret`], which is not importable and is excluded
//!    from the backup set. Archiving must never become a credential exfil path.

use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod apply;
mod carry;
mod index;
mod providers;

pub use apply::{
    apply, apply_with, apply_with_budgets, backup, backup_with_budgets, validate, ApplyReport,
    ApplyStatus, BackupManifest, BudgetUsage, StageBudgets, Staged, StagedItem, ValidateReport,
    ValidateStatus,
};
pub use carry::{
    classify_credential, count_jsonl_records, credential_manifest, looks_secret, pantheon_env_path,
    parse_env_names, parse_env_values, parse_hermes_mcp, parse_mcp_json, read_dotenv,
    read_mcp_declarations, transcript_format, write_credential_manifest, write_mcp_declaration,
    write_session_import, write_session_import_with_budget, CredentialManifest, CredentialMapping,
    CredentialTarget, EnvEntry, McpDeclaration, McpServer, SessionImport,
};
pub use index::{
    ensure_sessions_indexed, index_quarantine, parse_transcript, quarantine_dir, ImportedChunk,
    IndexReport,
};
pub use providers::{
    catalog_key_envs, merge_into_config, merge_into_live_config, parse_hermes_providers,
    provider_sidecar_path, read_providers, reconcile_key, reconcile_keys, render_custom_providers,
    summarise, write_provider_sidecar, CustomProvider, KeyMatch, KeyReport,
};

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub(crate) fn merr(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Runtime,
        false,
        cause,
        remediation,
        "see `pantheon migrate <source> --json` for the full plan",
    )
}

/// Which system we are importing from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SourceKind {
    Hermes,
    OpenClaw,
    Omp,
    ClaudeCode,
}

impl SourceKind {
    pub fn name(&self) -> &'static str {
        match self {
            SourceKind::Hermes => "hermes",
            SourceKind::OpenClaw => "openclaw",
            SourceKind::Omp => "omp",
            SourceKind::ClaudeCode => "claude",
        }
    }

    /// Every source the CLI accepts, in help order.
    pub fn all() -> [SourceKind; 4] {
        [
            SourceKind::Hermes,
            SourceKind::OpenClaw,
            SourceKind::Omp,
            SourceKind::ClaudeCode,
        ]
    }

    /// Parse a user-supplied source name. `omp` also answers to `oh-my-pi`
    /// and `pi` because that is what people type.
    pub fn parse(s: &str) -> Option<SourceKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hermes" => Some(SourceKind::Hermes),
            "openclaw" => Some(SourceKind::OpenClaw),
            "omp" | "oh-my-pi" | "pi" => Some(SourceKind::Omp),
            "claude" | "claude-code" | "claudecode" => Some(SourceKind::ClaudeCode),
            _ => None,
        }
    }

    /// Claude Code's home directory name.
    pub fn claude_dir_name() -> &'static str {
        ".claude"
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// Migration categories (user-facing filter groups)
// ---------------------------------------------------------------------------

/// User-facing migration categories that group item kinds into selectable
/// buckets. Each category maps to one or more [`ItemKind`]s, so a user can
/// say "migrate sessions and skills but not config" without knowing the
/// internal kind names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationCategory {
    /// Session transcripts (JSONL conversation logs).
    Sessions,
    /// Portable skills (SKILL.md directories).
    Skills,
    /// Agent identity files (SOUL.md, AGENTS.md, profile.yaml).
    Identity,
    /// Memory files (MEMORY.md, USER.md).
    Memory,
    /// Runtime plugins/extensions.
    Plugins,
    /// Provider/model endpoint configuration.
    Config,
    /// Credentials (API keys, tokens — names only, values carried into .env).
    Credentials,
    /// MCP server declarations.
    Mcp,
    /// Scheduled jobs.
    Schedules,
    /// Agent definitions (subagent markdown files).
    Agents,
    /// Rules and context files.
    Rules,
    /// Slash commands.
    Commands,
    /// Reusable prompt files.
    Prompts,
}

impl MigrationCategory {
    pub fn name(&self) -> &'static str {
        match self {
            MigrationCategory::Sessions => "sessions",
            MigrationCategory::Skills => "skills",
            MigrationCategory::Identity => "identity",
            MigrationCategory::Memory => "memory",
            MigrationCategory::Plugins => "plugins",
            MigrationCategory::Config => "config",
            MigrationCategory::Credentials => "credentials",
            MigrationCategory::Mcp => "mcp",
            MigrationCategory::Schedules => "schedules",
            MigrationCategory::Agents => "agents",
            MigrationCategory::Rules => "rules",
            MigrationCategory::Commands => "commands",
            MigrationCategory::Prompts => "prompts",
        }
    }

    /// The item kinds this category covers.
    pub fn kinds(&self) -> &'static [ItemKind] {
        match self {
            MigrationCategory::Sessions => &[ItemKind::Session],
            MigrationCategory::Skills => &[ItemKind::Skill],
            MigrationCategory::Identity => &[ItemKind::Persona],
            MigrationCategory::Memory => &[ItemKind::Memory],
            MigrationCategory::Plugins => &[ItemKind::Extension],
            MigrationCategory::Config => &[ItemKind::Provider],
            MigrationCategory::Credentials => &[ItemKind::Credentials],
            MigrationCategory::Mcp => &[ItemKind::Mcp],
            MigrationCategory::Schedules => &[ItemKind::Schedule],
            MigrationCategory::Agents => &[ItemKind::Agent],
            MigrationCategory::Rules => &[ItemKind::Rule],
            MigrationCategory::Commands => &[ItemKind::Command],
            MigrationCategory::Prompts => &[ItemKind::Prompt],
        }
    }

    /// Every category, in help order.
    pub fn all() -> &'static [MigrationCategory] {
        &[
            MigrationCategory::Sessions,
            MigrationCategory::Skills,
            MigrationCategory::Identity,
            MigrationCategory::Memory,
            MigrationCategory::Plugins,
            MigrationCategory::Config,
            MigrationCategory::Credentials,
            MigrationCategory::Mcp,
            MigrationCategory::Schedules,
            MigrationCategory::Agents,
            MigrationCategory::Rules,
            MigrationCategory::Commands,
            MigrationCategory::Prompts,
        ]
    }

    /// Parse a category name (case-insensitive).
    pub fn parse(s: &str) -> Option<MigrationCategory> {
        let s = s.trim().to_ascii_lowercase();
        Self::all().iter().copied().find(|c| c.name() == s)
    }
}

/// A migration filter: which categories to include. All categories are
/// enabled by default; the CLI's `--categories` flag selects a subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFilter {
    pub categories: Vec<MigrationCategory>,
}

impl MigrationFilter {
    /// All categories enabled (the default).
    pub fn all() -> Self {
        Self {
            categories: MigrationCategory::all().to_vec(),
        }
    }

    /// Only the named categories enabled.
    pub fn only(categories: Vec<MigrationCategory>) -> Self {
        Self { categories }
    }

    /// Check if an item kind passes this filter.
    pub fn allows(&self, kind: ItemKind) -> bool {
        self.categories.iter().any(|c| c.kinds().contains(&kind))
    }

    /// Apply this filter to a plan, returning a new plan with only the
    /// selected categories' items. Items that don't match any category
    /// (Secret, Opaque) are always excluded.
    pub fn apply(&self, plan: &MigrationPlan) -> MigrationPlan {
        MigrationPlan {
            source: plan.source.clone(),
            root: plan.root.clone(),
            source_version: plan.source_version.clone(),
            items: plan
                .items
                .iter()
                .filter(|i| self.allows(i.kind))
                .cloned()
                .collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Item classification
// ---------------------------------------------------------------------------

/// What kind of thing a detected item is. Drives both the import target and
/// the validate step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// A `SKILL.md` directory. Tier 1, portable, imports verbatim.
    Skill,
    /// A subagent definition (`<name>.md` with frontmatter).
    Agent,
    /// A rule / context file.
    Rule,
    /// A slash command.
    Command,
    /// A reusable prompt file.
    Prompt,
    /// A runtime-code plugin (Hermes `plugin.yaml` + `__init__.py`, OMP
    /// extension package). Tier 2.
    Extension,
    /// Agent persona / identity (Hermes `SOUL.md`, `profile.yaml`).
    Persona,
    /// A memory file (`MEMORY.md`, `USER.md`) that lands in the memory plane.
    Memory,
    /// Provider / model endpoint configuration. Archived by default: it
    /// routinely carries an inline API key, and Pantheon keeps its own
    /// provider registry in `<data_dir>/.env`.
    Provider,
    /// An MCP server definition.
    Mcp,
    /// A scheduled job.
    Schedule,
    /// A gateway / messaging channel definition.
    Channel,
    /// Session transcripts. Imported to a quarantine path with a manifest,
    /// so they can be indexed into `session_search` deliberately.
    Session,
    /// A *declaration* of credential names the source expects, with the
    /// Pantheon target for each. Names only; no value is ever read or copied.
    Credentials,
    /// A credential, token, or key store. **Never copied, never archived.**
    Secret,
    /// Recognised as belonging to the source but with no Pantheon
    /// equivalent. Archived with a reason.
    Opaque,
}

impl ItemKind {
    pub fn name(&self) -> &'static str {
        match self {
            ItemKind::Skill => "skill",
            ItemKind::Agent => "agent",
            ItemKind::Rule => "rule",
            ItemKind::Command => "command",
            ItemKind::Prompt => "prompt",
            ItemKind::Extension => "extension",
            ItemKind::Persona => "persona",
            ItemKind::Memory => "memory",
            ItemKind::Provider => "provider",
            ItemKind::Mcp => "mcp",
            ItemKind::Schedule => "schedule",
            ItemKind::Channel => "channel",
            ItemKind::Session => "session",
            ItemKind::Credentials => "credentials",
            ItemKind::Secret => "secret",
            ItemKind::Opaque => "opaque",
        }
    }

    /// Subdirectory under the data dir that imports of this kind land in.
    /// `None` means the item has no filesystem target (memory-plane only).
    pub fn target_dir(&self) -> Option<&'static str> {
        match self {
            ItemKind::Skill => Some("skills"),
            ItemKind::Agent => Some("agents"),
            ItemKind::Rule => Some("rules"),
            ItemKind::Command => Some("commands"),
            ItemKind::Prompt => Some("prompts"),
            ItemKind::Extension => Some("extensions"),
            ItemKind::Provider => Some("providers"),
            // (a reviewable sidecar; merging into config.toml is opt-in)
            ItemKind::Mcp => Some("mcp"),
            ItemKind::Schedule => Some("schedules"),
            ItemKind::Channel => Some("channels"),
            ItemKind::Session => Some("imported-sessions"),
            ItemKind::Credentials => Some("credentials"),
            // Persona / Memory go through the memory plane, not the filesystem.
            ItemKind::Persona | ItemKind::Memory => None,
            ItemKind::Secret | ItemKind::Opaque => None,
        }
    }

    /// Item kinds that are safe to write to disk. Secrets never are.
    pub fn is_importable(&self) -> bool {
        !matches!(self, ItemKind::Secret)
    }
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// Target layout
// ---------------------------------------------------------------------------

/// Where imports land. Mirrors the real read paths the runtime uses, so an
/// imported item is discoverable on the next session without extra wiring.
#[derive(Debug, Clone)]
pub struct Targets {
    pub data_dir: PathBuf,
    pub ext_dir: PathBuf,
}

impl Targets {
    pub fn new(data_dir: PathBuf, ext_dir: PathBuf) -> Self {
        Self { data_dir, ext_dir }
    }

    /// Default layout: skills under `<data_dir>/skills`, extensions under
    /// `PANTHEON_EXT_DIR` (or `<data_dir>/extensions`), matching
    /// `pantheon-tui`'s `data_dir()` / `ext_dir()`.
    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        let ext = std::env::var("PANTHEON_EXT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("extensions"));
        Self {
            data_dir,
            ext_dir: ext,
        }
    }

    /// Resolve the destination for one item kind. `None` for kinds with no
    /// filesystem target.
    pub fn dir_for(&self, kind: ItemKind) -> Option<PathBuf> {
        // Extensions have their own dir so PANTHEON_EXT_DIR keeps working.
        if kind == ItemKind::Extension {
            return Some(self.ext_dir.clone());
        }
        kind.target_dir().map(|d| self.data_dir.join(d))
    }

    /// Backups live under the data dir so they travel with the install.
    pub fn backup_root(&self) -> PathBuf {
        self.data_dir.join("migrate-backups")
    }
}

// ---------------------------------------------------------------------------
// Plan data types
// ---------------------------------------------------------------------------

/// Provenance carried by everything imported (spec section 23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    pub source_path: String,
    pub source_version: Option<String>,
    pub imported_at_ms: i64,
}

/// One discovered item on the source side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detected {
    pub kind: ItemKind,
    pub path: String,
    pub mappable: bool,
    pub note: String,
}

/// What the pipeline would do with one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Action {
    Import {
        target: String,
    },
    Archive {
        reason: String,
    },
    /// A credential. Reported, never written anywhere.
    Skip {
        reason: String,
    },
}

/// What a dry-run would do, item by item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub kind: ItemKind,
    pub path: String,
    pub action: Action,
    /// Why this item is treated the way it is. Carried from detection so the
    /// plan explains itself for imports as well as archives.
    pub note: String,
}

impl PlanItem {
    /// The import destination, if this item is an import.
    pub fn target(&self) -> Option<&str> {
        match &self.action {
            Action::Import { target } => Some(target),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub source: String,
    pub root: String,
    pub source_version: Option<String>,
    pub items: Vec<PlanItem>,
}

impl MigrationPlan {
    pub fn imports(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.action, Action::Import { .. }))
            .count()
    }
    pub fn archived(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.action, Action::Archive { .. }))
            .count()
    }
    pub fn skipped(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i.action, Action::Skip { .. }))
            .count()
    }
    /// Import items of one kind only.
    pub fn imports_of(&self, kind: ItemKind) -> Vec<&PlanItem> {
        self.items
            .iter()
            .filter(|i| i.kind == kind && matches!(i.action, Action::Import { .. }))
            .collect()
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Secret policy
// ---------------------------------------------------------------------------

/// Filenames that are credentials wherever they appear, in any of the three
/// source layouts. Matched case-insensitively on the file name.
const SECRET_NAMES: &[&str] = &[
    ".env",
    "auth.json",
    "credentials",
    "credentials.json",
    "google_client_secret.json",
    "google_token.json",
    "broker.token",
    ".room-link-grant-secret",
    "nous_auth.json",
    "service-account.json",
    "secrets.json",
    "identity.json",
    ".netrc",
    "id_rsa",
    "id_ed25519",
];

/// Extensions that are key stores or token files.
const SECRET_EXTS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore"];

/// A file name that is a credential, or a state file that holds credentials in
/// a database we do not read.
pub fn is_secret_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if SECRET_NAMES.iter().any(|s| *s == name) {
        return true;
    }
    if let Some(ext) = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
    {
        // `extension()` has no leading dot; SECRET_EXTS entries do.
        let dotted = format!(".{ext}");
        if SECRET_EXTS.iter().any(|s| *s == dotted) {
            return true;
        }
    }
    // Credential-bearing state databases. We never open these, and a copy
    // would carry the keys with it.
    matches!(name.as_str(), "agent.db" | "state.db" | "auth.db")
        || name.ends_with("-wal")
        || name.ends_with("-shm")
}

/// True when a config file is known to embed an inline API key. We classify
/// the whole file rather than parsing it, because the point is to never copy
/// a line we have not read.
pub fn config_may_hold_keys(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    // models.yml / config.yaml are the two that routinely carry `api_key:`
    // or `password_hash:` inline in the wild.
    matches!(
        name.as_str(),
        "models.yml" | "models.yaml" | "config.yaml" | "config.yml"
    )
}

fn detected_secret(path: &Path, what: &str) -> Detected {
    Detected {
        kind: ItemKind::Secret,
        path: path.to_string_lossy().to_string(),
        mappable: false,
        note: format!("{what}; credential material, never copied or archived"),
    }
}

/// A dot-prefixed entry. Every source keeps bookkeeping (`.hub`,
/// `.install-metadata.json`, `.curator_*`) in dot-dirs, and none of it is
/// importable content.
fn is_hidden(p: &Path) -> bool {
    p.file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
}

/// Frontmatter fields `parse_skill` requires. Declared here rather than
/// depending on `pantheon-exec` (which pulls in storage, memory, sandbox and
/// zip) so `migrate` stays a small, dependency-light crate.
#[derive(Debug, serde::Deserialize)]
struct SkillFrontmatter {
    name: Option<String>,
}

/// Validate a `SKILL.md` with the same rules `pantheon_exec::skills::parse_skill`
/// applies, so migrate never imports an artifact the runtime will reject on
/// the next session. Returns the skill's name on success.
pub fn validate_skill_md(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))?;
    let rest = raw
        .strip_prefix("---")
        .ok_or_else(|| "no frontmatter block".to_string())?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| "frontmatter not closed".to_string())?;
    let meta: SkillFrontmatter =
        serde_yaml::from_str(&rest[..end]).map_err(|e| format!("bad frontmatter: {e}"))?;
    match meta.name {
        Some(n) if !n.trim().is_empty() => Ok(n.trim().to_string()),
        _ => Err("frontmatter has no name".to_string()),
    }
}

// ---------------------------------------------------------------------------
// detect
// ---------------------------------------------------------------------------

/// Detect a source root without assuming a fixed absolute layout.
///
/// Hermes ships `config.yaml` + `plugins/` + `skills/`; OpenClaw ships
/// `openclaw.plugin.json` manifests inside `plugins/`; OMP ships
/// `agent/config.yml` under an `agent/` dir alongside an install id;
/// Claude Code ships `settings.json` + `projects/` + `todos/`.
pub fn detect(root: &Path) -> Vec<SourceKind> {
    let mut found: Vec<SourceKind> = Vec::new();
    if is_hermes(root) {
        found.push(SourceKind::Hermes);
    }
    if is_openclaw(root) {
        found.push(SourceKind::OpenClaw);
    }
    if is_omp(root) {
        found.push(SourceKind::Omp);
    }
    if is_claude_code(root) {
        found.push(SourceKind::ClaudeCode);
    }
    found
}

fn is_hermes(root: &Path) -> bool {
    root.join("config.yaml").exists()
        || (root.join("plugins").is_dir() && root.join("skills").is_dir())
        || root.join("SOUL.md").exists()
}

/// OpenClaw is identified by its own plugin manifest spelling, not by a
/// generic `plugins/` dir, so a Hermes root is never misread as OpenClaw.
fn is_openclaw(root: &Path) -> bool {
    root.join("clawhub.json").exists() || plugins_are_openclaw(root)
}

fn plugins_are_openclaw(root: &Path) -> bool {
    let dir = root.join("plugins");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return false;
    };
    for e in rd.flatten() {
        if e.path().join("openclaw.plugin.json").exists() {
            return true;
        }
    }
    false
}

/// OMP: `~/.omp/agent/config.yml` is the anchor. A project-scoped `.omp` has
/// the same shape, so both are detected by the same test.
fn is_omp(root: &Path) -> bool {
    let agent = root.join("agent");
    if !agent.is_dir() {
        return false;
    }
    agent.join("config.yml").exists()
        || agent.join("config.yaml").exists()
        || agent.join("models.yml").exists()
        || root.join("install-id").exists()
}

/// Claude Code: `~/.claude/` with settings.json, projects/, todos/, or
/// agents/ subdirs. The anchor is `settings.json` (always present in a
/// working install) or a `projects/` dir with JSONL transcripts.
fn is_claude_code(root: &Path) -> bool {
    root.join("settings.json").exists()
        || (root.join("projects").is_dir() && root.join("todos").is_dir())
        || root.join("agents").is_dir()
}

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------

/// Analyse a source root: what exists, what maps, what has to be archived,
/// and what is a credential. Read-only; no writes anywhere on the source side.
pub fn analyze(root: &Path, kind: SourceKind) -> Vec<Detected> {
    let mut out = match kind {
        SourceKind::Hermes => analyze_hermes(root),
        SourceKind::OpenClaw => analyze_openclaw(root),
        SourceKind::Omp => analyze_omp(root),
        SourceKind::ClaudeCode => analyze_claude_code(root),
    };
    out.sort_by_key(|a| (a.kind, a.path.clone()));
    out
}

/// Shared skill scan: a `SKILL.md` directory imports verbatim, but only once
/// the frontmatter has been validated — a skill the runtime would reject is
/// archived with the reason instead of imported as a known-broken artifact.
///
/// Recurses rather than sampling one level, because the nesting is not
/// uniform: besides `skills/<category>/<name>/` (Hermes) there is
/// `skills/superpowers/skills/<name>/`, two levels down, which a one-level scan
/// misses entirely. Bounded by [`SKILL_SCAN_DEPTH`] so a pathological tree
/// cannot make one import unbounded.
const SKILL_SCAN_DEPTH: usize = 4;

fn scan_skills(dir: &Path, out: &mut Vec<Detected>) {
    scan_skills_at(dir, out, 0);
}

fn scan_skills_at(dir: &Path, out: &mut Vec<Detected>, depth: usize) {
    if depth > SKILL_SCAN_DEPTH {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_dir() || is_hidden(&p) {
            continue;
        }
        let md = p.join("SKILL.md");
        if md.is_file() {
            // `name` is the path relative to the skills root, so the depth is
            // visible in the note and a deep find is not mistaken for a bug.
            let note = if depth == 0 {
                "portable SKILL.md; Tier 1 drop-in".to_string()
            } else {
                format!("portable SKILL.md (nested {depth} level(s)); Tier 1 drop-in")
            };
            out.push(classify_skill(&p, &md, &note));
            // A skill directory is a leaf: `references/`, `scripts/` and
            // `assets/` inside it are its own files, not more skills.
            continue;
        }
        scan_skills_at(&p, out, depth + 1);
    }
}

fn classify_skill(dir: &Path, md: &Path, ok_note: &str) -> Detected {
    match validate_skill_md(md) {
        Ok(_) => Detected {
            kind: ItemKind::Skill,
            path: dir.to_string_lossy().to_string(),
            mappable: true,
            note: ok_note.to_string(),
        },
        Err(why) => Detected {
            kind: ItemKind::Skill,
            path: dir.to_string_lossy().to_string(),
            mappable: false,
            note: format!("SKILL.md {why}; the runtime would reject it, archive"),
        },
    }
}

/// Shared markdown-file scan for rules / commands / prompts / agents.
fn scan_md(dir: &Path, kind: ItemKind, out: &mut Vec<Detected>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        let ext = p
            .extension()
            .map(|x| x.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if !matches!(ext.as_str(), "md" | "mdc" | "markdown") {
            continue;
        }
        if is_secret_path(&p) {
            out.push(detected_secret(&p, "markdown-named credential"));
            continue;
        }
        out.push(Detected {
            kind,
            path: p.to_string_lossy().to_string(),
            mappable: true,
            note: format!("{} markdown file; imports verbatim", kind.name()),
        });
    }
}

fn analyze_hermes(root: &Path) -> Vec<Detected> {
    let mut out = Vec::new();

    // Tier 2 plugins. A Python `register(ctx)` plugin loads natively; a
    // TypeScript entry needs the OpenClaw-compat adapter, which does not
    // exist yet, so it is archived rather than half-imported.
    if let Ok(rd) = std::fs::read_dir(root.join("plugins")) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() || is_hidden(&p) {
                continue;
            }
            if is_secret_path(&p) {
                out.push(detected_secret(&p, "plugin dir named like a credential"));
                continue;
            }
            let has_manifest = p.join("plugin.yaml").exists();
            let has_init = p.join("__init__.py").exists();
            let has_ts = p.join("index.ts").exists();
            let (mappable, note) = if has_manifest && has_init && !has_ts {
                (
                    true,
                    "python register(ctx) hook plugin; imports natively".to_string(),
                )
            } else if has_ts && !has_init {
                (
                    false,
                    "typescript entry; needs OpenClaw-compat adapter (not implemented)".to_string(),
                )
            } else {
                (
                    false,
                    "no __init__.py or no plugin.yaml; archive".to_string(),
                )
            };
            out.push(Detected {
                kind: ItemKind::Extension,
                path: p.to_string_lossy().to_string(),
                mappable,
                note,
            });
        }
    }

    scan_skills(&root.join("skills"), &mut out);

    // Persona + memory plane.
    for (name, kind, note) in [
        (
            "SOUL.md",
            ItemKind::Persona,
            "agent persona; imports into the memory plane as agent instructions",
        ),
        (
            "profile.yaml",
            ItemKind::Persona,
            "profile identity; imports as agent identity",
        ),
        (
            "MEMORY.md",
            ItemKind::Memory,
            "memory file; imports into the memory plane with provenance",
        ),
        (
            "USER.md",
            ItemKind::Memory,
            "user profile; imports into the memory plane with provenance",
        ),
    ] {
        let p = root.join(name);
        if p.is_file() {
            out.push(Detected {
                kind,
                path: p.to_string_lossy().to_string(),
                mappable: true,
                note: note.to_string(),
            });
        }
    }

    // Config and anything that looks like a credential.
    for name in [
        ".env",
        "auth.json",
        "config.yaml",
        ".room-link-grant-secret",
    ] {
        let p = root.join(name);
        if !p.exists() {
            continue;
        }
        out.push(detected_secret(&p, "hermes config / credential"));
    }
    // Credential subtrees we never walk into.
    for dir in ["mcp-tokens", "shared", "attachments"] {
        let p = root.join(dir);
        if p.is_dir() {
            out.push(detected_secret(&p, "credential store dir"));
        }
    }
    // MCP servers and credential names bridge through their own items, so
    // they are covered even though their host config stays a secret.
    scan_mcp_and_credentials(root, &mut out);
    scan_sessions(
        &[root.join("sessions"), root.join("state").join("sessions")],
        &mut out,
        "hermes",
    );
    if let Some((n, path)) = count_schedules(root) {
        out.push(Detected {
            kind: ItemKind::Schedule,
            path,
            mappable: false,
            note: format!("{n} scheduled job(s); cron syntax and delivery targets differ, archive for manual port"),
        });
    }
    for name in ["gateway_state.json", "channel_directory.json"] {
        let p = root.join(name);
        if p.is_file() {
            out.push(Detected {
                kind: ItemKind::Channel,
                path: p.to_string_lossy().to_string(),
                mappable: false,
                note: "gateway/channel runtime state; Pantheon owns its own allowlist, archive"
                    .to_string(),
            });
        }
    }
    out
}

fn count_schedules(root: &Path) -> Option<(usize, String)> {
    let p = root.join("cron").join("jobs.json");
    let body = std::fs::read_to_string(&p).ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let arr = v
        .as_array()
        .or_else(|| v.get("jobs").and_then(|j| j.as_array()))?;
    let enabled = arr
        .iter()
        .filter(|j| j.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true))
        .count();
    Some((enabled, p.to_string_lossy().to_string()))
}

// ---------------------------------------------------------------------------
// MCP / credentials / sessions
// ---------------------------------------------------------------------------

/// MCP servers and credential names, both of which live inside config files
/// that are themselves classified as secrets. Reporting them as their own
/// importable items is how they get covered without ever copying a value.
fn scan_mcp_and_credentials(root: &Path, out: &mut Vec<Detected>) {
    // MCP: a Hermes `config.yaml` block, or a standalone mcp.json.
    let mut servers: Vec<carry::McpServer> = Vec::new();
    let mut mcp_source: Option<String> = None;
    for cand in ["config.yaml", "config.yml"] {
        let p = root.join(cand);
        if let Ok(body) = std::fs::read_to_string(&p) {
            let found = carry::parse_hermes_mcp(&body);
            if !found.is_empty() {
                servers = found;
                mcp_source = Some(p.to_string_lossy().to_string());
                break;
            }
        }
    }
    for cand in ["mcp.json", ".mcp.json", "agent/mcp.json"] {
        let p = root.join(cand);
        if let Ok(body) = std::fs::read_to_string(&p) {
            if let Ok(found) = carry::parse_mcp_json(&body) {
                if !found.is_empty() {
                    servers = found;
                    mcp_source = Some(p.to_string_lossy().to_string());
                    break;
                }
            }
        }
    }
    if let (Some(src), false) = (mcp_source.clone(), servers.is_empty()) {
        let needs = servers.iter().filter(|s| s.needs_credentials).count();
        out.push(Detected {
            kind: ItemKind::Mcp,
            path: src,
            mappable: true,
            note: format!(
                "{} mcp server(s) bridge to a Pantheon declaration{}",
                servers.len(),
                if needs > 0 {
                    format!(
                        "; {needs} declared a credential, which is recorded as a requirement and not copied"
                    )
                } else {
                    String::new()
                }
            ),
        });
    }

    // Custom providers. Declared as their own item kind so they are visible
    // and reviewable instead of silently absent.
    for cand in ["config.yaml", "config.yml"] {
        let p = root.join(cand);
        if providers::read_providers(&p).is_empty() {
            continue;
        }
        let set = providers::read_providers(&p);
        let with_key = set
            .iter()
            .filter(|x| x.key_env.as_deref().is_some_and(|k| !k.starts_with('<')))
            .count();
        out.push(Detected {
            kind: ItemKind::Provider,
            path: p.to_string_lossy().to_string(),
            mappable: true,
            note: format!(
                "{}: {with_key} with a key variable; written as a reviewable sidecar, merged into config.toml only on request",
                providers::summarise(&set)
            ),
        });
        break;
    }

    // Credentials: names only. Read from `.env` and from any `auth.json`
    // key names, classified by name into a Pantheon target.
    let mut names: Vec<String> = Vec::new();
    for cand in [".env", "env", ".env.local"] {
        let p = root.join(cand);
        if let Ok(body) = std::fs::read_to_string(&p) {
            for e in carry::parse_env_names(&body) {
                if carry::classify_credential(&e.name) != carry::CredentialTarget::Unclassified
                    && !names.contains(&e.name)
                {
                    names.push(e.name);
                }
            }
        }
    }
    if !names.is_empty() {
        let by_target = |t: carry::CredentialTarget| {
            names
                .iter()
                .filter(|n| carry::classify_credential(n) == t)
                .count()
        };
        out.push(Detected {
            kind: ItemKind::Credentials,
            path: root.join(".env").to_string_lossy().to_string(),
            mappable: true,
            note: format!(
                "{} credential name(s) declared for migration: {} provider, {} channel, {} mcp; \
                 a names-only manifest is written, no value is read or copied",
                names.len(),
                by_target(carry::CredentialTarget::Provider),
                by_target(carry::CredentialTarget::Channel),
                by_target(carry::CredentialTarget::McpAuth),
            ),
        });
    }
}

/// Transcript directories, reported so sessions are covered rather than
/// silently dropped. They import as a quarantined copy plus a manifest.
fn scan_sessions(dirs: &[PathBuf], out: &mut Vec<Detected>, source: &str) {
    for dir in dirs {
        if !dir.is_dir() {
            continue;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let entries: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        let transcripts: Vec<&PathBuf> = entries
            .iter()
            .filter(|p| carry::transcript_format(p).is_some())
            .collect();
        if transcripts.is_empty() {
            continue;
        }
        out.push(Detected {
            kind: ItemKind::Session,
            path: dir.to_string_lossy().to_string(),
            mappable: true,
            note: format!(
                "{} transcript file(s) in {}; quarantined, then indexed into session_search under a 'migrated:{}' run id",
                transcripts.len(),
                dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
                source
            ),
        });
    }
}

fn analyze_openclaw(root: &Path) -> Vec<Detected> {
    let mut out = Vec::new();
    // OpenClaw ships its extensions either in `plugins/` or, in the reference
    // install, in a flat `extensions/` tree beside the runtime.
    for dir in ["plugins", "extensions"] {
        let base = root.join(dir);
        if !base.is_dir() {
            continue;
        }
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let p = e.path();
                if !p.is_dir() || is_hidden(&p) {
                    continue;
                }
                if is_secret_path(&p) {
                    out.push(detected_secret(&p, "plugin dir named like a credential"));
                    continue;
                }
                out.push(analyze_foreign_extension(&p));
            }
        }
    }
    scan_skills(&root.join("skills"), &mut out);
    scan_mcp_and_credentials(root, &mut out);
    scan_sessions(&[root.join("sessions")], &mut out, "openclaw");
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && is_secret_path(&p) {
                out.push(detected_secret(&p, "openclaw config / credential"));
            }
        }
    }
    out
}

/// Classify a foreign (OpenClaw / OMP) extension through the section 8
/// compat adapter. A plugin imports only if at least one of its hooks has a
/// real Pantheon equivalent; the note always names what was lost.
fn analyze_foreign_extension(dir: &Path) -> Detected {
    use pantheon_extensions::compat;
    let path = dir.to_string_lossy().to_string();
    let Some(report) = compat::inspect(dir) else {
        return Detected {
            kind: ItemKind::Extension,
            path,
            mappable: false,
            note: "no openclaw.plugin.json or omp/pi package field; archive".to_string(),
        };
    };
    let mut note = format!(
        "compat adapter ({}): {} hook(s) mapped",
        report.origin,
        report.mapped.len()
    );
    if !report.unsupported.is_empty() {
        note.push_str(&format!(
            "; {} hook(s) have no Pantheon equivalent and will not run: {}",
            report.unsupported.len(),
            report.unsupported.join(", ")
        ));
    }
    if !report.refused.is_empty() {
        note.push_str(&format!(
            "; {} capability(ies) refused: {}",
            report.refused.len(),
            report.refused.join(", ")
        ));
    }
    if !report.credentials.is_empty() {
        let vars: Vec<&str> = report
            .credentials
            .iter()
            .map(|c| c.env_var.as_str())
            .collect();
        note.push_str(&format!(
            "; needs {} credential(s), declared by name: {}",
            report.credentials.len(),
            vars.join(", ")
        ));
    }
    Detected {
        kind: ItemKind::Extension,
        path,
        mappable: !report.mapped.is_empty(),
        note,
    }
}

fn analyze_omp(root: &Path) -> Vec<Detected> {
    let mut out = Vec::new();
    let agent = root.join("agent");

    scan_skills(&agent.join("skills"), &mut out);
    scan_md(&agent.join("agents"), ItemKind::Agent, &mut out);
    scan_md(&agent.join("rules"), ItemKind::Rule, &mut out);
    scan_md(&agent.join("commands"), ItemKind::Command, &mut out);
    scan_md(&agent.join("prompts"), ItemKind::Prompt, &mut out);

    // OMP extensions are packages with a `package.json` carrying an `omp` or
    // `pi` field. A bare directory without that is not an extension.
    if let Ok(rd) = std::fs::read_dir(agent.join("extensions")) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            if is_secret_path(&p) {
                out.push(detected_secret(&p, "extension dir named like a credential"));
                continue;
            }
            let pkg = p.join("package.json");
            let has_manifest = pkg.is_file() && package_declares_extension(&pkg);
            out.push(Detected {
                kind: ItemKind::Extension,
                path: p.to_string_lossy().to_string(),
                mappable: has_manifest,
                note: if has_manifest {
                    "package.json declares an omp/pi field; Tier 2 extension".to_string()
                } else {
                    "no omp/pi field in package.json; archive".to_string()
                },
            });
        }
    }

    // Plugin runtime config is a lockfile, not code. Nothing to import.
    let lock = agent.join("omp-plugins.lock.json");
    if lock.is_file() {
        out.push(Detected {
            kind: ItemKind::Opaque,
            path: lock.to_string_lossy().to_string(),
            mappable: false,
            note: "plugin lockfile; OMP-specific install state, archive".to_string(),
        });
    }

    // Config files: reported as secrets because they carry inline keys.
    for name in ["config.yml", "config.yaml", "models.yml", "agent.db"] {
        let p = agent.join(name);
        if p.exists() {
            out.push(detected_secret(&p, "omp config / credential store"));
        }
    }
    if let Ok(rd) = std::fs::read_dir(root.join("run").join("daemons")) {
        for e in rd.flatten() {
            let tok = e.path().join("broker.token");
            if tok.exists() {
                out.push(detected_secret(&tok, "daemon broker token"));
            }
        }
    }
    // Session transcripts: real content, but the memory plane owns
    // transcripts now, so importing them would double-store.
    scan_sessions(&[agent.join("sessions")], &mut out, "omp");
    scan_mcp_and_credentials(root, &mut out);
    out
}

fn package_declares_extension(pkg: &Path) -> bool {
    let Ok(body) = std::fs::read_to_string(pkg) else {
        return false;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    v.get("omp").is_some() || v.get("pi").is_some()
}

/// Claude Code analyzer: sessions, skills, agents, memory, plugins, config.
///
/// Claude Code stores its data under `~/.claude/`:
/// - `projects/<project-hash>/*.jsonl` — session transcripts
/// - `skills/<name>/SKILL.md` — portable skills
/// - `agents/<name>.md` — subagent definitions
/// - `todos/*.md` — memory/task files
/// - `plugins/<name>/` — plugin directories
/// - `settings.json` — config (may hold keys)
fn analyze_claude_code(root: &Path) -> Vec<Detected> {
    let mut out = Vec::new();

    // Session transcripts under projects/
    scan_sessions(&[root.join("projects")], &mut out, "claude");

    // Skills
    scan_skills(&root.join("skills"), &mut out);

    // Agent definitions
    scan_md(&root.join("agents"), ItemKind::Agent, &mut out);

    // Memory files (todos/ and MEMORY.md / USER.md at root)
    scan_md(&root.join("todos"), ItemKind::Memory, &mut out);
    for name in ["MEMORY.md", "USER.md"] {
        let p = root.join(name);
        if p.is_file() {
            out.push(Detected {
                kind: ItemKind::Memory,
                path: p.to_string_lossy().to_string(),
                mappable: true,
                note: "memory file; imports into the memory plane with provenance".to_string(),
            });
        }
    }

    // Plugins
    if let Ok(rd) = std::fs::read_dir(root.join("plugins")) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() || is_hidden(&p) {
                continue;
            }
            if is_secret_path(&p) {
                out.push(detected_secret(&p, "plugin dir named like a credential"));
                continue;
            }
            out.push(analyze_foreign_extension(&p));
        }
    }

    // Config and credentials
    scan_mcp_and_credentials(root, &mut out);

    // Settings.json is config that may hold keys
    let settings = root.join("settings.json");
    if settings.is_file() {
        out.push(detected_secret(
            &settings,
            "claude settings / config (may hold keys)",
        ));
    }

    out
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

/// Build the plan. Unmappable items are archived with a reason, credentials
/// are skipped, and nothing is dropped. `targets` decides where imports land.
///
/// Two sources can legitimately offer the same skill name — Hermes mirrors
/// its marketing family into a `marketingskills/` subdirectory. Those collide
/// on one import target, so the first (in `analyze`'s sort order) wins and
/// every later one is archived with a reason naming the winner. Silently
/// letting the last write win would be exactly the "silently dropped" failure
/// the spec forbids.
pub fn plan(root: &Path, kind: SourceKind, targets: &Targets) -> MigrationPlan {
    let detected = analyze(root, kind);
    let mut items: Vec<PlanItem> = Vec::new();
    let mut claimed: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    for d in detected {
        let action = if d.kind == ItemKind::Secret {
            Action::Skip {
                reason: d.note.clone(),
            }
        } else if !d.mappable {
            Action::Archive {
                reason: d.note.clone(),
            }
        } else {
            match targets.dir_for(d.kind) {
                // No filesystem target (memory plane): still an import, the
                // CLI routes it to `pantheon memory put`. The name is in the
                // target so two persona files do not collide on one record.
                None => Action::Import {
                    target: format!("memory://{}/{}", d.kind.name(), item_name(&d)),
                },
                Some(dir) => {
                    let target = match d.kind {
                        // A source `.env` is carried into Pantheon's *own* key
                        // store, `<data_dir>/.env` — the same file `pantheon
                        // model` writes and `load_dotenv` reads, so an imported
                        // key works with no extra wiring. The names-only
                        // manifest is a sidecar under `credentials/`.
                        ItemKind::Credentials => carry::pantheon_env_path(&targets.data_dir)
                            .to_string_lossy()
                            .to_string(),
                        // Both remaining bridges name their artefact after the
                        // *source*, not the source file. The plan target must
                        // be the exact path the writer produces, or `validate`
                        // checks a file nothing ever creates. The two differ in
                        // shape: MCP is a flat declaration, sessions are a
                        // quarantined directory plus a manifest.
                        ItemKind::Mcp => dir
                            .join(format!("{}.json", kind.name()))
                            .to_string_lossy()
                            .to_string(),
                        ItemKind::Session => dir
                            .join(format!("{}/manifest.json", kind.name()))
                            .to_string_lossy()
                            .to_string(),
                        // Providers land in a sidecar with a fixed name. The
                        // plan cannot know whether `--merge-providers` will
                        // also touch `config.toml`, so it names the artefact
                        // that is always written; the apply outcome reports the
                        // merge when it happens.
                        ItemKind::Provider => providers::provider_sidecar_path(&targets.data_dir)
                            .to_string_lossy()
                            .to_string(),
                        _ => dir.join(item_name(&d)).to_string_lossy().to_string(),
                    };
                    match claimed.get(&target) {
                        Some(winner) => Action::Archive {
                            reason: format!(
                                "name collides with {}; that one is imported",
                                short(winner)
                            ),
                        },
                        None => {
                            claimed.insert(target.clone(), d.path.clone());
                            Action::Import { target }
                        }
                    }
                }
            }
        };
        items.push(PlanItem {
            kind: d.kind,
            path: d.path,
            action,
            note: d.note,
        });
    }
    MigrationPlan {
        source: kind.name().into(),
        root: root.to_string_lossy().to_string(),
        source_version: source_version(root, kind),
        items,
    }
}

/// Last two path segments, for a readable collision message.
fn short(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').rev().take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// The import name: the file stem for single files, the directory name for
/// directories. Used as `<target_dir>/<name>`.
fn item_name(d: &Detected) -> String {
    let p = Path::new(&d.path);
    if p.is_dir() {
        return p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "item".into());
    }
    p.file_stem()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            p.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "item".into())
        })
}

/// Best-effort source version, for provenance. Read-only and never fatal.
pub fn source_version(root: &Path, kind: SourceKind) -> Option<String> {
    match kind {
        SourceKind::Hermes => {
            let p = root.join("hermes-agent").join("install-stamp.json");
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
            v.get("displayVersion")
                .or_else(|| v.get("baseVersion"))
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        }
        SourceKind::Omp => {
            let p = root.join("agent").join("last-changelog-version");
            let s = std::fs::read_to_string(p).ok()?;
            let s = s.trim();
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        }
        SourceKind::OpenClaw => None,
        SourceKind::ClaudeCode => {
            // Claude Code does not write a version stamp. The closest thing is
            // the settings.json "version" field (added in recent releases).
            let p = root.join("settings.json");
            let v: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
            v.get("version")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        }
    }
}

// ---------------------------------------------------------------------------
// provenance + render
// ---------------------------------------------------------------------------

/// Provenance for everything the plan would import.
pub fn provenance(root: &Path, kind: SourceKind) -> Provenance {
    Provenance {
        source: kind.name().into(),
        source_path: root.to_string_lossy().to_string(),
        source_version: source_version(root, kind),
        imported_at_ms: now_ms(),
    }
}

/// Render a plan for humans (dry-run output).
pub fn render(p: &MigrationPlan) -> String {
    let mut s = format!("migrate {} from {}\n", p.source, p.root);
    if let Some(v) = &p.source_version {
        s.push_str(&format!("  source version: {v}\n"));
    }
    s.push_str(&format!(
        "  {} to import, {} to archive, {} skipped (credentials)\n",
        p.imports(),
        p.archived(),
        p.skipped()
    ));
    for i in &p.items {
        match &i.action {
            Action::Import { target } => {
                s.push_str(&format!("  + {:<10} {} -> {}\n", i.kind, i.path, target))
            }
            Action::Archive { reason } => s.push_str(&format!(
                "  ~ {:<10} {} (archive: {})\n",
                i.kind, i.path, reason
            )),
            Action::Skip { reason } => s.push_str(&format!(
                "  - {:<10} {} (skip: {})\n",
                i.kind, i.path, reason
            )),
        }
    }
    s
}

/// JSON view of a plan, for `pantheon migrate --json`.
pub fn plan_json(p: &MigrationPlan) -> String {
    serde_json::to_string_pretty(p).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

#[cfg(test)]
#[path = "apply_tests.rs"]
mod apply_tests;
#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
