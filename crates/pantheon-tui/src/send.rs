//! `pantheon send --to telegram|discord|mobile|home "message"` — push a
//! message to a gateway surface. For the user/cron, not the agent's normal
//! reply path.
//!
//! `telegram`/`discord` send immediately over the gateway's existing REST
//! transports, so a missing token is a loud error here rather than a queued
//! message nobody drains. `home`/`mobile` are session targets: the message
//! is written into the well-known home session's ledger (the session
//! auto-creates via `Ledger::ensure_home_session`; the mobile app renders
//! that same session, which is what carries `mobile` delivery to the
//! phone). Nothing here invents a transport.

use pantheon_gateway::schedule_delivery::{ChannelSender, Deliver, RestChannelSender};
use std::path::Path;

fn usage() -> &'static str {
    "usage:\n  \
     pantheon send --to telegram|discord|mobile|home \"message\""
}

/// What `parse_send_args` found.
#[derive(Debug, PartialEq, Eq)]
pub enum SendArgs {
    Help,
    Send { target: Deliver, message: String },
}

/// Parse `pantheon send` argv. Pure: unit-tested. `--to` is required; the
/// message is all remaining positional args joined with spaces, so quoting
/// is optional. An unknown target is a loud error naming the target.
pub fn parse_send_args(args: &[String]) -> Result<SendArgs, String> {
    let mut to: Option<String> = None;
    let mut message_parts: Vec<String> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--to" => {
                i += 1;
                to = Some(args.get(i).cloned().ok_or_else(|| {
                    "--to needs a target: telegram|discord|mobile|home".to_string()
                })?);
            }
            "--help" | "-h" => return Ok(SendArgs::Help),
            other if other.starts_with("--") => {
                return Err(format!("unknown flag '{other}'"));
            }
            text => message_parts.push(text.to_string()),
        }
        i += 1;
    }
    let target = match to {
        Some(t) => Deliver::parse(&t)?,
        None => return Err("missing --to <target>".to_string()),
    };
    let message = message_parts.join(" ");
    if message.trim().is_empty() {
        return Err("missing message".to_string());
    }
    Ok(SendArgs::Send { target, message })
}

/// Deliver one user-authored message to its target. Returns the
/// confirmation line for stdout. Every failure comes back as `Err` with the
/// exact cause — unknown target, missing token, ledger write failure — so
/// neither the CLI nor `/send` can fail silently.
pub fn send_to_target(data_dir: &Path, target: &Deliver, message: &str) -> Result<String, String> {
    let message = message.trim();
    if message.is_empty() {
        return Err("message is empty".to_string());
    }
    let sender = RestChannelSender;
    match target {
        Deliver::Telegram => sender
            .send_telegram(message)
            .map(|()| "sent to telegram".to_string()),
        Deliver::Discord => sender
            .send_discord(message)
            .map(|()| "sent to discord".to_string()),
        Deliver::Home => post_to_home_session(data_dir, message).map(|()| {
            format!(
                "posted to the home session (\"{}\")",
                pantheon_storage::HOME_SESSION_ID
            )
        }),
        // The mobile app renders the home session, so a message posted
        // there is what the phone shows; the session id is the conversation
        // context. No separate push transport exists to invent.
        Deliver::Mobile => post_to_home_session(data_dir, message).map(|()| {
            format!(
                "posted to the home session (\"{}\") for mobile delivery",
                pantheon_storage::HOME_SESSION_ID
            )
        }),
        other => Err(format!(
            "pantheon send supports telegram|discord|mobile|home, not '{}'",
            other.label()
        )),
    }
}

/// Write a user-authored message into the well-known home session's ledger.
/// The session auto-creates on first use. Mirrors the gateway's
/// `deliver_to_home_session` but without the scheduled-job framing — this
/// is a message, not a job result.
fn post_to_home_session(data_dir: &Path, message: &str) -> Result<(), String> {
    let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db"))
        .map_err(|e| format!("open ledger: {e}"))?;
    ledger
        .ensure_home_session()
        .map_err(|e| format!("home session: {e}"))?;
    let event = pantheon_api::events::Event::RunProgress {
        run_id: pantheon_storage::HOME_SESSION_ID.to_string(),
        detail: message.to_string(),
    };
    ledger
        .append(&event)
        .map_err(|e| format!("append to home session: {e}"))?;
    Ok(())
}

pub fn cmd_send(args: &[String]) {
    match parse_send_args(args) {
        Ok(SendArgs::Help) => println!("{}", usage()),
        Err(e) => {
            eprintln!("pantheon send: {e}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
        Ok(SendArgs::Send { target, message }) => {
            match send_to_target(&crate::terminal::data_dir(), &target, &message) {
                Ok(confirmation) => println!("{confirmation}"),
                Err(e) => {
                    eprintln!("pantheon send: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
