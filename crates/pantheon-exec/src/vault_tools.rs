//! Obsidian Vault Archive & Library tools.
//!
//! Exposes tools for the agent to archive long documents, notes, reports,
//! research, and project outputs into an Obsidian vault (`~/vault`),
//! keeping working memory lean while preserving a permanent, human-readable,
//! linked markdown library.

use crate::compact_output;
use crate::tools::ToolRegistry;
use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

fn verr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check vault path, permissions, or arguments",
        "",
    )
}

fn parse_vault_args<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, PantheonError> {
    serde_json::from_str(raw)
        .map_err(|e| verr("TOOL_BAD_ARGS", format!("failed to parse arguments: {e}")))
}

/// Options to configure vault tooling.
#[derive(Clone, Debug)]
pub struct VaultToolOptions {
    pub vault_dir: PathBuf,
}

impl Default for VaultToolOptions {
    fn default() -> Self {
        let vault_dir = std::env::var("PANTHEON_VAULT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".into());
                PathBuf::from(home).join("vault")
            });
        Self { vault_dir }
    }
}

/// Resolve a vault-relative path and prove it stays inside the vault.
///
/// A substring check for `..` is not confinement: a symlink planted inside
/// the vault (`vault/notes -> /etc`) passes it and then reads or writes
/// wherever it points. The check is therefore structural — canonicalize the
/// deepest existing ancestor, then confirm the result is still under the
/// canonical vault root.
///
/// A path that does not exist yet is fine (writes create it), so the walk
/// stops at the first component that is missing and the remaining suffix is
/// appended lexically. Components are rejected on the way, so no `..` can be
/// smuggled in through a name that only looks harmless.
fn resolve_safe_vault_path(vault_dir: &Path, rel_path: &str) -> Result<PathBuf, PantheonError> {
    let rel = rel_path.trim().trim_start_matches('/');
    if rel.is_empty() {
        return Err(verr("VAULT_BAD_PATH", "empty vault path".into()));
    }
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            return Err(verr(
                "VAULT_PATH_TRAVERSAL",
                "path traversal (..) is not permitted".into(),
            ));
        }
    }

    // Canonical root, so the comparison below is against real paths and not
    // against a vault dir that is itself reached through a symlink.
    let root = vault_dir
        .canonicalize()
        .map_err(|e| verr("VAULT_NO_ROOT", format!("vault dir unavailable: {e}")))?;
    // Walk down to the deepest component that exists on disk.
    let mut probe = root.clone();
    let mut tail: Option<std::ffi::OsString> = None;
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        match probe.join(comp).canonicalize() {
            Ok(real) => {
                if !real.starts_with(&root) {
                    return Err(verr(
                        "VAULT_ESCAPE",
                        format!("path resolves outside the vault: {rel_path}"),
                    ));
                }
                probe = real;
            }
            Err(_) => {
                // First missing component: everything after it is new, and
                // cannot contain a symlink.
                let mut rest = std::ffi::OsString::from(comp);
                for later in rel.split('/').skip_while(|c| *c != comp).skip(1) {
                    rest.push("/");
                    rest.push(later);
                }
                tail = Some(rest);
                break;
            }
        }
    }

    let mut final_path = match tail {
        Some(rest) => probe.join(rest),
        None => probe,
    };
    if final_path == root {
        return Err(verr(
            "VAULT_BAD_PATH",
            "path is the vault root, not a file in it".into(),
        ));
    }
    // Normalize away any `.` components the loop skipped.
    final_path = normalize_lexically(&final_path);
    Ok(final_path)
}

/// Strip `.` components without touching the filesystem. `canonicalize`
/// already removed them for the existing prefix; this handles the appended
/// suffix so the returned path is the one a caller will actually open.
fn normalize_lexically(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[derive(Deserialize)]
struct VaultArchiveArgs {
    category: String,
    title: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Deserialize)]
struct VaultReadArgs {
    path: String,
}

#[derive(Deserialize)]
struct VaultSearchArgs {
    query: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    10
}

#[derive(Deserialize)]
struct VaultListArgs {
    #[serde(default)]
    category: Option<String>,
    #[serde(default = "default_list_limit")]
    limit: usize,
}

fn default_list_limit() -> usize {
    50
}

/// Register the Obsidian Vault archive & library tools:
/// - `vault_archive`: Store a long-form document/output under a vault category (notes, projects, reference, etc.)
/// - `vault_read`: Read a document from the vault
/// - `vault_search`: Full-text/keyword search across markdown notes in the vault
/// - `vault_list`: List notes within a category or across the vault
pub fn register_vault_tools(reg: &mut ToolRegistry, opts: VaultToolOptions) {
    let vault_root = opts.vault_dir.clone();

    // 1. vault_archive
    {
        let root = vault_root.clone();
        reg.register(
            ToolSchema {
                name: "vault_archive".into(),
                description: "Archive long texts, project designs, research reports, or session notes into the Obsidian vault library for long-term reference.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "Category directory in the vault, e.g. 'notes', 'projects', 'reference', 'people'."
                        },
                        "title": {
                            "type": "string",
                            "description": "Title or slug for the file, e.g. 'agent-architecture-deepdive'."
                        },
                        "content": {
                            "type": "string",
                            "description": "The markdown body to archive."
                        },
                        "tags": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Optional list of tags, e.g. ['pantheon', 'architecture']."
                        }
                    },
                    "required": ["category", "title", "content"]
                }),
            },
            Capability::FilesystemWrite,
            move |raw_args| {
                let args: VaultArchiveArgs = parse_vault_args(raw_args)?;
                let category_clean = args.category.trim().trim_matches('/');
                let mut filename = args.title.trim().replace('/', "-");
                if !filename.ends_with(".md") {
                    filename.push_str(".md");
                }

                let target_dir = resolve_safe_vault_path(&root, category_clean)?;
                if let Err(e) = fs::create_dir_all(&target_dir) {
                    return Err(verr("VAULT_MKDIR_FAILED", format!("failed to create dir {}: {e}", target_dir.display())));
                }

                let target_file = target_dir.join(&filename);
                let now_utc = chrono_now();

                let mut body = String::new();
                body.push_str("---\n");
                body.push_str(&format!("title: \"{}\"\n", args.title.trim()));
                body.push_str(&format!("date: {}\n", now_utc));
                body.push_str(&format!("category: {}\n", category_clean));
                if !args.tags.is_empty() {
                    body.push_str("tags:\n");
                    for tag in &args.tags {
                        body.push_str(&format!("  - {}\n", tag.trim().trim_start_matches('#')));
                    }
                }
                body.push_str("---\n\n");
                body.push_str(args.content.trim());
                body.push('\n');

                if let Err(e) = fs::write(&target_file, body.as_bytes()) {
                    return Err(verr("VAULT_WRITE_FAILED", format!("failed to write {}: {e}", target_file.display())));
                }

                let rel_path = format!("{}/{}", category_clean, filename);
                Ok(format!("Successfully archived to vault: [[{}]] ({} bytes)", rel_path, body.len()))
            },
        );
    }

    // 2. vault_read
    {
        let root = vault_root.clone();
        reg.register(
            ToolSchema {
                name: "vault_read".into(),
                description: "Read the contents of an archived note or document from the Obsidian vault.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Relative path in vault, e.g. 'notes/ideation/2026-09-15-webhookwatch.md' or 'Home.md'."
                        }
                    },
                    "required": ["path"]
                }),
            },
            Capability::FilesystemRead,
            move |raw_args| {
                let args: VaultReadArgs = parse_vault_args(raw_args)?;
                let mut path_str = args.path.trim().to_string();
                if !path_str.ends_with(".md") && !path_str.contains('.') {
                    path_str.push_str(".md");
                }
                let target = resolve_safe_vault_path(&root, &path_str)?;
                if !target.exists() {
                    return Err(verr("VAULT_FILE_NOT_FOUND", format!("file not found in vault: {path_str}")));
                }

                let content = fs::read_to_string(&target)
                    .map_err(|e| verr("VAULT_READ_FAILED", format!("failed to read {}: {e}", target.display())))?;
                let compacted = compact_output(&content, &Default::default());
                Ok(compacted.text)
            },
        );
    }

    // 3. vault_search
    {
        let root = vault_root.clone();
        reg.register(
            ToolSchema {
                name: "vault_search".into(),
                description: "Search across the Obsidian vault markdown documents for keywords or phrases.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keyword or phrase to search for."
                        },
                        "category": {
                            "type": "string",
                            "description": "Optional subfolder to restrict search to, e.g. 'projects' or 'notes'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum matching snippets to return (default 10)."
                        }
                    },
                    "required": ["query"]
                }),
            },
            Capability::FilesystemRead,
            move |raw_args| {
                let args: VaultSearchArgs = parse_vault_args(raw_args)?;
                let search_dir = match &args.category {
                    Some(cat) => resolve_safe_vault_path(&root, cat.trim().trim_matches('/'))?,
                    None => root.clone(),
                };

                if !search_dir.exists() {
                    return Ok("Category directory not found in vault.".into());
                }

                let query_lower = args.query.to_lowercase();
                let terms: Vec<&str> = query_lower.split_whitespace().collect();
                if terms.is_empty() {
                    return Ok("Empty query.".into());
                }

                let mut hits = Vec::new();
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(TRAVERSE_BUDGET_MS);
                let complete = search_vault_dir(
                    &root,
                    &search_dir,
                    &terms,
                    args.limit,
                    &mut hits,
                    &deadline,
                )?;

                if hits.is_empty() {
                    return Ok(format!("No matches found in vault for '{}'.", args.query));
                }

                let mut out = format!("Found {} vault matches for '{}':\n\n", hits.len(), args.query);
                for hit in hits {
                    out.push_str(&format!("- **[[{}]]**:\n  > {}\n\n", hit.rel_path, hit.snippet));
                }
                if !complete {
                    out.push_str("\n[note: search hit the time budget on the vault mount; results are partial — narrow with category=]");
                }
                Ok(out)
            },
        );
    }

    // 4. vault_list
    {
        let root = vault_root.clone();
        reg.register(
            ToolSchema {
                name: "vault_list".into(),
                description: "List notes and documents archived in the Obsidian vault library.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "Optional category subfolder, e.g. 'projects', 'notes', 'reference'."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum items to list (default 50)."
                        }
                    }
                }),
            },
            Capability::FilesystemRead,
            move |raw_args| {
                let args: VaultListArgs = parse_vault_args(raw_args)?;
                let search_dir = match &args.category {
                    Some(cat) => resolve_safe_vault_path(&root, cat.trim().trim_matches('/'))?,
                    None => root.clone(),
                };

                if !search_dir.exists() {
                    return Ok("Vault directory does not exist.".into());
                }

                let mut files = Vec::new();
                let deadline = std::time::Instant::now()
                    + std::time::Duration::from_millis(TRAVERSE_BUDGET_MS);
                let complete =
                    collect_vault_files(&root, &search_dir, args.limit, &mut files, &deadline)?;

                if files.is_empty() {
                    return Ok("No files found in vault.".into());
                }

                let mut out = format!("Vault files ({} total):\n", files.len());
                for f in files {
                    out.push_str(&format!("- [[{}]]\n", f));
                }
                if !complete {
                    out.push_str("\n[note: listing hit the time budget; partial — narrow with category=]");
                }
                Ok(out)
            },
        );
    }
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{now}")
}

struct SearchHit {
    rel_path: String,
    snippet: String,
}

/// Wall-clock budget for one traversal. The vault can live on a FUSE
/// network mount (rclone); an unbounded walk stalls the tool call.
const TRAVERSE_BUDGET_MS: u64 = 2_500;
/// Skip markdown files bigger than this: archives are read into model
/// context only through compaction anyway, and huge reads on a network
/// mount dominate the budget.
const MAX_SCAN_BYTES: u64 = 512 * 1024;

fn should_skip_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "backups" | "target" | "node_modules" | "UoPeople")
}

fn search_vault_dir(
    root: &Path,
    dir: &Path,
    terms: &[&str],
    limit: usize,
    hits: &mut Vec<SearchHit>,
    deadline: &std::time::Instant,
) -> Result<bool, PantheonError> {
    if hits.len() >= limit || std::time::Instant::now() >= *deadline {
        return Ok(false);
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(true),
    };

    for entry in entries.flatten() {
        if hits.len() >= limit {
            return Ok(true);
        }
        if std::time::Instant::now() >= *deadline {
            return Ok(false);
        }
        let p = entry.path();
        if p.is_dir() {
            let file_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if should_skip_dir(file_name) {
                continue;
            }
            if !search_vault_dir(root, &p, terms, limit, hits, deadline)? {
                return Ok(false);
            }
        } else if p.is_file() && p.extension().is_some_and(|ext| ext == "md") {
            let meta_len = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            if meta_len > MAX_SCAN_BYTES {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&p) {
                let content_lower = content.to_lowercase();
                if terms.iter().all(|t| content_lower.contains(t)) {
                    let first_pos = content_lower.find(terms[0]).unwrap_or(0);
                    let start = first_pos.saturating_sub(60);
                    let end = (first_pos + 100).min(content.len());
                    let snippet = content[start..end].replace('\n', " ").trim().to_string();
                    let rel = p
                        .strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .to_string();
                    hits.push(SearchHit {
                        rel_path: rel,
                        snippet,
                    });
                }
            }
        }
    }
    Ok(true)
}

fn collect_vault_files(
    root: &Path,
    dir: &Path,
    limit: usize,
    files: &mut Vec<String>,
    deadline: &std::time::Instant,
) -> Result<bool, PantheonError> {
    if files.len() >= limit || std::time::Instant::now() >= *deadline {
        return Ok(false);
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(true),
    };

    for entry in entries.flatten() {
        if files.len() >= limit {
            return Ok(true);
        }
        if std::time::Instant::now() >= *deadline {
            return Ok(false);
        }
        let p = entry.path();
        if p.is_dir() {
            let file_name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if should_skip_dir(file_name) {
                continue;
            }
            if !collect_vault_files(root, &p, limit, files, deadline)? {
                return Ok(false);
            }
        } else if p.is_file() && p.extension().is_some_and(|ext| ext == "md") {
            let rel = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .to_string();
            files.push(rel);
        }
    }
    Ok(true)
}

#[cfg(test)]
#[path = "vault_tools_tests.rs"]
mod tests;
