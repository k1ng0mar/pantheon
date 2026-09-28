//! Turn-complete notifications for background session tabs.
//!
//! When a turn finishes on a tab that is not the visible one, the operator
//! is probably looking elsewhere. This module raises two best-effort
//! signals: a terminal bell and a desktop notification via the shared
//! cross-platform notifier (`pantheon_gateway::notify`: notify-send on
//! Linux, osascript on macOS, PowerShell toast on Windows). Both are
//! fire-and-forget: a missing helper or a failed spawn never affects the
//! turn. The visible tab never notifies — the transcript itself is the
//! signal there.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Pure trigger logic: notify only when the completed run is not the one
/// on screen. An empty run id never notifies (defensive; callers always
/// set it).
pub fn should_notify(completed_run_id: &str, visible_session_id: &str) -> bool {
    !completed_run_id.is_empty() && completed_run_id != visible_session_id
}

#[cfg(unix)]
fn is_runnable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    p.is_file()
        && p.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_runnable(p: &Path) -> bool {
    p.is_file()
}

/// Find `notify-send` on an explicit search path. Split out from PATH
/// lookup so tests can pass a temp dir without touching the environment.
/// (Linux-only helper kept for the eval tests; the live path uses the
/// cross-platform [`pantheon_gateway::notify`] instead.)
pub fn find_notify_send(search_dirs: &[PathBuf]) -> Option<PathBuf> {
    search_dirs
        .iter()
        .map(|d| d.join("notify-send"))
        .find(|p| is_runnable(p))
}

/// PATH directories of this process.
fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Build the desktop-notification command for an explicit binary path.
/// Split from discovery so tests assert argv without touching PATH.
pub fn notify_command_with(bin: &Path, title: &str, body: &str) -> Command {
    let mut cmd = Command::new(bin);
    cmd.arg("--app-name")
        .arg("Pantheon")
        .arg("--expire-time")
        .arg("8000")
        .arg(title)
        .arg(body);
    cmd
}

/// Build the desktop-notification command, or `None` when `notify-send`
/// is unavailable. Kept separate from spawning so tests can assert on the
/// exact argv without executing anything.
pub fn notify_command(title: &str, body: &str) -> Option<Command> {
    let bin = find_notify_send(&path_dirs())?;
    Some(notify_command_with(&bin, title, body))
}

/// Short human summary for the notification body: first line of the last
/// assistant message, capped; falls back to a generic note.
pub fn summarize_turn(last_assistant_text: Option<&str>, turns_completed: u32) -> String {
    match last_assistant_text.map(str::trim).filter(|s| !s.is_empty()) {
        Some(t) => {
            let first: String = t.lines().next().unwrap_or("").chars().take(120).collect();
            format!("turn {turns_completed} done — {first}")
        }
        None => format!("turn {turns_completed} done"),
    }
}

/// Best-effort notification: terminal bell, then desktop. Never panics,
/// never blocks the caller meaningfully.
pub fn emit_turn_notification(tab_label: &str, summary: &str) {
    // Terminal bell: the TUI owns stdout, so BEL rings the terminal.
    {
        use std::io::Write as _;
        let _ = write!(std::io::stdout(), "\x07");
        let _ = std::io::stdout().flush();
    }
    let title = format!("Pantheon — {tab_label}");
    // Cross-platform desktop notification; a failure is silent by design.
    let _ = pantheon_gateway::notify::desktop_notify(&title, summary);
}
