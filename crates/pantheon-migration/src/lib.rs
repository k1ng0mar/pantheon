//! migration pipeline: detect, analyze, plan, approve, back up, apply, validate.
//! planning is read-only; apply backs up each target first and validation makes
//! partial imports visible.
//!
//! unmappable data is archived with a reason, never dropped. secret-looking
//! paths aren't importable and stay out of backups.

use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::logging::now_ms;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub mod migration;

pub(crate) mod apply {
    pub(crate) use crate::migration::apply::*;
}
pub(crate) mod carry {
    pub(crate) use crate::migration::carry::*;
}
pub mod compat {
    pub use crate::migration::compat::*;
}
pub(crate) mod providers {
    pub(crate) use crate::migration::providers::*;
}
pub mod session_import {
    pub use crate::migration::session_import::*;
}

pub use migration::compat::{
    detect_kind, entry_file, inspect, map_hook, read_manifest, render_plugin_yaml, CompatKind,
    CompatReport, CredentialRequirement, HookMap,
};

pub use migration::apply::{
    apply, apply_with, apply_with_budgets, backup, backup_with_budgets, validate, ApplyReport,
    ApplyStatus, BackupManifest, BudgetUsage, StageBudgets, Staged, StagedItem, ValidateReport,
    ValidateStatus,
};
pub use migration::carry::{
    classify_credential, credential_manifest, looks_secret, pantheon_env_path, parse_env_names,
    parse_env_values, parse_hermes_mcp, parse_mcp_json, read_dotenv, read_mcp_declarations,
    server_readiness, transcript_format, write_credential_manifest, write_mcp_declaration,
    write_session_import_with_budget, CredentialManifest, CredentialMapping, CredentialTarget,
    EnvEntry, McpDeclaration, McpServer, SessionImport,
};
pub use migration::index::{
    ensure_sessions_indexed, index_quarantine, parse_transcript, quarantine_dir, ImportedChunk,
    IndexReport,
};
pub use migration::providers::{
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

    /// parse a source name. `omp` also answers to `oh-my-pi` and `pi` because
    /// that's what people type.
    pub fn parse(s: &str) -> Option<SourceKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hermes" => Some(SourceKind::Hermes),
            "openclaw" => Some(SourceKind::OpenClaw),
            "omp" | "oh-my-pi" | "pi" => Some(SourceKind::Omp),
            "claude" | "claude-code" | "claudecode" => Some(SourceKind::ClaudeCode),
            _ => None,
        }
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

/// user-facing buckets for item kinds, so you can say "sessions and skills but
/// not config" without knowing the internal kind names. each maps to one or
/// more [`ItemKind`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationCategory {
    Sessions,
    Skills,
    /// SOUL.md, AGENTS.md, profile.yaml
    Identity,
    /// MEMORY.md, USER.md
    Memory,
    Plugins,
    Config,
    /// names only; values go into .env
    Credentials,
    Mcp,
    Schedules,
    /// subagent markdown files
    Agents,
    Rules,
    Commands,
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

    /// kinds this category covers.
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

    /// every category, in help order.
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

    /// case-insensitive name lookup.
    pub fn parse(s: &str) -> Option<MigrationCategory> {
        let s = s.trim().to_ascii_lowercase();
        Self::all().iter().copied().find(|c| c.name() == s)
    }
}

/// which categories are in. all on by default; `--categories` picks a subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationFilter {
    pub categories: Vec<MigrationCategory>,
}

impl MigrationFilter {
    /// named categories only.
    pub fn only(categories: Vec<MigrationCategory>) -> Self {
        Self { categories }
    }

    /// does this kind pass?
    pub fn allows(&self, kind: ItemKind) -> bool {
        self.categories.iter().any(|c| c.kinds().contains(&kind))
    }

    /// filter a plan down to the selected categories. items in no category
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

/// what a detected item is. drives both the import target and the validate step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    /// SKILL.md dir. tier 1, imports verbatim.
    Skill,
    /// `<name>.md` with frontmatter.
    Agent,
    Rule,
    Command,
    Prompt,
    /// runtime-code plugin (hermes `plugin.yaml` + `__init__.py`, OMP
    /// extension package). tier 2.
    Extension,
    /// hermes `SOUL.md`, `profile.yaml`.
    Persona,
    /// `MEMORY.md`, `USER.md`, lands in the memory plane.
    Memory,
    /// provider/model config. archived by default: it routinely carries an
    /// inline api key, and pantheon keeps its own registry in `<data_dir>/.env`.
    Provider,
    /// MCP server definition.
    Mcp,
    /// scheduled job.
    Schedule,
    /// gateway/channel definition.
    Channel,
    /// session transcripts. imported to a quarantine path with a manifest, so
    /// indexing into `session_search` is a separate step.
    Session,
    /// names the source expects + the pantheon target each maps to. names only;
    /// no value is ever read or copied.
    Credentials,
    /// credential/token/key store. never copied, never archived.
    Secret,
    /// source-owned but no pantheon equivalent. archived with a reason.
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

    /// data dir subdir imports of this kind land in. `None` = no filesystem
    /// target (memory-plane only).
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
}

impl std::fmt::Display for ItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------
// Target layout
// ---------------------------------------------------------------------------

/// where imports land. mirrors the real read paths the runtime uses, so an
/// imported item shows up next session without extra wiring.
#[derive(Debug, Clone)]
pub struct Targets {
    pub data_dir: PathBuf,
    pub ext_dir: PathBuf,
}

impl Targets {
    pub fn new(data_dir: PathBuf, ext_dir: PathBuf) -> Self {
        Self { data_dir, ext_dir }
    }

    /// destination for one kind. `None` if there's no filesystem target.
    pub fn dir_for(&self, kind: ItemKind) -> Option<PathBuf> {
        // extensions get their own dir so PANTHEON_EXT_DIR still works.
        if kind == ItemKind::Extension {
            return Some(self.ext_dir.clone());
        }
        kind.target_dir().map(|d| self.data_dir.join(d))
    }

    /// backups live under the data dir so they travel with the install.
    pub fn backup_root(&self) -> PathBuf {
        self.data_dir.join("migrate-backups")
    }
}

// ---------------------------------------------------------------------------
// Plan data types
// ---------------------------------------------------------------------------

/// provenance carried by everything imported (spec section 23).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source: String,
    pub source_path: String,
    pub source_version: Option<String>,
    pub imported_at_ms: i64,
}

/// one item as found on the source side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detected {
    pub kind: ItemKind,
    pub path: String,
    pub mappable: bool,
    pub note: String,
}

/// what the pipeline would do with one item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "do", rename_all = "snake_case")]
pub enum Action {
    Import {
        target: String,
    },
    Archive {
        reason: String,
    },
    /// a credential. reported, never written anywhere.
    Skip {
        reason: String,
    },
}

/// what a dry-run would do, item by item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanItem {
    pub kind: ItemKind,
    pub path: String,
    pub action: Action,
    /// why this item is treated this way. carried from detection so the plan
    /// explains imports as well as archives.
    pub note: String,
}

impl PlanItem {
    /// import destination, if this is an import.
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
    /// imports of one kind only.
    pub fn imports_of(&self, kind: ItemKind) -> Vec<&PlanItem> {
        self.items
            .iter()
            .filter(|i| i.kind == kind && matches!(i.action, Action::Import { .. }))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Secret policy
// ---------------------------------------------------------------------------

/// credential filenames, wherever they appear in any of the three source
/// layouts. case-insensitive on the file name.
const SECRET_NAMES: &[&str] = &[
    ".env",
    "auth.json",
    "credentials",
    "credentials.json",
    "credentials.yaml",
    "credentials.yml",
    "google_client_secret.json",
    "google_token.json",
    "broker.token",
    ".room-link-grant-secret",
    "nous_auth.json",
    "service-account.json",
    "secrets.json",
    "secrets.yaml",
    "secrets.yml",
    "identity.json",
    ".netrc",
    "id_rsa",
    "id_ed25519",
];

/// extensions that are key stores or token files.
const SECRET_EXTS: &[&str] = &[".pem", ".key", ".p12", ".pfx", ".keystore"];

/// is this file a credential? (or a state file that holds credentials in a db
/// we never read)
pub fn is_secret_path(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if SECRET_NAMES.iter().any(|s| *s == name) {
        return true;
    }
    // `.env.local`, `.env.production`, ... hold the same material as `.env`;
    // an exact-name list alone would let them all slip through.
    if name.starts_with(".env.") {
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
    // credential-bearing state dbs. we never open these; a copy would carry
    // the keys with it.
    matches!(name.as_str(), "agent.db" | "state.db" | "auth.db")
        || name.ends_with("-wal")
        || name.ends_with("-shm")
}

/// does a config file embed an inline api key? we classify the whole file
/// rather than parsing it, because the point is to never copy a line we
/// haven't read. wired into [`flag_root_secrets`], which every analyzer calls
/// on its root; hermes additionally flags its `config.yaml` by name.
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

/// flag secret-like and key-bearing config files directly under `dir` as
/// [`ItemKind::Secret`] (never copied, never archived with content), skipping
/// already-reported paths. this is where [`config_may_hold_keys`] joins the
/// scan: a `models.yml` / `config.yaml` at a source root routinely carries an
/// inline `api_key:`, and before this pass it was invisible to the plan - not
/// flagged, not bridged. flagging the file doesn't block its mcp/provider
/// *shapes* from bridging as separate items (see [`scan_mcp_and_credentials`]):
/// only the file itself is never copied.
fn flag_root_secrets(dir: &Path, out: &mut Vec<Detected>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if !(is_secret_path(&p) || config_may_hold_keys(&p)) {
            continue;
        }
        let path = p.to_string_lossy().to_string();
        if out.iter().any(|d| d.path == path) {
            continue;
        }
        out.push(detected_secret(
            &p,
            "config / credential file (may hold keys)",
        ));
    }
}

fn detected_secret(path: &Path, what: &str) -> Detected {
    Detected {
        kind: ItemKind::Secret,
        path: path.to_string_lossy().to_string(),
        mappable: false,
        note: format!("{what}; credential material, never copied or archived"),
    }
}

/// a dot-prefixed entry. every source keeps bookkeeping (`.hub`,
/// `.install-metadata.json`, `.curator_*`) in dot-dirs, none importable.
fn is_hidden(p: &Path) -> bool {
    p.file_name()
        .map(|n| n.to_string_lossy().starts_with('.'))
        .unwrap_or(false)
}

/// validate a `SKILL.md` with the runtime's own parser
/// (`pantheon_exec::skills::parse_skill`), so migrate never imports something
/// the runtime will reject next session. returns the skill name on success.
///
/// delegating beats re-implementing: a partial check here (frontmatter + a
/// non-empty name) would pass skills the runtime rejects on one bad `exec:`
/// entry, so the import succeeds and the session start fails.
pub fn validate_skill_md(path: &Path) -> Result<String, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("unreadable: {e}"))?;
    let skill = pantheon_exec::skills::parse_skill(&raw, path).map_err(|e| e.cause.clone())?;
    Ok(skill.meta.name)
}

// ---------------------------------------------------------------------------
// detect
// ---------------------------------------------------------------------------

/// detect a source root without assuming a fixed absolute layout.
///
/// hermes: `config.yaml` + `plugins/` + `skills/`. openclaw:
/// `openclaw.plugin.json` inside `plugins/`. omp: `agent/config.yml` alongside
/// an install id. claude code: `settings.json` + `projects/` + `todos/`.
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

/// openclaw is identified by its own manifest spelling, not a generic
/// `plugins/` dir, so a hermes root is never misread as openclaw.
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

/// omp: `~/.omp/agent/config.yml` is the anchor. a project-scoped `.omp` has
/// the same shape, so both pass this test.
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

/// claude code: `~/.claude/` with settings.json / projects/ / todos/ /
/// agents/. anchor is `settings.json` (always present in a working install) or
/// a `projects/` dir with JSONL transcripts.
fn is_claude_code(root: &Path) -> bool {
    root.join("settings.json").exists()
        || (root.join("projects").is_dir() && root.join("todos").is_dir())
        || root.join("agents").is_dir()
}

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------

/// analyze a source root: what exists, what maps, what has to be archived,
/// what's a credential. read-only; no writes anywhere on the source side.
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

/// shared skill scan: a `SKILL.md` dir imports verbatim, but only once the
/// frontmatter validates - a skill the runtime would reject is archived with
/// the reason, not imported as known-broken.
///
/// recurses instead of sampling one level because the nesting isn't uniform:
/// besides `skills/<category>/<name>/` (hermes) there's
/// `skills/superpowers/skills/<name>/`, two levels down, which a one-level scan
/// misses entirely. bounded by [`SKILL_SCAN_DEPTH`] so a pathological tree
/// can't make one import unbounded.
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
            // `name` is the path relative to the skills root, so depth is
            // visible in the note and a deep find isn't mistaken for a bug.
            let note = if depth == 0 {
                "portable SKILL.md; Tier 1 drop-in".to_string()
            } else {
                format!("portable SKILL.md (nested {depth} level(s)); Tier 1 drop-in")
            };
            out.push(classify_skill(&p, &md, &note));
            // a skill dir is a leaf: `references/`, `scripts/`, `assets/`
            // inside it are its files, not more skills.
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

/// shared markdown scan for rules / commands / prompts / agents.
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

    // tier 2 plugins. a python `register(ctx)` plugin loads natively; a
    // typescript entry needs the openclaw-compat adapter, which doesn't exist
    // yet, so it's archived rather than half-imported.
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

    // persona + memory plane.
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

    // config + anything that looks like a credential.
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
    // credential subtrees we never walk into.
    for dir in ["mcp-tokens", "shared", "attachments"] {
        let p = root.join(dir);
        if p.is_dir() {
            out.push(detected_secret(&p, "credential store dir"));
        }
    }
    // root-level secret-like and key-bearing configs (`.env.local`,
    // `models.yml`, ...) aren't in the hardcoded list above; this pass catches
    // them, skipping ones already reported.
    flag_root_secrets(root, &mut out);
    // mcp servers and credential names bridge as their own items, so they're
    // covered even though their host config stays a secret.
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

/// mcp servers and credential names, both living inside config files that are
/// themselves secrets. reporting them as their own importable items is how
/// they get covered without ever copying a value.
fn scan_mcp_and_credentials(root: &Path, out: &mut Vec<Detected>) {
    // mcp: a hermes `config.yaml` block, or a standalone mcp.json.
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

    // custom providers get their own item kind so they're visible and
    // reviewable instead of silently absent.
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

    // credentials: names only. read from `.env` and `auth.json` key names,
    // classified by name into a pantheon target.
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

/// transcript directories, reported so sessions are covered, not silently
/// dropped. they import as a quarantined copy plus a manifest.
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
    // openclaw ships extensions either in `plugins/` or, in the reference
    // install, a flat `extensions/` tree beside the runtime.
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
    // root files: secret names and key-bearing configs. `is_secret_path` alone
    // would miss a root `config.yaml` / `models.yml`, which must not be copied.
    flag_root_secrets(root, &mut out);
    out
}

/// classify a foreign (openclaw / omp) extension through the section 8 compat
/// adapter. imports only if at least one hook has a real pantheon equivalent;
/// the note always names what was lost.
fn analyze_foreign_extension(dir: &Path) -> Detected {
    use crate::compat;
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
    // `agent/config.yml` and dotenv variants at the agent root are key-bearing,
    // never content to copy.
    flag_root_secrets(&agent, &mut out);

    // omp extensions are packages with a `package.json` carrying an `omp` or
    // `pi` field. a bare directory without that isn't an extension.
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

    // plugin runtime config is a lockfile, not code. nothing to import.
    let lock = agent.join("omp-plugins.lock.json");
    if lock.is_file() {
        out.push(Detected {
            kind: ItemKind::Opaque,
            path: lock.to_string_lossy().to_string(),
            mappable: false,
            note: "plugin lockfile; OMP-specific install state, archive".to_string(),
        });
    }

    // config files reported as secrets because they carry inline keys.
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
    // session transcripts are real content, but the memory plane owns
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

/// claude code analyzer. data lives under `~/.claude/`:
/// `projects/<hash>/*.jsonl` (sessions), `skills/<name>/SKILL.md`,
/// `agents/<name>.md`, `todos/*.md`, `plugins/<name>/`, `settings.json`
/// (config, may hold keys).
fn analyze_claude_code(root: &Path) -> Vec<Detected> {
    let mut out = Vec::new();

    // session transcripts under projects/
    scan_sessions(&[root.join("projects")], &mut out, "claude");

    scan_skills(&root.join("skills"), &mut out);
    scan_md(&root.join("agents"), ItemKind::Agent, &mut out);

    // memory files (todos/; MEMORY.md / USER.md at root)
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

    // plugins
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

    // config and credentials
    scan_mcp_and_credentials(root, &mut out);

    // settings.json is config that may hold keys
    let settings = root.join("settings.json");
    if settings.is_file() {
        out.push(detected_secret(
            &settings,
            "claude settings / config (may hold keys)",
        ));
    }
    // root dotenv variants and key-bearing configs, same rule as the other
    // analyzers (settings.json itself is skipped as already reported).
    flag_root_secrets(root, &mut out);

    out
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

/// build the plan. unmappable items are archived with a reason, credentials
/// skipped, nothing dropped. `targets` decides where imports land.
///
/// two sources can legitimately offer the same skill name - hermes mirrors
/// its marketing family into a `marketingskills/` subdir. they collide on one
/// import target, so the first (in `analyze`'s sort order) wins and every
/// later one is archived with a reason naming the winner. silently letting
/// the last write win would be the exact "silently dropped" failure the spec
/// forbids.
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
                // no filesystem target (memory plane): still an import, the
                // CLI routes it to `pantheon memory put`. name in the target so
                // two persona files don't collide on one record.
                None => Action::Import {
                    target: format!("memory://{}/{}", d.kind.name(), item_name(&d)),
                },
                Some(dir) => {
                    let target = match d.kind {
                        // a source `.env` goes into pantheon's *own* key store,
                        // `<data_dir>/.env` - the same file `pantheon model`
                        // writes and `load_dotenv` reads, so an imported key
                        // just works. the names-only manifest is a sidecar
                        // under `credentials/`.
                        ItemKind::Credentials => carry::pantheon_env_path(&targets.data_dir)
                            .to_string_lossy()
                            .to_string(),
                        // both remaining bridges name their artefact after the
                        // *source*, not the source file. plan target must be the
                        // exact path the writer produces, or `validate` checks a
                        // file nothing ever creates. the two differ in shape:
                        // mcp is a flat declaration, sessions are a quarantined
                        // dir + manifest.
                        ItemKind::Mcp => dir
                            .join(format!("{}.json", kind.name()))
                            .to_string_lossy()
                            .to_string(),
                        ItemKind::Session => dir
                            .join(format!("{}/manifest.json", kind.name()))
                            .to_string_lossy()
                            .to_string(),
                        // providers land in a sidecar with a fixed name. the
                        // plan can't know whether `--merge-providers` will also
                        // touch `config.toml`, so it names the artefact that's
                        // always written; the apply outcome reports the merge
                        // when it happens.
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

/// last two path segments, for a readable collision message.
fn short(p: &str) -> String {
    let parts: Vec<&str> = p.split('/').rev().take(2).collect();
    parts.into_iter().rev().collect::<Vec<_>>().join("/")
}

/// import name: file stem for single files, dir name for dirs. used as
/// `<target_dir>/<name>`.
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

/// best-effort source version, for provenance. read-only, never fatal.
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
            // claude code has no version stamp; the `version` field in
            // settings.json (added in recent releases) is the closest thing.
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

/// provenance for everything the plan would import.
pub fn provenance(root: &Path, kind: SourceKind) -> Provenance {
    Provenance {
        source: kind.name().into(),
        source_path: root.to_string_lossy().to_string(),
        source_version: source_version(root, kind),
        imported_at_ms: now_ms(),
    }
}

/// render a plan for humans (dry-run output).
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
                " - {:<10} {} (skip: {})\n",
                i.kind, i.path, reason
            )),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// dotenv variants and credential yaml names are secrets, not ordinary
    /// content; public-key material is not.
    #[test]
    fn secret_names_cover_dotenv_variants_and_credential_yaml() {
        for name in [
            ".env",
            ".env.local",
            ".env.production",
            "credentials.yaml",
            "credentials.yml",
            "secrets.yaml",
            "auth.json",
        ] {
            assert!(
                is_secret_path(Path::new(name)),
                "{name} must classify as a secret path"
            );
        }
        for name in ["notes.txt", "id_rsa.pub", "config.toml", "README.md"] {
            assert!(
                !is_secret_path(Path::new(name)),
                "{name} must not classify as a secret path"
            );
        }
    }

    /// `config_may_hold_keys` is wired into the scan: an OpenClaw root
    /// holding a `models.yml` / `config.yaml` / `.env.local` reports
    /// each as a Secret item, never as invisible-or-importable content.
    #[test]
    fn openclaw_root_flags_key_bearing_configs() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-migrate-secret-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["models.yml", "config.yaml", ".env.local"] {
            std::fs::write(dir.join(name), "api_key: sk-test\n").unwrap();
        }
        let found = analyze(&dir, SourceKind::OpenClaw);
        for name in ["models.yml", "config.yaml", ".env.local"] {
            let hit = found.iter().find(|d| {
                d.path == dir.join(name).to_string_lossy()
                    && d.kind == ItemKind::Secret
                    && !d.mappable
            });
            assert!(hit.is_some(), "{name} must be a non-mappable Secret item");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_skill(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("SKILL.md");
        std::fs::write(&path, body).unwrap();
        path
    }

    /// A skill with one bad `exec:` entry fails migration validation,
    /// exactly as the runtime rejects it at session start.
    #[test]
    fn validate_skill_md_rejects_bad_exec_entry() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-migrate-skill-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let good = write_skill(
            &dir,
            "---\nname: probe-skill\nexec:\n  - name: run-it\n    command: scripts/run.sh\n---\nBody.\n",
        );
        assert_eq!(validate_skill_md(&good).unwrap(), "probe-skill");
        let bad = write_skill(
            &dir,
            "---\nname: probe-skill\nexec:\n  - name: Bad Name\n    command: scripts/run.sh\n---\nBody.\n",
        );
        let err = validate_skill_md(&bad).unwrap_err();
        assert!(
            err.contains("Bad Name"),
            "error must name the bad entry: {err}"
        );
        let bad_path = write_skill(
            &dir,
            "---\nname: probe-skill\nexec:\n  - name: run-it\n    command: ../escape.sh\n---\nBody.\n",
        );
        assert!(validate_skill_md(&bad_path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
