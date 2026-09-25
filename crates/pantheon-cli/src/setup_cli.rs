//! `pantheon setup`: first-run configuration wizard.
//!
//! Quick mode: five steps (profile, model+fallback, execution policy,
//! memory backend, tool pack). Non-interactive friendly: every step can be
//! preset with a flag, so scripted setup never blocks on stdin. Anything
//! missing after the flags is prompted for.
//!
//! Secrets: the wizard only asks for the ENV VAR NAME, never the value.

use super::config_doc::{Config, FallbackEntry, MemorySection, ModelSection, ToolSection};
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
    pub tool_packs: Option<Vec<String>>,
    pub plugins: Option<Vec<String>>,
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

    // 5. Tool pack.
    let packs = answers.tool_packs.clone().unwrap_or_else(|| {
        if assume_defaults {
            vec!["core".into()]
        } else {
            let a = prompt("Tool packs (comma separated)", Some("core"));
            a.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        }
    });
    let plugins = answers.plugins.clone().unwrap_or_else(|| {
        if assume_defaults {
            Vec::new()
        } else {
            let a = prompt(
                "Plugins to auto-start (comma separated, empty = none)",
                Some(""),
            );
            a.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
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
        decision: None,
        compression: None,
        stt: None,
        tts: None,
        policy: Some(policy),
        memory: Some(MemorySection {
            backend: memory_backend.clone(),
            options: std::collections::HashMap::new(),
        }),
        tools: Some(ToolSection { packs, plugins }),
        server: Some(super::config_doc::ServerSection {
            port: 18789,
            host: "127.0.0.1".into(),
        }),
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
            if let Some(env) = &cfg.model.as_ref().and_then(|m| m.api_key_env.clone()) {
                println!("make sure {env} is exported before starting a session");
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
mod tests {
    use super::*;

    #[test]
    fn flag_only_setup_writes_a_complete_config_without_stdin() {
        let dir = std::env::temp_dir().join(format!("pantheon-setup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = run_setup(
            &dir,
            SetupAnswers {
                profile: Some("dev".into()),
                provider: Some("router".into()),
                model: Some("big-model".into()),
                api_key_env: Some("PANTHEON_API_KEY".into()),
                policy: Some(PolicyPreset::CoderMemory),
                memory_backend: Some("native".into()),
                tool_packs: Some(vec!["core".into()]),
                plugins: Some(vec![]),
                ..Default::default()
            },
            true,
        );
        // Everything the wizard was asked for landed in the file.
        let loaded = Config::load(&dir).unwrap();
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.model.as_ref().unwrap().provider, "router");
        assert_eq!(loaded.policy, Some(PolicyPreset::CoderMemory),);
        // Backend selection file was synced.
        let sel = load_backend_selection(&dir);
        assert_eq!(sel.name, "native");
        // No raw secrets anywhere.
        let text = std::fs::read_to_string(Config::path(&dir)).unwrap();
        assert!(!text.contains("sk-"));
    }
}
