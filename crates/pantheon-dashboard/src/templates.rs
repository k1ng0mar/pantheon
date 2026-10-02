//! Shared JSON-file storage for the template galleries (teams, experts).
//!
//! Each gallery keeps one file per template under
//! `<data_dir>/<kind>/<id>.json` (`kind` is `"teams"` or `"experts"`).
//! Ids are slugs (`pantheon_api::ident::is_slug`: ASCII alphanumerics,
//! `-`, `_`), validated on every read path that takes an id from the URL
//! so a crafted `:id` can never escape the gallery directory. Writes go
//! through [`crate::util::atomic_write`]. Seeding is idempotent: bundled
//! templates are written only when the gallery directory holds no `.json`
//! files, so a re-seed never overwrites user edits or user-added items.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Slug rule for template ids, shared by teams and experts.
pub(crate) fn valid_slug(id: &str) -> bool {
    pantheon_api::ident::is_slug(id)
}

/// `<data_dir>/<kind>`.
pub(crate) fn store_dir(data_dir: &Path, kind: &str) -> PathBuf {
    data_dir.join(kind)
}

/// Path of one template file. `None` when the id is not a slug - the
/// caller turns that into a 400 before touching the filesystem. The
/// `starts_with` guard is defense in depth: a valid slug cannot contain
/// `/`, `\`, or `..`, so the join cannot escape `store_dir` anyway.
pub(crate) fn item_path(data_dir: &Path, kind: &str, id: &str) -> Option<PathBuf> {
    if !valid_slug(id) {
        return None;
    }
    let dir = store_dir(data_dir, kind);
    let path = dir.join(format!("{id}.json"));
    if path.starts_with(&dir) {
        Some(path)
    } else {
        None
    }
}

/// `true` when the gallery directory holds at least one `.json` file.
/// A missing directory counts as empty (first access seeds it).
fn has_items(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.filter_map(|e| e.ok()).any(|e| {
        e.path()
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("json"))
    })
}

/// Read and parse one template file.
pub(crate) fn read_item<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// All templates in a gallery, sorted by id. A single corrupt file does
/// not fail the whole listing - it is skipped, and the gallery stays
/// usable (the file can be fixed or deleted through the API).
pub(crate) fn list_items<T: DeserializeOwned>(dir: &Path) -> Result<Vec<(String, T)>, String> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let is_json = path
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("json"));
        if !is_json {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !valid_slug(stem) {
            continue;
        }
        if let Ok(item) = read_item::<T>(&path) {
            out.push((stem.to_string(), item));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// Write one template file, creating the gallery directory as needed.
pub(crate) fn write_item<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let text =
        serde_json::to_string_pretty(value).map_err(|e| format!("serialize template: {e}"))?;
    crate::util::atomic_write(path, &text)
}

/// Delete one template file. `Ok(false)` when there was nothing there
/// (the caller turns that into a 404).
pub(crate) fn delete_item(path: &Path) -> Result<bool, String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("delete {}: {e}", path.display())),
    }
}

/// Write the bundled seeds when the gallery is empty. Returns `true`
/// when seeding happened. Never touches an existing file, so re-seeding
/// is a no-op and user edits / user-added templates always survive.
pub(crate) fn seed_if_empty<T: Serialize>(
    data_dir: &Path,
    kind: &str,
    seeds: &[T],
    id_of: impl Fn(&T) -> &str,
) -> Result<bool, String> {
    let dir = store_dir(data_dir, kind);
    if has_items(&dir) {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for seed in seeds {
        let id = id_of(seed);
        debug_assert!(valid_slug(id), "bundled seed id must be a slug: {id}");
        let path = dir.join(format!("{id}.json"));
        // `create_new`: a concurrent seeder winning the race must not
        // clobber the file it wrote.
        if path.exists() {
            continue;
        }
        let text =
            serde_json::to_string_pretty(seed).map_err(|e| format!("serialize seed: {e}"))?;
        crate::util::atomic_write(&path, &text)
            .map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(true)
}
