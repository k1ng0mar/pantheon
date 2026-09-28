//! Phone approval notifications (opt-in `[approvals]` config).
//!
//! When a run parks on an approval and `notify_channel` is set, the TUI
//! sends a Telegram/Discord message with the tool call (redacted args,
//! file diff when applicable) plus Grant/Deny buttons. The button
//! callback carries the run id so the gateway daemon can resolve the
//! exact pending scope even though the run was started locally — the
//! daemon's thread→run map only knows runs it initiated itself.
//!
//! Delivery is best-effort and never blocks the TUI: the hook spawns a
//! thread. Button taps are handled by `pantheon gateway`, which must be
//! running with the same bot; without it the buttons do nothing.

use std::path::{Path, PathBuf};

use pantheon_api::logging::redact;
use pantheon_gateway::discord::DiscordTransport;
use pantheon_gateway::telegram::TelegramTransport;
use serde_json::json;

/// Parsed approval notice: everything the phone message needs.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalNotice {
    pub run_id: String,
    pub call_id: String,
    pub tool: String,
    /// Args with secrets redacted through the log redactor.
    pub args_redacted: String,
    /// Unified diff when the tool writes a file and the current content is
    /// readable. Redacted like the args.
    pub diff: Option<String>,
    /// Callback payloads for the Grant/Deny buttons.
    pub grant_callback: String,
    pub deny_callback: String,
}

/// Split an approval scope (`call_id:tool:args`) into its parts.
/// Args may contain colons, so only the first two are separators.
pub fn parse_scope(scope: &str) -> Option<(String, String, String)> {
    let (call_id, rest) = scope.split_once(':')?;
    let (tool, args) = rest.split_once(':')?;
    Some((call_id.to_string(), tool.to_string(), args.to_string()))
}

/// Button callback data: `grant:{run_id}:{scope}` / `deny:{run_id}:{scope}`.
/// The run id lets the daemon resolve the pending approval directly;
/// legacy buttons sent before this carried only `grant:{scope}`.
pub fn callback_data(grant: bool, run_id: &str, scope: &str) -> String {
    format!(
        "{}:{}:{}",
        if grant { "grant" } else { "deny" },
        run_id,
        scope
    )
}

/// Parse button callback data back into `(grant, run_id, scope)`.
/// Shared with the gateway daemon's own parsing (`parse_approval_callback`
/// in `pantheon-gateway`), so the notifier and the daemon can never
/// disagree about the format.
pub fn parse_callback(data: &str) -> Option<(bool, Option<String>, String)> {
    let (answer, run_id, scope) = pantheon_gateway::channel::parse_approval_callback(data)?;
    let grant = matches!(answer, pantheon_gateway::channel::ApprovalAnswer::Grant);
    Some((grant, run_id, scope))
}

/// Build a notice from a parked approval. Returns `None` when the scope
/// doesn't parse (nothing useful to show on the phone).
pub fn build_notice(run_id: &str, scope: &str, workdir: &Path) -> Option<ApprovalNotice> {
    let (call_id, tool, args) = parse_scope(scope)?;
    let args_redacted = redact(&args);
    let diff = file_diff(&tool, &args, workdir).map(|d| redact(&d));
    Some(ApprovalNotice {
        run_id: run_id.to_string(),
        call_id,
        tool,
        args_redacted,
        diff,
        grant_callback: callback_data(true, run_id, scope),
        deny_callback: callback_data(false, run_id, scope),
    })
}

/// Render the phone message: tool, redacted args, diff when available.
pub fn format_message(n: &ApprovalNotice) -> String {
    let mut msg = format!(
        "🔐 Approval needed\nrun `{}`\ntool: `{}`\nargs: {}",
        n.run_id, n.tool, n.args_redacted
    );
    if let Some(diff) = &n.diff {
        msg.push_str(&format!("\n```diff\n{}\n```", truncate(diff, 3500)));
    }
    msg
}

/// Truncate to `max` chars on a char boundary, marking the cut.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// For file-writing tools, diff the current file against the proposed
/// content. Only `write_file` (path + content) is handled: other tools
/// either don't touch files or carry edits in shapes not worth diffing
/// on a phone screen. Returns `None` when there's nothing to diff.
fn file_diff(tool: &str, args: &str, workdir: &Path) -> Option<String> {
    if tool != "write_file" {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let path = v.get("path")?.as_str()?;
    let new_content = v.get("content")?.as_str()?;
    let full: PathBuf = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        workdir.join(path)
    };
    let old = std::fs::read_to_string(&full).unwrap_or_default();
    Some(unified_diff(path, &old, new_content))
}

/// Minimal unified diff (no external dep). Changed regions are shown
/// with 3 lines of context; long unchanged gaps are elided. The caller
/// caps the total message size.
fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines == new_lines {
        return "(no changes)".to_string();
    }
    let mut out = format!("--- {path}\n+++ {path}\n");
    // Simple LCS on lines; approval-time diffs are small.
    let n = old_lines.len();
    let m = new_lines.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            ops.push((' ', old_lines[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(('-', old_lines[i]));
            i += 1;
        } else {
            ops.push(('+', new_lines[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(('-', old_lines[i]));
        i += 1;
    }
    while j < m {
        ops.push(('+', new_lines[j]));
        j += 1;
    }
    // Emit each changed region with context; elide long unchanged gaps.
    const CTX: usize = 3;
    const MAX_LINES: usize = 120;
    let mut emitted = 0usize;
    let mut k = 0usize;
    let mut pending_ctx = 0usize; // context lines queued before next change
    while k < ops.len() && emitted < MAX_LINES {
        let (c, line) = ops[k];
        if c == ' ' {
            pending_ctx = (pending_ctx + 1).min(CTX);
            // Look ahead: elide the gap when a change follows far ahead.
            let mut ahead = k + 1;
            while ahead < ops.len() && ops[ahead].0 == ' ' {
                ahead += 1;
            }
            if ahead < ops.len() && ahead - k > CTX * 2 {
                if emitted > 0 {
                    out.push_str("@@\n");
                }
                // Emit the trailing context already queued, then skip.
                k = ahead - CTX;
                pending_ctx = 0;
                continue;
            }
            out.push(c);
            out.push_str(line);
            out.push('\n');
            emitted += 1;
            k += 1;
            continue;
        }
        // Flush queued context (dedup: never rewind past emitted lines).
        let start = k.saturating_sub(pending_ctx);
        for (c2, line2) in &ops[start..k] {
            out.push(*c2);
            out.push_str(line2);
            out.push('\n');
            emitted += 1;
        }
        pending_ctx = 0;
        out.push(c);
        out.push_str(line);
        out.push('\n');
        emitted += 1;
        k += 1;
    }
    if k < ops.len() {
        out.push_str("… (diff truncated)\n");
    }
    out
}

/// Send the notice through the configured channel. Tokens come from the
/// same env vars the gateway daemon uses and are never logged.
pub fn send_notice(channel: &str, chat_id: &str, notice: &ApprovalNotice) -> Result<(), String> {
    let text = format_message(notice);
    match channel {
        "telegram" => {
            let token = std::env::var("PANTHEON_TELEGRAM_BOT_TOKEN")
                .map_err(|_| "PANTHEON_TELEGRAM_BOT_TOKEN is not set".to_string())?;
            let transport = pantheon_gateway::telegram::TelegramRestTransport::new(token);
            let payload = json!({
                "text": text,
                "reply_markup": {"inline_keyboard": [[
                    {"text": "✅ Grant", "callback_data": notice.grant_callback},
                    {"text": "⛔ Deny", "callback_data": notice.deny_callback},
                ]]}
            });
            transport
                .send_message(chat_id, &payload)
                .map_err(|e| format!("telegram send: {e}"))?;
            Ok(())
        }
        "discord" => {
            let token = std::env::var("PANTHEON_DISCORD_BOT_TOKEN")
                .map_err(|_| "PANTHEON_DISCORD_BOT_TOKEN is not set".to_string())?;
            let transport = pantheon_gateway::discord::DiscordRestTransport::new(token);
            let payload = json!({
                "content": text,
                "components": [{
                    "type": 1,
                    "components": [
                        {"type": 2, "style": 3, "label": "Grant", "custom_id": notice.grant_callback},
                        {"type": 2, "style": 4, "label": "Deny", "custom_id": notice.deny_callback},
                    ]
                }]
            });
            transport
                .send_message(chat_id, &payload)
                .map_err(|e| format!("discord send: {e}"))?;
            Ok(())
        }
        other => Err(format!("unknown notify channel: {other}")),
    }
}
