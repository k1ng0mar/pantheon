//! `pantheon fallback <add|list|remove>` - the ordered provider/model
//! fallback chain.
//!
//! The chain already exists in config as `[model].fallbacks` and is honoured
//! by the provider chain at runtime; it just had no surface. Without one, the
//! only way to change it was hand-editing config.toml, which is exactly the
//! kind of thing that drifts and then silently does nothing.
//!
//! Order is the contract: the runtime walks the list in order and only moves
//! to the next entry when the previous one fails. So `add` appends and
//! `remove` takes an index, not a name - two entries can legitimately name
//! the same provider with different models.

use crate::config::{Config, FallbackEntry};
use std::path::Path;

fn usage() -> &'static str {
    "usage:\n  \
     pantheon fallback list\n  \
     pantheon fallback add <provider> <model>\n  \
     pantheon fallback remove <index>\n  \
     pantheon fallback insert <index> <provider> <model>"
}

pub fn cmd_fallback(args: &[String]) {
    let sub = match args.get(2).map(String::as_str) {
        Some(s) => s,
        None => {
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };
    let dd = crate::terminal::data_dir();
    // One load, one save: read-modify-write so a concurrent edit cannot be
    // half-applied, and so validation runs against what is actually on disk.
    let mut cfg = match Config::load(&dd) {
        Ok(c) => c,
        Err(e) if e.code == "CONFIG_OPEN" => {
            eprintln!("no config at {} - run `pantheon setup` first", dd.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("read config: {e}");
            std::process::exit(1);
        }
    };

    // A fallback chain is meaningless without a default to fall back
    // from, and creating `[model]` here with empty provider/model used
    // to poison resolution: the empty config values beat the environment
    // and the local default in `build_model_policy`. So nothing here
    // ever creates a `[model]` section: `list` reads the existing chain
    // (or nothing), `remove` without a section is an out-of-range
    // error, and `add`/`insert` refuse until a real default model
    // exists. Named `section` not `model`: the `add`/`insert` arms bind
    // a `model` string parameter, which would otherwise shadow this.
    // The write arms take their `&mut` borrow inside the arm, after
    // argument validation, so a bad invocation never mutates `cfg`.
    match sub {
        "list" => {
            let chain = cfg
                .model
                .as_ref()
                .map(|m| m.fallbacks.clone())
                .unwrap_or_default();
            if chain.is_empty() {
                println!("no fallbacks configured");
                println!("add one: pantheon fallback add <provider> <model>");
                return;
            }
            println!("fallback chain (tried in order after the default fails):");
            for (i, f) in chain.iter().enumerate() {
                println!("  {i}  {} / {}", f.provider, f.model);
            }
        }
        "add" => {
            let (provider, model) = match (args.get(3), args.get(4)) {
                (Some(p), Some(m)) => (p.clone(), m.clone()),
                _ => {
                    eprintln!("{}", usage());
                    std::process::exit(2);
                }
            };
            if provider.trim().is_empty() || model.trim().is_empty() {
                eprintln!("provider and model must both be non-empty");
                std::process::exit(2);
            }
            let Some(section) = cfg.model.as_mut() else {
                eprintln!(
                    "no [model] section yet; set a default model first (e.g. `pantheon model`), then add fallbacks"
                );
                std::process::exit(2);
            };
            warn_about(&dd, section, &provider, &model);
            section.fallbacks.push(FallbackEntry { provider, model });
            let n = section.fallbacks.len();
            save(&dd, &cfg);
            println!("fallback {n} added");
        }
        "insert" => {
            let (idx, provider, model) = match (args.get(3), args.get(4), args.get(5)) {
                (Some(i), Some(p), Some(m)) => (i.clone(), p.clone(), m.clone()),
                _ => {
                    eprintln!("{}", usage());
                    std::process::exit(2);
                }
            };
            let at: usize = match idx.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("'{idx}' is not an index");
                    std::process::exit(2);
                }
            };
            let Some(section) = cfg.model.as_mut() else {
                eprintln!(
                    "no [model] section yet; set a default model first (e.g. `pantheon model`), then add fallbacks"
                );
                std::process::exit(2);
            };
            if at > section.fallbacks.len() {
                eprintln!(
                    "index {at} is past the end (chain has {} entries)",
                    section.fallbacks.len()
                );
                std::process::exit(2);
            }
            warn_about(&dd, section, &provider, &model);
            section
                .fallbacks
                .insert(at, FallbackEntry { provider, model });
            let len = section.fallbacks.len();
            save(&dd, &cfg);
            let _ = len;
            println!("fallback inserted at {at}");
        }
        "remove" => {
            let idx = match args.get(3) {
                Some(i) => i.clone(),
                None => {
                    eprintln!("{}", usage());
                    std::process::exit(2);
                }
            };
            let at: usize = match idx.parse() {
                Ok(v) => v,
                Err(_) => {
                    eprintln!("'{idx}' is not an index; run `pantheon fallback list`");
                    std::process::exit(2);
                }
            };
            let Some(section) = cfg.model.as_mut() else {
                eprintln!("index {at} is out of range (no fallbacks configured)");
                std::process::exit(2);
            };
            if at >= section.fallbacks.len() {
                eprintln!(
                    "index {at} is out of range (chain has {} entries)",
                    section.fallbacks.len()
                );
                std::process::exit(2);
            }
            let gone = section.fallbacks.remove(at);
            let label = format!("{} / {}", gone.provider, gone.model);
            save(&dd, &cfg);
            println!("removed {label}");
        }
        other => {
            eprintln!("unknown subcommand '{other}'");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    }
}

/// Warnings for a fallback that would burn retries at runtime:
/// unknown provider, a duplicate of an earlier entry, an entry
/// identical to the default (a fallback to itself), or a provider
/// whose key env var is unset in both the process env and
/// `<data_dir>/.env`. Warnings, not refusals: the chain walk still
/// decides at runtime, but adding a dead entry silently used to cost
/// 3 retries per turn before anyone noticed.
fn warn_about(dd: &Path, section: &crate::config::ModelSection, provider: &str, model: &str) {
    let known = pantheon_providers::catalog::provider(provider).is_some()
        || provider.starts_with("http://")
        || provider.starts_with("https://");
    if !known {
        eprintln!("warning: '{provider}' is not a catalog provider; this fallback will fail as PROVIDER_CONFIG");
    }
    if section.provider == provider && section.model == model {
        eprintln!("warning: fallback {provider}/{model} is identical to the default model; it adds retries, not resilience");
    }
    if section
        .fallbacks
        .iter()
        .any(|f| f.provider == provider && f.model == model)
    {
        eprintln!("warning: fallback {provider}/{model} duplicates an existing entry");
    }
    if known {
        let key_env = pantheon_providers::catalog::provider(provider)
            .map(|p| {
                if p.key_env.trim().is_empty() {
                    crate::config::provider_key_env(provider, None)
                } else {
                    p.key_env.clone()
                }
            })
            .unwrap_or_else(|| crate::config::provider_key_env(provider, None));
        let in_env = std::env::var(&key_env)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .is_some();
        let in_dotenv = std::fs::read_to_string(dd.join(".env"))
            .map(|text| {
                text.lines().any(|line| match line.trim().split_once('=') {
                    Some((name, value)) => {
                        name.trim() == key_env && !value.trim().trim_matches('"').is_empty()
                    }
                    None => false,
                })
            })
            .unwrap_or(false);
        if !in_env && !in_dotenv {
            eprintln!("warning: {key_env} is not set; this fallback will 401 before it can help");
        }
    }
}

fn save(dd: &Path, cfg: &Config) {
    if let Err(e) = cfg.save(dd) {
        eprintln!("write config: {e}");
        std::process::exit(1);
    }
}
