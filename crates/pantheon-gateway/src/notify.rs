//! Cross-platform desktop notifications.
//!
//! One best-effort notifier shared by the TUI turn-complete bell path and
//! the scheduler's `--deliver notify` mode:
//!
//! - Linux: `notify-send` (libnotify)
//! - macOS: `osascript` display notification
//! - Windows: PowerShell toast via WinRT `Windows.UI.Notifications`
//!
//! Everything is fire-and-forget by contract: a missing helper or a failed
//! spawn is an `Err`, never a panic, and callers are expected to ignore it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Which desktop notifier this host can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notifier {
    /// `notify-send` at the given path (Linux/libnotify).
    NotifySend(PathBuf),
    /// `osascript` display notification (macOS).
    Osascript,
    /// PowerShell WinRT toast (Windows).
    PowerShell,
    /// Nothing usable found.
    None,
}

impl Notifier {
    pub fn name(&self) -> &'static str {
        match self {
            Notifier::NotifySend(_) => "notify-send",
            Notifier::Osascript => "osascript",
            Notifier::PowerShell => "powershell-toast",
            Notifier::None => "none",
        }
    }
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

/// Find a helper binary on an explicit search path. Split out from PATH
/// lookup so tests can pass a temp dir without touching the environment.
fn find_on_path(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
    let file = if cfg!(target_os = "windows") && !name.contains('.') {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    dirs.iter().map(|d| d.join(&file)).find(|p| is_runnable(p))
}

/// Detect the notifier for this platform from an explicit search path
/// (real callers pass PATH). Pure apart from the filesystem checks, so
/// tests inject a temp dir.
pub fn detect_notifier(path_dirs: &[PathBuf]) -> Notifier {
    if cfg!(target_os = "macos") {
        return if find_on_path(path_dirs, "osascript").is_some() {
            Notifier::Osascript
        } else {
            Notifier::None
        };
    }
    if cfg!(target_os = "windows") {
        return if find_on_path(path_dirs, "powershell").is_some() {
            Notifier::PowerShell
        } else {
            Notifier::None
        };
    }
    // Linux and other unixes: notify-send.
    match find_on_path(path_dirs, "notify-send") {
        Some(bin) => Notifier::NotifySend(bin),
        None => Notifier::None,
    }
}

/// AppleScript string-literal escaping for `display notification`.
pub fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// PowerShell single-quoted string escaping (interior quotes doubled).
pub fn escape_powershell(s: &str) -> String {
    s.replace('\'', "''")
}

/// The WinRT toast script. Single-quoted PowerShell strings throughout;
/// title/body are pre-escaped by [`escape_powershell`].
fn powershell_toast_script(title: &str, body: &str) -> String {
    format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
         $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $x = $t.GetElementsByTagName('text'); \
         $x.Item(0).AppendChild($t.CreateTextNode('{title}')) | Out-Null; \
         $x.Item(1).AppendChild($t.CreateTextNode('{body}')) | Out-Null; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Pantheon').Show($t)",
        title = escape_powershell(title),
        body = escape_powershell(body),
    )
}

/// Build the notifier command without running it, so tests can assert the
/// exact argv per platform without spawning anything.
pub fn command_for(notifier: &Notifier, title: &str, body: &str) -> Option<Command> {
    match notifier {
        Notifier::NotifySend(bin) => {
            let mut cmd = Command::new(bin);
            cmd.arg("--app-name")
                .arg("Pantheon")
                .arg("--expire-time")
                .arg("8000")
                .arg(title)
                .arg(body);
            Some(cmd)
        }
        Notifier::Osascript => {
            let script = format!(
                "display notification \"{}\" with title \"{}\"",
                escape_applescript(body),
                escape_applescript(title)
            );
            let mut cmd = Command::new("osascript");
            cmd.arg("-e").arg(script);
            Some(cmd)
        }
        Notifier::PowerShell => {
            let mut cmd = Command::new("powershell");
            cmd.arg("-NoProfile")
                .arg("-NonInteractive")
                .arg("-Command")
                .arg(powershell_toast_script(title, body));
            Some(cmd)
        }
        Notifier::None => None,
    }
}

/// PATH directories of this process.
fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Best-effort desktop notification. `Ok` means the notifier ran cleanly;
/// `Err` carries the reason (no notifier on this host, spawn failure, or a
/// non-zero exit). Callers treat failure as "no notification", never fatal.
pub fn desktop_notify(title: &str, body: &str) -> Result<(), String> {
    let notifier = detect_notifier(&path_dirs());
    let mut cmd = command_for(&notifier, title, body).ok_or_else(|| {
        format!(
            "no desktop notifier on this platform ({} backend)",
            notifier.name()
        )
    })?;
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("{}: {e}", notifier.name()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{} exited with {status}", notifier.name()))
    }
}
