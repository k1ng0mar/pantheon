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
    /// A config file exists but does not parse. Show repair guidance and
    /// stop: never route this into setup (the wizard will not overwrite the
    /// file, so it would strand the user) and never silently discard it.
    Repair,
}

/// The three states a bare `pantheon` invocation can find the install in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigState {
    /// Parseable config with a model: open a session.
    Ready,
    /// No config file, or one with no model: first run, open setup.
    Missing,
    /// Config exists but does not parse: repair guidance, not setup.
    Broken,
}

impl Entry {
    /// The resolution itself, separated from the terminal so it is testable.
    ///
    /// `configured` is the signal from the config, and it is deliberately
    /// narrow: a config with no model cannot produce an answer, so it is not
    /// a usable agent no matter what else it declares. Anything less than a
    /// model and a provider means setup.
    pub fn resolve(configured: bool, resume: Option<String>) -> Entry {
        Self::resolve_state(
            if configured {
                ConfigState::Ready
            } else {
                ConfigState::Missing
            },
            resume,
        )
    }

    /// Tri-state resolution: a broken config is repair, not setup.
    pub fn resolve_state(state: ConfigState, resume: Option<String>) -> Entry {
        match state {
            ConfigState::Ready => Entry::Session { resume },
            ConfigState::Missing => Entry::Setup { resume },
            ConfigState::Broken => Entry::Repair,
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
    matches!(config_state(data_dir), ConfigState::Ready)
}

/// The full three-way read of the install state.
///
/// `Config::load` distinguishes "no file" (`CONFIG_OPEN`) from "file does
/// not parse" (`CONFIG_PARSE`); collapsing both into "not configured" sent
/// users with a typo'd config into the setup wizard, which refuses to
/// overwrite the file and left them stranded with no diagnosis.
pub fn config_state(data_dir: &Path) -> ConfigState {
    // `load`, not `load_or_report`: the reporting variant exits the process on
    // a parse error, which makes it unusable here and untestable. The exit
    // path belongs to the verb that owns the message.
    match crate::config::Config::load(data_dir) {
        Ok(cfg) => {
            let usable = cfg
                .model
                .as_ref()
                .map(|m| !m.model.trim().is_empty())
                .unwrap_or(false);
            if usable {
                ConfigState::Ready
            } else {
                ConfigState::Missing
            }
        }
        Err(e) if e.code == "CONFIG_OPEN" => ConfigState::Missing,
        Err(_) => ConfigState::Broken,
    }
}

/// Open the terminal product.
///
/// The ordering here is deliberate. Config is read *before* the terminal is
/// taken, so a user with no config is not flashed a blank alternate screen
/// before the wizard draws. Setup runs, and on success the same process falls
/// into a session without the user retyping anything.
pub fn run() {
    let data_dir = crate::terminal::data_dir();
    match Entry::resolve_state(config_state(&data_dir), None) {
        Entry::Session { resume } => launch(data_dir, resume),
        Entry::Setup { resume } => {
            // Setup owns the terminal for the whole wizard. It is a TUI
            // screen sequence, not a prompt loop, so there is no text mode
            // and no way to answer it without a terminal.
            crate::setup_wizard::run_setup_flow(&data_dir);
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
        Entry::Repair => {
            // The config exists but does not parse. Print the doctor's
            // findings in human-readable form and stop with a non-zero
            // exit. This is the "invalid config" arm of the bare-command
            // auto-pick: onboarding when fresh, repair guidance when
            // broken, a session when healthy.
            let report = crate::doctor::run_system_doctor(&data_dir);
            eprintln!("pantheon: the config file is broken, so there is no session to open.");
            let mut shown = 0;
            for c in &report.checks {
                if c.status != "ok" {
                    shown += 1;
                    eprintln!("pantheon: [{}] {}: {}", c.status, c.section, c.detail);
                    if !c.fix.is_empty() {
                        eprintln!("pantheon:   fix: {}", c.fix);
                    }
                }
            }
            if shown == 0 {
                eprintln!("pantheon: the doctor found nothing actionable; see `pantheon doctor` for the full report.");
            } else {
                eprintln!("pantheon: fix the TOML by hand, or move it aside and run `pantheon setup` to start fresh.");
            }
            std::process::exit(2);
        }
    }
}

/// `pantheon --resume [run_id]`. Same product, same terminal, same rules.
pub fn run_with_resume(resume: Option<String>) {
    let data_dir = crate::terminal::data_dir();
    if !is_configured(&data_dir) {
        eprintln!("pantheon: nothing is configured yet, so there is no session to resume.");
        eprintln!("pantheon: run `pantheon setup` first.");
        std::process::exit(1);
    }
    launch(data_dir, resume);
}

/// `pantheon --profile <name>` (also `-p`, `--agent`): open the TUI as the
/// named agent profile instead of the configured default.
///
/// The profile is validated *before* the terminal is taken: an unknown
/// name fails here with a clear error, because the session path folds
/// resolution failures into the anonymous fallback and would otherwise
/// open the wrong agent silently. The validated name is installed as the
/// process-lifetime override, which `Config::resolve_profile` consults —
/// session construction needs no new parameter.
pub fn run_with_profile(name: &str) {
    let data_dir = crate::terminal::data_dir();
    if !is_configured(&data_dir) {
        eprintln!("pantheon: nothing is configured yet, so there is no session to open.");
        eprintln!("pantheon: run `pantheon setup` first.");
        std::process::exit(1);
    }
    match crate::config::Config::load(&data_dir) {
        Ok(cfg) => {
            if let Err(e) = cfg.resolve_profile(Some(name)) {
                eprintln!("pantheon: --profile {name:?}: {e}");
                eprintln!("pantheon: fix: declare [agents.{name}] in config.toml");
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("pantheon: cannot read config: {e}");
            std::process::exit(1);
        }
    }
    pantheon_api::config::set_profile_override(Some(name.to_string()));
    run();
}

/// Fail fast with Ollama guidance when the configured provider is
/// `local` and nothing answers at its base URL.
///
/// The `local` provider is Ollama (install.sh installs it), and a bare
/// `pantheon` with Ollama down used to open a session whose first
/// message died with a bare PROVIDER_HTTP network error — no mention of
/// Ollama, no `pantheon setup` hint. Probing before the TUI opens turns
/// that into the fix: start Ollama, pull the model, or pick another
/// provider.
///
/// Only `local` is probed: a cloud endpoint hiccup at launch time must
/// not refuse to open the session.
fn check_local_provider(data_dir: &std::path::Path) {
    let model = match crate::config::Config::load(data_dir) {
        Ok(cfg) => match cfg.model {
            Some(m) => m,
            None => return,
        },
        // config_state already decided this is not a session; the
        // Setup/Repair arms own the message.
        Err(_) => return,
    };
    if model.provider != "local" {
        return;
    }
    let url = match pantheon_providers::catalog::resolve_base_url("local") {
        Ok(u) => u,
        Err(_) => return,
    };
    if crate::doctor::tcp_probe(&url, std::time::Duration::from_secs(3)) {
        return;
    }
    eprintln!("pantheon: the configured provider \"local\" (Ollama) is not reachable at {url}.");
    eprintln!(
        "pantheon: if you use Ollama: start it with `ollama serve`, then `ollama pull {}`.",
        model.model
    );
    eprintln!("pantheon: or run `pantheon setup` to choose a different provider.");
    std::process::exit(1);
}

/// Hand off to the TUI and make sure a failure leaves the user's shell sane.
///
/// A panic between entering raw mode and leaving the alternate screen leaves
/// the shell unusable until `reset`, so the hook restores the terminal first
/// and then hands the panic to the default hook for the backtrace. An error
/// return already restores via the session's own cleanup; the hook only fires
/// on a panic, which would otherwise skip it entirely.
fn launch(data_dir: std::path::PathBuf, resume: Option<String>) -> ! {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("pantheon: the terminal interface needs a terminal on stdin and stdout.");
        std::process::exit(1);
    }
    check_local_provider(&data_dir);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
        use std::io::IsTerminal;
        let _ = disable_raw_mode();
        if std::io::stdout().is_terminal() {
            use crossterm::ExecutableCommand;
            let _ = std::io::stdout().execute(LeaveAlternateScreen);
        }
        default_hook(info);
    }));
    let result = crate::session::run_tui_session_with(data_dir, resume);
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
