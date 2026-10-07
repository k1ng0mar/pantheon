//! Setup, as a sequence of TUI screens.
//!
//! The wizard is an orchestrator over the shared component layer, not a
//! second implementation of it. Every screen here is a `Select`, a
//! `MultiSelect`, a `TextInput`, or a `Confirm` from `pantheon-tui`, drawn by
//! the same event loop that draws a session. The branch is resolved from the
//! answers, so the step indicator counts the screens this run actually has.
//!
//! Three entry modes. Recommended asks the fewest questions that give a
//! working agent: provider, model, then the recommended providers for web
//! search, browser, memory, and computer use - no Tools screen, no
//! STT/TTS. Full Setup runs the Tools screen pre-ticked, and its answers
//! gate the provider screens (STT+TTS when Voice is on). Full Setup also
//! has a Gateway screen for the optional Telegram/Discord bot tokens;
//! they land in the secrets store, never in config.toml. Both agent modes
//! end with a service-install permission screen: the platform's install
//! mechanism is shown before the question is asked, and the gateway
//! installs as an always-on service only on explicit confirmation.
//! Blank Slate provisions the runtime without configuring an agent at all.
//!
//! Auxiliary model pins are deliberately not asked here: they are managed
//! anytime with `pantheon model`, which the done summary mentions.
//!
//! Screen 3 (model) merges the catalog's curated entries with the
//! provider's live `/models` list for OpenAI-compatible providers,
//! keyless-first with a graceful curated fallback; each row tags its
//! context window and per-million-token price when known.
//!
//! Nothing here writes config directly. Answers are collected, then handed to
//! `setup::run_setup`, which is the same code path `pantheon setup --yes`
//! uses and the eval suite covers. A screen that appears without a runtime
//! consumer behind it is a lie the user finds out about later, so the only
//! screens offered are ones that write something the runtime reads.

use std::collections::HashMap;
use std::path::Path;

use crate::notify::is_executable_file;
use crate::setup_graph::{sections, Answers, Mode, Section};
use crate::widget::Item;

use crate::config_schema::PolicyPreset;
use crate::setup::{CustomProviderSpec, SetupAnswers};
use crate::setup_providers;
use pantheon_api::config::{McpServerEntry, ToolGroup};

/// Run the wizard to completion, then write the result.
///
/// Returns nothing and never loops: if the user cancels a screen, the answers
/// gathered so far are still written, because a half-configured agent is
/// closer to working than no config file at all, and the entry point re-checks
/// whether the result is usable before opening a session.
pub fn run_setup_flow(data_dir: &Path) {
    // --- Mode -----------------------------------------------------------
    // Asked first, because it decides every other screen. Blank Slate skips
    // the whole agent section, so it short-circuits before any agent
    // question is asked.
    let mode = pick_mode();
    if mode == Mode::Blank {
        // No agent, no model, no tools. Provision the runtime and stop.
        commit(
            data_dir,
            SetupAnswers {
                provider: None,
                model: None,
                api_key_env: None,
                fallback_provider: None,
                fallback_model: None,
                policy: None,
                tools: None,
                websearch: None,
                browser: None,
                stt: None,
                tts: None,
                memory: None,
                computer: None,
                key: None,
                custom_provider: None,
                skipped_tools: Vec::new(),
                skipped_stt: false,
                skipped_tts: false,
                // Blank provisions no agent and no skills, so the
                // skill-deps screen never runs here.
                skill_deps_skipped: Vec::new(),
                mcp_servers: Vec::new(),
            },
        );
        return;
    }

    // --- Provider and model ---------------------------------------------
    // Both agent modes start here: Recommended fills in everything else,
    // Full goes on to ask it.
    let (provider, custom) = match pick_provider() {
        Some(p) => p,
        None => return,
    };
    let model = pick_model(provider.as_str());
    if model.is_none() {
        return;
    }
    // --- API key --------------------------------------------------------
    // The wizard asks where the provider's API key lives and writes the
    // env var name (never the key) to `[model].api_key_env`. Local
    // providers need no key, so the screen is skipped for them; a custom
    // endpoint already named its key env on its own screen.
    let api_key_env = pick_model_key_env(provider.as_str(), custom.as_ref());

    if mode == Mode::Recommended {
        run_recommended(data_dir, provider, model, custom, api_key_env);
    } else {
        run_full(data_dir, provider, model, custom, api_key_env);
    }
}

/// Where the model provider's API key lives: the env var name the
/// config stores under `[model].api_key_env`. Esc (or an empty answer)
/// means none. `None` here is "no key needed", never "ask again later".
fn pick_model_key_env(provider: &str, custom: Option<&CustomProviderSpec>) -> Option<String> {
    if provider == "local" {
        return None;
    }
    if let Some(spec) = custom {
        return spec.key_env.clone();
    }
    let meta = pantheon_providers::catalog::provider(provider);
    let label = meta
        .as_ref()
        .map(|m| m.label.clone())
        .unwrap_or_else(|| provider.to_string());
    let prefill = meta.map(|m| m.key_env.clone()).unwrap_or_default();
    let answer = crate::prompt::pick_text(
        "API key",
        &format!("Env var holding the {label} API key (empty = none)"),
        &prefill,
    )?;
    let answer = answer.trim().to_string();
    if answer.is_empty() {
        None
    } else {
        Some(answer)
    }
}

/// Recommended mode: minimum decisions. The toolset is fixed (every
/// group except Voice - STT/TTS are skipped entirely), and each
/// provider screen resolves the recommended row with NO picker: only
/// the kind-driven follow-ups run (key env var, URL, local
/// detect+install-or-skip).
fn run_recommended(
    data_dir: &Path,
    provider: String,
    model: Option<String>,
    custom: Option<CustomProviderSpec>,
    api_key_env: Option<String>,
) {
    // Skip is offered in the follow-ups: a skipped group comes off the
    // toolset, so no section is written and the runtime never registers
    // the tool. A cancelled follow-up keeps the old behavior (the shared
    // setup path resolves the recommended default).
    let mut tools = recommended_tools();
    let mut skipped: Vec<ToolGroup> = Vec::new();
    let websearch_pick =
        setup_providers::complete_recommended("Web search", setup_providers::websearch_providers());
    let browser_pick =
        setup_providers::complete_recommended("Browser", setup_providers::browser_providers());
    // Memory native is keyless, so this call records silently.
    let memory_pick =
        setup_providers::complete_recommended("Memory", setup_providers::memory_providers());
    let computer_pick = setup_providers::complete_recommended(
        "Computer use",
        setup_providers::computer_providers(),
    );
    let websearch = setup_providers::apply_pick(
        &mut tools,
        &mut skipped,
        ToolGroup::WebSearch,
        &websearch_pick,
    );
    let browser =
        setup_providers::apply_pick(&mut tools, &mut skipped, ToolGroup::Browser, &browser_pick);
    let memory =
        setup_providers::apply_pick(&mut tools, &mut skipped, ToolGroup::Memory, &memory_pick);
    let computer = setup_providers::apply_pick(
        &mut tools,
        &mut skipped,
        ToolGroup::ComputerUse,
        &computer_pick,
    );
    // Owned labels before the commit moves the answers.
    let ids = (
        answer_summary(&websearch_pick),
        answer_summary(&browser_pick),
        answer_summary(&memory_pick),
        answer_summary(&computer_pick),
    );
    // Skill dependencies apply in every mode: the skills work the same
    // no matter which provider screens ran.
    let skill_deps_skipped = run_skill_deps_screen_tui();
    // The service-install permission screen: the last question in
    // Recommended too, so an always-on background service never
    // installs without explicit consent on any agent path.
    pick_gateway_service_install(data_dir);

    commit(
        data_dir,
        SetupAnswers {
            provider: Some(provider),
            model,
            api_key_env,
            fallback_provider: None,
            fallback_model: None,
            policy: Some(PolicyPreset::Coder),
            tools: Some(tools),
            websearch,
            browser,
            stt: None,
            tts: None,
            memory,
            computer,
            key: None,
            custom_provider: custom,
            skipped_tools: skipped,
            skipped_stt: false,
            skipped_tts: false,
            skill_deps_skipped,
            mcp_servers: Vec::new(),
        },
    );
    // The recommended set, actually configured. `commit` already printed
    // the "wrote ..." and "model is ..." lines above these.
    println!("pantheon: web search: {}", ids.0);
    println!("pantheon: browser: {}", ids.1);
    println!("pantheon: memory: {}", ids.2);
    println!("pantheon: computer use: {}", ids.3);
    println!("pantheon: voice: off");
}

/// The recommended toolset: every group except Voice. STT/TTS are not
/// part of the recommended setup at all.
fn recommended_tools() -> Vec<ToolGroup> {
    ToolGroup::all()
        .into_iter()
        .filter(|g| *g != ToolGroup::Voice)
        .collect()
}

/// One-line id (or skip state) for the done summary. `Skipped` means
/// the user left the tool unconfigured; `Cancelled` means the user
/// pressed Esc in the follow-ups, so the shared setup path resolves
/// the recommended default itself.
fn answer_summary(p: &setup_providers::ProviderPick) -> String {
    match p {
        setup_providers::ProviderPick::Chosen(a) if a.skipped => "skipped".to_string(),
        setup_providers::ProviderPick::Chosen(a) => a.id.clone(),
        setup_providers::ProviderPick::Skipped => "skipped".to_string(),
        setup_providers::ProviderPick::Cancelled => "recommended default".to_string(),
    }
}

/// Full Setup: everything, skipping anything that does not apply. The
/// Tools screen runs pre-ticked, and its answers gate the provider
/// screens: browser, web search, and speech (STT+TTS) get their
/// provider screens only when the group is on.
fn run_full(
    data_dir: &Path,
    provider: String,
    model: Option<String>,
    custom: Option<CustomProviderSpec>,
    api_key_env: Option<String>,
) {
    // The branch is computed, not assumed. After the Tools screen it is
    // recomputed, because the tool answers gate the provider screens:
    // browser, web search, and speech are tool groups, so their provider
    // screens exist only when the group is on.
    let mut answers = Answers {
        mode: Some(Mode::Full),
        ..Default::default()
    };
    let branch: Vec<Section> = sections(&answers);
    let wants = |s: Section| branch.contains(&s);

    // The Tools screen replaces the old Permissions screen: one
    // multi-select over every capability instead of a policy preset.
    // Policy stays the default coder preset - there is no permissions
    // screen anymore. Session search is default-on and hidden from the
    // options.
    let tools = if wants(Section::Tools) {
        pick_tools()
    } else {
        ToolGroup::all().to_vec()
    };
    // Skip is per screen: a skipped group comes off `tools`, so the
    // shared setup path writes no section for it and the runtime never
    // registers the tool. `skipped` feeds the done summary.
    let mut tools = tools;
    let mut skipped: Vec<ToolGroup> = Vec::new();
    answers.browser_enabled = tools.contains(&ToolGroup::Browser);
    answers.web_search_enabled = tools.contains(&ToolGroup::WebSearch);
    answers.tts_enabled = tools.contains(&ToolGroup::Voice);
    answers.memory_enabled = tools.contains(&ToolGroup::Memory);
    answers.computer_use_enabled = tools.contains(&ToolGroup::ComputerUse);
    answers.extensions_enabled = tools.contains(&ToolGroup::Plugins);
    // The Gateway screen is Full-only: chat-surface tokens are a
    // power-user surface and stay out of Recommended entirely.
    answers.gateways_enabled = true;
    let branch: Vec<Section> = sections(&answers);
    let wants = |s: Section| branch.contains(&s);

    let policy = Some(PolicyPreset::Coder);

    // Skill dependencies: third-party packages the skill library needs.
    // One screen with a status line per package (found/missing, which
    // skill needs it) and install-or-skip per missing item. Skipped ids
    // are recorded in `[skill_deps]` so `pantheon doctor` surfaces the
    // gap on a later run.
    let skill_deps_skipped = if wants(Section::SkillDeps) {
        run_skill_deps_screen_tui()
    } else {
        Vec::new()
    };

    let browser = if wants(Section::Browser) {
        let pick = setup_providers::pick_provider(
            "Browser",
            "Choose the backend behind the browser tools",
            setup_providers::browser_providers(),
        );
        setup_providers::apply_pick(&mut tools, &mut skipped, ToolGroup::Browser, &pick)
    } else {
        None
    };

    let websearch = if wants(Section::WebSearch) {
        let pick = setup_providers::pick_provider(
            "Web search",
            "Choose the provider behind the `web_search` tool",
            setup_providers::websearch_providers(),
        );
        setup_providers::apply_pick(&mut tools, &mut skipped, ToolGroup::WebSearch, &pick)
    } else {
        None
    };

    // Speech is one tool group with two backends: STT first, then TTS.
    // Skip is granular per backend - a skipped backend writes no
    // section; the Voice group only comes off when both are skipped.
    // A cancelled pick keeps the old behavior (the shared setup path
    // resolves the recommended default).
    let voice = if wants(Section::Tts) {
        let stt_pick = setup_providers::pick_provider(
            "Speech to text",
            "Choose the STT backend",
            setup_providers::stt_providers(),
        );
        let tts_pick = setup_providers::pick_provider(
            "Text to speech",
            "Choose the TTS backend",
            setup_providers::tts_providers(),
        );
        setup_providers::apply_voice_picks(&mut tools, &mut skipped, &stt_pick, &tts_pick)
    } else {
        setup_providers::VoicePicks::default()
    };

    let memory = if wants(Section::Memory) {
        let pick = setup_providers::pick_provider(
            "Memory",
            "Choose a memory backend",
            setup_providers::memory_providers(),
        );
        setup_providers::apply_pick(&mut tools, &mut skipped, ToolGroup::Memory, &pick)
    } else {
        None
    };

    // Computer use: the only driver is the CUA driver; the screen
    // detects it, offers the verified install, or skips.
    let computer = if wants(Section::ComputerUse) {
        let pick = setup_providers::pick_provider(
            "Computer use",
            "Choose the driver behind desktop control",
            setup_providers::computer_providers(),
        );
        setup_providers::apply_pick(&mut tools, &mut skipped, ToolGroup::ComputerUse, &pick)
    } else {
        None
    };

    // Extensions (Full only): MCP servers, plus the bundled-plugin
    // status line. Skip turns the Plugins group off - same as every
    // other provider screen: intentionally unconfigured means the
    // group comes off the enabled set and nothing is written.
    let mcp_servers = if wants(Section::Extensions) {
        match pick_extensions(data_dir) {
            Some(servers) => servers,
            None => {
                apply_extensions_skip(&mut tools, &mut skipped);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Gateway (Full only): chat-surface tokens for `gateway run`.
    // Both optional; the screen writes them straight to the secrets
    // store, never to config.toml.
    if wants(Section::Gateways) {
        pick_gateways(data_dir);
    }

    let (fallback_provider, fallback_model, fallback_custom) = if wants(Section::Fallback) {
        let p = pick_provider();
        match p {
            Some((fp, fc)) => {
                let model = pick_model(&fp);
                (Some(fp), model, fc)
            }
            None => (None, None, None),
        }
    } else {
        (None, None, None)
    };

    // The service-install permission screen: the last screen in Full
    // Setup, after every other screen. Declining is clean and never
    // re-asked; confirming installs via the shared `pantheon init`
    // plumbing.
    pick_gateway_service_install(data_dir);

    commit(
        data_dir,
        SetupAnswers {
            provider: Some(provider),
            model,
            api_key_env,
            fallback_provider,
            fallback_model,
            policy,
            tools: Some(tools),
            websearch,
            browser,
            stt: voice.stt,
            tts: voice.tts,
            memory,
            computer,
            key: None,
            // The custom endpoint row is named "custom" either way; the
            // fallback's spec wins when both picks used the Custom row
            // with different URLs.
            custom_provider: fallback_custom.or(custom),
            skipped_tools: skipped,
            skipped_stt: voice.skipped_stt,
            skipped_tts: voice.skipped_tts,
            skill_deps_skipped,
            mcp_servers,
        },
    );
}

/// The wizard's "Skill dependencies" screen: one status line per
/// third-party package the skill library needs (found/missing, source,
/// which skill needs it), then install-or-skip per missing item.
/// Returns the skipped ids for `[skill_deps].skipped`.
fn run_skill_deps_screen_tui() -> Vec<String> {
    crate::skill_deps::run_skill_deps_screen(
        &mut |line: &str| println!("pantheon: {line}"),
        &mut |question: &str, def: bool| {
            crate::prompt::pick_confirm("Skill dependencies", question, def)
        },
        &mut crate::skill_deps::shell_install,
        &crate::setup_providers::detect_binary,
    )
}

/// Write the answers through the shared setup path.
fn commit(data_dir: &Path, answers: SetupAnswers) {
    let skipped: Vec<&str> = answers.skipped_tools.iter().map(|g| g.key()).collect();
    let deps_skipped = answers.skill_deps_skipped.clone();
    let cfg = crate::setup::run_setup(data_dir, answers, true);
    println!("pantheon: wrote {}", data_dir.join("config.toml").display());
    // The config is the record. Printing the model makes a first run legible
    // without asking the user to go read a TOML file.
    if let Some(m) = &cfg.model {
        println!("pantheon: model is {}/{}", m.provider, m.model);
    }
    // Auxiliary model pins are not asked in the wizard: they are managed
    // anytime with `pantheon model`.
    println!("pantheon: manage auxiliary models anytime with `pantheon model`");
    // The tools the user deliberately left unconfigured. Empty → omit.
    if !skipped.is_empty() {
        println!("pantheon: skipped: {}", skipped.join(", "));
    }
    // Same for skill dependencies: recorded in `[skill_deps]`, and
    // `pantheon doctor` reports the gap on later runs.
    if !deps_skipped.is_empty() {
        println!("pantheon: skill deps skipped: {}", deps_skipped.join(", "));
    }
}

// ---------------------------------------------------------------------------
// screens
// ---------------------------------------------------------------------------

fn pick_mode() -> Mode {
    let items = vec![
        Item::new("Recommended", "recommended")
            .desc("pick your provider and model; everything else gets recommended defaults"),
        Item::new("Full Setup", "full").desc("everything, skipping anything that does not apply"),
        Item::new("Blank Slate", "blank").desc("create the runtime without configuring an agent"),
    ];
    match crate::prompt::pick_one("Setup", "Choose your setup", items) {
        Some(v) => match v.as_str() {
            "full" => Mode::Full,
            "blank" => Mode::Blank,
            _ => Mode::Recommended,
        },
        // Cancelling the first screen means the user changed their mind
        // about being here. Recommended is the least presumptuous default.
        None => Mode::Recommended,
    }
}

/// The provider picker. `router` is filtered out here rather than in the
/// widget, because "development endpoints are not offered" is a product
/// decision and belongs where a reader can see why.
/// Picker order for model providers: recommended first, stable for the
/// rest. Defensive pin for Nous Research - Umar's designated recommended
/// model provider - in case the catalog hasn't landed its marker yet; a
/// catalog marker always wins when present.
fn order_model_providers(
    mut providers: Vec<pantheon_providers::catalog::ProviderMeta>,
) -> Vec<pantheon_providers::catalog::ProviderMeta> {
    if !providers.iter().any(|p| p.recommended) {
        if let Some(n) = providers.iter_mut().find(|p| p.id == "nous") {
            n.recommended = true;
        }
    }
    providers.sort_by_key(|p| !p.recommended);
    providers
}

/// A post-selection note for a provider, if one applies. Kept separate
/// from the picker so the wording is unit-testable without the TUI.
fn provider_note(provider: &str) -> Option<&'static str> {
    if provider == "nous" {
        Some("note: the Nous Portal primarily uses OAuth; NOUS_API_KEY covers the inference API / evaluation tier, check the Portal docs if auth fails")
    } else {
        None
    }
}

/// Pick a model provider. Returns the provider id plus, for the Custom
/// row, the endpoint spec `run_setup` persists to
/// `[custom_providers.custom]` - the in-memory registration alone does
/// not survive the process, so the wizard used to write a config
/// pointing at an endpoint only it remembered.
fn pick_provider() -> Option<(String, Option<CustomProviderSpec>)> {
    // The recommended provider (at most one) leads the list, tagged in
    // the picker; the cursor starts on the first row, making it the
    // pre-selected choice.
    let providers = order_model_providers(pantheon_providers::catalog::selectable_providers());
    let mut items: Vec<Item> = providers
        .into_iter()
        .map(|p| {
            let mut item = Item::new(p.label.clone(), p.id.clone());
            if p.recommended {
                let tag = if p.tag.is_empty() {
                    "recommended".to_string()
                } else {
                    format!("{} · recommended", p.tag)
                };
                item = item.tag(tag);
            } else if !p.tag.is_empty() {
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
    let chosen = crate::prompt::pick_one("Provider", "Choose a model provider", items)?;
    if chosen == "custom" {
        let url =
            crate::prompt::pick_text("Custom endpoint", "Base URL", "https://api.example.com/v1")?;
        if url.trim().is_empty() {
            return None;
        }
        // Wire format: the runtime maps anything that is not "anthropic"
        // to OpenAI, so ask rather than assume.
        let api_mode = match crate::prompt::pick_one(
            "Custom endpoint",
            "Wire format",
            vec![
                Item::new("OpenAI-compatible", "openai").desc("OpenAI-style /v1 API"),
                Item::new("Anthropic", "anthropic").desc("Anthropic Messages API"),
            ],
        )?
        .as_str()
        {
            "anthropic" => pantheon_providers::catalog::ApiMode::Anthropic,
            _ => pantheon_providers::catalog::ApiMode::OpenAi,
        };
        let key_env = crate::prompt::pick_text(
            "Custom endpoint",
            "Env var holding the API key (empty = none)",
            "PANTHEON_KEY_CUSTOM",
        )?;
        let key_env = {
            let k = key_env.trim();
            if k.is_empty() {
                None
            } else {
                Some(k.to_string())
            }
        };
        // Registered through the same path `pantheon provider add` uses, so a
        // custom endpoint set up in the wizard is a real catalog entry and
        // not a string only this wizard remembers.
        pantheon_providers::catalog::register_custom_provider(
            pantheon_providers::catalog::ProviderMeta {
                id: "custom".into(),
                label: "Custom endpoint".into(),
                base_url: url.clone(),
                api_mode,
                base_env: String::new(),
                key_env: key_env.clone().unwrap_or_default(),
                key_header: String::new(),
                models: Vec::new(),
                prominent: true,
                recommended: false,
                dev: false,
                tag: "custom".into(),
            },
        );
        let spec = CustomProviderSpec {
            name: "custom".to_string(),
            base_url: url,
            api_mode: match api_mode {
                pantheon_providers::catalog::ApiMode::Anthropic => "anthropic".to_string(),
                _ => "openai".to_string(),
            },
            key_env,
        };
        return Some(("custom".into(), Some(spec)));
    }
    // Nous honesty: the Portal primarily uses OAuth, so a static key may
    // not be the whole auth story. Printed here, in the shared picker, so
    // it appears in both agent flows (and the Full flow's fallback pick).
    if let Some(note) = provider_note(&chosen) {
        println!("{note}");
    }
    Some((chosen, None))
}

/// Curated catalog entries normalized for the live-list merge.
fn curated_rows(
    m: &pantheon_providers::catalog::ProviderMeta,
) -> Vec<crate::model_catalog::CatalogModel> {
    m.models
        .iter()
        .map(|mm| crate::model_catalog::CatalogModel {
            id: mm.model.clone(),
            context_limit: mm.context_limit.map(u64::from),
            input_per_mtok_usd: mm.cost.input_per_mtok_usd,
            output_per_mtok_usd: mm.cost.output_per_mtok_usd,
        })
        .collect()
}

fn pick_model(provider: &str) -> Option<String> {
    let meta = pantheon_providers::catalog::provider(provider);
    let label = meta
        .as_ref()
        .map(|m| m.label.clone())
        .unwrap_or_else(|| provider.to_string());
    // Live list for OpenAI-compatible providers: the catalog's curated
    // entries merged with `{base}/models`, fetched keyless-first. A
    // 401/403 falls back to curated with a "full list after API key"
    // note; other failures name their kind. Non-OpenAI providers use
    // the curated list only.
    let (rows, note): (Vec<crate::model_catalog::ModelRow>, Option<String>) = match &meta {
        Some(m) if m.api_mode == pantheon_providers::catalog::ApiMode::OpenAi => {
            let curated = curated_rows(m);
            match crate::model_catalog::fetch_live_models(&m.base_url) {
                Ok(live) => (
                    crate::model_catalog::merge_model_rows(&curated, &live),
                    None,
                ),
                Err(crate::model_catalog::FetchError::Auth) => (
                    crate::model_catalog::merge_model_rows(&curated, &[]),
                    Some("full list after API key".to_string()),
                ),
                Err(e) => (
                    crate::model_catalog::merge_model_rows(&curated, &[]),
                    Some(format!("live list unavailable ({})", e.kind())),
                ),
            }
        }
        Some(m) => (
            crate::model_catalog::merge_model_rows(&curated_rows(m), &[]),
            None,
        ),
        None => (Vec::new(), None),
    };
    let items: Vec<Item> = rows
        .iter()
        .map(|row| {
            let mut item = Item::new(row.id.clone(), format!("{provider}/{}", row.id));
            if let Some(tag) = crate::model_catalog::row_tag(row) {
                item = item.tag(tag);
            }
            item
        })
        .collect();
    if items.is_empty() {
        // No rows at all - neither curated nor fetchable. The generic
        // adapter is the path and the model name is a free-text answer.
        // Inventing a list here would be the fake configuration the spec
        // forbids.
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
    let subtitle = match note {
        Some(n) => format!("{label} · {n}"),
        None => label,
    };
    let chosen = crate::prompt::pick_one("Model", &subtitle, items)?;
    // The picker's value is "provider/model", which is how the row was built
    // so the filter can match either half. `setup` composes the pair
    // itself, so strip the prefix or the config ends up as
    // "openai/openai/gpt-4o" and no provider resolves.
    Some(match chosen.split_once('/') {
        Some((_, m)) => m.to_string(),
        None => chosen,
    })
}

/// The Tools screen: one multi-select over the 15 supported tool
/// groups. Everything is pre-selected (absent = enabled); session
/// search stays on and is not listed. Esc keeps every tool on; a
/// confirmed empty selection turns them all off.
fn pick_tools() -> Vec<ToolGroup> {
    let items: Vec<Item> = ToolGroup::all()
        .iter()
        .map(|g| Item::new(g.label(), g.key()).desc(tool_group_desc(*g)))
        .collect();
    // Every group is preselected: the runtime gates each one - Computer
    // Use included - behind the ToolGroup toggle, the CUA driver binary,
    // the MCP server approval, and per-call human approval under the
    // default policy. Checking the box offers the capability; the
    // runtime still asks before it acts.
    let preselected: Vec<&str> = ToolGroup::all().iter().map(|g| g.key()).collect();
    match crate::prompt::pick_many(
        "Tools",
        "Space toggles \u{00b7} enter confirms \u{2014} session search stays on and is not listed",
        items,
        &preselected,
    ) {
        Some(checked) => checked.iter().filter_map(|k| ToolGroup::parse(k)).collect(),
        None => ToolGroup::all().to_vec(),
    }
}

/// One-line blurb per tool group. Groups with a provider screen say
/// so; Computer Use is a real capability now - the CUA driver registers
/// its tools through the MCP manager when approved, and every desktop
/// action parks for human approval under the default policy.
fn tool_group_desc(g: ToolGroup) -> &'static str {
    match g {
        ToolGroup::WebSearch => "web_search tool \u{2014} provider screen follows",
        ToolGroup::Browser => "browser automation \u{2014} provider screen follows",
        ToolGroup::Terminal => "run shell commands",
        ToolGroup::Files => "read, write, and list files",
        ToolGroup::Memory => "persistent memory \u{2014} provider screen follows",
        ToolGroup::Skills => "SKILL.md capabilities",
        ToolGroup::Tasks => "the agent's todo list",
        ToolGroup::Delegation => "spawn subagents",
        ToolGroup::AskUser => "pause and ask the user a question",
        ToolGroup::Vault => "Obsidian vault archive and library",
        ToolGroup::Voice => "speech to text + text to speech \u{2014} provider screens follow",
        ToolGroup::Vision => "image understanding via the [vision] aux model",
        ToolGroup::VideoAnalysis => "video understanding via the [video] aux model",
        ToolGroup::ComputerUse => {
            "desktop control via the CUA driver \u{2014} provider screen follows"
        }
        ToolGroup::Plugins => "plugin tools and MCP server integrations",
        ToolGroup::CodeIntel => "LSP diagnostics + repo-level git undo (code-intel tool group)",
    }
}

// ---------------------------------------------------------------------------
// Gateway screen (Full setup only)
// ---------------------------------------------------------------------------

/// The Gateway screen: Telegram and Discord bot tokens for `gateway run`.
/// Both optional - skipping leaves the chat surfaces disabled and the
/// gateway runs scheduler-only. Tokens go straight into the secrets store
/// (`<data_dir>/gateway.env`), never into config.toml, never printed or
/// logged; a process env var always wins at runtime.
///
/// There is deliberately no `SetupAnswers` field for these: secrets bypass
/// the shared answer struct the same way the setup `key` bypasses
/// config.toml, so the screen writes them the moment they are entered.
fn pick_gateways(data_dir: &Path) {
    println!("pantheon: gateway chat surfaces are optional - skip for a scheduler-only gateway");
    loop {
        let items = vec![
            Item::new("Telegram bot token", "telegram").desc(token_status(
                data_dir,
                pantheon_secrets::TELEGRAM_TOKEN_NAME,
            )),
            Item::new("Discord bot token", "discord")
                .desc(token_status(data_dir, pantheon_secrets::DISCORD_TOKEN_NAME)),
            Item::new("Done", "done").desc("continue setup"),
            Item::new("Skip", setup_providers::SKIP_VALUE)
                .desc("no chat surfaces - continue without them"),
        ];
        match crate::prompt::pick_one("Gateway", "chat surfaces for `gateway run`", items)
            .as_deref()
        {
            None | Some("done") => break,
            Some(v) if v == setup_providers::SKIP_VALUE => break,
            Some("telegram") => {
                edit_channel_token(data_dir, pantheon_secrets::TELEGRAM_TOKEN_NAME, "Telegram")
            }
            Some("discord") => {
                edit_channel_token(data_dir, pantheon_secrets::DISCORD_TOKEN_NAME, "Discord")
            }
            // The picker only produces the values above; anything else
            // fails closed to "done".
            _ => break,
        }
    }
    let tg = token_status(data_dir, pantheon_secrets::TELEGRAM_TOKEN_NAME);
    let dc = token_status(data_dir, pantheon_secrets::DISCORD_TOKEN_NAME);
    println!("pantheon: gateway tokens - telegram: {tg}; discord: {dc}");
}

/// Masked one-line status for a channel token: `set (••••1234)` or
/// `not set`. The full value is never displayed.
fn token_status(data_dir: &Path, name: &str) -> String {
    match pantheon_secrets::gateway_token(data_dir, name) {
        Some(v) => format!("set ({})", pantheon_secrets::mask_token(&v)),
        None => "not set".to_string(),
    }
}

/// One token entry: masked input, empty clears the stored token, esc
/// keeps the current value.
fn edit_channel_token(data_dir: &Path, name: &str, label: &str) {
    let subtitle = format!("{label} bot token - enter to save, empty clears, esc keeps current");
    match pick_secret("Gateway", &subtitle, "paste the bot token") {
        None => {}
        Some(raw) => {
            let value = raw.trim().to_string();
            let res = if value.is_empty() {
                pantheon_secrets::delete_gateway_token(data_dir, name)
            } else {
                pantheon_secrets::set_gateway_token(
                    data_dir,
                    name,
                    pantheon_secrets::SecretValue::new(value),
                )
            };
            match res {
                Ok(()) => println!("pantheon: {label} token updated in the secrets store"),
                Err(e) => println!("pantheon: could not update {label} token: {e}"),
            }
        }
    }
}

/// Masked single-line entry: the value is never echoed on screen.
/// `prompt` has no secret variant and the token fields are the only
/// callers, so this stays local to the wizard.
fn pick_secret(title: &str, subtitle: &str, placeholder: &str) -> Option<String> {
    let widget = crate::widget::TextInput::new(title)
        .secret()
        .hint("enter confirm  esc back".to_string())
        .placeholder(placeholder.to_string());
    let mut app = crate::app::TuiApp::new();
    app.push(crate::app::Screen::text(subtitle, widget));
    if app.run().is_err() {
        return None;
    }
    app.take_value().and_then(|s| s.text().map(String::from))
}

// ---------------------------------------------------------------------------
// Gateway service install screen (Full and Recommended, last screen)
// ---------------------------------------------------------------------------

/// Marker file recording a declined service install. The file's presence
/// is the decision: a recorded decline is never re-asked. `pantheon init`
/// installs the service on demand, so a decline stays reversible.
fn service_install_declined_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("gateway-service-declined")
}

fn service_install_declined(data_dir: &Path) -> bool {
    service_install_declined_path(data_dir).exists()
}

fn record_service_install_declined(data_dir: &Path) {
    // Best effort: a marker that cannot be written just means the
    // question comes back next run, which is the safe direction.
    if std::fs::create_dir_all(data_dir).is_ok() {
        let _ = std::fs::write(service_install_declined_path(data_dir), "declined\n");
    }
}

/// User-facing name for the install mechanism, e.g. "systemd user unit".
/// Pure, so the per-platform wording is unit-testable on any host.
fn service_mechanism_label(mechanism: &pantheon_gateway::ServiceMechanism) -> &'static str {
    use pantheon_gateway::ServiceMechanism as Mechanism;
    match mechanism {
        Mechanism::Systemd => "systemd user unit",
        Mechanism::Cron => "crontab @reboot entry",
        Mechanism::Launchd => "launchd agent",
        Mechanism::TaskScheduler => "Task Scheduler logon task",
        Mechanism::None => "no supported service manager",
    }
}

/// What the user's answer to the install question means. Kept separate
/// from the screen so the decision logic is unit-testable without the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceInstallChoice {
    Install,
    Decline,
}

fn service_install_choice(answer: Option<bool>) -> ServiceInstallChoice {
    match answer {
        // Only an explicit yes installs. Esc and a TUI error are the
        // safe answer: nothing installs without confirmation.
        Some(true) => ServiceInstallChoice::Install,
        _ => ServiceInstallChoice::Decline,
    }
}

/// Run the install through the shared `pantheon init` plumbing and
/// report the outcome honestly. A failed install prints its error and
/// never reads as success.
fn install_gateway_service(data_dir: &Path) {
    let exe = match pantheon_gateway::self_exe() {
        Some(e) => e,
        None => {
            println!(
                "pantheon: could not resolve the pantheon binary path; service install skipped"
            );
            return;
        }
    };
    match pantheon_gateway::install_service(data_dir, &exe) {
        pantheon_gateway::InstallOutcome::Installed { mechanism, changed } => {
            if changed {
                println!(
                    "pantheon: gateway service installed via {}.",
                    mechanism.as_str()
                );
            } else {
                println!(
                    "pantheon: gateway service already installed via {}; ensured running.",
                    mechanism.as_str()
                );
            }
            println!("      status: pantheon gateway status");
        }
        pantheon_gateway::InstallOutcome::Unavailable { note } => {
            println!("pantheon: {note}");
        }
        pantheon_gateway::InstallOutcome::Failed { mechanism, error } => {
            println!(
                "pantheon: service install via {} failed: {error}",
                mechanism.as_str()
            );
        }
    }
}

/// The final setup screen: install the gateway as an always-on
/// background service.
///
/// The platform's mechanism is shown BEFORE the question is asked, and
/// the install runs only on explicit confirmation. Declining is clean
/// no partial install - and records the marker so the question never
/// comes back. Skips silently when there is no service manager, when a
/// decline was recorded, or when the service is already installed.
fn pick_gateway_service_install(data_dir: &Path) {
    let status = pantheon_gateway::service_status();
    if status.detected == pantheon_gateway::ServiceMechanism::None {
        return;
    }
    if service_install_declined(data_dir) {
        return;
    }
    if status.installed.is_some() {
        return;
    }
    let question = format!(
        "Install the Pantheon gateway as an always-on background service (a {})? It starts on login and keeps scheduled tasks running. Saying no is fine: `pantheon init` installs it anytime.",
        service_mechanism_label(&status.detected),
    );
    match service_install_choice(crate::prompt::pick_confirm(
        "Service install",
        &question,
        false,
    )) {
        ServiceInstallChoice::Install => install_gateway_service(data_dir),
        ServiceInstallChoice::Decline => {
            record_service_install_declined(data_dir);
            println!(
                "pantheon: gateway service install declined; `pantheon init` installs it anytime"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Extensions screen (Full setup only)
// ---------------------------------------------------------------------------

/// The Extensions screen: MCP servers plus bundled plugins. Returns
/// `Some(servers)` - possibly empty - on Done/Esc, and `None` when the
/// user picks Skip. Esc keeps whatever was added and continues: the
/// wizard's standing contract is that a cancelled screen keeps the
/// answers gathered so far, and there is no recommended default to
/// fall back to.
///
/// SEAM (bundled-MCP catalog): the sibling catalog
/// (`pantheon_mcp::bundled`) has landed its recipe types. A catalog row
/// enters the wizard as `(recipe.name.to_string(),
/// recipe.to_config_entry(true))` - secrets already materialized as
/// `env:NAME` placeholders by the catalog, `enabled` flipped on by the
/// user's choice - and flows through the existing `mcp_servers` answer
/// into `mcp_section`, which writes the same `[mcp.servers.<name>]`
/// tables (and `enabled` flags) the dashboard/app toggles and
/// `set_bundled_enabled` operate on. Reconciling the catalog UI is
/// additive: new picker rows here plus that one constructor call, not a
/// screen rewrite. (The `bundled` module is still stabilizing in the
/// tree as of this writing, so the rows are not wired yet.)
fn pick_extensions(data_dir: &Path) -> Option<Vec<(String, McpServerEntry)>> {
    print_bundled_status(data_dir);
    let mut servers: Vec<(String, McpServerEntry)> = Vec::new();
    loop {
        let subtitle = if servers.is_empty() {
            "no MCP servers added yet".to_string()
        } else {
            let names: Vec<&str> = servers.iter().map(|(n, _)| n.as_str()).collect();
            format!("added: {}", names.join(", "))
        };
        let items = vec![
            Item::new("Add an MCP server", "add")
                .desc("stdio command or sse/http URL \u{2014} validated, never handshaked"),
            Item::new("Done", "done").desc("continue setup with the servers above"),
            Item::new("Skip", setup_providers::SKIP_VALUE)
                .desc("leave extensions unconfigured \u{2014} continue setup without them"),
        ];
        match crate::prompt::pick_one("Extensions", &subtitle, items).as_deref() {
            None | Some("done") => return Some(servers),
            Some(v) if v == setup_providers::SKIP_VALUE => return None,
            Some("add") => {
                if let Some(server) = add_mcp_server(&servers) {
                    servers.push(server);
                }
            }
            // The picker only produces the three values above; an
            // unknown value fails closed to "done with what was added".
            _ => return Some(servers),
        }
    }
}

/// Apply the Extensions screen's Skip: the Plugins group comes off the
/// enabled set and is recorded, so `run_setup` writes no `[mcp]`
/// section and the runtime never registers plugin tools. Pure - no
/// TUI - so the skip contract is unit-testable.
fn apply_extensions_skip(tools: &mut Vec<ToolGroup>, skipped: &mut Vec<ToolGroup>) {
    tools.retain(|g| *g != ToolGroup::Plugins);
    if !skipped.contains(&ToolGroup::Plugins) {
        skipped.push(ToolGroup::Plugins);
    }
}

/// One-line bundled-plugin status, printed when the Extensions screen
/// opens. Bundled (first-party) plugins live under
/// `<data_dir>/extensions/bundled/` and load without approval; there
/// is no enable/disable switch for them in `pantheon-extensions`
/// removing the directory is the disable path - so the screen reports
/// what is there and moves on. Today nothing ships there, and the
/// screen says so instead of inventing a registry.
fn print_bundled_status(data_dir: &Path) {
    let names = bundled_plugin_names(data_dir);
    if names.is_empty() {
        println!("pantheon: no bundled plugins installed");
    } else {
        println!(
            "pantheon: bundled plugins (always on): {}",
            names.join(", ")
        );
    }
}

/// Names of plugin dirs under `<data_dir>/extensions/bundled`
/// containing a `plugin.yaml`. Sorted; missing dir = no plugins.
fn bundled_plugin_names(data_dir: &Path) -> Vec<String> {
    let dir = data_dir.join("extensions").join("bundled");
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_dir() && e.path().join("plugin.yaml").exists())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

/// One MCP server's answers: name, transport, command+args or URL, env
/// vars. Returns `None` on cancel or failed validation - the screen
/// loop then continues without recording anything half-configured.
fn add_mcp_server(existing: &[(String, McpServerEntry)]) -> Option<(String, McpServerEntry)> {
    let name = crate::prompt::pick_text(
        "Extensions",
        "Server name \u{2014} becomes [mcp.servers.<name>]",
        "my-server",
    )?;
    let name = name.trim().to_string();
    if let Some(reason) = validate_server_name(&name, existing) {
        println!("pantheon: {reason}");
        return None;
    }
    let transports = vec![
        Item::new("stdio", "stdio").desc("spawn a local command"),
        Item::new("sse", "sse").desc("server-sent events endpoint"),
        Item::new("http", "http").desc("streamable HTTP endpoint"),
    ];
    let transport = crate::prompt::pick_one("Extensions", "Transport", transports)?;
    let mut entry = McpServerEntry {
        transport: transport.clone(),
        command: None,
        args: Vec::new(),
        env: HashMap::new(),
        url: None,
        enabled: true,
        timeout_secs: None,
    };
    match transport.as_str() {
        "stdio" => {
            let cmd = crate::prompt::pick_text(
                "Extensions",
                "Command to spawn (on PATH, or a full path)",
                "",
            )?;
            let cmd = cmd.trim().to_string();
            if cmd.is_empty() {
                return None;
            }
            entry.command = Some(cmd);
            let args = crate::prompt::pick_text(
                "Extensions",
                "Arguments (space-separated, empty = none)",
                "",
            )?;
            entry.args = split_args(&args);
        }
        "sse" | "http" => {
            let url = crate::prompt::pick_text("Extensions", "Endpoint URL", "https://")?;
            let url = url.trim().to_string();
            if url.is_empty() {
                return None;
            }
            entry.url = Some(url);
        }
        // The picker only offers the three above; anything else is a
        // bug, and recording it would write a config the runtime
        // rejects at startup.
        _ => return None,
    }
    let env = crate::prompt::pick_text(
        "Extensions",
        "Env vars, KEY=value comma-separated (a value of env:NAME reads $NAME at spawn; empty = none)",
        "",
    )?;
    entry.env = parse_env_spec(&env);
    let problems = validate_mcp_server(&name, &entry);
    if !problems.is_empty() {
        for p in &problems {
            println!("pantheon: {p}");
        }
        println!("pantheon: server not added \u{2014} fix the above and try again, or pick Skip");
        return None;
    }
    // Recorded, not handshaked: no live connection is attempted here.
    // `pantheon doctor` (config validation) and the MCP manager at
    // session start verify the server actually works.
    println!("pantheon: MCP server {name:?} recorded");
    Some((name, entry))
}

/// Setup-time validation for one MCP server: the config's own
/// [`McpServerEntry::problems`] - so the wizard can never disagree
/// with config validation - plus the two setup checks the config
/// cannot do: a stdio command must resolve on PATH (or be an
/// executable path), and an sse/http URL must be an http(s) URL. Pure.
fn validate_mcp_server(name: &str, entry: &McpServerEntry) -> Vec<String> {
    let mut problems = entry.problems(name);
    if entry.transport == "stdio" {
        if let Some(cmd) = entry
            .command
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        {
            if !command_on_path(cmd) {
                problems.push(format!(
                    "mcp.servers.{name}: command {cmd:?} not found on PATH"
                ));
            }
        }
    }
    if matches!(entry.transport.as_str(), "sse" | "http") {
        if let Some(url) = entry
            .url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                problems.push(format!(
                    "mcp.servers.{name}: url {url:?} is not an http(s) URL"
                ));
            }
        }
    }
    problems
}

/// A server name becomes a TOML table key (`[mcp.servers.<name>]`), so
/// it must be non-empty, unique within this screen, and free of
/// characters that would split or break the key. `Some(reason)` when
/// invalid.
fn validate_server_name(name: &str, existing: &[(String, McpServerEntry)]) -> Option<String> {
    if name.is_empty() {
        return Some("server name is empty".to_string());
    }
    if existing.iter().any(|(n, _)| n == name) {
        return Some(format!("a server named {name:?} is already added"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Some(format!(
            "server name {name:?}: use letters, digits, - and _ only"
        ));
    }
    // Bundled recipe names are reserved: a custom server borrowing one
    // would run under the bundled name while carrying an arbitrary
    // command. Mirrors the dashboard add endpoint's rejection - the
    // catalog entry is enabled, never shadowed.
    if pantheon_api::mcp_catalog::find(name).is_some() {
        return Some(format!(
            "server name {name:?} is a bundled Pantheon MCP server: enable the catalog entry instead of adding a custom one"
        ));
    }
    None
}

/// True when `cmd` resolves: an executable file when it contains a
/// slash, otherwise found in some `PATH` directory. No shell is
/// involved, so a hostile command string cannot execute anything here.
fn command_on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return is_executable_file(Path::new(cmd));
    }
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| is_executable_file(&dir.join(cmd))))
        .unwrap_or(false)
}

/// Parse `KEY=value, KEY2=value2` into the env map. A value starting
/// with `env:` is preserved verbatim: the MCP manager resolves it
/// from the operator's environment at spawn time, so secrets never
/// land in the config file. Malformed pairs (no `=`, empty key or
/// value, bad key shape) are dropped.
fn parse_env_spec(spec: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in spec.split(',') {
        let pair = pair.trim();
        let Some((k, v)) = pair.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() || v.is_empty() || !is_env_key(k) {
            continue;
        }
        out.insert(k.to_string(), v.to_string());
    }
    out
}

fn is_env_key(k: &str) -> bool {
    let mut chars = k.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn split_args(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::validate_server_name;
    use pantheon_api::config::McpServerEntry;
    use std::collections::HashMap;

    fn dummy_entry() -> McpServerEntry {
        McpServerEntry {
            transport: "stdio".to_string(),
            command: Some("x".to_string()),
            args: Vec::new(),
            env: HashMap::new(),
            url: None,
            enabled: false,
            timeout_secs: None,
        }
    }

    /// Bundled recipe names are reserved for the catalog: a custom
    /// server borrowing one would run under the bundled name while
    /// carrying an arbitrary command. Mirrors the dashboard add
    /// endpoint's rejection.
    #[test]
    fn rejects_bundled_recipe_names() {
        for name in [
            "github",
            "google-workspace",
            "chrome-devtools",
            "cloudflare",
            "playwright",
            "notion",
        ] {
            assert!(
                validate_server_name(name, &[]).is_some(),
                "bundled recipe name {name:?} must be rejected as a custom server name"
            );
        }
    }

    #[test]
    fn accepts_ordinary_names() {
        assert!(validate_server_name("my-server", &[]).is_none());
        assert!(validate_server_name("my_server2", &[]).is_none());
    }

    #[test]
    fn still_rejects_empty_and_duplicates() {
        assert!(validate_server_name("", &[]).is_some());
        let existing = vec![("my-server".to_string(), dummy_entry())];
        assert!(validate_server_name("my-server", &existing).is_some());
    }
}
