//! `pantheon uninstall [--yes] [--include-secrets]`.
//!
//! Removes Pantheon's config and data, and tells the operator exactly how
//! to finish the job by hand. This verb never edits shell rc files: PATH
//! lines are printed as instructions, because a program rewriting the
//! user's shell config on the way out is how uninstalls go wrong.
//!
//! What is removed with `--yes`:
//! - `<data_dir>/config.toml` and every database (`*.db`)
//! - every subdirectory of the data dir (backups, gateway state, skills,
//!   extensions, mcp state, logs)
//! - `<data_dir>/.env` and `<data_dir>/.secrets.key` ONLY with
//!   `--include-secrets` (default: kept — secrets are the one thing an
//!   uninstall must not surprise-delete)
//!
//! What it does NOT do (printed as instructions instead):
//! - delete the `pantheon` binary (a running binary removing itself is
//!   platform-dependent; the path is printed)
//! - remove the PATH line from shell rc files (printed verbatim)

use std::path::PathBuf;

pub fn usage() -> &'static str {
    "usage: pantheon uninstall [--yes] [--include-secrets]\n\
     \n\
     Remove Pantheon's config and data from this machine.\n\
     Without --yes this only lists what would be removed.\n\
     Secrets (.env, .secrets.key) are kept unless --include-secrets is passed.\n\
     The binary and shell PATH lines are never touched; the exact\n\
     removal steps are printed instead."
}

/// What uninstall would delete, for the dry-run listing and the --yes
/// confirmation. Secret files (`.env`, `.secrets.key`) are excluded unless
/// `include_secrets`.
fn planned_removals(data_dir: &std::path::Path, include_secrets: bool) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let entries = std::fs::read_dir(data_dir).map(|r| r.flatten().collect::<Vec<_>>());
    for e in entries.unwrap_or_default() {
        let p = e.path();
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Secrets are the one thing an uninstall must not surprise-delete.
        if (name == ".env" || name == ".secrets.key") && !include_secrets {
            continue;
        }
        out.push(p);
    }
    out.sort();
    out
}

fn shell_rc_candidates() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return Vec::new();
    }
    let shell = std::env::var("SHELL").unwrap_or_default();
    let base = std::path::Path::new(&shell)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut out = Vec::new();
    match base.as_str() {
        "zsh" => out.push(format!("{home}/.zshrc")),
        "fish" => out.push(format!("{home}/.config/fish/config.fish")),
        _ => out.push(format!("{home}/.bashrc")),
    }
    // Always mention the other two: SHELL can be wrong (login vs
    // interactive shell), and the installer may have written elsewhere.
    for extra in [".bashrc", ".zshrc", ".config/fish/config.fish"] {
        let p = format!("{home}/{extra}");
        if !out.iter().any(|q| q == &p) {
            out.push(p);
        }
    }
    out.into_iter().map(PathBuf::from).collect()
}

pub fn cmd_uninstall(args: &[String]) {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", usage());
        return;
    }
    if let Some(other) = args
        .iter()
        .skip(2)
        .find(|a| a.starts_with("--") && *a != "--yes" && *a != "--include-secrets")
    {
        eprintln!("uninstall: unknown flag '{other}'");
        eprintln!("{usage}", usage = usage());
        std::process::exit(2);
    }
    let yes = args.iter().any(|a| a == "--yes");
    let include_secrets = args.iter().any(|a| a == "--include-secrets");
    let data_dir = crate::terminal::data_dir();
    let removals = planned_removals(&data_dir, include_secrets);

    let bin = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<could not locate the pantheon binary>".to_string());
    let bin_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.display().to_string()))
        .unwrap_or_else(|| "$HOME/.local/bin".to_string());

    if !yes {
        println!("would remove (re-run with --yes to do it):");
        if removals.is_empty() {
            println!("  (nothing: {} is empty or missing)", data_dir.display());
        }
        for p in &removals {
            println!("  {}", p.display());
        }
        if !include_secrets {
            for secret in [".env", ".secrets.key"] {
                if data_dir.join(secret).is_file() {
                    println!(
                        "  (keeping {} — pass --include-secrets to remove it)",
                        data_dir.join(secret).display()
                    );
                }
            }
        }
        println!();
        println!("manual steps (never done automatically):");
        println!("  1. delete the binary: rm {bin}");
        println!("  2. remove the PATH line the installer added. Look for this line");
        println!("     (and the '# added by the pantheon installer' marker above it):");
        println!("       export PATH=\"{bin_dir}:$PATH\"");
        println!("     in:");
        for rc in shell_rc_candidates() {
            println!("       {}", rc.display());
        }
        return;
    }

    // --yes: data removal. The operator was told to stop every Pantheon
    // process first (see the dry-run output); deleting databases out from
    // under a running gateway corrupts in ways a restore cannot fix, so
    // the instruction is repeated, not just implied.
    if removals
        .iter()
        .any(|p| p.file_name().map(|n| n == "ledger.db").unwrap_or(false))
    {
        eprintln!("note: make sure no Pantheon process is running (serve, gateway, sessions)");
        eprintln!("before its databases are deleted from under it.");
    }
    let mut removed = 0;
    for p in &removals {
        let res = if p.is_dir() {
            std::fs::remove_dir_all(p)
        } else {
            std::fs::remove_file(p)
        };
        match res {
            Ok(()) => {
                removed += 1;
            }
            Err(e) => {
                eprintln!("uninstall: could not remove {}: {e}", p.display());
                std::process::exit(1);
            }
        }
    }
    println!("removed {removed} item(s) from {}", data_dir.display());
    for secret in [".env", ".secrets.key"] {
        if !include_secrets && data_dir.join(secret).is_file() {
            println!(
                "kept {} (secrets); re-run with --include-secrets to remove it",
                data_dir.join(secret).display()
            );
        }
    }
    println!();
    println!("manual steps to finish:");
    println!("  1. delete the binary: rm {bin}");
    println!("  2. remove this line (and its '# added by the pantheon installer' marker)");
    println!("     from your shell rc file:");
    println!("       export PATH=\"{bin_dir}:$PATH\"");
    for rc in shell_rc_candidates() {
        println!("       check {}", rc.display());
    }
}
