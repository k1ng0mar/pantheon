//! `pantheon auth` — manage provider API keys without hand-editing `.env`.
//!
//! Keys live once per provider under `PANTHEON_KEY_<PROVIDER>` (comma
//! separated when stacked). Aux slots and fallbacks pick providers and
//! models from the catalog; they never own keys, so this verb is the
//! single place keys are managed.

use std::path::Path;

pub fn data_dir() -> std::path::PathBuf {
    if let Some(d) = std::env::var_os("PANTHEON_DATA_DIR") {
        if !d.is_empty() {
            return std::path::PathBuf::from(d);
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        std::path::PathBuf::from(home).join(".pantheon")
    } else {
        std::path::PathBuf::from(".pantheon")
    }
}

fn env_key_name(provider: &str) -> String {
    format!("PANTHEON_KEY_{}", provider.to_ascii_uppercase())
}

fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        "****".to_string()
    } else {
        let head: String = chars.iter().take(4).collect();
        let tail: String = chars.iter().skip(chars.len() - 4).collect();
        format!("{head}...{tail}")
    }
}

fn read_keys(dir: &Path, provider: &str) -> Vec<String> {
    let name = env_key_name(provider);
    let raw = pantheon_api::dotenv::read_dotenv_value(dir, &name).unwrap_or_default();
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn write_keys(dir: &Path, provider: &str, keys: &[String]) {
    let name = env_key_name(provider);
    if keys.is_empty() {
        let _ = pantheon_api::dotenv::delete_dotenv_key(dir, &name);
    } else {
        pantheon_api::dotenv::upsert_dotenv(dir, &name, &keys.join(",")).unwrap_or_else(|e| {
            eprintln!("auth: failed to write {}: {e}", name);
            std::process::exit(1);
        });
    }
}

pub fn cmd_auth(args: &[String]) {
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list");
    match sub {
        "add" => cmd_add(args),
        "list" => cmd_list(args),
        "remove" | "rm" => cmd_remove(args),
        "priority" => cmd_priority(args),
        "reset" => cmd_reset(args),
        "status" => cmd_status(args),
        "policy" => cmd_policy(args),
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("auth: unknown command '{other}'");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!("pantheon auth — provider key pools");
    println!("  add <provider> <key>        stack a key onto the provider pool");
    println!("  list [provider]             show pools with masked previews");
    println!("  remove <provider> [index]   remove one stacked key (default last)");
    println!("  priority <provider> <i> <j> move key at index i to index j");
    println!("  reset <provider>            clear exhaustion markers");
    println!("  status [provider]           presence, count, env name per provider");
    println!("  policy [fill_first|round_robin|random]  get or set rotation");
    println!();
    println!("Keys live once per provider as PANTHEON_KEY_<PROVIDER>.");
    println!("Aux slots and fallbacks just pick providers/models; they share these keys.");
}

fn cmd_add(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: pantheon auth add <provider> <key>");
            std::process::exit(2);
        }
    };
    let key = match args.get(4) {
        Some(k) => k.trim().to_string(),
        None => {
            eprintln!("usage: pantheon auth add <provider> <key>");
            std::process::exit(2);
        }
    };
    if key.is_empty() || key.contains(',') {
        eprintln!("auth: key must be non-empty and must not contain a comma");
        std::process::exit(2);
    }
    let dir = data_dir();
    let mut keys = read_keys(&dir, &provider);
    if keys.iter().any(|k| k == &key) {
        println!(
            "auth: key already in pool for {provider} ({} keys)",
            keys.len()
        );
        return;
    }
    keys.push(key);
    write_keys(&dir, &provider, &keys);
    println!(
        "auth: added key to {provider} ({} keys in pool)",
        keys.len()
    );
}

fn cmd_list(args: &[String]) {
    let dir = data_dir();
    let filter = args.get(3).cloned();
    let providers = list_providers(&dir, filter.as_deref());
    if providers.is_empty() {
        println!("auth: no keys configured");
        return;
    }
    for provider in &providers {
        let keys = read_keys(&dir, provider);
        if keys.is_empty() {
            continue;
        }
        println!("{provider} ({}):", keys.len());
        for (i, k) in keys.iter().enumerate() {
            println!("  [{i}] {}  priority {}", mask(k), i);
        }
    }
}

fn cmd_remove(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: pantheon auth remove <provider> [index]");
            std::process::exit(2);
        }
    };
    let dir = data_dir();
    let mut keys = read_keys(&dir, &provider);
    if keys.is_empty() {
        println!("auth: {provider} has no keys");
        return;
    }
    let idx = match args.get(4) {
        Some(s) => s.parse::<usize>().unwrap_or(usize::MAX),
        None => keys.len() - 1,
    };
    if idx >= keys.len() {
        eprintln!("auth: index {idx} out of range (0..{})", keys.len());
        std::process::exit(2);
    }
    let removed = keys.remove(idx);
    write_keys(&dir, &provider, &keys);
    println!(
        "auth: removed key [{}] {} from {provider} ({} left)",
        idx,
        mask(&removed),
        keys.len()
    );
}

fn cmd_priority(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: pantheon auth priority <provider> <from> <to>");
            std::process::exit(2);
        }
    };
    let from: usize = args
        .get(4)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let to: usize = args
        .get(5)
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX);
    let dir = data_dir();
    let mut keys = read_keys(&dir, &provider);
    if from >= keys.len() || to >= keys.len() {
        eprintln!("auth: index out of range (0..{})", keys.len());
        std::process::exit(2);
    }
    let k = keys.remove(from);
    keys.insert(to, k);
    write_keys(&dir, &provider, &keys);
    println!("auth: moved {provider} key {from} -> {to}");
    for (i, k) in keys.iter().enumerate() {
        println!("  [{i}] {}", mask(k));
    }
}

fn cmd_reset(args: &[String]) {
    let provider = match args.get(3) {
        Some(p) => p.clone(),
        None => {
            eprintln!("usage: pantheon auth reset <provider>");
            std::process::exit(2);
        }
    };
    let dir = data_dir();
    let keys = read_keys(&dir, &provider);
    if keys.is_empty() {
        println!("auth: {provider} has no keys to reset");
        return;
    }
    println!(
        "auth: {provider} has no exhaustion markers to clear ({} keys)",
        keys.len()
    );
}

fn cmd_status(args: &[String]) {
    let dir = data_dir();
    let filter = args.get(3).cloned();
    let providers = list_providers(&dir, filter.as_deref());
    if providers.is_empty() {
        println!("auth: no providers with keys");
        return;
    }
    for provider in &providers {
        let keys = read_keys(&dir, provider);
        let env = env_key_name(provider);
        if keys.is_empty() {
            println!("{provider}: no keys ({env})");
        } else {
            println!("{provider}: {} key(s) ({env})", keys.len());
        }
    }
}

fn cmd_policy(args: &[String]) {
    let dir = data_dir();
    match args.get(3) {
        Some(p) => {
            let policy = p.trim().to_ascii_lowercase();
            match policy.as_str() {
                "fill_first" | "round_robin" | "random" => {}
                _ => {
                    eprintln!("auth: policy must be fill_first, round_robin, or random");
                    std::process::exit(2);
                }
            }
            pantheon_api::dotenv::upsert_dotenv(&dir, "PANTHEON_AUTH_ROTATION", &policy)
                .unwrap_or_else(|e| {
                    eprintln!("auth: failed to write policy: {e}");
                    std::process::exit(1);
                });
            println!("auth: rotation policy set to {policy}");
        }
        None => {
            let cur = pantheon_api::dotenv::read_dotenv_value(&dir, "PANTHEON_AUTH_ROTATION")
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "fill_first".to_string());
            println!("auth: rotation policy is {cur}");
            println!("  set with: pantheon auth policy <fill_first|round_robin|random>");
        }
    }
}

fn list_providers(dir: &Path, filter: Option<&str>) -> Vec<String> {
    let path = pantheon_api::dotenv::dotenv_file_path(dir, ".env");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let parsed = pantheon_api::dotenv::parse_dotenv(&text);
    let mut out: Vec<String> = parsed
        .iter()
        .filter_map(|(k, _)| {
            k.strip_prefix("PANTHEON_KEY_")
                .map(|p| p.to_ascii_lowercase())
        })
        .collect();
    out.sort();
    out.dedup();
    if let Some(f) = filter {
        out.retain(|p| p == f);
    }
    out
}
