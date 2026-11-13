//! The one resolver for Pantheon's data directory.
//!
//! Security decisions depend on this path (the file-tool deny list and
//! the shell sandbox's hidden paths cover it), so it must resolve the
//! same way everywhere. Before this module the deny list only knew the
//! data dir when `PANTHEON_DATA_DIR` was exported, which left the default
//! `~/.pantheon` (and its `.env`) readable and writable by the file tools
//! (audit SEC-02).

use std::path::PathBuf;

/// `PANTHEON_DATA_DIR` when set and non-empty, else `$HOME/.pantheon`
/// (`%USERPROFILE%` on Windows), else `$XDG_DATA_HOME/pantheon`, else a
/// per-user directory under the system temp dir.
pub fn resolve() -> PathBuf {
    if let Some(d) = std::env::var_os("PANTHEON_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    if let Some(h) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(h).join(".pantheon");
    }
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(xdg).join("pantheon");
    }
    let user = std::env::var_os("USER")
        .or_else(|| std::env::var_os("USERNAME"))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".into());
    std::env::temp_dir()
        .join("pantheon")
        .join(user)
        .join(".pantheon")
}
