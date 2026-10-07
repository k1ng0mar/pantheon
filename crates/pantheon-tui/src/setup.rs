//! `pantheon setup`: first-run configuration wizard.
//!
//! Five steps (model+fallback, execution policy, tools, provider-backed
//! tool groups, memory backend). Non-interactive friendly: every step can
//! be preset with a flag, so scripted setup never blocks on stdin.
//! Anything missing after the flags is prompted for.
//!
//! Provider-backed groups (web search, browser, STT, TTS, memory)
//! enumerate the owning crate's registry, so each picker lists every
//! implemented backend with its auth requirement (API key / keyless /
//! self-hosted / local binary). The choices land in `[websearch]`,
//! `[browser]`, `[stt]`, `[tts]`, `[memory]`; the runtime builds each
//! backend from the same registry, so a pick here is always one the
//! runtime can honor.
//!
//! Secrets: the wizard only asks for the ENV VAR NAME, never the value.

use super::config::{Config, FallbackEntry, MemorySection, ModelSection};
use super::config_schema::PolicyPreset;
use crate::setup_providers::{self, ProviderAnswer, ProviderKind, ProviderMeta};
use pantheon_api::config::{
    BrowserSection, ComputerUseSection, McpSection, McpServerEntry, ToolGroup, ToolsSection,
    VoiceSection,
};
use std::collections::HashMap;
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
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key_env: Option<String>,
    pub fallback_provider: Option<String>,
    pub fallback_model: Option<String>,
    pub policy: Option<PolicyPreset>,
    /// Tool groups from the Tools screen. `None` = the screen never ran;
    /// treated as all enabled (absent = enabled).
    pub tools: Option<Vec<ToolGroup>>,
    /// Provider answers, written to `[websearch]`, `[browser]`,
    /// `[stt]`/`[tts]`, `[memory]`, `[computer_use]`. `None` = the screen
    /// never ran; the non-interactive path resolves the recommended
    /// provider.
    pub websearch: Option<ProviderAnswer>,
    pub browser: Option<ProviderAnswer>,
    pub stt: Option<ProviderAnswer>,
    pub tts: Option<ProviderAnswer>,
    pub memory: Option<ProviderAnswer>,
    pub computer: Option<ProviderAnswer>,
    /// A raw API key, for scripting setup. Never written to config.toml:
    /// it goes into `<data_dir>/.env` under `api_key_env` (or a derived
    /// `PANTHEON_KEY_<PROVIDER>` name) and the config stores only the name.
    pub key: Option<String>,
    /// A custom endpoint the wizard's provider picker collected. Written
    /// to `[custom_providers.<name>]` on the same save path `pantheon
    /// provider add` uses, so a custom endpoint set up in the wizard is
    /// durable - not a string only the wizard remembers. `None` = the
    /// Custom row was never picked.
    pub custom_provider: Option<CustomProviderSpec>,
    /// Tool groups the wizard's Skip row left unconfigured. The wizard
    /// already removed them from `tools`, so no section is written and
    /// the runtime never registers them; this is the done-summary
    /// record. The flags path leaves it empty.
    pub skipped_tools: Vec<ToolGroup>,
    /// Per-backend voice skips from the wizard. STT and TTS share the
    /// Voice tool group, so the group toggle cannot express "STT
    /// skipped, TTS configured": a skipped backend resolves to nothing
    /// here instead of the recommended default. The flags path leaves
    /// these false.
    pub skipped_stt: bool,
    pub skipped_tts: bool,
    /// MCP servers from the wizard's Extensions screen, as
    /// `(name, entry)` pairs - the single seam where hand-typed and
    /// (later) catalog-backed servers meet. Written to
    /// `[mcp.servers.<name>]`, using the same `enabled` flags the
    /// config/dashboard/app toggles use: the config document is the
    /// only source of truth, never a parallel wizard-side file.
    /// Empty = the screen never ran, nothing was added, or the user
    /// skipped extensions. The flags path leaves it empty.
    pub mcp_servers: Vec<(String, McpServerEntry)>,
    /// Third-party skill dependencies skipped at install-or-skip time.
    /// Written to `[skill_deps].skipped` so `pantheon doctor` can report
    /// the gap on later runs. The wizard's screen fills this in; the
    /// `--yes` path detects everything and records the missing ids
    /// without prompting or installing.
    pub skill_deps_skipped: Vec<String>,
}

/// A custom model endpoint collected by the wizard's provider picker:
/// everything needed to persist a `[custom_providers.<name>]` row.
#[derive(Debug, Clone, Default)]
pub struct CustomProviderSpec {
    /// Table name under `[custom_providers]` (the wizard uses "custom").
    pub name: String,
    /// Full base URL, no trailing slash.
    pub base_url: String,
    /// Wire format: `openai` or `anthropic`.
    pub api_mode: String,
    /// Env var holding the API key. Never the key itself.
    pub key_env: Option<String>,
}

/// Run the wizard. `stdin` is only touched for missing answers.
pub fn run_setup(data_dir: &Path, answers: SetupAnswers, assume_defaults: bool) -> Config {
    println!("pantheon setup");
    println!("  data dir: {}", data_dir.display());
    println!();

    // 1. Model + fallback.
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
                    super::config::sanitize_env_suffix(&provider)
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

    // 2. Execution policy.
    let policy = answers.policy.unwrap_or_else(|| {
        if assume_defaults {
            PolicyPreset::Coder
        } else {
            println!("Execution policy:");
            println!("  reader       read-only, nothing executes");
            println!("  coder        shell + edits allowed (recommended)");
            println!("  coder_memory coder + memory writes");
            let a = prompt("Policy", Some("coder"));
            PolicyPreset::parse(&a).unwrap_or(PolicyPreset::Coder)
        }
    });

    // 3. Provider-backed tool groups. One kind-driven flow serves web
    // search, browser, STT, TTS, and memory: the rows come from the
    // owning crate's registry, the follow-ups from the provider kind
    // (keyless / cloud key env / self-hosted URL / local detect+install).
    // A pre-set answer (flag, wizard) is validated against the registry;
    // otherwise the recommended provider resolves, with stdin prompts
    // unless --yes. A disabled tool group resolves to nothing: the
    // section is not written and the runtime never registers the tools.
    let group_on = |g: ToolGroup| {
        answers
            .tools
            .as_ref()
            .map(|t| t.contains(&g))
            .unwrap_or(true)
    };
    let websearch = resolve_provider(
        "web search",
        setup_providers::websearch_providers(),
        answers.websearch.clone(),
        group_on(ToolGroup::WebSearch),
        assume_defaults,
    );
    let browser = resolve_provider(
        "browser",
        setup_providers::browser_providers(),
        answers.browser.clone(),
        group_on(ToolGroup::Browser),
        assume_defaults,
    );
    let (stt, tts) = if group_on(ToolGroup::Voice) {
        // A wizard-skipped backend resolves to nothing - never the
        // recommended default. Without this gate, "STT skipped, TTS
        // configured" would still write a recommended [stt] section the
        // user explicitly declined.
        let stt = if answers.skipped_stt {
            None
        } else {
            resolve_provider(
                "STT",
                setup_providers::stt_providers(),
                answers.stt.clone(),
                true,
                assume_defaults,
            )
        };
        let tts = if answers.skipped_tts {
            None
        } else {
            resolve_provider(
                "TTS",
                setup_providers::tts_providers(),
                answers.tts.clone(),
                true,
                assume_defaults,
            )
        };
        (stt, tts)
    } else {
        (None, None)
    };
    // Memory: the existing selection file wins when no flag or wizard
    // answer overrode it (setup never silently downgrades a working
    // backend).
    let existing = load_backend_selection(data_dir);
    let memory = if !group_on(ToolGroup::Memory) {
        None
    } else if let Some(a) = answers.memory.clone() {
        Some(validated_answer(
            "memory",
            &setup_providers::memory_providers(),
            a,
        ))
    } else if existing.name != "native" {
        Some(ProviderAnswer {
            id: existing.name.clone(),
            ..Default::default()
        })
    } else {
        default_or_prompt(
            "memory",
            setup_providers::memory_providers(),
            assume_defaults,
        )
    };
    // Computer use: one driver today (cua-driver). A disabled group
    // resolves to nothing: no section is written and the runtime never
    // spawns the driver.
    let computer = resolve_provider(
        "computer use",
        setup_providers::computer_providers(),
        answers.computer.clone(),
        group_on(ToolGroup::ComputerUse),
        assume_defaults,
    );

    // Skill dependencies: third-party packages the skill library
    // needs. `--yes` (and the wizard, which already ran the screen)
    // detects only and records the missing ids - a scripted setup never
    // installs behind the user's back. The interactive stdin path runs
    // the full install-or-skip screen.
    let skill_deps_skipped = if assume_defaults {
        crate::skill_deps::detect_all_skipped(&crate::setup_providers::detect_binary)
    } else {
        crate::skill_deps::run_skill_deps_screen(
            &mut |line: &str| println!("pantheon: {line}"),
            &mut |q: &str, d: bool| Some(confirm(q, d)),
            &mut crate::skill_deps::shell_install,
            &crate::setup_providers::detect_binary,
        )
    };

    let fresh = Config {
        agent: None,
        model: Some(ModelSection {
            reasoning_budget: None,
            provider,
            model,
            api_key_env: if api_key_env.is_empty() {
                None
            } else {
                Some(api_key_env)
            },
            fallbacks,
            reasoning: None,
        }),
        judge: None,
        embeddings: None,
        search_synthesis: None,
        browser: browser.as_ref().map(browser_section),
        computer_use: computer.as_ref().map(|a| ComputerUseSection {
            driver: Some(a.id.clone()),
            binary: None,
        }),
        websearch: websearch.as_ref().map(|a| super::config::WebsearchSection {
            enabled: Some(true),
            provider: Some(a.id.clone()),
            api_key_secret: a.key_env.clone(),
            base_url: a.url.clone(),
            max_results: None,
        }),
        verify: None,
        mcp: mcp_section(&answers.mcp_servers),
        vision: None,
        scheduled: None,
        mcp_synthesis: None,
        compression: None,
        title_gen: None,
        stt: stt.as_ref().map(voice_section),
        tts: tts.as_ref().map(voice_section),
        policy: Some(policy),
        // Setup never picks a permission mode: a fresh install starts on
        // the conservative `Ask` default, and the operator opts into
        // `smart` or `allow_all` deliberately afterwards.
        permission_mode: None,
        memory: memory_section(&memory),
        server: Some(super::config::ServerSection {
            port: 18789,
            host: "127.0.0.1".into(),
        }),
        custom_providers: Default::default(),
        agents: Default::default(),
        secrets: None,
        retention: None,
        tui: None,
        reflect: None,
        extraction: None,
        planner: None,
        rerank: None,
        consolidation: None,
        nightly: None,
        // Nightly repair phase: not configured by the wizard. Absent
        // keeps LLM diagnosis off - the phase still runs
        // deterministically (bounded retry, then disable/pause +
        // escalate) inside an enabled nightly pass; pinning `[repair]`
        // only adds repair-model diagnosis.
        repair: None,
        temporal: None,
        budget: None,
        // Swarm caps (workstream: swarm backend): not configured by the
        // wizard; absent keeps the documented defaults (4/2/4).
        swarm: None,
        goal: None,
        video: None,
        plugins: Default::default(),
        // Only deviations from the all-on default are written, so a
        // default setup keeps the config quiet about a choice it did not
        // make.
        tools: answers
            .tools
            .as_ref()
            .and_then(|t| ToolsSection::from_enabled(t)),
        // Skipped skill dependencies: the durable record `pantheon
        // doctor` reports. Empty = the screen found (or installed)
        // everything, so no section is written.
        skill_deps: if skill_deps_skipped.is_empty() {
            None
        } else {
            Some(super::config::SkillDepsSection {
                skipped: skill_deps_skipped,
            })
        },
        // The wizard does not configure the Cloudflare integration; absent
        // section keeps the integration off (no token injection).
        cloudflare: None,
        // The wizard does not configure gateway channels; absent section
        // keeps the previous default.
        gateway: None,
        // Live voice mode stays off unless the user opts in later.
        voice: None,
    };

    // Merge, don't clobber: the wizard manages a fixed set of sections
    // (model, the tool/provider sections, policy, server, skill_deps).
    // Everything the operator may have added by hand - [budget],
    // [nightly], [agents.*], [custom_providers.*], aux model pins,
    // [gateway], [voice], [plugins] - survives a re-run of setup, and
    // the wizard names what it kept on stderr. Previously run_setup
    // built a fresh Config and saved it, wiping all of the above every
    // time the wizard ran.
    //
    // stt/tts use explicit-clear semantics: the Voice screen always
    // resolves them (defaults, answers, or explicit skips), so the
    // wizard's value - including None for "group off / backend
    // skipped" - is authoritative and overwrites.
    //
    // A config that exists but does not parse aborts setup outright: the
    // old `unwrap_or_default()` fell back to a fresh default and the
    // save below silently wiped the user's file.
    let mut cfg = match Config::load(data_dir) {
        Ok(c) => c,
        Err(e) if e.code == "CONFIG_OPEN" => Config::default(),
        Err(e) => {
            eprintln!("setup: refusing to run: {}", e.cause);
            eprintln!("setup: fix: {}", e.remediation);
            std::process::exit(1);
        }
    };
    {
        let mut preserved: Vec<&str> = Vec::new();
        let mut keep = |name: &'static str, set: bool| {
            if set {
                preserved.push(name);
            }
        };
        keep("[agent]", cfg.agent.is_some());
        keep("[budget]", cfg.budget.is_some());
        keep("[nightly]", cfg.nightly.is_some());
        keep("[repair]", cfg.repair.is_some());
        keep("[temporal]", cfg.temporal.is_some());
        keep("[goal]", cfg.goal.is_some());
        keep("[swarm]", cfg.swarm.is_some());
        keep("[retention]", cfg.retention.is_some());
        keep("[secrets]", cfg.secrets.is_some());
        keep("[tui]", cfg.tui.is_some());
        keep("[gateway]", cfg.gateway.is_some());
        keep("[voice]", cfg.voice.is_some());
        keep("[agents.*]", !cfg.agents.is_empty());
        keep("[custom_providers.*]", !cfg.custom_providers.is_empty());
        keep("[plugins]", !cfg.plugins.is_empty());
        // Aux model pins the wizard never touches.
        keep("[judge]", cfg.judge.is_some());
        keep("[embeddings]", cfg.embeddings.is_some());
        keep("[search_synthesis]", cfg.search_synthesis.is_some());
        keep("[vision]", cfg.vision.is_some());
        keep("[video]", cfg.video.is_some());
        keep("[scheduled]", cfg.scheduled.is_some());
        keep("[mcp_synthesis]", cfg.mcp_synthesis.is_some());
        keep("[extraction]", cfg.extraction.is_some());
        keep("[rerank]", cfg.rerank.is_some());
        keep("[planner]", cfg.planner.is_some());
        keep("[verify]", cfg.verify.is_some());
        keep("[compression]", cfg.compression.is_some());
        keep("[title_gen]", cfg.title_gen.is_some());
        keep("[reflect]", cfg.reflect.is_some());
        keep("[consolidation]", cfg.consolidation.is_some());
        if !preserved.is_empty() {
            eprintln!(
                "setup: kept existing {} (not managed by the wizard)",
                preserved.join(", ")
            );
        }
    }
    cfg.model = fresh.model;
    cfg.browser = fresh.browser;
    cfg.computer_use = fresh.computer_use;
    cfg.websearch = fresh.websearch;
    cfg.mcp = fresh.mcp;
    cfg.stt = fresh.stt;
    cfg.tts = fresh.tts;
    cfg.policy = fresh.policy;
    cfg.memory = fresh.memory;
    cfg.server = fresh.server;
    cfg.tools = fresh.tools;
    cfg.skill_deps = fresh.skill_deps;
    // The wizard only ever ADDS a custom endpoint row; existing rows
    // are preserved above, never cleared. Models the operator named on
    // this endpoint are kept, mirroring `upsert_custom_row` (the
    // `pantheon provider add` path) - replacing the row used to empty
    // its model list on a re-pick.
    if let Some(spec) = &answers.custom_provider {
        let models = cfg
            .custom_providers
            .get(&spec.name)
            .map(|s| s.models.clone())
            .unwrap_or_default();
        cfg.custom_providers.insert(
            spec.name.clone(),
            super::config::CustomProviderSection {
                base_url: spec.base_url.clone(),
                api_mode: spec.api_mode.clone(),
                key_env: spec.key_env.clone(),
                models,
            },
        );
    }

    match cfg.save(data_dir) {
        Ok(()) => {
            // Keep the memory-backend selection file in sync: it is what
            // the runtime instantiates from.
            save_backend_selection(
                data_dir,
                &pantheon_memory::BackendSelection {
                    name: memory
                        .as_ref()
                        .map(|a| a.id.clone())
                        .unwrap_or_else(|| "native".to_string()),
                    options: memory.as_ref().map(memory_options).unwrap_or_default(),
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

/// Validate a pre-set provider answer against the registry. An unknown
/// id is a hard error: writing it would produce a config the runtime
/// rejects at startup.
fn validated_answer(what: &str, metas: &[ProviderMeta], a: ProviderAnswer) -> ProviderAnswer {
    if setup_providers::find_provider(metas, &a.id).is_some() {
        return a;
    }
    let ids = metas
        .iter()
        .map(|p| p.id.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!("setup: unknown {what} provider {:?}", a.id);
    eprintln!("setup: choose one of: {ids}");
    std::process::exit(2);
}

/// The non-interactive answer for one provider group: the recommended
/// row (first row when nothing is marked), with kind defaults under
/// `--yes` and stdin follow-ups otherwise.
fn default_or_prompt(
    what: &str,
    metas: Vec<ProviderMeta>,
    assume_defaults: bool,
) -> Option<ProviderAnswer> {
    let meta = setup_providers::recommended_provider(&metas)
        .or_else(|| metas.first())?
        .clone();
    if assume_defaults {
        return Some(default_answer(&meta));
    }
    println!("{what} provider:");
    for p in &metas {
        println!("  {:<12} {}", p.id, p.name);
    }
    loop {
        let choice = prompt(&format!("{what} provider"), Some(&meta.id));
        if let Some(m) = setup_providers::find_provider(&metas, &choice) {
            let m = m.clone();
            return setup_providers::complete_answer(
                &m,
                &mut |label, prefill| Some(prompt(label, Some(prefill))),
                &mut |question, def| Some(confirm(question, def)),
            );
        }
        let ids = metas
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!("unknown provider '{choice}'; choose one of: {ids}");
    }
}

/// The `--yes` answer for one provider: kind defaults, no prompts, no
/// installs. A missing local binary records `skipped` so `pantheon
/// doctor` can report the gap instead of the config pretending the
/// provider works.
fn default_answer(meta: &ProviderMeta) -> ProviderAnswer {
    let mut answer = ProviderAnswer {
        id: meta.id.clone(),
        ..Default::default()
    };
    match &meta.kind {
        ProviderKind::Keyless => {}
        ProviderKind::Cloud { env_var } => {
            answer.key_env = Some(env_var.clone());
        }
        ProviderKind::SelfHosted { default_url, .. } => {
            answer.url = non_empty(default_url.clone());
        }
        ProviderKind::Local { local } => {
            if !setup_providers::detect_binary(&local.detect_cmd) {
                answer.skipped = true;
            }
        }
    }
    for f in &meta.extra {
        if !f.default.is_empty() {
            answer.options.push((f.key.clone(), f.default.clone()));
        }
    }
    answer
}

/// Resolve one provider-backed group: a disabled tool group resolves
/// to nothing (the section is not written); a pre-set answer is
/// validated; otherwise the recommended provider resolves.
fn resolve_provider(
    what: &str,
    metas: Vec<ProviderMeta>,
    pre: Option<ProviderAnswer>,
    group_on: bool,
    assume_defaults: bool,
) -> Option<ProviderAnswer> {
    if !group_on {
        return None;
    }
    match pre {
        Some(a) => Some(validated_answer(what, &metas, a)),
        None => default_or_prompt(what, metas, assume_defaults),
    }
}

/// `[browser]`: the key env and URL route to backend-specific fields;
/// the ids come from the registry's `BackendKind::id()`.
fn browser_section(a: &ProviderAnswer) -> BrowserSection {
    let mut s = BrowserSection {
        backend: Some(a.id.clone()),
        ..Default::default()
    };
    match a.id.as_str() {
        "steel" => {
            s.steel_api_key_secret = a.key_env.clone();
        }
        "browserbase" => {
            s.browserbase_api_key_secret = a.key_env.clone();
        }
        "lightpanda" => {
            s.lightpanda_cdp_url = a.url.clone();
        }
        _ => {}
    }
    for (k, v) in &a.options {
        match k.as_str() {
            "steel_base_url" => s.steel_base_url = Some(v.clone()),
            "browserbase_project_id" => s.browserbase_project_id = Some(v.clone()),
            _ => {}
        }
    }
    s
}

/// `[stt]` / `[tts]`: the backend id plus its options. The key env is
/// written only when the user named a different one than the catalog
/// default - the default stays implicit.
fn voice_section(a: &ProviderAnswer) -> VoiceSection {
    let mut options: HashMap<String, String> = a.options.iter().cloned().collect();
    if let Some(env) = &a.key_env {
        let is_default = pantheon_providers::voice_key_env(&a.id) == Some(env.as_str());
        if !is_default {
            options.insert("api_key_env".to_string(), env.clone());
        }
    }
    VoiceSection {
        backend: a.id.clone(),
        options,
    }
}

/// `[mcp]`: one `[mcp.servers.<name>]` table per server from the
/// wizard's Extensions screen - the single seam every MCP producer
/// funnels through (the Extensions screen today, the bundled catalog
/// tomorrow). Empty = the screen never ran, nothing was added, or
/// extensions were skipped: no section is written and the master
/// switch keeps its default (on, with zero servers).
fn mcp_section(servers: &[(String, McpServerEntry)]) -> Option<McpSection> {
    if servers.is_empty() {
        return None;
    }
    Some(McpSection {
        enabled: None,
        servers: servers.iter().cloned().collect(),
    })
}

/// `[memory]`: native is the runtime default, so it writes nothing.
/// Anything else records the backend plus its URL option.
fn memory_section(answer: &Option<ProviderAnswer>) -> Option<MemorySection> {
    let a = answer.as_ref()?;
    if a.id == "native" {
        return None;
    }
    Some(MemorySection {
        backend: a.id.clone(),
        options: memory_options(a),
    })
}

/// The option map for a non-native memory backend. The `http` bridge
/// reads `PANTHEON_MEMORY_HTTP_URL` from the environment, not
/// `options.url`, so a URL answer for it becomes an export instruction
/// instead of a config value the runtime would ignore.
fn memory_options(a: &ProviderAnswer) -> HashMap<String, String> {
    let mut options = HashMap::new();
    if let Some(url) = &a.url {
        if a.id == "http" {
            println!(
                "pantheon: the http memory backend reads its URL from the environment - export PANTHEON_MEMORY_HTTP_URL={url} in your shell"
            );
        } else {
            options.insert("url".to_string(), url.clone());
        }
    }
    options
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
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

/// `pantheon setup --help`.
fn setup_help() {
    eprintln!("usage: pantheon setup [--yes] [options]");
    eprintln!(
        "  --yes                  accept defaults for anything without a flag (non-interactive)"
    );
    eprintln!("  --provider ID          default model provider (catalog id, URL, or \"local\")");
    eprintln!("  --model MODEL          default model name");
    eprintln!("  --api-key-env NAME     env var holding the provider API key");
    eprintln!("  --key SECRET           raw API key (goes to <data_dir>/.env, never config.toml)");
    eprintln!("  --fallback-provider ID fallback provider");
    eprintln!("  --fallback-model MODEL fallback model");
    eprintln!("  --policy POLICY        execution policy: reader | coder | coder_memory");
    eprintln!("  --websearch-provider ID [--websearch-key-env NAME] [--websearch-url URL]");
    eprintln!("  --browser-provider ID   [--browser-key-env NAME]   [--browser-url URL]");
    eprintln!("  --stt-provider ID       [--stt-key-env NAME]       [--stt-url URL]");
    eprintln!("  --tts-provider ID       [--tts-key-env NAME]       [--tts-url URL]");
    eprintln!("  --memory ID            memory backend");
    eprintln!("  --computer ID          computer-use driver");
    eprintln!();
    eprintln!("Without flags (or for anything without a flag) the wizard prompts");
    eprintln!("interactively. Re-running setup merges: the wizard rewrites the");
    eprintln!("sections it manages and keeps everything else ([budget],");
    eprintln!("[nightly], [agents.*], [custom_providers.*], aux pins, ...).");
}

/// `pantheon setup`: flag parsing over the wizard.
pub fn cmd_setup(args: &[String]) {
    let raw: &[String] = &args[1.min(args.len())..];
    // `--help` used to fall into the wizard and prompt for a provider;
    // help is not a setup run.
    if raw.iter().any(|a| a == "--help" || a == "-h") {
        setup_help();
        std::process::exit(0);
    }
    let parsed = crate::args::Args::parse(raw);
    let flag = |name: &str| parsed.flag(name);
    let policy = flag("policy")
        .and_then(|p| PolicyPreset::parse(&p))
        .or_else(|| {
            let bad = flag("policy");
            if let Some(b) = bad {
                eprintln!("setup: unknown policy {b:?} (reader|coder|coder_memory)");
                std::process::exit(2);
            }
            None
        });
    // Provider answers from flags: ids are validated against the
    // registries in run_setup; key env / URL ride along where given.
    let websearch = flag("websearch-provider").map(|id| ProviderAnswer {
        id,
        key_env: flag("websearch-key-env"),
        url: flag("websearch-url"),
        ..Default::default()
    });
    let browser = flag("browser-provider").map(|id| ProviderAnswer {
        id,
        key_env: flag("browser-key-env"),
        url: flag("browser-url"),
        ..Default::default()
    });
    let stt = flag("stt-provider").map(|id| ProviderAnswer {
        id,
        key_env: flag("stt-key-env"),
        url: flag("stt-url"),
        ..Default::default()
    });
    let tts = flag("tts-provider").map(|id| ProviderAnswer {
        id,
        key_env: flag("tts-key-env"),
        url: flag("tts-url"),
        ..Default::default()
    });
    let answers = SetupAnswers {
        provider: flag("provider"),
        model: flag("model"),
        api_key_env: flag("api-key-env"),
        fallback_provider: flag("fallback-provider"),
        fallback_model: flag("fallback-model"),
        policy,
        tools: None,
        websearch,
        browser,
        stt,
        tts,
        memory: flag("memory").map(|id| ProviderAnswer {
            id,
            ..Default::default()
        }),
        computer: flag("computer").map(|id| ProviderAnswer {
            id,
            ..Default::default()
        }),
        key: flag("key"),
        custom_provider: None,
        // The flags path never skips: skip is a wizard-screen decision.
        skipped_tools: Vec::new(),
        skipped_stt: false,
        skipped_tts: false,
        skill_deps_skipped: Vec::new(),
        mcp_servers: Vec::new(),
    };
    // --yes: accept defaults for anything without a flag (non-interactive).
    run_setup(&crate::terminal::data_dir(), answers, parsed.has("yes"));
}
