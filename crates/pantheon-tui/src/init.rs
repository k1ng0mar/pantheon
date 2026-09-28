//! `pantheon init`: idempotent, non-interactive bootstrap of the always-on
//! gateway service (chat surfaces + scheduler loop).
//!
//! Detects the platform's service manager and installs accordingly —
//! systemd user unit, cron `@reboot` fallback, launchd agent, or Windows
//! Task Scheduler — sharing the same jobs and durable claim ledger as
//! `pantheon schedule tick`. Re-running converges: it updates the install
//! and ensures the service is enabled and running, never duplicating it.
//! With no supported manager it fails open, printing the manual cron line.
//! Never prompts; always safe to call.

use pantheon_gateway::{install_service, self_exe, InstallOutcome};

fn print_usage() {
    eprintln!(
        "usage: pantheon init\n\n\
         Install the Pantheon gateway as an always-on user service\n\
         (chat surfaces + scheduled tasks), idempotently:\n\
         - Linux + systemd: user unit + `systemctl --user enable --now`\n\
         - Linux w/o systemd: cron `@reboot` entry\n\
         - macOS: LaunchAgent + `launchctl bootstrap`\n\
         - Windows: Task Scheduler logon task\n\
         - otherwise: prints the manual cron line instead of failing\n\n\
         Re-running updates the install and ensures it is running.\n\
         See also: pantheon gateway status|restart"
    );
}

/// Entry point for `pantheon init`. `args` is the full argv.
pub fn cmd_init(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }
    if let Some(extra) = args.get(2) {
        eprintln!("init: unexpected argument '{extra}'");
        print_usage();
        std::process::exit(2);
    }
    let data_dir = crate::terminal::data_dir();
    let exe = match self_exe() {
        Some(e) => e,
        None => {
            eprintln!("init: cannot resolve the pantheon binary path");
            std::process::exit(1);
        }
    };
    match install_service(&data_dir, &exe) {
        InstallOutcome::Installed { mechanism, changed } => {
            if changed {
                println!(
                    "init: gateway service installed via {}.",
                    mechanism.as_str()
                );
            } else {
                println!(
                    "init: gateway service already installed via {}; ensured running.",
                    mechanism.as_str()
                );
            }
            println!("      status: pantheon gateway status");
            println!("      restart: pantheon gateway restart");
        }
        InstallOutcome::Unavailable { note } => {
            // Fail open: no manager is not an error, just a manual setup.
            println!("init: {note}");
        }
        InstallOutcome::Failed { mechanism, error } => {
            eprintln!("init: install via {} failed: {error}", mechanism.as_str());
            std::process::exit(1);
        }
    }
}
