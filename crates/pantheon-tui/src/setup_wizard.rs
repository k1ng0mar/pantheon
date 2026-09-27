//! Setup, as a sequence of TUI screens.
//!
//! The wizard is an orchestrator over the shared component layer, not a
//! second implementation of it. Every screen here is a `Select`, a
//! `MultiSelect`, a `TextInput`, or a `Confirm` from `pantheon-tui`, drawn by
//! the same event loop that draws a session. The branch is resolved from the
//! answers, so the step indicator counts the screens this run actually has.
//!
//! Nothing here writes config directly. Answers are collected, then handed to
//! `setup::run_setup`, which is the same code path `pantheon setup --yes`
//! uses and the eval suite covers. A screen that appears without a runtime
//! consumer behind it is a lie the user finds out about later, so the only
//! screens offered are ones that write something the runtime reads.

use std::path::Path;

use crate::setup_graph::{sections, Answers, Mode, Section};
use crate::widget::Item;

use crate::config_schema::PolicyPreset;
use crate::setup::SetupAnswers;

/// Run the wizard to completion, then write the result.
///
/// Returns nothing and never loops: if the user cancels a screen, the answers
/// gathered so far are still written, because a half-configured agent is
/// closer to working than no config file at all, and the entry point re-checks
/// whether the result is usable before opening a session.
pub fn run_setup_flow(data_dir: &Path) {
    // --- Mode -----------------------------------------------------------
    // Asked first, because it decides every other screen. Blank Slate skips
    // the whole agent section, so asking it after the profile would be asking
    // about work the user just declined.
    let answers = Answers {
        mode: Some(pick_mode()),
        ..Default::default()
    };
    if answers.mode == Some(Mode::Blank) {
        // No agent, no model, no tools. Provision the runtime and stop.
        commit(
            data_dir,
            SetupAnswers {
                profile: None,
                provider: None,
                model: None,
                api_key_env: None,
                fallback_provider: None,
                fallback_model: None,
                policy: None,
                memory_backend: None,
                key: None,
            },
        );
        return;
    }

    // --- Profile --------------------------------------------------------
    let profile = pick_profile();
    if profile.is_empty() {
        return;
    }

    // --- Provider and model ---------------------------------------------
    let provider = pick_provider();
    if provider.is_none() {
        return;
    }
    // Every optional section stays off. Not because they are unwanted, but
    // because none of them has a runtime consumer yet, and offering a screen
    // that writes a setting nothing reads is the fake configuration the spec
    // forbids. Each flag flips on as its screen ships.

    let model = pick_model(provider.as_deref().unwrap_or_default());
    if model.is_none() {
        return;
    }

    // --- Everything the screen set says to ask --------------------------
    // The branch is computed, not assumed. `tools_enabled` is the only honest
    // source for whether the tool screens exist, and it is the same flag the
    // review screen prints, so the two cannot disagree.
    let branch: Vec<Section> = sections(&answers);
    let wants = |s: Section| branch.contains(&s);

    // Policy is a real runtime input: it gates tool execution. Offering a
    // preset and writing it is honest. Offering a screen whose answer nothing
    // reads would not be.
    let policy = if wants(Section::Permissions) {
        Some(pick_policy())
    } else {
        Some(PolicyPreset::Coder)
    };

    let memory_backend = if wants(Section::Memory) {
        pick_memory_backend()
    } else {
        None
    };

    let (fallback_provider, fallback_model) = if wants(Section::Fallback) {
        let p = pick_provider();
        match p {
            Some(fp) => {
                let model = pick_model(&fp);
                (Some(fp), model)
            }
            None => (None, None),
        }
    } else {
        (None, None)
    };

    commit(
        data_dir,
        SetupAnswers {
            profile: Some(profile),
            provider,
            model,
            api_key_env: None,
            fallback_provider,
            fallback_model,
            policy,
            memory_backend,
            key: None,
        },
    );
}

/// Write the answers through the shared setup path.
fn commit(data_dir: &Path, answers: SetupAnswers) {
    let cfg = crate::setup::run_setup(data_dir, answers, true);
    println!("pantheon: wrote {}", data_dir.join("config.toml").display());
    // The config is the record. Printing the model makes a first run legible
    // without asking the user to go read a TOML file.
    if let Some(m) = &cfg.model {
        println!("pantheon: model is {}/{}", m.provider, m.model);
    }
}

// ---------------------------------------------------------------------------
// screens
// ---------------------------------------------------------------------------

fn pick_mode() -> Mode {
    let items = vec![
        Item::new("Quick Setup", "quick").desc("the fewest decisions that give a working agent"),
        Item::new("Full Setup", "full").desc("everything, skipping anything that does not apply"),
        Item::new("Blank Slate", "blank").desc("create the runtime without configuring an agent"),
    ];
    match crate::prompt::pick_one("Pantheon Setup", "How would you like to start?", items) {
        Some(v) => match v.as_str() {
            "full" => Mode::Full,
            "blank" => Mode::Blank,
            _ => Mode::Quick,
        },
        // Cancelling the first screen means the user changed their mind
        // about being here. Quick is the least presumptuous default.
        None => Mode::Quick,
    }
}

fn pick_profile() -> String {
    let items = vec![
        Item::new("Default", "default").desc("a general-purpose Pantheon agent"),
        Item::new("Developer", "developer").desc("coding-focused"),
        Item::new("Researcher", "researcher").desc("research and analysis focused"),
    ];
    let chosen = crate::prompt::pick_one(
        "Agent Profile",
        "Start from a Pantheon-provided personality",
        items,
    );
    match chosen {
        Some(v) => v,
        // Cancelling here writes nothing, so the entry point tells the user
        // setup did not finish instead of opening an agent with no identity.
        None => {
            eprintln!("pantheon: setup cancelled before the profile was chosen.");
            String::new()
        }
    }
}

/// The provider picker. `router` is filtered out here rather than in the
/// widget, because "development endpoints are not offered" is a product
/// decision and belongs where a reader can see why.
fn pick_provider() -> Option<String> {
    let mut items: Vec<Item> = pantheon_providers::catalog::selectable_providers()
        .into_iter()
        .map(|p| {
            let mut item = Item::new(p.label.clone(), p.id.clone());
            if !p.tag.is_empty() {
                item = item.tag(p.tag);
            }
            if p.models.is_empty() {
                // No curated models means the generic adapter and whatever
                // name the user types. Saying so beats an empty list.
                item = item.desc("no curated models, name one yourself");
            }
            item
        })
        .collect();
    items.push(Item::new("Custom", "custom").desc("an OpenAI-shaped endpoint you provide"));
    let chosen = crate::prompt::pick_one("Model", "Choose a model provider", items)?;
    if chosen == "custom" {
        let url =
            crate::prompt::pick_text("Custom endpoint", "Base URL", "https://api.example.com/v1")?;
        if url.trim().is_empty() {
            return None;
        }
        // Registered through the same path `pantheon provider add` uses, so a
        // custom endpoint set up in the wizard is a real catalog entry and
        // not a string only this wizard remembers.
        pantheon_providers::catalog::register_custom_provider(
            pantheon_providers::catalog::ProviderMeta {
                id: "custom".into(),
                label: "Custom endpoint".into(),
                base_url: url,
                api_mode: pantheon_providers::catalog::ApiMode::OpenAi,
                base_env: String::new(),
                key_env: "PANTHEON_KEY_CUSTOM".into(),
                key_header: String::new(),
                models: Vec::new(),
                prominent: true,
                dev: false,
                tag: "custom".into(),
            },
        );
        return Some("custom".into());
    }
    Some(chosen)
}

fn pick_model(provider: &str) -> Option<String> {
    let meta = pantheon_providers::catalog::provider(provider);
    let label = meta
        .as_ref()
        .map(|m| m.label.clone())
        .unwrap_or_else(|| provider.to_string());
    let items: Vec<Item> = match &meta {
        Some(m) => m
            .models
            .iter()
            .map(|mm| {
                let mut item = Item::new(mm.model.clone(), format!("{provider}/{}", mm.model));
                if let Some(limit) = mm.context_limit {
                    item = item.tag(format!("{}k", limit / 1000));
                }
                item
            })
            .collect(),
        None => Vec::new(),
    };
    if items.is_empty() {
        // This provider has no curated models, so the generic adapter is the
        // path and the model name is a free-text answer. Inventing a list
        // here would be the fake configuration the spec forbids.
        let name = crate::prompt::pick_text(
            "Model",
            &format!("{label} has no curated models. Name the model to use."),
            "",
        )?;
        let name = name.trim().to_string();
        if name.is_empty() {
            return None;
        }
        // Bare model name: setup pairs it with the provider.
        return Some(name);
    }
    let chosen = crate::prompt::pick_one("Model", &label, items)?;
    // The picker's value is "provider/model", which is how the row was built
    // so the filter can match either half. `setup` composes the pair
    // itself, so strip the prefix or the config ends up as
    // "openai/openai/gpt-4o" and no provider resolves.
    Some(match chosen.split_once('/') {
        Some((_, m)) => m.to_string(),
        None => chosen,
    })
}

fn pick_policy() -> PolicyPreset {
    let items = vec![
        Item::new("Developer", "coder").desc("read and modify files, run commands"),
        Item::new("Reader", "reader").desc("inspect without modifying"),
        Item::new("Developer + Memory", "coder_memory")
            .desc("developer access plus persistent memory writes"),
    ];
    let chosen = crate::prompt::pick_one("Permissions", "What can this agent do?", items);
    match chosen.as_deref() {
        Some("reader") => PolicyPreset::Reader,
        Some("coder_memory") => PolicyPreset::CoderMemory,
        _ => PolicyPreset::Coder,
    }
}

fn pick_memory_backend() -> Option<String> {
    let items = vec![
        Item::new("Pantheon Native", "native").desc("local agent-native memory"),
        Item::new("GalaxyMem", "galaxymem").desc("external memory provider"),
        Item::new("Mem0", "mem0").desc("external memory provider"),
    ];
    let chosen = crate::prompt::pick_one("Memory", "Choose a memory backend", items)?;
    if chosen == "native" {
        // Native is the default the runtime already uses. Writing
        // `memory_backend = "native"` would be noise, so the answer is None
        // and the config stays quiet about a choice it does not need to record.
        None
    } else {
        Some(chosen)
    }
}
