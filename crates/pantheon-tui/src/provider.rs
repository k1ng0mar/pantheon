//! `pantheon provider`: the custom-endpoint registry.
//!
//! Builtins live in the catalog; anything else lives here, as
//! `[custom_providers.<name>]` rows in config.toml (plus keys and
//! endpoint values in `<data_dir>/.env`):
//!
//!   pantheon provider add [--name N] [--base-url U|:port] [--api-mode M]
//!                         [--key K1,K2] [--api-key-env E] [--set VAR=VAL]...
//!   pantheon provider list
//!   pantheon provider remove [NAME] [--delete-key]
//!
//! No subcommand (or `list`) prints the registry. `add` without flags is
//! interactive (TTY picker for the wire mode, prompts for the rest).
//! `remove` without a name opens a picker over the customs. Overriding a
//! builtin id (e.g. pointing `openai` at a proxy) is allowed but always
//! announced. Selecting models stays in `pantheon model`.

use super::config::{self, Config};
use crate::model::{
    builtin_provider_id, ensure_template_vars, flag, flags, has_flag, key_status,
    normalize_base_url, parse_api_mode, parse_set_pairs, pick, print_template_status, prompt_line,
    valid_provider_id, wire_mode_items, PickItem,
};
use pantheon_providers::catalog::{self, ApiMode};
use std::io::IsTerminal;

fn data_dir() -> std::path::PathBuf {
    crate::terminal::data_dir()
}

/// `[custom_providers.*]` names, sorted. Shared with the model picker.
pub(crate) fn custom_names(data_dir: &std::path::Path) -> Vec<String> {
    Config::load(data_dir)
        .map(|c| {
            let mut n: Vec<String> = c.custom_providers.keys().cloned().collect();
            n.sort();
            n
        })
        .unwrap_or_default()
}

fn api_mode_flag(rest: &[String]) -> ApiMode {
    parse_api_mode(flag(rest, "--api-mode").as_deref())
}

fn parse_sets(rest: &[String]) -> Vec<(String, String)> {
    parse_set_pairs(flags(rest, "--set"))
}

/// Persist one custom row + register it in-memory. Returns whether the id
/// shadows a builtin (the caller announces it).
fn save_provider(
    data_dir: &std::path::Path,
    name: &str,
    base_url: &str,
    api_mode: ApiMode,
    key_env: &str,
) -> Result<bool, String> {
    config::upsert_custom_row(data_dir, name, base_url, api_mode, key_env)
}

fn cmd_list() {
    let cfg = Config::load(&data_dir()).unwrap_or_default();
    let names = custom_names(&data_dir());
    if names.is_empty() {
        println!("custom providers: none");
    } else {
        println!("custom providers:");
        for n in &names {
            let s = &cfg.custom_providers[n];
            println!(
                "  {n}: {} ({})  [{}]",
                s.base_url,
                s.api_mode,
                key_status(s.key_env.as_deref().or(Some(&format!(
                    "PANTHEON_KEY_{}",
                    config::sanitize_env_suffix(n)
                ))))
            );
            print_template_status(n);
        }
    }
    println!("builtins: pantheon providers  ·  select: pantheon model");
}

fn cmd_add(rest: &[String]) {
    let dd = data_dir();
    let name_flag = flag(rest, "--name").or_else(|| flag(rest, "--provider"));
    let interactive =
        name_flag.is_none() && flag(rest, "--base-url").is_none() && flag(rest, "--key").is_none();
    if interactive && !std::io::stdin().is_terminal() {
        eprintln!("no TTY: pass --name + --base-url (see `pantheon provider add --help`)");
        std::process::exit(2);
    }

    // 1. name.
    let name = match name_flag {
        Some(n) if !n.trim().is_empty() => n.trim().to_string(),
        _ if !interactive => {
            eprintln!("provider add needs --name");
            std::process::exit(2);
        }
        _ => loop {
            let n = prompt_line("Custom provider name (e.g. my-llm)", "");
            if !valid_provider_id(&n) {
                eprintln!("  use letters/digits/dash/underscore, no spaces or ':'");
            } else {
                break n;
            }
        },
    };
    if !valid_provider_id(&name) {
        eprintln!(
            "bad provider name {name:?}: use letters/digits/dash/underscore, no spaces or ':'"
        );
        std::process::exit(2);
    }
    let overriding = builtin_provider_id(&name);
    if overriding {
        if interactive {
            let a = prompt_line(
                &format!("'{name}' is a builtin — override it with this endpoint? (y/N)"),
                "n",
            );
            if !matches!(a.to_ascii_lowercase().as_str(), "y" | "yes") {
                println!("cancelled — nothing changed");
                return;
            }
        } else {
            println!("note: overriding builtin '{name}' (visible in `pantheon provider list`)");
        }
    }

    // 2. base URL.
    let url = match flag(rest, "--base-url") {
        Some(u) if !u.trim().is_empty() => normalize_base_url(&u),
        _ if !interactive => {
            eprintln!("provider add needs --base-url");
            std::process::exit(2);
        }
        _ => loop {
            let u = prompt_line("Base URL or :port for localhost (e.g. :8015)", "");
            if !u.is_empty() {
                break normalize_base_url(&u);
            }
            eprintln!("  base URL is required");
        },
    };

    // 3. wire mode.
    let api_mode = if flag(rest, "--api-mode").is_some() || !interactive {
        api_mode_flag(rest)
    } else {
        match pick("wire mode", &wire_mode_items()) {
            Some(1) => ApiMode::Anthropic,
            _ => ApiMode::OpenAi,
        }
    };

    // 4. key env + keys.
    let stored_key_env = Config::load(&dd)
        .ok()
        .and_then(|c| {
            c.custom_providers
                .get(&name)
                .and_then(|s| s.key_env.clone())
        })
        .filter(|s| !s.is_empty())
        .or_else(|| {
            catalog::provider(&name)
                .map(|p| p.key_env)
                .filter(|s| !s.is_empty())
        });
    let key_env_default = config::provider_key_env(&name, stored_key_env.as_deref());
    let (key_env, raw_keys) = if interactive {
        let ke = prompt_line("Key env var", &key_env_default);
        let existing = super::dotenv::read_dotenv_value(&dd, &ke).filter(|v| !v.trim().is_empty());
        if let Some(cur) = &existing {
            println!("  current: {} (empty keeps)", crate::model::mask_key(cur));
        }
        let keys = prompt_line("API key(s), comma-separated to stack", "");
        let raw = if keys.trim().is_empty() {
            if existing.is_none() {
                println!("  no key stored — keyless endpoints only");
            }
            None
        } else {
            Some(keys.trim().to_string())
        };
        (ke, raw)
    } else {
        let ke = flag(rest, "--api-key-env").unwrap_or(key_env_default);
        let raw = flag(rest, "--key")
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        (ke, raw)
    };
    if let Some(keys) = &raw_keys {
        if let Err(e) = super::dotenv::persist_dotenv_value(&dd, &key_env, keys.trim()) {
            eprintln!("provider add: write .env: {e}");
            std::process::exit(1);
        }
    }

    // 5. template values (`{region}` etc.).
    let sets = parse_sets(rest);
    if let Err(e) = ensure_template_vars(&dd, &name, &url, &sets, interactive) {
        eprintln!("endpoint config: {e}");
        std::process::exit(2);
    }

    match save_provider(&dd, &name, &url, api_mode, &key_env) {
        Ok(true) => println!(
            "overrode builtin '{name}': {url}  [keys → {}]",
            dd.join(".env").display()
        ),
        Ok(false) => println!(
            "added custom provider '{name}': {url}  [keys → {}]  — select it with `pantheon model`",
            dd.join(".env").display()
        ),
        Err(e) => {
            eprintln!("provider add: {e}");
            std::process::exit(1);
        }
    }
}

fn cmd_remove(rest: &[String]) {
    let dd = data_dir();
    let name = match flag(rest, "--remove")
        .or_else(|| flag(rest, "--name"))
        .or_else(|| rest.first().cloned())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && !s.starts_with("--"))
    {
        Some(n) => n,
        None => {
            // Picker when no name given.
            if !std::io::stdin().is_terminal() {
                eprintln!("usage: pantheon provider remove NAME [--delete-key]");
                std::process::exit(2);
            }
            let names = custom_names(&dd);
            if names.is_empty() {
                println!("custom providers: none");
                return;
            }
            let items: Vec<PickItem> = names
                .iter()
                .map(|n| PickItem {
                    label: n.clone(),
                    desc: "delete endpoint + config row".into(),
                })
                .collect();
            let Some(idx) = pick("remove which custom provider?", &items) else {
                println!("cancelled — nothing changed");
                return;
            };
            names[idx].clone()
        }
    };
    let delete_key = has_flag(rest, "--delete-key");
    if let Err(e) = crate::model::remove_custom_provider(&dd, &name, Some(delete_key)) {
        eprintln!("remove: {e}");
        std::process::exit(1);
    }
}

fn usage() -> ! {
    eprintln!("usage: pantheon provider <add|list|remove> [options]");
    eprintln!("  add [--name N] [--base-url U|:port] [--api-mode openai|anthropic]");
    eprintln!("      [--key K1,K2] [--api-key-env E] [--set VAR=VAL]...");
    eprintln!("  list                        custom endpoints (builtins: pantheon providers)");
    eprintln!("  remove [NAME] [--delete-key]");
    std::process::exit(2);
}

pub fn cmd_provider(args: &[String]) {
    let dd = data_dir();
    config::init_env_and_catalog(&dd);
    let rest: Vec<String> = args.iter().skip(2).cloned().collect();
    match rest.first().map(|s| s.as_str()) {
        None | Some("list") => cmd_list(),
        Some("add") => cmd_add(&rest[1..]),
        Some("remove") => cmd_remove(&rest[1..]),
        Some("models") => cmd_models(&dd, &rest[1..]),
        Some("--help") | Some("-h") | Some("help") => usage(),
        Some(other) => {
            eprintln!("unknown provider subcommand {other:?} (add|list|remove|models)");
            std::process::exit(2);
        }
    }
}

/// `pantheon provider models <name>` — the live model list for one endpoint.
///
/// Fetched from the endpoint, not read from config. A custom endpoint's models
/// change without notice (aggregators add and retire dozens a week), so a
/// stored list is only ever a record of what the operator has named, never the
/// authority on what the endpoint serves. The remembered names are shown
/// alongside, marked, so the two are never confused.
fn cmd_models(dd: &std::path::Path, args: &[String]) {
    let name = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .cloned()
        .unwrap_or_default();
    if name.is_empty() {
        eprintln!("usage: pantheon provider models <provider>");
        std::process::exit(2);
    }
    let cfg = config::Config::load(dd).unwrap_or_default();
    let Some(sec) = cfg.custom_providers.get(&name) else {
        eprintln!("no custom provider {name:?}");
        eprintln!(
            "  known: {}",
            cfg.custom_providers
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
        std::process::exit(1);
    };
    let key_env = sec
        .key_env
        .clone()
        .unwrap_or_else(|| format!("PANTHEON_KEY_{}", name.to_uppercase()));
    let key = std::env::var(&key_env).unwrap_or_default();

    let remembered: Vec<String> = sec.models.iter().map(|m| m.id.clone()).collect();
    println!("{name}: {}", sec.base_url);
    let mode = match sec.api_mode.as_str() {
        "anthropic" => ApiMode::Anthropic,
        _ => ApiMode::OpenAi,
    };
    match crate::model::fetch_models(&sec.base_url, &key, mode) {
        Ok(ids) => {
            println!("  {} model(s) live at the endpoint", ids.len());
            for id in &ids {
                let mark = if remembered.iter().any(|r| r == id) {
                    "*"
                } else {
                    " "
                };
                println!("   {mark} {id}");
            }
            let missing: Vec<&String> = remembered.iter().filter(|r| !ids.contains(r)).collect();
            if !missing.is_empty() {
                println!(
                    "  * recorded but not offered now: {}",
                    missing
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
            println!("  (* = a model you have named on this endpoint)");
        }
        Err(e) => {
            println!("  live fetch failed: {e}");
            if remembered.is_empty() {
                println!("  no models recorded either; name one with `pantheon model`");
            } else {
                println!("  falling back to {} recorded model(s):", remembered.len());
                for r in &remembered {
                    println!("    {r}");
                }
            }
        }
    }
}
