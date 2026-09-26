//! `pantheon setup`: first-run configuration wizard.
//!
//! Quick mode: five steps (profile, model+fallback, execution policy,
//! memory backend, tool pack). Non-interactive friendly: every step can be
//! preset with a flag, so scripted setup never blocks on stdin. Anything
//! missing after the flags is prompted for.
//!
//! Secrets: the wizard only asks for the ENV VAR NAME, never the value.

use super::config_doc::{Config, FallbackEntry, MemorySection, ModelSection};
use super::config_schema::PolicyPreset;
use std::io::BufRead;
use std::path::Path;

fn prompt(line: &str, default: Option<&str>) -> String {
    match default {
        Some(d) => print!("{line} [{d}]: "),
        None => print!("{line}: "),
    }
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let _ = std::io::stdin().lock().read_line(&mut buf);
    let answer = buf.trim();
    if answer.is_empty() {
        default.unwrap_or("").to_string()
    } else {
        answer.to_string()
    }
}

fn confirm(question: &str, default_yes: bool) -> bool {
    let d = if default_yes { "Y/n" } else { "y/N" };
    let a = prompt(&format!("{question} ({d})"), None);
    match a.to_ascii_lowercase().as_str() {
        "y" | "yes" => true,
        "n" | "no" => false,
        _ => default_yes,
    }
}

/// One wizard step, pre-settable from flags. `Some` values skip the prompt.
#[derive(Debug, Default, Clone)]
pub struct SetupAnswers {
    pub profile: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub fallback_provider: Option<String>,
    pub fallback_model: Option<String>,
    pub policy: Option<PolicyPreset>,
    pub memory_backend: Option<String>,
    /// A raw API key, for scripting setup. Never written to config.toml:
    /// it goes into `<data_dir>/.env` under `api_key_env` (or a derived
    /// `PANTHEON_KEY_<PROVIDER>` name) and the config stores only the name.
    pub key: Option<String>,
}

/// Run the wizard. `stdin` is only touched for missing answers.
pub fn run_setup(data_dir: &Path, answers: SetupAnswers, assume_defaults: bool) -> Config {
    println!("pantheon setup");
    println!("  data dir: {}", data_dir.display());
    println!();

    // 1. Profile: a label, nothing more.
    let profile = answers.profile.clone().unwrap_or_else(|| {
        if assume_defaults {
            "default".into()
        } else {
            prompt("Profile name", Some("default"))
        }
    });

    // 2. Model + fallback.
    let provider = answers.provider.clone().unwrap_or_else(|| {
        if assume_defaults {
            "local".into()
        } else {
            prompt("Default provider", Some("local"))
        }
    });
    let model = answers.model.clone().unwrap_or_else(|| {
        if assume_defaults {
            "llama3.2".into()
        } else {
            prompt("Default model", Some("llama3.2"))
        }
    });
    let api_key_env = answers.api_key_env.clone().unwrap_or_else(|| {
        if assume_defaults {
            String::new()
        } else {
            prompt("Env var holding the API key (empty = none)", Some(""))
        }
    });
    // A raw key needs a name to live under. Derive one from the provider
    // using the same convention `pantheon model` uses, so a scripted setup
    // and a wizard run agree on where the key is read from later.
    //
    // A URL is an endpoint, not an id: sanitizing `http://127.0.0.1:8015/v1`
    // produced `HTTP___127_0_0_1_8015_V1`, which doctor's own UPPER_SNAKE
    // check then rejected. A custom endpoint gets one stable name instead.
    let raw_key = answers.key.clone().filter(|k| !k.trim().is_empty());
    let api_key_env = match (api_key_env.is_empty(), raw_key.is_some()) {
        (true, true) => {
            if looks_like_url(&provider) {
                "PANTHEON_KEY_CUSTOM".to_string()
            } else {
                format!(
                    "PANTHEON_KEY_{}",
                    super::config_doc::sanitize_env_suffix(&provider)
                )
            }
        }
        _ => api_key_env,
    };
    let mut fallbacks = Vec::new();
    let fb_provider = answers.fallback_provider.clone().or_else(|| {
        if assume_defaults {
            None
        } else if confirm("Add a fallback model?", false) {
            Some(prompt("Fallback provider", Some("local")))
        } else {
            None
        }
    });
    if let Some(fp) = fb_provider {
        let fm = answers
            .fallback_model
            .clone()
            .unwrap_or_else(|| prompt("Fallback model", Some("llama3.2")));
        if !fp.is_empty() && !fm.is_empty() {
            fallbacks.push(FallbackEntry {
                provider: fp,
                model: fm,
            });
        }
    }

    // 3. Execution policy.
    let policy = answers.policy.unwrap_or_else(|| {
        if assume_defaults {
            PolicyPreset::Coder
        } else {
            println!("Execution policy:");
            println!("  reader       read-only, nothing executes");
            println!("  coder        shell + edits allowed (recommended)");
            println!("  coder_memory coder + memory writes");
            let a = prompt("Policy", Some("coder"));
            PolicyPreset::from_str(&a).unwrap_or(PolicyPreset::Coder)
        }
    });

    // 4. Memory backend. Existing selection file wins if present and no
    // flag overrode it (setup never silently downgrades a working backend).
    let existing = load_backend_selection(data_dir);
    let memory_backend = answers.memory_backend.clone().unwrap_or_else(|| {
        if existing.name != "native" {
            existing.name.clone()
        } else if assume_defaults {
            "native".into()
        } else {
            prompt("Memory backend", Some("native"))
        }
    });

    let cfg = Config {
        profile: Some(profile),
        model: Some(ModelSection {
            provider,
            model,
            api_key_env: if api_key_env.is_empty() {
                None
            } else {
                Some(api_key_env)
            },
            fallbacks,
        }),
        judge: None,
        embeddings: None,
        search_synthesis: None,
        vision: None,
        scheduled: None,
        mcp_synthesis: None,
        compression: None,
        title_gen: None,
        stt: None,
        tts: None,
        policy: Some(policy),
        memory: Some(MemorySection {
            backend: memory_backend.clone(),
            options: std::collections::HashMap::new(),
        }),
        server: Some(super::config_doc::ServerSection {
            port: 18789,
            host: "127.0.0.1".into(),
        }),
        tools: None,
        custom_providers: Default::default(),
        agents: Default::default(),
    };

    match cfg.save(data_dir) {
        Ok(()) => {
            // Keep the memory-backend selection file in sync.
            save_backend_selection(
                data_dir,
                &pantheon_memory::BackendSelection {
                    name: memory_backend,
                    options: std::collections::HashMap::new(),
                },
            );
            println!();
            println!("config written: {}", Config::path(data_dir).display());
            // The key lands in <data_dir>/.env, never in config.toml, which
            // holds only the env var name. Surface a write failure loudly:
            // a silent one leaves a config that points at a key nobody has.
            if let Some(key) = raw_key.as_deref() {
                let env_name = cfg
                    .model
                    .as_ref()
                    .and_then(|m| m.api_key_env.clone())
                    .unwrap_or_default();
                match super::dotenv::upsert_dotenv(data_dir, &env_name, key) {
                    Ok(()) => {
                        println!("key written: {}/.env ({env_name})", data_dir.display());
                        // Make the key live in this process too, so a
                        // `setup && doctor` chain in one shell works.
                        std::env::set_var(&env_name, key);
                    }
                    Err(e) => {
                        eprintln!(
                            "setup: config written but the key could not be saved to {}/.env: {e}",
                            data_dir.display()
                        );
                        eprintln!("fix: export {env_name}=... manually, then rerun doctor");
                        std::process::exit(1);
                    }
                }
            } else if let Some(env) = cfg.model.as_ref().and_then(|m| m.api_key_env.clone()) {
                println!("make sure {env} is exported before starting a session");
            } else if let Some(provider) = cfg.model.as_ref().map(|m| m.provider.as_str()) {
                // A bare endpoint is not evidence that it is unauthenticated.
                // Local servers usually need no key, but a hosted
                // OpenAI-compatible gateway almost always does, and guessing
                // wrong costs the user a confusing 401 on first chat.
                println!(
                    "no API key configured for {provider}; if it needs one, \
                     rerun with --api-key-env <NAME> --key <SECRET>"
                );
            } else {
                println!("no API key configured");
            }
            println!("next: pantheon doctor");
        }
        Err(e) => {
            eprintln!("setup: {e}");
            std::process::exit(1);
        }
    }
    cfg
}

/// True when a provider string is a bare endpoint rather than a catalog id.
fn looks_like_url(provider: &str) -> bool {
    provider.starts_with("http://") || provider.starts_with("https://")
}

fn backend_selection_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("memory-backend.toml")
}

fn load_backend_selection(data_dir: &Path) -> pantheon_memory::BackendSelection {
    std::fs::read_to_string(backend_selection_path(data_dir))
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_backend_selection(data_dir: &Path, sel: &pantheon_memory::BackendSelection) {
    if let Ok(text) = toml::to_string_pretty(sel) {
        let _ = std::fs::write(backend_selection_path(data_dir), text);
    }
}

#[cfg(test)]
#[path = "setup_cli_tests.rs"]
mod tests;
