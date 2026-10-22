//! `pantheon worktree`: first-class git worktrees for parallel work.
//!
//! Managed worktrees live under `<data_dir>/worktrees/<repo>-<hash>/`
//! (not inside the repository), so a worktree never becomes part of the
//! checkout it was created from. Branches are created in the
//! `pantheon/<name>` namespace. `path` prints the managed path so a
//! shell can `cd "$(pantheon worktree path review)"`; `create` also
//! accepts `--path` for an explicit location.

use crate::terminal::data_dir;
use pantheon_exec::worktree::WorktreeManager;
use std::path::PathBuf;

pub fn cmd_worktree(args: &[String]) {
    let verb = args.first().map(|s| s.as_str()).unwrap_or("list");
    if matches!(verb, "help" | "--help" | "-h") {
        help();
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let dd = data_dir();
    let manager = match WorktreeManager::new(&cwd, &dd) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("worktree: {e}");
            std::process::exit(1);
        }
    };
    match verb {
        "list" | "" => list(&manager, args),
        "create" => create(&manager, args),
        "remove" | "rm" => remove(&manager, args),
        "path" => paths(&manager, args),
        "enter" => enter(&manager, args),
        "root" => println!("{}", manager.root().display()),
        other => {
            eprintln!("worktree: unknown verb '{other}'");
            eprintln!("  usage: pantheon workspace worktree <list|create|remove|path|enter|root>");
            std::process::exit(2);
        }
    }
}

fn help() {
    println!(concat!(
        "pantheon workspace worktree <command> [args]\n\n",
        "Commands:\n",
        "  list [--json]                  list main and linked worktrees\n",
        "  create <name> [--base REF] [--path DIR]\n",
        "                                  create pantheon/<name> and check it out\n",
        "  remove <name|path> [--force]   remove a linked worktree\n",
        "  path <name>                    print the worktree path\n",
        "  enter <name> [-- args...]      launch a session rooted in the worktree\n",
        "  root                           print the managed worktree root"
    ));
}

fn has_json(args: &[String]) -> bool {
    args.iter().any(|a| a == "--json")
}

fn list(manager: &WorktreeManager, args: &[String]) {
    match manager.list() {
        Ok(items) => {
            if has_json(args) {
                match serde_json::to_string_pretty(&items) {
                    Ok(s) => println!("{s}"),
                    Err(e) => eprintln!("worktree: encode failed: {e}"),
                }
                return;
            }
            if items.is_empty() {
                println!("no worktrees");
                return;
            }
            for w in items {
                let branch = w.branch.as_deref().unwrap_or("(detached)");
                let tag = if w.main { " main" } else { "" };
                println!(
                    "{:<8} {:<32} {}{}",
                    w.head.chars().take(8).collect::<String>(),
                    branch,
                    w.path.display(),
                    tag
                );
                if w.locked {
                    println!("  (locked)");
                }
                if w.prunable {
                    println!("  (prunable)");
                }
            }
        }
        Err(e) => {
            eprintln!("worktree: list failed: {e}");
            std::process::exit(1);
        }
    }
}

fn need_name(args: &[String], verb: &str) -> String {
    match args.get(1) {
        Some(n) if !n.starts_with('-') => n.clone(),
        _ => {
            eprintln!("worktree {verb}: needs a name");
            std::process::exit(2);
        }
    }
}

fn create(manager: &WorktreeManager, args: &[String]) {
    let name = need_name(args, "create");
    let mut base: Option<&str> = None;
    let mut path: Option<PathBuf> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--base" => {
                base = args.get(i + 1).map(String::as_str);
                if base.is_none() {
                    eprintln!("worktree create: --base needs a revision");
                    std::process::exit(2);
                }
                i += 2;
            }
            "--path" => {
                path = args.get(i + 1).map(PathBuf::from);
                if path.is_none() {
                    eprintln!("worktree create: --path needs a directory");
                    std::process::exit(2);
                }
                i += 2;
            }
            "--json" => i += 1,
            other => {
                eprintln!("worktree create: unknown option '{other}'");
                eprintln!("  usage: pantheon worktree create <name> [--base REV] [--path DIR]");
                std::process::exit(2);
            }
        }
    }
    match manager.create(&name, base, path.as_deref()) {
        Ok(w) => {
            if has_json(args) {
                match serde_json::to_string_pretty(&w) {
                    Ok(s) => println!("{s}"),
                    Err(e) => eprintln!("worktree: encode failed: {e}"),
                }
            } else {
                println!("created {}", w.path.display());
                if let Some(b) = w.branch {
                    println!("branch  {b}");
                }
                println!("next    cd \"$(pantheon worktree path {name})\"");
            }
        }
        Err(e) => {
            eprintln!("worktree: create failed: {e}");
            std::process::exit(1);
        }
    }
}

fn remove(manager: &WorktreeManager, args: &[String]) {
    let target = need_name(args, "remove");
    let force = args.iter().any(|a| a == "--force" || a == "-f");
    match manager.remove(&target, force) {
        Ok(w) => println!("removed {}", w.path.display()),
        Err(e) => {
            eprintln!("worktree: remove failed: {e}");
            std::process::exit(1);
        }
    }
}

/// `enter <name> [-- <args>...]`: launch a fresh Pantheon session with
/// the worktree as its process cwd. Process cwd is Pantheon's workspace
/// boundary, so this is session relocation without mutating the running
/// process or silently changing another session's relative paths.
fn enter(manager: &WorktreeManager, args: &[String]) {
    let name = need_name(args, "enter");
    let path = match manager.find(&name) {
        Ok(Some(w)) => w.path,
        Ok(None) => match manager.path_for(&name) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("worktree: {e}");
                std::process::exit(1);
            }
        },
        Err(e) => {
            eprintln!("worktree: {e}");
            std::process::exit(1);
        }
    };
    if !path.is_dir() {
        eprintln!(
            "worktree: {} does not exist; create it with `pantheon worktree create {}`",
            path.display(),
            name
        );
        std::process::exit(1);
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("worktree: locate current executable: {e}");
            std::process::exit(1);
        }
    };
    println!("entering {}", path.display());
    let mut cmd = std::process::Command::new(exe);
    cmd.current_dir(&path);
    if let Some(pos) = args.iter().position(|a| a == "--") {
        cmd.args(&args[pos + 1..]);
    }
    match cmd.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => {
            eprintln!("worktree: launch session: {e}");
            std::process::exit(1);
        }
    }
}

fn paths(manager: &WorktreeManager, args: &[String]) {
    let name = need_name(args, "path");
    let path = match manager.find(&name) {
        Ok(Some(w)) => Ok(w.path),
        Ok(None) => manager.path_for(&name),
        Err(e) => Err(e),
    };
    match path {
        Ok(p) => println!("{}", p.display()),
        Err(e) => {
            eprintln!("worktree: {e}");
            std::process::exit(1);
        }
    }
}
