//! Generic provider setup for the setup wizard's tool screens.
//!
//! Every provider-backed tool group (web search, browser, STT, TTS,
//! memory) enumerates its providers from the owning crate's registry,
//! normalized into [`ProviderMeta`]. The flow is identical for all
//! groups - no provider-specific branching:
//!
//! 1. pick a provider: recommended first, preselected on the
//!    recommended row, provider notes in the descriptions;
//! 2. follow-up by kind:
//!   - [`ProviderKind::Keyless`] → nothing to ask;
//!   - [`ProviderKind::Cloud`] → the ENV VAR NAME holding the key
//!      (never the value - the wizard never handles secrets);
//!   - [`ProviderKind::SelfHosted`] → the instance URL;
//!   - [`ProviderKind::Local`] → detect the binary; when it is missing
//!      the metadata's dependency notes are shown and the user gets
//!      install-or-skip (a known `install_cmd` runs on confirmation,
//!      then detection re-runs);
//! 3. extra fields (voice model, project id, command, ...) as declared.
//!
//! A provider whose wire is not live-verified never reaches the picker:
//! the row builders filter to what the runtime can actually construct,
//! so setup cannot write a choice the runtime cannot honor.
//!
//! Recommended mode skips the pick: [`complete_recommended`] resolves
//! the recommended row without a picker screen and goes straight to
//! the follow-ups. Every screen reports a [`ProviderPick`]: `Chosen`
//! (an answer to write), `Skipped` (the tool is intentionally left
//! unconfigured - no section is written and the group comes off the
//! enabled-tools vec), or `Cancelled` (Esc - the caller keeps its
//! current behavior, which today means the shared setup path resolves
//! the recommended default).

use crate::widget::Item;
use pantheon_api::config::ToolGroup;

/// One follow-up question a provider needs beyond the pick itself.
#[derive(Debug, Clone)]
pub struct ExtraField {
    /// Config options key (e.g. `"voice"`, `"project_id"`, `"cmd"`).
    pub key: String,
    /// Prompt text shown to the user.
    pub prompt: String,
    /// Prefill for the text prompt.
    pub default: String,
    /// Empty answers are rejected and reprompted.
    pub required: bool,
}

/// What a local (binary) provider needs for detection and install.
#[derive(Debug, Clone)]
pub struct LocalMeta {
    /// Shell snippet; exit status 0 = the binary is present.
    pub detect_cmd: String,
    /// Shown when the binary is missing: dependencies and how to
    /// install by hand (voice-model assets for Piper, the fetch step
    /// for Camoufox, ...).
    pub install_hint: String,
    /// Shell command run when the user confirms the install prompt.
    /// `None` = no verified one-liner exists; the hint is the whole
    /// story and skip is the only path.
    pub install_cmd: Option<String>,
}

/// How a provider authenticates / installs, as the generic flow needs.
#[derive(Debug, Clone)]
pub enum ProviderKind {
    /// Works out of the box; nothing to ask.
    Keyless,
    /// Remote API: ask for the env var NAME holding the key.
    Cloud { env_var: String },
    /// User-run instance: ask for its URL. `required` providers reprompt
    /// on empty; optional ones fall back to the backend's own default
    /// (memory bridges read env/`default_url` when `options.url` is
    /// unset).
    SelfHosted { default_url: String, required: bool },
    /// Local binary: detect, then install-or-skip.
    Local { local: LocalMeta },
}

/// One normalized provider row, built from a group catalog.
#[derive(Debug, Clone)]
pub struct ProviderMeta {
    /// Stable id: the config value (`[websearch] provider`,
    /// `[browser] backend`, `[stt]`/`[tts]` backend, `[memory]` backend).
    pub id: String,
    /// Human display name.
    pub name: String,
    /// One-line picker blurb; the catalog's setup notes live here.
    pub blurb: String,
    /// Sorted first and preselected in the picker.
    pub recommended: bool,
    pub kind: ProviderKind,
    /// Follow-up questions asked after the pick.
    pub extra: Vec<ExtraField>,
}

/// The wizard's answer for one provider-backed group.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderAnswer {
    /// Provider/backend id for the config section.
    pub id: String,
    /// Env var NAME holding the key (cloud providers). Never the value.
    pub key_env: Option<String>,
    /// Instance URL (self-hosted providers).
    pub url: Option<String>,
    /// Extra-field answers as `(key, value)` pairs.
    pub options: Vec<(String, String)>,
    /// Local provider: binary missing and the user skipped the install.
    /// The config still records the choice; `doctor` reports the gap.
    pub skipped: bool,
}

/// Find a provider row by id (case-insensitive), for validating
/// pre-filled answers from flags.
pub fn find_provider<'a>(providers: &'a [ProviderMeta], id: &str) -> Option<&'a ProviderMeta> {
    providers.iter().find(|p| p.id.eq_ignore_ascii_case(id))
}

/// The recommended row, if any - the default the non-interactive setup
/// path resolves to.
pub fn recommended_provider(providers: &[ProviderMeta]) -> Option<&ProviderMeta> {
    providers.iter().find(|p| p.recommended)
}

// ---------------------------------------------------------------------------
// generic flow
// ---------------------------------------------------------------------------

/// The outcome of one provider screen.
///
/// Skip and cancel are different intents. `Skipped` means the user
/// deliberately left the tool unconfigured: the wizard removes the
/// tool group from the enabled-tools vec, so the shared setup path
/// writes no provider section and the runtime never registers the
/// tool - a clean "not configured", never a half-written config.
/// `Cancelled` (Esc) keeps its current meaning: the group is untouched
/// and `run_setup` resolves the recommended default, as before.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderPick {
    /// The user picked a provider and completed its follow-ups.
    Chosen(ProviderAnswer),
    /// The user chose Skip: leave this tool unconfigured.
    Skipped,
    /// The user pressed Esc: keep the previous behavior.
    Cancelled,
}

/// The Skip row's picker value. One constant feeds both the row
/// builder and the result mapping, so they cannot drift apart. The
/// value is namespaced to never collide with a registry provider id.
pub const SKIP_VALUE: &str = "__skip__";

/// The Skip row appended to every provider picker: last row, always
/// visible, so skipping is an explicit choice rather than a hidden
/// escape hatch.
fn skip_item(title: &str) -> Item {
    Item::new("Skip", SKIP_VALUE).desc(format!(
        "leave {title} unconfigured \u{2014} continue setup without it"
    ))
}

/// Map a picker return onto a [`ProviderPick`], minus the follow-ups.
/// Pure so the Skip row's value is pinned by tests without the TUI:
/// `None` (Esc) → `Cancelled`, the Skip row → `Skipped`, a provider id
/// → `follow_up` on its row (`None` from a follow-up → `Cancelled`,
/// an unknown value → `Cancelled`, fail closed).
fn pick_from_value(
    providers: &[ProviderMeta],
    value: Option<&str>,
    follow_up: impl FnOnce(&ProviderMeta) -> Option<ProviderAnswer>,
) -> ProviderPick {
    match value {
        None => ProviderPick::Cancelled,
        Some(v) if v == SKIP_VALUE => ProviderPick::Skipped,
        Some(id) => match providers.iter().find(|p| p.id == id) {
            Some(meta) => match follow_up(meta) {
                Some(answer) => ProviderPick::Chosen(answer),
                None => ProviderPick::Cancelled,
            },
            None => ProviderPick::Cancelled,
        },
    }
}

/// Run the pick → follow-up flow over normalized provider rows.
///
/// The picker ends with an explicit Skip row; Esc cancels. A provider
/// cancelled in its follow-ups is `Cancelled`, not `Skipped`.
pub fn pick_provider(title: &str, subtitle: &str, providers: Vec<ProviderMeta>) -> ProviderPick {
    // Recommended first, stable for the rest. The registry pre-sorts,
    // but the picker enforces the rule itself so a registry reorder can
    // never bury the default. The cursor starts on the first row, which
    // makes the recommended provider the pre-selected choice. The Skip
    // row is always last: skipping is deliberate, never the default.
    let mut providers = providers;
    providers.sort_by_key(|p| !p.recommended);
    let mut items: Vec<Item> = providers
        .iter()
        .map(|p| {
            let mut item = Item::new(p.name.clone(), p.id.clone()).desc(p.blurb.clone());
            if p.recommended {
                item = item.tag("recommended");
            }
            item
        })
        .collect();
    items.push(skip_item(title));
    let chosen = crate::prompt::pick_one(title, subtitle, items);
    let title = title.to_string();
    let ask_title = title.clone();
    pick_from_value(&providers, chosen.as_deref(), |meta| {
        complete_answer(
            meta,
            &mut move |label, prefill| crate::prompt::pick_text(&ask_title, label, prefill),
            &mut move |label, def| crate::prompt::pick_confirm(&title, label, def),
        )
    })
}

/// The Recommended-mode answer for one provider-backed group: the
/// recommended row (`recommended_provider`, first row when none is
/// marked - the same fallback `default_or_prompt` uses), with
/// kind-driven follow-ups and the same `crate::prompt` ask/confirm
/// closures `pick_provider` uses. NO picker screen: this is the
/// "straight to keys" path, so the user only sees the follow-ups
/// (env var name, instance URL, local detect → install-or-skip).
///
/// A non-keyless provider is prefaced with "Set up <name> now?":
/// No → `Skipped` (the group comes off, no section is written). Note
/// the `Confirm` widget submits `false` on Esc, so Esc on the preface
/// is also a skip - a yes/no question's safe answer is no. Keyless
/// providers have nothing to ask, so they resolve straight to `Chosen`
/// with no preface. Cancelling inside a follow-up (or a prompt run
/// error) → `Cancelled`, which keeps the previous behavior: the group
/// stays on and the shared setup path resolves the recommended
/// default.
pub fn complete_recommended(title: &str, providers: Vec<ProviderMeta>) -> ProviderPick {
    let title = title.to_string();
    let ask_title = title.clone();
    complete_recommended_with(
        providers,
        &mut move |label, prefill| crate::prompt::pick_text(&ask_title, label, prefill),
        &mut move |label, def| crate::prompt::pick_confirm(&title, label, def),
    )
}

/// The injectable core of [`complete_recommended`]: `ask` prompts for
/// text (`None` = cancelled) and `confirm` for yes/no, so tests stub
/// them and the panicking-stub tests prove which prompts a path never
/// touches.
pub fn complete_recommended_with(
    providers: Vec<ProviderMeta>,
    ask: &mut dyn FnMut(&str, &str) -> Option<String>,
    confirm: &mut dyn FnMut(&str, bool) -> Option<bool>,
) -> ProviderPick {
    let Some(meta) = recommended_provider(&providers)
        .or_else(|| providers.first())
        .cloned()
    else {
        // No rows at all: nothing to set up and nothing to skip.
        return ProviderPick::Cancelled;
    };
    if !matches!(meta.kind, ProviderKind::Keyless) {
        match confirm(&format!("Set up {} now?", meta.name), true) {
            // A prompt run error: keep the previous cancel behavior.
            None => return ProviderPick::Cancelled,
            Some(false) => return ProviderPick::Skipped,
            Some(true) => {}
        }
    }
    match complete_answer(&meta, ask, confirm) {
        Some(answer) => ProviderPick::Chosen(answer),
        None => ProviderPick::Cancelled,
    }
}

/// Map one screen's [`ProviderPick`] onto the wizard's downstream
/// state. Pure - no TUI, no prompts - so the skip/cancel contract is
/// unit-testable:
///
/// - `Chosen` → the answer, everything else untouched;
/// - `Skipped` → no answer; the group is removed from the enabled-tools
///   vec and recorded in `skipped`, so `run_setup` writes no section
///   for it and the runtime never registers the tool;
/// - `Cancelled` → no answer, group untouched: `run_setup` resolves the
///   recommended default, exactly as before skip existed.
pub fn apply_pick(
    tools: &mut Vec<ToolGroup>,
    skipped: &mut Vec<ToolGroup>,
    group: ToolGroup,
    pick: &ProviderPick,
) -> Option<ProviderAnswer> {
    match pick {
        ProviderPick::Chosen(answer) => Some(answer.clone()),
        ProviderPick::Skipped => {
            tools.retain(|g| *g != group);
            if !skipped.contains(&group) {
                skipped.push(group);
            }
            None
        }
        ProviderPick::Cancelled => None,
    }
}

/// Per-backend voice resolution for the Full wizard. STT and TTS are
/// picked independently but share the Voice tool group, so skip is
/// granular per backend: a skipped backend yields no answer and sets
/// its skip flag (the shared setup path then writes no section for it
/// instead of resolving the recommended default). The Voice group only
/// comes off - and is recorded in `skipped` - when BOTH backends are
/// skipped. A cancelled pick keeps current behavior: no answer, group
/// untouched.
#[derive(Debug, Clone, Default)]
pub struct VoicePicks {
    pub stt: Option<ProviderAnswer>,
    pub tts: Option<ProviderAnswer>,
    /// The STT screen was skipped: `run_setup` must not resolve the
    /// recommended STT default.
    pub skipped_stt: bool,
    /// The TTS screen was skipped: `run_setup` must not resolve the
    /// recommended TTS default.
    pub skipped_tts: bool,
}

pub fn apply_voice_picks(
    tools: &mut Vec<ToolGroup>,
    skipped: &mut Vec<ToolGroup>,
    stt_pick: &ProviderPick,
    tts_pick: &ProviderPick,
) -> VoicePicks {
    let stt = match stt_pick {
        ProviderPick::Chosen(a) => Some(a.clone()),
        _ => None,
    };
    let tts = match tts_pick {
        ProviderPick::Chosen(a) => Some(a.clone()),
        _ => None,
    };
    let skipped_stt = matches!(stt_pick, ProviderPick::Skipped);
    let skipped_tts = matches!(tts_pick, ProviderPick::Skipped);
    if skipped_stt && skipped_tts {
        tools.retain(|g| *g != ToolGroup::Voice);
        if !skipped.contains(&ToolGroup::Voice) {
            skipped.push(ToolGroup::Voice);
        }
    }
    VoicePicks {
        stt,
        tts,
        skipped_stt,
        skipped_tts,
    }
}

/// Run the kind-driven follow-ups for one already-picked provider.
///
/// The branching is on [`ProviderKind`], never on the provider id: one
/// flow serves web search, browser, STT, TTS, and memory. `ask`
/// prompts for text (`None` = cancelled) and `confirm` for yes/no, so
/// the TUI picker and the stdin setup path share this exact logic.
pub fn complete_answer(
    meta: &ProviderMeta,
    ask: &mut dyn FnMut(&str, &str) -> Option<String>,
    confirm: &mut dyn FnMut(&str, bool) -> Option<bool>,
) -> Option<ProviderAnswer> {
    let mut answer = ProviderAnswer {
        id: meta.id.clone(),
        ..Default::default()
    };
    match &meta.kind {
        ProviderKind::Keyless => {}
        ProviderKind::Cloud { env_var } => {
            let env = ask(
                &format!(
                    "Env var holding the {} API key (empty = provider default)",
                    meta.name
                ),
                env_var,
            )?;
            answer.key_env = non_empty(env).or(Some(env_var.clone()));
        }
        ProviderKind::SelfHosted {
            default_url,
            required,
        } => {
            // A provider with no default URL that requires one cannot
            // work without it, so an empty answer reprompts instead of
            // writing a broken config. Optional URLs fall back to the
            // backend's own default (env var / compiled default).
            loop {
                let url = ask(
                    &format!(
                        "{} instance URL{}",
                        meta.name,
                        if *required {
                            ""
                        } else {
                            " (empty = backend default)"
                        }
                    ),
                    default_url,
                )?;
                if let Some(u) = non_empty(url) {
                    answer.url = Some(u);
                    break;
                }
                if !default_url.is_empty() || !required {
                    break;
                }
                println!("pantheon: a URL is required for {}", meta.name);
            }
        }
        ProviderKind::Local { local } => {
            if detect_binary(&local.detect_cmd) {
                println!("pantheon: {} detected", meta.name);
            } else {
                println!("pantheon: {} not found", meta.name);
                println!("pantheon: {}", local.install_hint);
                answer.skipped =
                    !offer_install(&meta.name, local, &mut |label, def| confirm(label, def));
            }
        }
    }
    for field in &meta.extra {
        loop {
            let value = ask(&field.prompt, &field.default)?;
            match non_empty(value) {
                Some(v) => {
                    answer.options.push((field.key.clone(), v));
                    break;
                }
                None if !field.required => break,
                None => {
                    println!("pantheon: {} is required", field.key);
                }
            }
        }
    }
    Some(answer)
}

/// Shell out to check a `detect_cmd`; exit 0 = present. The command
/// comes from provider metadata, not user input.
///
/// Bounded by `DETECT_TIMEOUT`: a probe that hangs must not stall
/// `doctor` or the wizard. A `python3 -m pip --version` once took ~9.5s
/// on a cold box - with no timeout that single row held the whole
/// preflight hostage.
const DETECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

pub(crate) fn detect_binary(detect_cmd: &str) -> bool {
    let mut child = match std::process::Command::new("sh")
        .arg("-c")
        .arg(detect_cmd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let deadline = std::time::Instant::now() + DETECT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

/// Install-or-skip for a missing local binary. Returns true when the
/// binary is present afterwards (was already, or the install worked).
fn offer_install(
    name: &str,
    local: &LocalMeta,
    confirm: &mut dyn FnMut(&str, bool) -> Option<bool>,
) -> bool {
    let Some(install_cmd) = &local.install_cmd else {
        println!("pantheon: install it manually, then rerun `pantheon doctor`");
        return false;
    };
    let confirmed = confirm(&format!("Install {name} now?"), false).unwrap_or(false);
    if !confirmed {
        return false;
    }
    println!("pantheon: running: {install_cmd}");
    let ok = std::process::Command::new("sh")
        .arg("-c")
        .arg(install_cmd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok && detect_binary(&local.detect_cmd) {
        println!("pantheon: {name} installed and detected");
        return true;
    }
    println!("pantheon: install finished but {name} is still not detected");
    println!("pantheon: {}", local.install_hint);
    false
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim().to_string();
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

// ---------------------------------------------------------------------------
// row builders: one per provider-backed group, from the owning registry
// ---------------------------------------------------------------------------

/// Web search providers from the `pantheon-web` registry.
pub fn websearch_providers() -> Vec<ProviderMeta> {
    use pantheon_web::websearch::{ProviderAuth, ProviderInfo};
    pantheon_web::websearch::all_providers()
        .into_iter()
        .map(|p: ProviderInfo| {
            let kind = match p.auth {
                ProviderAuth::ApiKey { env_var } => ProviderKind::Cloud {
                    env_var: env_var.to_string(),
                },
                ProviderAuth::Keyless => ProviderKind::Keyless,
                ProviderAuth::SelfHosted { default_url } => ProviderKind::SelfHosted {
                    default_url: default_url.to_string(),
                    required: true,
                },
            };
            ProviderMeta {
                id: p.id.to_string(),
                name: p.name.to_string(),
                blurb: p.blurb.to_string(),
                recommended: p.recommended,
                kind,
                extra: Vec::new(),
            }
        })
        .collect()
}

/// Browser backends from the `pantheon-web` registry. The auth/install
/// shape per backend is wizard metadata: the registry describes what
/// the backend needs, this maps it onto the generic flow.
pub fn browser_providers() -> Vec<ProviderMeta> {
    use pantheon_web::browser::BackendKind;
    pantheon_web::browser::all_backends()
        .into_iter()
        .map(|b| {
            let (kind, extra) = match b.kind {
                BackendKind::Gsd => (
                    ProviderKind::Local {
                        local: LocalMeta {
                            detect_cmd: "command -v gsd-browser".into(),
                            install_hint: pantheon_web::browser::INSTALL_INSTRUCTIONS.into(),
                            install_cmd: Some("npm install -g @opengsd/gsd-browser".into()),
                        },
                    },
                    Vec::new(),
                ),
                BackendKind::ChromiumOxide => (
                    ProviderKind::Local {
                        local: LocalMeta {
                            detect_cmd: "command -v chromium || command -v chromium-browser || command -v google-chrome || command -v google-chrome-stable".into(),
                            install_hint: "install Chrome/Chromium via your package manager, then rerun `pantheon doctor`".into(),
                            install_cmd: None,
                        },
                    },
                    Vec::new(),
                ),
                BackendKind::Steel => (
                    ProviderKind::Cloud {
                        env_var: "STEEL_API_KEY".into(),
                    },
                    vec![ExtraField {
                        key: "steel_base_url".into(),
                        prompt: "Self-hosted Steel base URL (empty = Steel cloud)".into(),
                        default: String::new(),
                        required: false,
                    }],
                ),
                BackendKind::Browserbase => (
                    ProviderKind::Cloud {
                        env_var: "BROWSERBASE_API_KEY".into(),
                    },
                    vec![ExtraField {
                        key: "browserbase_project_id".into(),
                        prompt: "Browserbase project id".into(),
                        default: String::new(),
                        required: true,
                    }],
                ),
                BackendKind::Lightpanda => (
                    ProviderKind::SelfHosted {
                        // No compiled default: the backend needs a
                        // running `lightpanda serve`, so the URL is
                        // required (the flow reprompts on empty).
                        default_url: String::new(),
                        required: true,
                    },
                    Vec::new(),
                ),
                BackendKind::Playwright => (
                    ProviderKind::Local {
                        local: LocalMeta {
                            detect_cmd: "command -v playwright-cli".into(),
                            install_hint: "install the Playwright CLI (`@playwright/cli`), then rerun `pantheon doctor`".into(),
                            install_cmd: Some("npm install -g @playwright/cli".into()),
                        },
                    },
                    Vec::new(),
                ),
                BackendKind::Camofox => (
                    ProviderKind::Local {
                        local: LocalMeta {
                            detect_cmd: "python3 -c \"import camoufox\"".into(),
                            install_hint: pantheon_web::browser::CAMOFOX_INSTALL_INSTRUCTIONS
                                .into(),
                            install_cmd: Some(
                                "pip install \"camoufox[geoip]\" && python -m camoufox fetch".into(),
                            ),
                        },
                    },
                    Vec::new(),
                ),
            };
            ProviderMeta {
                id: b.kind.id().to_string(),
                name: b.name.to_string(),
                // Role + needs: the registry's own description of what
                // the backend is and what it requires.
                blurb: format!("{} Needs: {}", b.role, b.needs),
                // GSD is the documented default backend.
                recommended: matches!(b.kind, BackendKind::Gsd),
                kind,
                extra,
            }
        })
        .collect()
}

/// STT backends from the `pantheon-providers` voice registry.
///
/// Every registry entry is offered: all of them are constructible via
/// `open_stt` (the bespoke wires are implemented from public docs and
/// pinned by fixtures; first live runs are verification runs).
pub fn stt_providers() -> Vec<ProviderMeta> {
    use pantheon_providers::{AuthRequirement, VoiceBackendKind};
    pantheon_providers::stt_providers()
        .into_iter()
        .map(|b| {
            let extra: Vec<ExtraField> = b
                .setup_fields
                .iter()
                .map(|f| ExtraField {
                    key: f.key.to_string(),
                    prompt: f.prompt.to_string(),
                    default: f.default.to_string(),
                    required: f.required,
                })
                .collect();
            let kind = match (&b.kind, &b.auth, &b.local) {
                (VoiceBackendKind::Subprocess, _, Some(local)) => ProviderKind::Local {
                    local: LocalMeta {
                        detect_cmd: local.detect_cmd.to_string(),
                        install_hint: local.install_hint.to_string(),
                        install_cmd: local.install_cmd.map(str::to_string),
                    },
                },
                (_, AuthRequirement::ApiKey { env_var }, _) => ProviderKind::Cloud {
                    env_var: env_var.to_string(),
                },
                _ => ProviderKind::Keyless,
            };
            ProviderMeta {
                id: b.name.to_string(),
                name: b.label.to_string(),
                blurb: b.setup_note.to_string(),
                recommended: b.recommended,
                kind,
                extra,
            }
        })
        .collect()
}

/// TTS backends from the `pantheon-providers` voice registry.
///
/// Every registry entry is offered: all of them are constructible via
/// `open_tts` (the bespoke wires are implemented from public docs and
/// pinned by fixtures; first live runs are verification runs).
/// `kokoro-local` needs an explicit `cmd` - community kokoro CLIs differ
/// in arg shape, so the wizard asks for the full invocation.
pub fn tts_providers() -> Vec<ProviderMeta> {
    use pantheon_providers::{AuthRequirement, VoiceBackendKind};
    pantheon_providers::tts_providers()
        .into_iter()
        .map(|b| {
            let extra: Vec<ExtraField> = b
                .setup_fields
                .iter()
                .map(|f| ExtraField {
                    key: f.key.to_string(),
                    prompt: f.prompt.to_string(),
                    default: f.default.to_string(),
                    required: f.required,
                })
                .collect();
            let kind = match (&b.kind, &b.auth, &b.local) {
                (VoiceBackendKind::Subprocess, _, Some(local)) => ProviderKind::Local {
                    local: LocalMeta {
                        detect_cmd: local.detect_cmd.to_string(),
                        install_hint: local.install_hint.to_string(),
                        install_cmd: local.install_cmd.map(str::to_string),
                    },
                },
                (_, AuthRequirement::ApiKey { env_var }, _) => ProviderKind::Cloud {
                    env_var: env_var.to_string(),
                },
                _ => ProviderKind::Keyless,
            };
            ProviderMeta {
                id: b.name.to_string(),
                name: b.label.to_string(),
                blurb: b.setup_note.to_string(),
                recommended: b.recommended,
                kind,
                extra,
            }
        })
        .collect()
}

/// Memory backends from the `pantheon-memory` registry. `native` needs
/// nothing; every other backend is a bridge whose URL and key resolve
/// from `options.url`/`options.key` or the fixed env convention
/// `PANTHEON_MEMORY_<NAME>_URL` / `PANTHEON_MEMORY_<NAME>_KEY` - the
/// wizard requires the URL (no compiled default exists; the env var is
/// a runtime fallback, not a wizard escape hatch) and names the env
/// vars, but never collects the key value. The `fake` backend is a test
/// double and never offered.
pub fn memory_providers() -> Vec<ProviderMeta> {
    pantheon_memory::BackendRegistry::with_defaults()
        .catalog()
        .into_iter()
        .filter(|b| b.name != "fake")
        .map(|b| {
            let native = b.name == "native";
            let kind = if native {
                ProviderKind::Keyless
            } else {
                ProviderKind::SelfHosted {
                    default_url: String::new(),
                    // No compiled default exists for any bridge backend
                    // (the vendor defaults were removed as broken-by-design),
                    // so an empty answer must reprompt rather than write a
                    // URL-less selection the runtime would fail closed on.
                    required: true,
                }
            };
            let prefix = b.name.to_uppercase().replace('-', "_");
            let blurb = if native {
                b.auth.clone()
            } else {
                format!(
                    "{}. url/key: options or $PANTHEON_MEMORY_{prefix}_URL / $PANTHEON_MEMORY_{prefix}_KEY",
                    b.auth
                )
            };
            ProviderMeta {
                id: b.name.clone(),
                name: b.label.clone(),
                blurb,
                // Native is the default the runtime already uses.
                recommended: native,
                kind,
                extra: Vec::new(),
            }
        })
        .collect()
}

/// Desktop computer-use drivers. Today there is exactly one: the CUA
/// driver (trycua/cua, MIT), a background desktop driver exposed to the
/// runtime as an MCP server over stdio. Local: detect the binary, offer
/// the verified install one-liner, or skip.
pub fn computer_providers() -> Vec<ProviderMeta> {
    vec![ProviderMeta {
        id: "cua-driver".into(),
        name: "CUA Driver".into(),
        blurb: "Desktop control through the CUA driver (trycua/cua). Drives the real desktop: screenshots, clicks, typing. Needs a graphical session (X11/XWayland on Linux) and a running `cua-driver serve` daemon."
            .into(),
        recommended: true,
        kind: ProviderKind::Local {
            local: LocalMeta {
                detect_cmd: "command -v cua-driver".into(),
                install_hint: "Install with the official installer:\n  /bin/bash -c \"$(curl -fsSL https://cua.ai/driver/install.sh)\"\nLinux needs X11/XWayland; start the daemon inside your graphical session with `cua-driver serve`."
                    .into(),
                install_cmd: Some(
                    "/bin/bash -c \"$(curl -fsSL https://cua.ai/driver/install.sh)\"".into(),
                ),
            },
        },
        extra: Vec::new(),
    }]
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Item 1: the voice registry (pantheon-providers) is the source of
    /// truth for backend ids; pantheon-api's validation lists must cover
    /// every backend the registry can construct, or `doctor` rejects what
    /// `setup` writes (the setup/doctor infinite loop).
    #[test]
    fn voice_registry_matches_api_validation_lists() {
        for b in pantheon_providers::stt_backends() {
            assert!(
                pantheon_api::config::STT_BACKENDS.contains(&b.name),
                "stt backend {:?} is in the registry but not in pantheon-api's STT_BACKENDS",
                b.name
            );
        }
        for b in pantheon_providers::tts_backends() {
            assert!(
                pantheon_api::config::TTS_BACKENDS.contains(&b.name),
                "tts backend {:?} is in the registry but not in pantheon-api's TTS_BACKENDS",
                b.name
            );
        }
    }

    /// Item 9: a hanging probe returns false instead of stalling.
    /// (`sleep 30` must die at the 5s timeout, well under the 10s bound.)
    #[test]
    fn detect_binary_times_out_on_hang() {
        let start = std::time::Instant::now();
        assert!(!detect_binary("sleep 30"));
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "detect_binary did not time out"
        );
    }

    #[test]
    fn detect_binary_fast_paths() {
        assert!(detect_binary("command -v sh"));
        assert!(!detect_binary(
            "command -v pantheon-definitely-not-a-binary-xyz"
        ));
    }
}
