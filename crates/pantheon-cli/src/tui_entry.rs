//! The single interactive entry point.
//!
//! Everything a person types goes through here. Two decisions live in this
//! module and nowhere else: whether the install is configured enough to open
//! a session, and how the terminal is restored when something fails.
//!
//! Setup runs inside the TUI when the config is incomplete. There is no
//! separate wizard binary and no text prompt fallback, because a user who
//! gets a different program depending on their terminal is a user who files
//! two bug reports about two products.

use std::io::IsTerminal;
use std::path::Path;

/// What the install looks like before we decide what to open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    /// A usable agent configuration. Open a session.
    Session { resume: Option<String> },
    /// Nothing configured yet. Run setup first, then a session.
    Setup { resume: Option<String> },
}

impl Entry {
    /// The resolution itself, separated from the terminal so it is testable.
    ///
    /// `configured` is the signal from the config, and it is deliberately
    /// narrow: a config with no model cannot produce an answer, so it is not
    /// a usable agent no matter what else it declares. Anything less than a
    /// model and a provider means setup.
    pub fn resolve(configured: bool, resume: Option<String>) -> Entry {
        if configured {
            Entry::Session { resume }
        } else {
            Entry::Setup { resume }
        }
    }
}

/// Whether the install can open a session right now.
///
/// A missing config file, an unreadable one, or one without a model all mean
/// the same thing to a user on first run: nothing is set up.
///
/// This deliberately does not treat a *malformed* config as "not configured".
/// Overwriting a file the user broke would destroy the evidence of what went
/// wrong, and the entry point's job is to decide what to open, not to repair
/// anything. A malformed config is reported by `doctor` and by the reader that
/// owns that message; here it simply means "not a usable session", which routes
/// the user to setup, which will not overwrite the file.
pub fn is_configured(data_dir: &Path) -> bool {
    // `load`, not `load_or_report`: the reporting variant exits the process on
    // a parse error, which makes it unusable here and untestable. The exit
    // path belongs to the verb that owns the message.
    let Ok(cfg) = crate::config_doc::Config::load(data_dir) else {
        return false;
    };
    cfg.model
        .as_ref()
        .map(|m| !m.model.trim().is_empty())
        .unwrap_or(false)
}

/// Open the terminal product.
///
/// The ordering here is deliberate. Config is read *before* the terminal is
/// taken, so a user with no config is not flashed a blank alternate screen
/// before the wizard draws. Setup runs, and on success the same process falls
/// into a session without the user retyping anything.
pub fn run() {
    let data_dir = crate::data_dir();
    match Entry::resolve(is_configured(&data_dir), None) {
        Entry::Session { resume } => launch(data_dir, resume),
        Entry::Setup { resume } => {
            // Setup owns the terminal for the whole wizard. It is a TUI
            // screen sequence, not a prompt loop, so there is no text mode
            // and no way to answer it without a terminal.
            crate::tui_setup::run_setup_flow(&data_dir);
            // Re-read after setup: setup writes the config, and if the user
            // quit early there may be nothing usable. Falling into a session
            // with no model would look like a broken agent rather than an
            // unfinished install.
            if is_configured(&data_dir) {
                launch(data_dir, resume);
            } else {
                eprintln!("pantheon: setup did not finish, so there is no session to open.");
                eprintln!("pantheon: run `pantheon setup` when you are ready.");
                std::process::exit(1);
            }
        }
    }
}

/// `pantheon --resume [run_id]`. Same product, same terminal, same rules.
pub fn run_with_resume(resume: Option<String>) {
    let data_dir = crate::data_dir();
    if !is_configured(&data_dir) {
        eprintln!("pantheon: nothing is configured yet, so there is no session to resume.");
        eprintln!("pantheon: run `pantheon setup` first.");
        std::process::exit(1);
    }
    launch(data_dir, resume);
}

/// Hand off to the TUI and make sure a failure leaves the user's shell sane.
///
/// `run_tui_session` takes the alternate screen and installs its own panic
/// hook, but the hook only fires on a panic. An error return skips the
/// restore entirely, which is how a user ends up with a shell that thinks it
/// is still in the TUI and needs `reset`.
fn launch(data_dir: std::path::PathBuf, resume: Option<String>) -> ! {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("pantheon: the terminal interface needs a terminal on stdin and stdout.");
        std::process::exit(1);
    }
    let result = crate::tui::run_tui_session_with(data_dir, resume);
    if let Err(e) = result {
        // The TUI should have restored the screen, but a restore that itself
        // failed is exactly the case where the user needs to be told rather
        // than left guessing why their terminal is broken.
        eprintln!("pantheon: terminal interface failed: {e}");
        eprintln!("pantheon: if your terminal looks wrong, run `reset`.");
        std::process::exit(1);
    }
    std::process::exit(0);
}

#[cfg(test)]
#[path = "tui_entry_tests.rs"]
mod tests;
