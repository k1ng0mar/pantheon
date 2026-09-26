//! `pantheon fallback <add|list|remove>` — the ordered provider/model
//! fallback chain.
//!
//! The chain already exists in config as `[model].fallbacks` and is honoured
//! by the provider chain at runtime; it just had no surface. Without one, the
//! only way to change it was hand-editing config.toml, which is exactly the
//! kind of thing that drifts and then silently does nothing.
//!
//! Order is the contract: the runtime walks the list in order and only moves
//! to the next entry when the previous one fails. So `add` appends and
//! `remove` takes an index, not a name — two entries can legitimately name
//! the same provider with different models.

use crate::config_doc::{Config, FallbackEntry};
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
    let dd = crate::data_dir();
    // One load, one save: read-modify-write so a concurrent edit cannot be
    // half-applied, and so validation runs against what is actually on disk.
    let mut cfg = match Config::load(&dd) {
        Ok(c) => c,
        Err(e) if e.code == "CONFIG_OPEN" => {
            eprintln!("no config at {} — run `pantheon setup` first", dd.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("read config: {e}");
            std::process::exit(1);
        }
    };

    // A fallback chain is meaningless without a default to fall back from,
    // so create the section rather than erroring on a config that has none.
    if cfg.model.is_none() {
        cfg.model = Some(crate::config_doc::ModelSection {
            provider: String::new(),
            model: String::new(),
            api_key_env: None,
            fallbacks: Vec::new(),
        });
    }
    // Named `section` not `model`: the `add`/`insert` arms bind a `model`
    // string parameter, which would otherwise shadow this.
    let section = cfg.model.as_mut().expect("just ensured");

    match sub {
        "list" => {
            let chain = section.fallbacks.clone();
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
            if at > section.fallbacks.len() {
                eprintln!(
                    "index {at} is past the end (chain has {} entries)",
                    section.fallbacks.len()
                );
                std::process::exit(2);
            }
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

fn save(dd: &Path, cfg: &Config) {
    if let Err(e) = cfg.save(dd) {
        eprintln!("write config: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
#[path = "fallback_cli_tests.rs"]
mod tests;
