//! Scheduled-task delivery: route a fired job's result somewhere user-facing.
//!
//! A job declares `deliver` (`log` | `telegram` | `discord` | `notify` |
//! `file:<path>` | `mobile` | `home`); after the run completes, the
//! run's final summary - the last assistant message, redacted and truncated
//! is sent there. The Telegram/Discord senders are the gateway's existing
//! REST transports, reused rather than reinvented; `notify` uses the
//! cross-platform desktop notifier ([`crate::notify`]: notify-send on Linux,
//! osascript on macOS, PowerShell toast on Windows). `mobile` and `home`
//! are session targets, not channel transports: they name the well-known
//! home session (id `home`) and are resolved by the gateway's
//! default-delivery routing, never by [`ChannelSender`]. Delivery failure
//! never fails the job:

use std::path::{Path, PathBuf};

use crate::discord::DiscordTransport;
use crate::telegram::TelegramTransport;

/// Where a scheduled job's result goes. Default: [`Deliver::Log`], which
/// keeps today's behavior (the run sits in the ledger; the tick printed it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deliver {
    /// Keep today's behavior: result stays in the ledger / tick stdout.
    Log,
    /// Telegram DM via the gateway's REST transport.
    Telegram,
    /// Discord DM via the gateway's REST transport.
    Discord,
    /// Desktop notification (cross-platform: notify-send / osascript /
    /// PowerShell toast).
    Notify,
    /// Append to a file.
    File(PathBuf),
    /// Post into the well-known home session (session id `home`). The
    /// session auto-creates and is pinned by the gateway; senders just
    /// target the id. A session id is not a channel transport, so this is
    /// resolved by the gateway's default-delivery routing, not by
    /// [`ChannelSender`] - see [`deliver_summary`].
    Home,
    /// Deliver to the user's mobile device, with the home session as the
    /// conversation context. Same session-targeted routing as [`Deliver::Home`].
    Mobile,
}

impl Deliver {
    /// Parse a `--deliver` value. Unknown targets are a loud error at
    /// registration, not a silent no-delivery at 3am.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "log" => Ok(Self::Log),
            "telegram" => Ok(Self::Telegram),
            "discord" => Ok(Self::Discord),
            "notify" => Ok(Self::Notify),
            "home" => Ok(Self::Home),
            "mobile" => Ok(Self::Mobile),
            other => match other.strip_prefix("file:") {
                Some(p) if !p.trim().is_empty() => Ok(Self::File(PathBuf::from(p.trim()))),
                _ => Err(format!(
                    "unknown --deliver target '{other}': log|telegram|discord|notify|file:<path>|mobile|home"
                )),
            },
        }
    }

    /// Short label for `schedule list`.
    pub fn label(&self) -> String {
        match self {
            Self::Log => "log".to_string(),
            Self::Telegram => "telegram".to_string(),
            Self::Discord => "discord".to_string(),
            Self::Notify => "notify".to_string(),
            Self::Home => "home".to_string(),
            Self::Mobile => "mobile".to_string(),
            Self::File(p) => format!("file:{}", p.display()),
        }
    }
}

/// Build the deliverable summary: redact secrets, truncate to ~2000 chars.
pub fn build_summary(raw: &str) -> String {
    const MAX_CHARS: usize = 2000;
    let redacted = pantheon_api::logging::redact(raw.trim());
    if redacted.chars().count() <= MAX_CHARS {
        redacted
    } else {
        let mut s: String = redacted.chars().take(MAX_CHARS - 3).collect();
        s.push_str("...");
        s
    }
}

/// Channel sending, injectable so delivery routing is testable without
/// hitting Telegram/Discord.
pub trait ChannelSender: Send + Sync {
    fn send_telegram(&self, text: &str) -> Result<(), String>;
    fn send_discord(&self, text: &str) -> Result<(), String>;
}

fn env_first(keys: &[&str]) -> Result<String, String> {
    for k in keys {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
    }
    Err(format!("none of {} is set", keys.join(" / ")))
}

/// The real sender: the gateway's existing REST transports.
pub struct RestChannelSender;

impl ChannelSender for RestChannelSender {
    fn send_telegram(&self, text: &str) -> Result<(), String> {
        let token = env_first(&["PANTHEON_TELEGRAM_BOT_TOKEN"])?;
        let chat = env_first(&["PANTHEON_DELIVER_TELEGRAM_TO"]).map_err(|_| {
            "PANTHEON_DELIVER_TELEGRAM_TO is not set (the chat id to deliver to)".to_string()
        })?;
        let transport = crate::telegram::TelegramRestTransport::new(token);
        let payload = serde_json::json!({ "text": text });
        transport
            .send_message(&chat, &payload)
            .map_err(|e| format!("telegram send: {e}"))?;
        Ok(())
    }

    fn send_discord(&self, text: &str) -> Result<(), String> {
        let token = env_first(&["PANTHEON_DISCORD_TOKEN", "PANTHEON_DISCORD_BOT_TOKEN"])?;
        let channel = env_first(&["PANTHEON_DELIVER_DISCORD_TO"]).map_err(|_| {
            "PANTHEON_DELIVER_DISCORD_TO is not set (the channel id to deliver to)".to_string()
        })?;
        let transport = crate::discord::DiscordRestTransport::new(token);
        let payload = serde_json::json!({ "content": text });
        transport
            .send_message(&channel, &payload)
            .map_err(|e| format!("discord send: {e}"))?;
        Ok(())
    }
}

fn notify_desktop(title: &str, body: &str) -> Result<(), String> {
    // Cross-platform: notify-send on Linux, osascript on macOS, PowerShell
    // toast on Windows. Shared with the TUI turn-complete notifier.
    crate::notify::desktop_notify(title, body)
}

fn append_file(path: &Path, job_label: &str, summary: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let entry = format!("## {job_label} - {now_ms}\n\n{summary}\n\n");
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .and_then(|mut f| {
            use std::io::Write as _;
            f.write_all(entry.as_bytes())
        })
        .map_err(|e| format!("append to {}: {e}", path.display()))?;
    Ok(())
}

/// Route a job's summary to its delivery target. `job_label` identifies the
/// job in the message (id or short name).
pub fn deliver_summary(
    sender: &dyn ChannelSender,
    target: &Deliver,
    job_label: &str,
    summary: &str,
) -> Result<(), String> {
    match target {
        Deliver::Log => Ok(()),
        Deliver::Telegram => sender.send_telegram(&format!("⏰ `{job_label}`:\n\n{summary}")),
        Deliver::Discord => sender.send_discord(&format!("⏰ **{job_label}**:\n\n{summary}")),
        Deliver::Notify => {
            notify_desktop("Pantheon scheduled job", &format!("{job_label}: {summary}"))
        }
        Deliver::File(path) => append_file(path, job_label, summary),
        // Session targets, not channel transports: there is no
        // `ChannelSender` method that posts into a session id, and
        // inventing one here would silently misroute. The gateway's
        // default-delivery routing resolves the home session id to a
        // surface; until a caller goes through that routing, this is a
        // loud error - never a silent no-delivery.
        Deliver::Home | Deliver::Mobile => Err(format!(
            "'{}' is a session target, not a channel transport: it needs the \
             gateway's default-delivery routing to resolve the home session",
            target.label()
        )),
    }
}

/// Never-failing delivery: returns `Some(error)` instead of propagating.
/// A failed delivery must never fail the job itself - the caller logs the
/// message and moves on.
pub fn deliver_best_effort(
    sender: &dyn ChannelSender,
    target: &Deliver,
    job_label: &str,
    summary: &str,
) -> Option<String> {
    deliver_summary(sender, target, job_label, summary).err()
}

// ---------------------------------------------------------------------------
// Home session delivery
// ---------------------------------------------------------------------------

/// Whether a job's raw `deliver` value routes through the home session:
/// no explicit target (`None`), `"mobile"`, and `"home"` all do. Matched
/// against the raw string rather than the parsed [`Deliver`] so this works
/// whether or not `Deliver::parse` knows a `mobile` spelling - `parse`
/// itself is owned elsewhere and stays untouched.
pub fn routes_via_home_session(raw: Option<&str>) -> bool {
    match raw {
        // No explicit target: the home session is the default delivery
        // target.
        None => true,
        Some(s) => {
            let t = s.trim();
            t.eq_ignore_ascii_case("mobile") || t.eq_ignore_ascii_case("home")
        }
    }
}

/// Write a job result into the home session's ledger so the user finds it
/// in the pinned session (dashboard, mobile app, TUI picker). The home
/// session is auto-created on first use. Never fails: returns
/// `Some(error)` instead, mirroring [`deliver_best_effort`] - a delivery
/// failure must never fail the job.
pub fn deliver_to_home_session(data_dir: &Path, job_label: &str, summary: &str) -> Option<String> {
    let ledger = match pantheon_storage::Ledger::open(&data_dir.join("ledger.db")) {
        Ok(l) => l,
        Err(e) => return Some(format!("open ledger: {e}")),
    };
    if let Err(e) = ledger.ensure_home_session() {
        return Some(format!("home session: {e}"));
    }
    let event = pantheon_api::events::Event::RunProgress {
        run_id: pantheon_storage::HOME_SESSION_ID.to_string(),
        detail: format!("scheduled job {job_label}: {summary}"),
    };
    ledger
        .append(&event)
        .err()
        .map(|e| format!("append to home session: {e}"))
}
