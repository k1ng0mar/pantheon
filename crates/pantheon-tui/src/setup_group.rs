//! setup_group: setup, setup_wizard, setup_graph in one file.

pub mod setup {
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

    use crate::config::{Config, FallbackEntry, MemorySection, ModelSection};
    use crate::config_schema::PolicyPreset;
    use crate::setup_providers::{self, ProviderAnswer, ProviderKind, ProviderMeta};
    use pantheon_api::config::{
        BrowserSection, ComputerUseSection, McpSection, McpServerEntry, ToolGroup, ToolsSection,
        VoiceSection,
    };
    use std::collections::HashMap;
    use std::io::BufRead;
    use std::io::IsTerminal;
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
                        crate::config::sanitize_env_suffix(&provider)
                    )
                }
            }
            _ => api_key_env,
        };
        // Fallback chains live under `pantheon fallback` - setup never
        // prompts for one. A pre-filled answer is still honored.
        let mut fallbacks = Vec::new();
        // Both halves must be pre-filled; there is no prompt here by design.
        if let (Some(fp), Some(fm)) = (
            answers.fallback_provider.clone(),
            answers.fallback_model.clone(),
        ) {
            if !fp.is_empty() && !fm.is_empty() {
                fallbacks.push(FallbackEntry {
                    provider: fp,
                    model: fm,
                    key_env: None,
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
            data_dir,
            None,
        );
        let browser = resolve_provider(
            "browser",
            setup_providers::browser_providers(),
            answers.browser.clone(),
            group_on(ToolGroup::Browser),
            assume_defaults,
            data_dir,
            None,
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
                    data_dir,
                    None,
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
                    data_dir,
                    None,
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
                &setup_providers::memory_providers(data_dir),
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
                setup_providers::memory_providers(data_dir),
                assume_defaults,
                data_dir,
                Some("bring your own memory system: `pantheon memory backend scaffold NAME [http|stdio]` lists it here"),
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
            data_dir,
            None,
        );

        // Skill dependencies: third-party packages the skill library
        // needs. `--yes` (and the wizard, which already ran the screen)
        // detects only and records the missing ids - a scripted setup never
        // installs behind the user's back. The interactive stdin path runs
        // the full install-or-skip screen.
        let skill_deps_skipped = if assume_defaults {
            pantheon_doctor::skill_deps::detect_all_skipped(&crate::setup_providers::detect_binary)
        } else {
            pantheon_doctor::skill_deps::run_skill_deps_screen(
                &mut |line: &str| println!("pantheon: {line}"),
                &mut |q: &str, d: bool| Some(confirm(q, d)),
                &mut pantheon_doctor::skill_deps::shell_install,
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
                timeout_ms: None,
            }),
            judge: None,
            embeddings: None,
            search_synthesis: None,
            browser: browser.as_ref().map(browser_section),
            computer_use: computer.as_ref().map(|a| ComputerUseSection {
                driver: Some(a.id.clone()),
                binary: None,
            }),
            egress: None,
            admin: None,
            routing: None,
            websearch: websearch.as_ref().map(|a| crate::config::WebsearchSection {
                enabled: Some(true),
                provider: Some(a.id.clone()),
                api_key_secret: a.key_env.clone(),
                // Per-provider, never global: the picked provider's URL is
                // stored under its own id, so a later provider switch
                // cannot send another provider's calls to this host.
                base_url: None,
                base_urls: a
                    .url
                    .clone()
                    .map(|u| [(a.id.clone(), u)].into_iter().collect())
                    .unwrap_or_default(),
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
            server: Some(crate::config::ServerSection {
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
            // Delegation routes: not configured by the wizard; absent
            // means no routes, so routed delegate calls are refused
            // and direct delegate calls work as before.
            delegation: None,
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
            // Post-edit formatting is opt-in and off by default; the wizard
            // never enables it, so a setup run leaves the section absent.
            formatters: None,
            // Skipped skill dependencies: the durable record `pantheon
            // doctor` reports. Empty = the screen found (or installed)
            // everything, so no section is written.
            skill_deps: if skill_deps_skipped.is_empty() {
                None
            } else {
                Some(crate::config::SkillDepsSection {
                    skipped: skill_deps_skipped,
                })
            },

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
                crate::config::CustomProviderSection {
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
                    match crate::dotenv::upsert_dotenv(data_dir, &env_name, key) {
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
                    if std::env::var(&env)
                        .map(|v| !v.trim().is_empty())
                        .unwrap_or(false)
                    {
                        println!("API key: {env} configured");
                    } else {
                        println!("make sure {env} is exported before starting a session");
                    }
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
        data_dir: &Path,
        hint: Option<&str>,
    ) -> Option<ProviderAnswer> {
        let meta = setup_providers::recommended_provider(&metas)
            .or_else(|| metas.first())?
            .clone();
        if assume_defaults {
            return Some(default_answer(&meta));
        }
        println!("{what} provider:");
        if let Some(hint) = hint {
            println!("  ({hint})");
        }
        for p in &metas {
            println!("  {:<12} {}", p.id, p.name);
        }
        loop {
            let choice = prompt(&format!("{what} provider"), Some(&meta.id));
            if let Some(m) = setup_providers::find_provider(&metas, &choice) {
                let m = m.clone();
                let answer = setup_providers::complete_answer(
                    &m,
                    &mut |label, prefill| Some(prompt(label, Some(prefill))),
                    &mut |question, def| Some(confirm(question, def)),
                )?;
                // Pick → paste → next group, the same contract the wizard
                // follows: the value is asked once, right after the pick,
                // and only when the var is not already set in this shell.
                if let Some(env) = answer.key_env.as_deref() {
                    collect_answer_key(data_dir, what, env);
                }
                return Some(answer);
            }
            let ids = metas
                .iter()
                .map(|p| p.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            println!("unknown provider '{choice}'; choose one of: {ids}");
        }
    }

    /// Text-mode counterpart of the wizard's key collection: pasted value
    /// goes to `<data_dir>/.env` (mode 0600) and is exported in-process so
    /// the rest of this setup run can use it. Empty input leaves the var
    /// unset without re-asking (doctor reports the gap later). Only for
    /// keys whose standard var is not already set - switching providers
    /// therefore never overwrites another provider's existing key.
    fn collect_answer_key(data_dir: &Path, what: &str, env: &str) {
        let already = std::env::var(env)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if already {
            return;
        }
        let value = prompt(
            &format!("{what}: paste the key for {env} (saved to .env; empty = skip)"),
            Some(""),
        );
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        match crate::dotenv::upsert_dotenv(data_dir, env, value) {
            Ok(()) => {
                std::env::set_var(env, value);
                println!("key written: {}/.env ({env})", data_dir.display());
            }
            Err(e) => {
                eprintln!("setup: could not save the key to .env: {e}");
                eprintln!("setup: export {env}=... yourself before starting a session");
            }
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
            ProviderKind::SelfHosted {
                default_url,
                key_env,
                ..
            } => {
                answer.url = non_empty(default_url.clone());
                // --yes is scripted: record the standard name, never prompt.
                // The value comes from the user's shell (agent doctor
                // reports the gap when neither is set).
                answer.key_env = key_env.clone();
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
    #[allow(clippy::too_many_arguments)]
    fn resolve_provider(
        what: &str,
        metas: Vec<ProviderMeta>,
        pre: Option<ProviderAnswer>,
        group_on: bool,
        assume_defaults: bool,
        data_dir: &Path,
        hint: Option<&str>,
    ) -> Option<ProviderAnswer> {
        if !group_on {
            return None;
        }
        match pre {
            Some(a) => Some(validated_answer(what, &metas, a)),
            None => default_or_prompt(what, metas, assume_defaults, data_dir, hint),
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
            // Setup-only row id: same backend, instance URL instead of a
            // cloud key. The config stores the real backend id so the
            // runtime never sees the picker fiction.
            "steel_selfhosted" => {
                s.backend = Some("steel".to_string());
                s.steel_base_url = a.url.clone();
            }
            "browserbase" => {
                s.browserbase_api_key_secret = a.key_env.clone();
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
        eprintln!(
            "  --provider ID          default model provider (catalog id, URL, or \"local\")"
        );
        eprintln!("  --model MODEL          default model name");
        eprintln!("  --api-key-env NAME     env var holding the provider API key");
        eprintln!(
            "  --key SECRET           raw API key (goes to <data_dir>/.env, never config.toml)"
        );
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
        // Unknown flags used to vanish into Args::parse: `setup --bogus`
        // ran the wizard (or the text path, on a pipe) as if nothing had
        // been asked for. A stray positional is the same class of error.
        {
            let known = [
                "provider",
                "model",
                "api-key-env",
                "fallback-provider",
                "fallback-model",
                "policy",
                "websearch-provider",
                "websearch-key-env",
                "websearch-url",
                "browser-provider",
                "browser-key-env",
                "browser-url",
                "stt-provider",
                "stt-key-env",
                "stt-url",
                "tts-provider",
                "tts-key-env",
                "tts-url",
                "memory",
                "computer",
                "key",
                "yes",
                "help",
                "h",
            ];
            // Every flag here except --yes/--help takes a value, in `--flag
            // VALUE` or `--flag=VALUE` form (Args::parse reads both), so a
            // value token must be skipped, not mistaken for a stray
            // positional: `setup --provider openai` is legitimate, while a
            // bare `setup openai` is not. Mirroring Args::parse, a value
            // flag followed by another `--` flag is a boolean switch, so
            // the following flag is not eaten as its value.
            let value_flags = [
                "provider",
                "model",
                "api-key-env",
                "fallback-provider",
                "fallback-model",
                "policy",
                "websearch-provider",
                "websearch-key-env",
                "websearch-url",
                "browser-provider",
                "browser-key-env",
                "browser-url",
                "stt-provider",
                "stt-key-env",
                "stt-url",
                "tts-provider",
                "tts-key-env",
                "tts-url",
                "memory",
                "computer",
                "key",
            ];
            let toks: &[String] = raw.get(1..).unwrap_or(&[]);
            let mut i = 0;
            while i < toks.len() {
                let tok = &toks[i];
                if let Some(stripped) = tok.strip_prefix("--") {
                    let name = stripped.split('=').next().unwrap_or(stripped);
                    if !known.contains(&name) {
                        eprintln!("setup: unknown flag '--{name}' (see `pantheon setup --help`)");
                        std::process::exit(2);
                    }
                    if value_flags.contains(&name) && !tok.contains('=') {
                        if let Some(v) = toks.get(i + 1) {
                            if !v.starts_with("--") {
                                i += 1;
                            }
                        }
                    }
                } else if tok.starts_with('-') {
                    eprintln!("setup: unknown flag '{tok}' (see `pantheon setup --help`)");
                    std::process::exit(2);
                } else {
                    eprintln!("setup: unexpected argument '{tok}' (this command takes flags only)");
                    std::process::exit(2);
                }
                i += 1;
            }
        }
        let parsed = crate::args::Args::parse(raw);
        // A terminal with no flags is the interactive case, and the TUI
        // wizard is the experience that was built for it: lists, pickers,
        // and a visible flow instead of a stdin prompt loop. The text path
        // stays for scripts (--yes or explicit flags) and for pipes, where
        // a full-screen wizard cannot render at all. Without this, the
        // installer's "run `pantheon setup`" advice handed the user the
        // weaker of the two paths.
        if std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && !parsed.has("yes")
            && parsed.flag("provider").is_none()
            && parsed.flag("model").is_none()
        {
            crate::setup_wizard::run_setup_flow(&crate::terminal::data_dir());
            std::process::exit(0);
        }
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
}

pub mod setup_wizard {
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

    /// One screen of the wizard's linear spine. Esc moves to the
    /// previous screen; Esc on the first screen cancels the wizard.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum SetupStep {
        Mode,
        Provider,
        Model,
    }

    /// Where Esc from `step` goes: `None` on the first screen cancels
    /// the wizard outright; any later screen steps back one screen.
    /// Scoped to this wizard: the shared pickers still just return
    /// `None` on Esc, and only this loop decides what that means — no
    /// global picker behaviour changes.
    fn esc_step(step: SetupStep) -> Option<SetupStep> {
        match step {
            SetupStep::Mode => None,
            SetupStep::Provider => Some(SetupStep::Mode),
            SetupStep::Model => Some(SetupStep::Provider),
        }
    }

    /// Run the wizard to completion, then write the result.
    ///
    /// Esc on the first screen cancels with nothing committed (the
    /// entry point re-checks usability before opening a session); Esc
    /// on a later screen steps back one screen instead of aborting the
    /// run. A cancelled run writes no config — only key values already
    /// pasted to `.env` persist, as before.
    pub fn run_setup_flow(data_dir: &Path) {
        // --- Mode → provider → key → model, with back navigation --------
        // Asked first, because mode decides every other screen. Blank
        // Slate skips the whole agent section, so it short-circuits
        // before any agent question is asked.
        //
        // Order matters: the model step fetches the live `/models` list
        // *with* the key, so the key env has to be chosen first. Both
        // agent modes start here; Recommended fills in everything else,
        // Full goes on to ask it.
        let mut step = SetupStep::Mode;
        let mut mode = Mode::Recommended;
        let mut provider = String::new();
        let mut custom: Option<CustomProviderSpec> = None;
        loop {
            match step {
                SetupStep::Mode => match pick_mode() {
                    Some(m) => {
                        if m == Mode::Blank {
                            // No agent, no model, no tools. Provision the
                            // runtime and stop.
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
                        mode = m;
                        step = SetupStep::Provider;
                    }
                    None => return,
                },
                SetupStep::Provider => match pick_provider("Step 2/5") {
                    Some((p, c)) => {
                        provider = p;
                        custom = c;
                        step = SetupStep::Model;
                    }
                    None => match esc_step(step) {
                        Some(prev) => step = prev,
                        None => return,
                    },
                },
                SetupStep::Model => {
                    let api_key_env = pick_model_key_env(
                        data_dir,
                        provider.as_str(),
                        custom.as_ref(),
                        "Step 3/5",
                    );
                    match pick_model(
                        provider.as_str(),
                        custom.as_ref(),
                        api_key_env.as_deref(),
                        "Step 4/5",
                    ) {
                        Some(model) => {
                            if mode == Mode::Recommended {
                                run_recommended(
                                    data_dir,
                                    provider,
                                    Some(model),
                                    custom,
                                    api_key_env,
                                );
                            } else {
                                run_full(data_dir, provider, Some(model), custom, api_key_env);
                            }
                            return;
                        }
                        None => match esc_step(step) {
                            Some(prev) => step = prev,
                            None => return,
                        },
                    }
                }
            }
        }
    }

    /// Collect a key's value when its env var is not already set, save it
    /// to `<data_dir>/.env` (mode 0600), and export it in-process so the
    /// model fetch right after can use it. A declined or empty paste leaves
    /// the var unset on purpose; the model screen then names the var so the
    /// fix is obvious. Shared by the catalog and custom-endpoint paths.
    fn ensure_key_value(data_dir: &Path, name: &str, label: &str, step: &str) {
        let already_set = std::env::var(name)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if already_set {
            return;
        }
        let Some(value) = crate::prompt::pick_secret_text(
            "API key",
            &format!("{step} · Paste your {label} key (saved to .env, owner-only; empty to skip)"),
            "paste the key, or leave empty to set it yourself later",
        ) else {
            return;
        };
        let value = value.trim();
        if value.is_empty() {
            return;
        }
        match crate::dotenv::upsert_dotenv(data_dir, name, value) {
            Ok(()) => std::env::set_var(name, value),
            Err(e) => {
                eprintln!("setup: could not save the key to .env: {e}");
                eprintln!("setup: export {name}=... yourself before the fetch can use it");
            }
        }
    }

    /// Collect a tool provider's key value when its env var is not already
    /// set: masked paste → `<data_dir>/.env` → in-process export, the same
    /// path model keys take. Empty/skipped leaves it unset; the runtime
    /// skips keyless providers cleanly and `doctor` reports the gap.
    /// Keyless/local answers carry no `key_env` and pass through untouched.
    fn ensure_answer_key(data_dir: &Path, answer: &setup_providers::ProviderAnswer, label: &str) {
        if let Some(env) = answer.key_env.as_deref() {
            ensure_key_value(data_dir, env, label, label);
        }
    }

    /// Pick → key collection in one step: after a provider choice lands,
    /// a `Chosen` answer with a key gets its value prompt immediately, so
    /// the flow is pick → paste → next group. Skipped/cancelled picks pass
    /// through untouched.
    fn collect_pick_key(data_dir: &Path, pick: &setup_providers::ProviderPick, label: &str) {
        if let setup_providers::ProviderPick::Chosen(a) = pick {
            ensure_answer_key(data_dir, a, label);
        }
    }

    /// Configure the model provider's API key: the env var name the config
    /// stores under `[model].api_key_env`, plus the value when the var is
    /// not already set. Esc (or an empty name) means none. `None` here is
    /// "no key needed", never "ask again later".
    ///
    /// Naming a var the user never exported is the trap that made the model
    /// fetch come back empty (groq's `/models` is 401 without a key, so the
    /// list fell back to the single catalog model). So when the named var
    /// is not set in this shell, the wizard offers to take the value now,
    /// writes it to `<data_dir>/.env` (mode 0600, via the same path the
    /// text setup uses), and exports it in-process so the model fetch below
    /// can actually use it. A declined paste still records the name; the
    /// model screen then says plainly that the var is unset.
    fn pick_model_key_env(
        data_dir: &Path,
        provider: &str,
        custom: Option<&CustomProviderSpec>,
        step: &str,
    ) -> Option<String> {
        if provider == "local" {
            return None;
        }
        // The env var name: a custom endpoint already named its key env on
        // its own screen; a catalog provider is asked here.
        let (name, label) = match custom {
            // True custom endpoint (id is not a builtin): the handle derives
            // from the endpoint host. A builtin override keeps the catalog's
            // standard env name - the key did not move, only the URL did.
            Some(spec) if !crate::model::builtin_provider_id(&spec.name) => {
                let derived = spec
                    .key_env
                    .clone()
                    .filter(|v| !v.trim().is_empty())
                    .unwrap_or_else(|| custom_key_env(&spec.base_url));
                (derived, "Custom endpoint".to_string())
            }
            _ => {
                let meta = pantheon_providers::catalog::provider(provider);
                let label = meta
                    .as_ref()
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| provider.to_string());
                // Default the env var to the provider's standard name (e.g.
                // OPENROUTER_API_KEY) and save the value into `<data_dir>/.env`
                // below instead of making the user export anything. The name is
                // still configurable through the custom-endpoint path.
                let name = meta.map(|m| m.key_env.clone()).unwrap_or_default();
                if name.trim().is_empty() {
                    return None;
                }
                (name, label)
            }
        };
        ensure_key_value(data_dir, &name, &label, step);
        Some(name)
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
        let websearch_pick = setup_providers::complete_recommended(
            "Web search",
            setup_providers::websearch_providers(),
        );
        let browser_pick =
            setup_providers::complete_recommended("Browser", setup_providers::browser_providers());
        // Memory gets a picker (not the silent recommended default): the
        // backend choice matters and bridges need a URL.
        let memory_pick = setup_providers::pick_provider(
            "Memory",
            "Choose a memory backend \u{2014} bring your own: pantheon memory backend scaffold NAME [http|stdio]",
            setup_providers::memory_providers(data_dir),
        );
        let computer_pick = setup_providers::complete_recommended(
            "Computer use",
            setup_providers::computer_providers(),
        );
        // Pick → paste → next group: keyed answers collect their value now.
        collect_pick_key(data_dir, &websearch_pick, "Web search");
        collect_pick_key(data_dir, &browser_pick, "Browser");
        collect_pick_key(data_dir, &memory_pick, "Memory");
        collect_pick_key(data_dir, &computer_pick, "Computer use");
        let websearch = setup_providers::apply_pick(
            &mut tools,
            &mut skipped,
            ToolGroup::WebSearch,
            &websearch_pick,
        );
        let browser = setup_providers::apply_pick(
            &mut tools,
            &mut skipped,
            ToolGroup::Browser,
            &browser_pick,
        );
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
                // Fallback chains live under `pantheon fallback`, never in
                // setup: these stay None so `run_setup` writes no fallback.
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
        answers.skills_enabled = tools.contains(&ToolGroup::Skills);
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
            collect_pick_key(data_dir, &pick, "Browser");
            setup_providers::apply_pick(&mut tools, &mut skipped, ToolGroup::Browser, &pick)
        } else {
            None
        };

        let websearch = if wants(Section::WebSearch) {
            let pick = setup_providers::pick_provider(
                "Web search",
                "Choose the provider behind the `web_search` tool. Esc does not skip: it leaves this group on the recommended default (TinyFish) with no key, so `web_search` stays unregistered until a key is set - use the Skip row to leave it unconfigured instead",
                setup_providers::websearch_providers(),
            );
            collect_pick_key(data_dir, &pick, "Web search");
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
            collect_pick_key(data_dir, &stt_pick, "Speech to text");
            collect_pick_key(data_dir, &tts_pick, "Text to speech");
            setup_providers::apply_voice_picks(&mut tools, &mut skipped, &stt_pick, &tts_pick)
        } else {
            setup_providers::VoicePicks::default()
        };

        let memory = if wants(Section::Memory) {
            let pick = setup_providers::pick_provider(
                "Memory",
                "Choose a memory backend \u{2014} bring your own: pantheon memory backend scaffold NAME [http|stdio]",
                setup_providers::memory_providers(data_dir),
            );
            collect_pick_key(data_dir, &pick, "Memory");
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
                // Fallback chains live under `pantheon fallback`, never in
                // setup: these stay None so `run_setup` writes no fallback.
                fallback_provider: None,
                fallback_model: None,
                policy,
                tools: Some(tools),
                websearch,
                browser,
                stt: voice.stt,
                tts: voice.tts,
                memory,
                computer,
                key: None,
                custom_provider: custom,
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
        pantheon_doctor::skill_deps::run_skill_deps_screen(
            &mut |line: &str| println!("pantheon: {line}"),
            &mut |question: &str, def: bool| {
                crate::prompt::pick_confirm("Skill dependencies", question, def)
            },
            &mut pantheon_doctor::skill_deps::shell_install,
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

    /// The mode screen. `None` means Esc: the user changed their mind
    /// about being here, so the wizard cancels instead of inventing an
    /// answer — proceeding with a default mode the user never picked
    /// would write a config they did not ask for.
    fn pick_mode() -> Option<Mode> {
        let items = vec![
            Item::new("Recommended", "recommended")
                .desc("pick your provider and model; everything else gets recommended defaults"),
            Item::new("Full Setup", "full")
                .desc("everything, skipping anything that does not apply"),
            Item::new("Blank Slate", "blank")
                .desc("create the runtime without configuring an agent"),
        ];
        crate::prompt::pick_one("Setup", "Choose your setup", items).map(|v| match v.as_str() {
            "full" => Mode::Full,
            "blank" => Mode::Blank,
            _ => Mode::Recommended,
        })
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

    /// The first-run shortlist: a handful of providers a newcomer can pick
    /// between without knowing the field, then "Show all" for the rest.
    /// Nous leads because it is the recommended default. Ids not in the
    /// catalog are skipped, so the list never shows a dead row.
    fn provider_shortlist() -> Vec<String> {
        ["nous", "openrouter", "anthropic", "openai", "local"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// What key a provider needs, for the picker's description line. Local
    /// needs none; everyone else names their env var. This plus the cost
    /// tier is the only per-row info that helps at setup time, which is why
    /// the old "[Chinese lab]" / "no curated models" blurbs are gone.
    fn provider_key_hint(m: &pantheon_providers::catalog::ProviderMeta) -> String {
        if m.id == "local" || m.id == "lmstudio" {
            "no key needed".to_string()
        } else if m.key_env.is_empty() {
            "key required".to_string()
        } else {
            format!("needs {}", m.key_env)
        }
    }

    /// A free/cheap/paid tier derived from the provider's curated model
    /// costs, never invented. Local is free; otherwise the cheapest curated
    /// input price decides. No curated cost data means no claim at all.
    fn provider_tier(m: &pantheon_providers::catalog::ProviderMeta) -> Option<String> {
        if m.id == "local" || m.id == "lmstudio" {
            return Some("free · local".to_string());
        }
        let min_input = m
            .models
            .iter()
            .filter_map(|mm| mm.cost.input_per_mtok_usd)
            .fold(f64::INFINITY, f64::min);
        if min_input.is_infinite() {
            None
        } else if min_input == 0.0 {
            Some("free tier".to_string())
        } else if min_input < 1.0 {
            Some("cheap".to_string())
        } else {
            Some("paid".to_string())
        }
    }

    /// One provider row: label, cost tier as the right tag, and the key
    /// requirement (plus a recommended marker) as the description. No lab
    /// blurb, no curated-model count.
    fn provider_item(m: &pantheon_providers::catalog::ProviderMeta) -> Item {
        let mut item = Item::new(m.label.clone(), m.id.clone());
        if let Some(t) = provider_tier(m) {
            item = item.tag(t);
        }
        let mut hint = provider_key_hint(m);
        if m.recommended {
            hint = format!("recommended · {hint}");
        }
        item.desc(hint)
    }

    /// Pick a model provider. Returns the provider id plus, for the Custom
    /// row or an edited preset URL, the endpoint spec `run_setup` persists
    /// to `[custom_providers.<id>]` - the in-memory registration alone does
    /// not survive the process, so the wizard used to write a config
    /// pointing at an endpoint only it remembered.
    ///
    /// Existing custom endpoints appear as ordinary rows below the presets
    /// (no separate section); the Custom row creates a new one without
    /// overwriting an existing `custom` id.
    ///
    /// The first screen is a shortlist with a "Show all providers..." row;
    /// a first-run user should not face 35 rows. Expanding lists every
    /// selectable provider. `router` is filtered out of the full list (a
    /// dev endpoint), a product decision that belongs here, not in the
    /// widget.
    fn pick_provider(step: &str) -> Option<(String, Option<CustomProviderSpec>)> {
        let subtitle = format!("{step} · Choose a model provider");
        let mut items: Vec<Item> = provider_shortlist()
            .iter()
            .filter_map(|id| pantheon_providers::catalog::provider(id))
            .map(|m| provider_item(&m))
            .collect();
        items.push(
            Item::new("Show all providers…", "__show_all__").desc("every provider Pantheon knows"),
        );
        items.push(
            Item::new("Custom endpoint", "custom").desc("an OpenAI-shaped endpoint you provide"),
        );
        let chosen = crate::prompt::pick_one("Provider", &subtitle, items)?;
        match chosen.as_str() {
            "__show_all__" => pick_provider_full(&subtitle),
            "custom" => pick_custom_provider(),
            id => {
                if let Some(note) = provider_note(id) {
                    println!("{note}");
                }
                if id == "local" {
                    // Local Ollama is a self-hosted runtime, not just an
                    // endpoint: warn when it is not reachable so the user is not
                    // surprised by a 401-free 404 on first chat.
                    let ollama_present = crate::setup_providers::detect_binary("command -v ollama");
                    if !ollama_present {
                        println!("pantheon: Ollama was not found on PATH.");
                        println!(
                            "  install it first, then `ollama serve` and `ollama pull <model>`"
                        );
                    }
                }
                // The endpoint, shown for every provider: Enter keeps the
                // catalog URL, typing replaces it. An edit becomes an
                // override row shadowing the builtin (visible in
                // `pantheon model --list`, removable) instead of a silent
                // fork the user cannot see.
                let meta = pantheon_providers::catalog::provider(id);
                let base = meta
                    .as_ref()
                    .map(|m| m.base_url.clone())
                    .unwrap_or_default();
                let typed = crate::prompt::pick_text(
                    "Provider",
                    &format!("Base URL for {id} (Enter keeps {base})"),
                    &base,
                )?;
                let typed = typed.trim();
                let url = if typed.is_empty() {
                    base.clone()
                } else {
                    crate::model::normalize_base_url(typed)
                };
                if url != base && !base.is_empty() {
                    println!("  saving as an override of builtin '{id}'");
                    let mode = meta
                        .as_ref()
                        .map(|m| m.api_mode)
                        .unwrap_or(pantheon_providers::catalog::ApiMode::OpenAi);
                    let api_mode = match mode {
                        pantheon_providers::catalog::ApiMode::Anthropic => "anthropic",
                        _ => "openai",
                    }
                    .to_string();
                    let spec = CustomProviderSpec {
                        name: id.to_string(),
                        base_url: url,
                        api_mode,
                        key_env: None,
                    };
                    pantheon_providers::catalog::register_custom_provider(
                        pantheon_providers::catalog::ProviderMeta {
                            id: id.into(),
                            label: format!("{id} (override)"),
                            base_url: spec.base_url.clone(),
                            api_mode: mode,
                            base_env: String::new(),
                            key_env: String::new(),
                            key_header: String::new(),
                            models: Vec::new(),
                            prominent: false,
                            recommended: false,
                            dev: false,
                            tag: "override".into(),
                            auth_command: None,
                            auth_refresh_ms: None,
                            reserved: false,
                            allowed_params: vec![],
                            oauth_apps: vec![],
                            sigv4: false,
                            reasoning_map: Default::default(),
                        },
                    );
                    Some((id.to_string(), Some(spec)))
                } else {
                    Some((id.to_string(), None))
                }
            }
        }
    }

    /// The expanded provider list: every selectable provider, same row
    /// shape as the shortlist, plus Custom.
    fn pick_provider_full(subtitle: &str) -> Option<(String, Option<CustomProviderSpec>)> {
        let providers = order_model_providers(pantheon_providers::catalog::selectable_providers());
        let mut items: Vec<Item> = providers.iter().map(provider_item).collect();
        items.push(Item::new("Custom", "custom").desc("an OpenAI-shaped endpoint you provide"));
        let chosen = crate::prompt::pick_one("All providers", subtitle, items)?;
        if chosen == "custom" {
            return pick_custom_provider();
        }
        // Nous honesty: the Portal primarily uses OAuth, so a static key may
        // not be the whole auth story. Printed in both pickers so it appears
        // wherever Nous can be chosen.
        if let Some(note) = provider_note(&chosen) {
            println!("{note}");
        }
        Some((chosen, None))
    }

    /// First free custom id: `custom`, then `custom-2`, .... The wizard
    /// used to always write `custom`, silently overwriting a previous custom
    /// endpoint with the same id.
    fn free_custom_name() -> String {
        if pantheon_providers::catalog::provider("custom").is_none() {
            return "custom".to_string();
        }
        let mut n = 2u32;
        loop {
            let cand = format!("custom-{n}");
            if pantheon_providers::catalog::provider(&cand).is_none() {
                return cand;
            }
            n += 1;
        }
    }

    /// Derive the key handle from the endpoint host:
    /// `https://api.example.com/v1` → `CUSTOM_API_EXAMPLE_COM_API_KEY`.
    /// Host-derived, so two customs never share a var and the name says
    /// where the key goes.
    fn custom_key_env(base_url: &str) -> String {
        let host = base_url
            .split("://")
            .nth(1)
            .unwrap_or(base_url)
            .split('/')
            .next()
            .unwrap_or("");
        let mut slug = String::new();
        for c in host.chars() {
            if c.is_ascii_alphanumeric() {
                slug.push(c.to_ascii_uppercase());
            } else if !slug.ends_with('_') {
                slug.push('_');
            }
        }
        let slug = slug.trim_matches('_');
        if slug.is_empty() {
            "CUSTOM_API_KEY".to_string()
        } else {
            format!("CUSTOM_{slug}_API_KEY")
        }
    }

    /// The Custom endpoint flow: base URL, wire format, then the key value
    /// on the next screen. Registered through the same path
    /// `pantheon provider add` uses, so a custom endpoint set up in the
    /// wizard is a real catalog entry and not a string only this wizard
    /// remembers.
    fn pick_custom_provider() -> Option<(String, Option<CustomProviderSpec>)> {
        let raw =
            crate::prompt::pick_text("Custom endpoint", "Base URL", "https://api.example.com/v1")?;
        if raw.trim().is_empty() {
            return None;
        }
        let url = crate::model::normalize_base_url(raw.trim());
        let name = free_custom_name();
        if name != "custom" {
            println!("  id 'custom' is taken - using '{name}'");
        }
        // Wire format. Auto sniffs the URL just typed; Gemini rides the
        // OpenAI wire through Google's compatibility endpoint, so it needs
        // no special transport.
        let api_mode = match crate::prompt::pick_one(
            "Custom endpoint",
            "Wire format",
            vec![
                Item::new("Auto-detect", "auto")
                    .desc("Anthropic host → /messages, otherwise /chat/completions"),
                Item::new("OpenAI-compatible", "openai").desc("OpenAI-style /v1 API"),
                Item::new("Anthropic", "anthropic").desc("Anthropic Messages API"),
                Item::new("Gemini", "gemini").desc("Google's OpenAI-compatible endpoint"),
            ],
        )?
        .as_str()
        {
            "anthropic" => pantheon_providers::catalog::ApiMode::Anthropic,
            "auto" => crate::model::sniff_wire_mode(&url),
            _ => pantheon_providers::catalog::ApiMode::OpenAi,
        };
        // No key-env prompt: the handle derives from the host and the value
        // is collected on the key screen.
        let key_env: Option<String> = None;
        pantheon_providers::catalog::register_custom_provider(
            pantheon_providers::catalog::ProviderMeta {
                id: name.clone(),
                label: format!("Custom endpoint ({name})"),
                base_url: url.clone(),
                api_mode,
                base_env: String::new(),
                key_env: String::new(),
                key_header: String::new(),
                models: Vec::new(),
                prominent: true,
                recommended: false,
                dev: false,
                tag: "custom".into(),
                auth_command: None,
                auth_refresh_ms: None,
                reserved: false,
                allowed_params: vec![],
                oauth_apps: vec![],
                sigv4: false,
                reasoning_map: Default::default(),
            },
        );
        let spec = CustomProviderSpec {
            name: name.clone(),
            base_url: url,
            api_mode: match api_mode {
                pantheon_providers::catalog::ApiMode::Anthropic => "anthropic".to_string(),
                _ => "openai".to_string(),
            },
            key_env,
        };
        Some((name, Some(spec)))
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
                vision: Some(mm.vision),
                reasoning: Some(mm.reasoning),
            })
            .collect()
    }

    /// Resolve a key env var's value from the process environment. The
    /// wizard stores the env var *name*, never the key, so the live model
    /// fetch reads the value the user has already exported. An unset or
    /// empty var yields an empty string, which degrades the fetch to
    /// keyless rather than sending a bogus `Bearer`.
    fn resolve_key(key_env: Option<&str>) -> String {
        key_env
            .and_then(|n| std::env::var(n).ok())
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    fn pick_model(
        provider: &str,
        _custom: Option<&CustomProviderSpec>,
        key_env: Option<&str>,
        step: &str,
    ) -> Option<String> {
        let meta = pantheon_providers::catalog::provider(provider);
        let label = meta
            .as_ref()
            .map(|m| m.label.clone())
            .unwrap_or_else(|| provider.to_string());
        let base_url = meta
            .as_ref()
            .map(|m| m.base_url.clone())
            .unwrap_or_default();
        let api_mode = meta
            .as_ref()
            .map(|m| m.api_mode)
            .unwrap_or(pantheon_providers::catalog::ApiMode::OpenAi);
        let curated = meta.as_ref().map(curated_rows).unwrap_or_default();

        // Live list, fetched *with* the key so a keyed endpoint (the router,
        // most hosted providers) returns its real models instead of a 401
        // that would drop the wizard back to curated-only. Keyless is the
        // fallback for endpoints that serve /models without a key when the
        // var is unset. The key doubles as a validation call: a 401 here
        // means the key is wrong or missing, and the note says so.
        let key = resolve_key(key_env);
        let key_name = key_env.unwrap_or("the API key");
        let (live, note): (Vec<crate::model_catalog::LiveModel>, Option<String>) = {
            let keyed = if key.is_empty() {
                Err(crate::model_catalog::FetchError::Auth)
            } else {
                crate::model_catalog::fetch_live_models_keyed(&base_url, &key, api_mode)
            };
            match keyed {
                Ok(l) => (l, None),
                Err(crate::model_catalog::FetchError::Auth) => {
                    if !key.is_empty() {
                        // A key was sent and the endpoint refused it. Saying
                        // "rejected" is the honest signal; "not set" would
                        // send the user to export a key they already have.
                        (
                            Vec::new(),
                            Some(format!(
                                "the {label} key was rejected (401); check {key_name} is correct"
                            )),
                        )
                    } else {
                        // No key at all: try keyless, then name the unset var
                        // so the fix is obvious.
                        match crate::model_catalog::fetch_live_models(&base_url) {
                            Ok(l) => (l, None),
                            Err(_) => (
                                Vec::new(),
                                Some(format!(
                                    "{key_name} is not set; export it or paste it during setup to load the full list"
                                )),
                            ),
                        }
                    }
                }
                Err(e) => (
                    Vec::new(),
                    Some(format!("live list unavailable ({})", e.kind())),
                ),
            }
        };

        let rows = crate::model_catalog::merge_model_rows(&curated, &live);

        let mut items: Vec<Item> = Vec::new();
        for row in rows.iter() {
            let mut item = Item::new(row.id.clone(), format!("{provider}/{}", row.id));
            // Context and price go on their own dim line, not the right tag,
            // so a long "131k · $0.15 in / $0.60 out" never clips against
            // the model id on a narrow terminal.
            if let Some(m) = crate::model_catalog::row_meta(row) {
                item = item.meta(m);
            }
            items.push(item);
        }
        if items.is_empty() {
            // No rows at all - neither curated nor fetchable. The generic
            // adapter is the path and the model name is a free-text answer.
            // Inventing a list here would be the fake configuration the spec
            // forbids.
            let sub = match &note {
                Some(n) => format!("{step} · {label} · {n}"),
                None => format!("{step} · {label} has no listed models. Name the model to use."),
            };
            let name = crate::prompt::pick_text("Model", &sub, "")?;
            let name = name.trim().to_string();
            if name.is_empty() {
                return None;
            }
            // Bare model name: setup pairs it with the provider.
            return Some(name);
        }
        let subtitle = match &note {
            Some(n) => format!("{step} · {label} · {n}"),
            None => format!("{step} · {label}"),
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
        // Everything except Extensions is preselected: the runtime gates each
        // one - Computer Use included - behind the ToolGroup toggle, the CUA
        // driver binary, the MCP server approval, and per-call human
        // approval under the default policy. Extensions (MCP/plugin servers)
        // start off so a first run does not force an MCP-setup screen into
        // the core config flow; opting in enables it on demand.
        let preselected: Vec<&str> = ToolGroup::all()
            .iter()
            .filter(|g| **g != ToolGroup::Plugins)
            .map(|g| g.key())
            .collect();
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
            ToolGroup::Memory => {
                "persistent memory + session_search recall \u{2014} provider screen follows"
            }
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
            ToolGroup::Plugins => {
                "plugin tools, MCP server integrations, and OAuth connector tools"
            }
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
        println!(
            "pantheon: gateway chat surfaces are optional - skip for a scheduler-only gateway"
        );
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
        let subtitle =
            format!("{label} bot token - enter to save, empty clears, esc keeps current");
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
    /// `<data_dir>/extensions/bundled/` and load without third-party
    /// approval, but they are config-gated: `[plugins.<name>] enabled`
    /// (and the manifest default) decides whether one runs - they are not
    /// "always on". The exec registry also seeds bundled hook plugins
    /// there on load, so the directory is normally non-empty; the screen
    /// reports what is there and moves on.
    fn print_bundled_status(data_dir: &Path) {
        let names = bundled_plugin_names(data_dir);
        if names.is_empty() {
            println!("pantheon: no bundled plugins installed");
        } else {
            println!(
                "pantheon: bundled plugins (config-gated): {}",
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
            headers: HashMap::new(),
            auth: None,
            scopes: Vec::new(),
            client_id: None,
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
            println!(
                "pantheon: server not added \u{2014} fix the above and try again, or pick Skip"
            );
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
            .map(|paths| {
                std::env::split_paths(&paths).any(|dir| is_executable_file(&dir.join(cmd)))
            })
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
                headers: HashMap::new(),
                auth: None,
                scopes: Vec::new(),
                client_id: None,
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

        // --- wizard Esc: cancel on the first screen, back a step after --

        #[test]
        fn wizard_esc_cancels_first_and_steps_back_later() {
            use super::{esc_step, SetupStep};
            assert_eq!(esc_step(SetupStep::Mode), None);
            assert_eq!(esc_step(SetupStep::Provider), Some(SetupStep::Mode));
            assert_eq!(esc_step(SetupStep::Model), Some(SetupStep::Provider));
        }

        // --- first-run provider picker rows ---------------------------------

        use super::{provider_key_hint, provider_tier};
        use pantheon_providers::catalog::{ApiMode, ModelCost, ModelMeta, ProviderMeta};

        fn provider(id: &str, key_env: &str, recommended: bool) -> ProviderMeta {
            ProviderMeta {
                id: id.into(),
                label: id.into(),
                base_url: String::new(),
                api_mode: ApiMode::OpenAi,
                base_env: String::new(),
                key_env: key_env.into(),
                key_header: String::new(),
                models: Vec::new(),
                prominent: false,
                recommended,
                dev: false,
                tag: String::new(),
                auth_command: None,
                auth_refresh_ms: None,
                reserved: false,
                allowed_params: vec![],
                oauth_apps: vec![],
                sigv4: false,
                reasoning_map: Default::default(),
            }
        }

        fn model_with_cost(input: Option<f64>) -> ModelMeta {
            ModelMeta {
                provider: "p".into(),
                model: "m".into(),
                context_limit: None,
                max_output_tokens: None,
                tools: true,
                vision: false,
                video: false,
                reasoning: false,
                streaming: true,
                cost: ModelCost {
                    input_per_mtok_usd: input,
                    output_per_mtok_usd: None,
                },
                cache_cost: None,
                aliases: vec![],
            }
        }

        #[test]
        fn key_hint_names_the_env_var_or_says_none() {
            assert_eq!(
                provider_key_hint(&provider("openai", "OPENAI_API_KEY", false)),
                "needs OPENAI_API_KEY"
            );
            assert_eq!(
                provider_key_hint(&provider("local", "", false)),
                "no key needed"
            );
            assert_eq!(provider_key_hint(&provider("x", "", false)), "key required");
        }

        #[test]
        fn tier_is_free_for_local() {
            assert_eq!(
                provider_tier(&provider("local", "", false)).as_deref(),
                Some("free · local")
            );
        }

        #[test]
        fn tier_derives_from_cheapest_curated_input() {
            let mut cheap = provider("p", "K", false);
            cheap.models = vec![model_with_cost(Some(0.15)), model_with_cost(Some(10.0))];
            assert_eq!(provider_tier(&cheap).as_deref(), Some("cheap"));

            let mut pricey = provider("p", "K", false);
            pricey.models = vec![model_with_cost(Some(3.0))];
            assert_eq!(provider_tier(&pricey).as_deref(), Some("paid"));
        }

        #[test]
        fn tier_is_none_without_cost_data() {
            // No curated cost means no claim, never an invented "paid".
            assert_eq!(
                provider_tier(&provider("openrouter", "OPENROUTER_API_KEY", false)),
                None
            );
        }
    }
}

pub mod setup_graph {
    //! The setup section graph.
    //!
    //! The graph resolves which sections apply given
    //! the answers so far, so the progress indicator can never claim "7 of 12"
    //! for a branch that has nine screens.

    use std::collections::BTreeMap;

    /// One setup section, in declaration order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Section {
        Entry,
        Provider,
        Model,
        Reasoning,
        Workspace,
        Execution,
        DockerNetwork,
        Gateways,
        Tools,
        /// The skill-dependencies screen: every third-party package the
        /// skill library needs, detected with install-or-skip per missing
        /// item. Skills are mode-independent, so this screen runs in both
        /// Recommended and Full.
        SkillDeps,
        Browser,
        WebSearch,
        Tts,
        Memory,
        ComputerUse,
        Extensions,
        ServiceInstall,
        Review,
        Provision,
        Done,
    }

    impl Section {
        /// Lowercase name used in the `SETUP · NAME` indicator.
        pub fn label(self) -> &'static str {
            match self {
                Section::Entry => "entry",
                Section::Provider => "model",
                Section::Model => "model",
                Section::Reasoning => "reasoning",
                Section::Workspace => "workspace",
                Section::Execution => "execution",
                Section::DockerNetwork => "network",
                Section::Gateways => "gateways",
                Section::Tools => "tools",
                Section::SkillDeps => "skill-deps",
                Section::Browser => "browser",
                Section::WebSearch => "search",
                Section::Tts => "speech",
                Section::Memory => "memory",
                Section::ComputerUse => "computer",
                Section::Extensions => "extensions",
                Section::ServiceInstall => "service",
                Section::Review => "review",
                Section::Provision => "install",
                Section::Done => "ready",
            }
        }
    }

    /// Which entry mode the user chose.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        /// Minimum decisions: pick provider+model, everything else gets
        /// recommended defaults.
        Recommended,
        /// Complete runtime, skipping irrelevant sections automatically.
        Full,
        /// Create the runtime without configuring agent/model/tooling.
        Blank,
    }

    /// The decisions that change which sections run.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct Answers {
        pub mode: Option<Mode>,
        pub execution_is_docker: bool,
        /// Tool groups the user enabled. Browser, web search, and speech are
        /// tool groups, so they gate their own provider screens.
        pub browser_enabled: bool,
        pub web_search_enabled: bool,
        pub tts_enabled: bool,
        pub memory_enabled: bool,
        pub computer_use_enabled: bool,
        pub gateways_enabled: bool,
        pub extensions_enabled: bool,
        /// Skills group tick: its dependency screen (third-party packages
        /// the skill library drives) follows the tick like every other
        /// group screen - unticking Skills skips it entirely.
        pub skills_enabled: bool,
        /// True when the selected model declares reasoning support.
        pub model_supports_reasoning: bool,
    }

    /// Resolve the ordered section list for the current answers.
    ///
    /// The point of this function is that the count is *derived*, never
    /// hardcoded. Turning off Docker removes a screen; turning off the browser
    /// tool removes its provider screen. Any screen whose runtime consumer does
    /// not exist yet is omitted, so setup can never offer a choice the runtime
    /// cannot honor.
    pub fn sections(a: &Answers) -> Vec<Section> {
        let mode = a.mode.unwrap_or(Mode::Recommended);
        let mut out = vec![Section::Entry];

        match mode {
            Mode::Blank => {
                // No agent configuration at all. Runtime and workspace only.
                out.extend([
                    Section::Workspace,
                    Section::Review,
                    Section::Provision,
                    Section::Done,
                ]);
                return out;
            }
            Mode::Recommended => {
                // Fixed provider screens, no Tools screen: the recommended
                // toolset is every group except Voice (STT/TTS are skipped
                // entirely), and the provider screens are driven by it, not
                // by tool answers. Memory native is keyless, so its screen
                // records silently.
                out.extend([
                    Section::Provider,
                    Section::Model,
                    Section::WebSearch,
                    Section::Browser,
                    Section::Memory,
                    Section::ComputerUse,
                    // Skills work the same in every mode, so their
                    // third-party packages are resolved here too.
                    Section::SkillDeps,
                    // The service-install permission screen: one question,
                    // after every other screen, in both agent modes.
                    Section::ServiceInstall,
                    Section::Review,
                    Section::Provision,
                    Section::Done,
                ]);
                return out;
            }
            Mode::Full => {}
        }

        out.push(Section::Provider);
        out.push(Section::Model);
        // Reasoning is offered only for a model that declares it, so the screen
        // cannot promise a setting the request pipeline will not honor.
        if a.model_supports_reasoning {
            out.push(Section::Reasoning);
        }
        out.push(Section::Workspace);
        out.push(Section::Execution);
        if a.execution_is_docker {
            out.push(Section::DockerNetwork);
        }
        if a.gateways_enabled {
            out.push(Section::Gateways);
        }
        // The Tools screen always runs in Full: it is the one place the user
        // sees every capability, and its answers gate the provider screens
        // below. Policy stays the default coder preset - there is no
        // permissions screen anymore.
        out.push(Section::Tools);
        // Skill dependencies come right after the Tools screen: the skills
        // are part of the toolset, so their third-party packages are
        // resolved before the provider screens. The screen follows the
        // Skills tick itself - an unticked group skips its screens entirely.
        if a.skills_enabled {
            out.push(Section::SkillDeps);
        }
        if a.browser_enabled {
            out.push(Section::Browser);
        }
        if a.web_search_enabled {
            out.push(Section::WebSearch);
        }
        if a.tts_enabled {
            out.push(Section::Tts);
        }
        if a.memory_enabled {
            out.push(Section::Memory);
        }
        if a.computer_use_enabled {
            out.push(Section::ComputerUse);
        }
        if a.extensions_enabled {
            out.push(Section::Extensions);
        }
        // The service-install permission screen: the last real screen in
        // Full, after every provider screen.
        out.push(Section::ServiceInstall);
        out.extend([Section::Review, Section::Provision, Section::Done]);
        out
    }

    /// `SETUP · TOOLS`, uppercased for the indicator.
    pub fn indicator(s: Section) -> String {
        format!("SETUP · {}", s.label().to_uppercase())
    }

    /// `Tools    6 of 9`, where both numbers come from the resolved branch.
    pub fn progress(s: Section, list: &[Section]) -> String {
        let idx = list
            .iter()
            .position(|x| *x == s)
            .map(|i| i + 1)
            .unwrap_or(0);
        format!("{}  {} of {}", s.label(), idx, list.len())
    }

    /// What a provisioning step can do when it fails.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StepAction {
        Retry,
        Skip,
        Abort,
    }

    /// The result of one provisioning step. `Skip` is a real outcome, not a
    /// failure: it records that the capability is absent so a later health check
    /// can say so rather than the agent pretending it has the tool.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum StepOutcome {
        Done,
        Skipped { why: String },
        Failed { why: String, action: StepAction },
    }

    /// Aggregate the steps. Abort means the run never becomes usable; Skipped
    /// does not, because an agent without a browser is still an agent.
    pub fn provisioning_ok(steps: &[(String, StepOutcome)]) -> bool {
        !steps.iter().any(|(_, o)| {
            matches!(
                o,
                StepOutcome::Failed {
                    action: StepAction::Abort,
                    ..
                }
            )
        })
    }

    /// A durable record of what provisioning produced, so `doctor` can report a
    /// skipped capability instead of silently having one fewer tool.
    pub type ProvisionRecord = BTreeMap<String, StepOutcome>;
}
