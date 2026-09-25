//! `pantheon skills`: Tier 1 skill lifecycle.
//!
//! Three verbs:
//!   list    — every discovered skill, with scope and origin provenance.
//!   import  — copy an external SKILL.md into <data_dir>/skills so it
//!             survives across processes (cross-format: Hermes, OpenClaw,
//!             `.agents`, `.claude`, native).
//!   doctor  — loud preflight: every discovered skill parsed, every broken
//!             one named with its error code.
//!
//! All three are read-only on the source side; `import` is the only op
//! that writes, and only into <data_dir>/skills.

use crate::data_dir;
use pantheon_core::error::PantheonError;
use pantheon_exec::skills::{discover_skills_ext, import_skill, SkillSource};
use std::path::{Path, PathBuf};

/// Extra discovery roots beyond the built-in cross-tool scopes.
fn extra_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(d) = std::env::var("PANTHEON_SKILLS_DIR") {
        for entry in d.split(':').filter(|s| !s.is_empty()) {
            roots.push(PathBuf::from(entry));
        }
    }
    roots
}

fn project_root() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| data_dir())
}

fn scope_label(path: &Path) -> String {
    path.parent()
        .and_then(|p| p.file_name())
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// List every discovered skill.
pub fn cmd_skills_list(_args: &[String]) {
    let dd = data_dir();
    let found = discover_skills_ext(&dd, &project_root(), &extra_roots());
    if found.is_empty() {
        println!("no skills discovered");
        return;
    }
    println!("{:24} {:12} {:12} {}", "NAME", "SCOPE", "ORIGIN", "DESCRIPTION");
    for s in &found {
        println!(
            "{:24} {:12} {:12} {}",
            s.meta.name,
            scope_label(&s.path),
            s.meta.origin,
            s.meta.description
        );
    }
}

/// Import one named skill into <data_dir>/skills. Errors if unknown.
///
/// Four modes:
///   `pantheon skills import <name> [--scope DIR]`
///       discover a local skill by name and copy it.
///   `pantheon skills import --url <URL> [name]`
///       fetch a single SKILL.md from a URL and import it.
///   `pantheon skills import --repo <URL> [--sub DIR]`
///       shallow-clone a repo and import every SKILL.md found in it.
///   `pantheon skills import --clawhub <slug> [--owner OWNER]`
///       import a skill from the OpenClaw ClawHub registry (public API).
pub fn cmd_skills_import(args: &[String]) {
    if args.len() < 2 {
        eprintln!("usage: pantheon skills import <name> [--scope DIR] | --url <URL> [name] | --repo <URL> [--sub DIR] | --clawhub <slug> [--owner OWNER]");
        std::process::exit(2);
    }
    let dd = data_dir();

    if let Some(url) = flag_value(args, "--repo") {
        let sub = flag_value(args, "--sub");
        match pantheon_exec::skills::import_skills_from_repo(&dd, &url, sub.as_deref()) {
            Ok(items) => {
                if items.is_empty() {
                    println!("no skills found in {url}");
                } else {
                    for (n, p) in &items {
                        println!("imported {n} -> {}", p.display());
                    }
                    println!("{} skill(s) imported from {url}", items.len());
                }
            }
            Err(e) => {
                eprintln!("skills import: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(slug) = flag_value(args, "--clawhub") {
        let owner = flag_value(args, "--owner").map(|s| s.to_string());
        match pantheon_exec::skills::import_skill_from_clawhub(&dd, &slug, owner.as_deref()) {
            Ok((path, skill)) => println!("imported {} <- clawhub://{slug} -> {}", skill.meta.name, path.display()),
            Err(e) => {
                eprintln!("skills import: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(url) = flag_value(args, "--url") {
        let name = flag_value(args, "--name");
        match pantheon_exec::skills::import_skill_from_url(&dd, &url) {
            Ok((path, skill)) => {
                let n = name.unwrap_or_else(|| skill.meta.name.clone());
                println!("imported {n} <- {url} -> {}", path.display());
            }
            Err(e) => {
                eprintln!("skills import: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    let name = &args[1];
    let mut scope: Option<PathBuf> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--scope" => {
                i += 1;
                if i < args.len() {
                    scope = Some(PathBuf::from(&args[i]));
                }
            }
            _ => {}
        }
        i += 1;
    }
    let mut roots: Vec<PathBuf> = extra_roots();
    if let Some(s) = scope {
        roots.push(s);
    }
    let found = discover_skills_ext(&dd, &project_root(), &roots);
    let skill = found.iter().find(|s| s.meta.name == *name).unwrap_or_else(|| {
        eprintln!("skills import: no skill named '{name}'");
        std::process::exit(1);
    });
    match import_skill(&dd, skill) {
        Ok(p) => println!("imported {} -> {}", name, p.display()),
        Err(e) => {
            eprintln!("skills import: {e}");
            std::process::exit(1);
        }
    }
}

/// Extract a `--flag value` pair from the arg list, returning the value.
fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|p| args.get(p + 1))
        .filter(|s| !s.starts_with("--"))
        .map(|s| s.to_string())
}
pub fn cmd_skills_doctor(_args: &[String]) {
    let dd = data_dir();
    let found = discover_skills_ext(&dd, &project_root(), &extra_roots());
    let mut errors = 0usize;
    for s in &found {
        match pantheon_exec::skills::load_skill(&s.path) {
            Some(_) => println!("ok   {} ({})", s.meta.name, s.meta.origin),
            None => {
                errors += 1;
                eprintln!("fail {} — broken SKILL.md", s.meta.name);
            }
        }
    }
    if found.is_empty() {
        println!("(no skills discovered)");
    }
    if errors > 0 {
        eprintln!("{} broken skill(s)", errors);
        std::process::exit(1);
    }
}

/// Resolve a `SkillSource` from a name, for `--scope` provenance tagging.
#[allow(dead_code)]
pub fn parse_scope(s: &str) -> Option<SkillSource> {
    match s.to_lowercase().as_str() {
        "hermes" => Some(SkillSource::Hermes),
        "openclaw" => Some(SkillSource::OpenClaw),
        "agents" => Some(SkillSource::Agents),
        "claude" => Some(SkillSource::Claude),
        "external" => Some(SkillSource::External),
        _ => None,
    }
}

/// Top-level error helper for the skills verb.
fn _aerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(code, pantheon_core::error::Layer::Execution, false, cause, "see docs", "")
}