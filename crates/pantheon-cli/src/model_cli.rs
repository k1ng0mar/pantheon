//! `pantheon model`: point pantheon at a provider and pick the model.
//!
//! Interactive (a TTY picker):
//!   1. provider list (builtins + customs) — scroll or type to filter,
//!      Enter to select. New endpoints are created in `pantheon provider
//!      add`; URL edits that diverge from a builtin are saved as explicit
//!      override rows.
//!   2. base URL (prefilled from the catalog, `:port` = localhost) + wire
//!      mode (OpenAI-compatible vs Anthropic) picker.
//!   3. API key prompt — comma-separated to stack multiple keys. Stored in
//!      `<data_dir>/.env` (`PANTHEON_KEY_X=k1,k2`), never in config.toml.
//!      The chain rotates stacked keys on 401/403/429.
//!   4. models are fetched live from `{base}/models`; pick one (or type an
//!      id manually when the endpoint has no `/models`).
//!   5. bottom step: optionally pin judge/compression/title/other auxiliaries.
//!
//! Bare `pantheon model` configures the default chat model;
//! `pantheon model --auxiliary judge|compression|title_gen|...` pins one
//! auxiliary instead. `pantheon model --list` shows the current setup.
//!
//! Non-interactive (scripts): pass `--provider` + `--model` (and `--key`,
//! `--base-url`, `--api-mode`, `--api-key-env`, `--remove` as needed).

use super::config_doc::{
    self, CompressionSection, Config, EmbeddingsSection, JudgeSection, McpSynthesisSection,
    ScheduledSection, SearchSynthesisSection, TitleGenSection, VisionSection,
};
use pantheon_providers::catalog::{self, ApiMode};
use std::io::IsTerminal;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) fn flag(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            return args.get(i + 1).cloned();
        }
        i += 1;
    }
    None
}

/// Every `--name value` occurrence (for repeatable flags like `--set`).
pub(crate) fn flags(args: &[String], name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == name {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
            }
        }
        i += 1;
    }
    out
}

pub(crate) fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

pub(crate) fn data_dir() -> PathBuf {
    crate::data_dir()
}

// ---------------------------------------------------------------------------
// tiny scroll/search picker (crossterm alternate screen)
// ---------------------------------------------------------------------------

pub struct PickItem {
    pub label: String,
    pub desc: String,
}

struct TermGuard;
impl Drop for TermGuard {
    fn drop(&mut self) {
        use crossterm::terminal::disable_raw_mode;
        let _ = disable_raw_mode();
        use crossterm::{cursor, execute};
        let _ = execute!(std::io::stdout(), cursor::Show);
    }
}

/// Scroll + type-to-filter selection. Returns the index into `items`.
pub fn pick(title: &str, items: &[PickItem]) -> Option<usize> {
    use crossterm::{
        cursor,
        event::{self, Event, KeyCode, KeyModifiers},
        execute,
        terminal::{enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };
    if items.is_empty() {
        return None;
    }
    enable_raw_mode().ok()?;
    let _guard = TermGuard;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, cursor::Hide).ok()?;
    // LeaveAlternateScreen must run before the guard's Drop (which only
    // restores raw mode + cursor); do it explicitly on every exit path.
    let leave = |out: &mut std::io::Stdout| {
        let _ = execute!(*out, LeaveAlternateScreen);
    };

    let mut filter = String::new();
    let mut selected = 0usize;
    let visible_rows = 12usize;

    let filtered = |filter: &str| -> Vec<usize> {
        let f = filter.to_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, it)| {
                f.is_empty()
                    || it.label.to_lowercase().contains(&f)
                    || it.desc.to_lowercase().contains(&f)
            })
            .map(|(i, _)| i)
            .collect()
    };

    loop {
        let hits = filtered(&filter);
        if selected >= hits.len() {
            selected = hits.len().saturating_sub(1);
        }
        // Window the list around the selection.
        let start = if hits.len() <= visible_rows {
            0
        } else {
            selected
                .saturating_sub(visible_rows / 2)
                .min(hits.len() - visible_rows)
        };

        execute!(stdout, cursor::MoveTo(0, 0)).ok();
        let _ = crossterm::terminal::Clear(crossterm::terminal::ClearType::All);
        println!("{title}\r");
        println!("  filter: {filter}\r");
        println!("  ↑↓ navigate · type to filter · Enter select · Esc cancel\r");
        if hits.is_empty() {
            println!("  (no matches)\r");
        }
        for (row, &idx) in hits.iter().skip(start).take(visible_rows).enumerate() {
            let marker = if start + row == selected { ">" } else { " " };
            let it = &items[idx];
            if start + row == selected {
                println!("  {marker} \x1b[1m{}\x1b[0m  {}\r", it.label, it.desc);
            } else {
                println!("  {marker} {}  {}\r", it.label, it.desc);
            }
        }
        println!(
            "\r  {} match{}\r",
            hits.len(),
            if hits.len() == 1 { "" } else { "es" }
        );
        let _ = stdout.flush();

        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => {
                leave(&mut stdout);
                return None;
            }
        };
        if let Event::Key(k) = ev {
            match k.code {
                KeyCode::Esc => {
                    leave(&mut stdout);
                    return None;
                }
                KeyCode::Enter => {
                    if hits.is_empty() {
                        continue;
                    }
                    leave(&mut stdout);
                    return Some(hits[selected]);
                }
                KeyCode::Up => selected = selected.saturating_sub(1),
                KeyCode::Down => {
                    if selected + 1 < hits.len() {
                        selected += 1;
                    }
                }
                KeyCode::Home => selected = 0,
                KeyCode::End => selected = hits.len().saturating_sub(1),
                KeyCode::PageUp => selected = selected.saturating_sub(visible_rows),
                KeyCode::PageDown => {
                    selected = (selected + visible_rows).min(hits.len().saturating_sub(1));
                }
                KeyCode::Backspace => {
                    filter.pop();
                    selected = 0;
                }
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    leave(&mut stdout);
                    return None;
                }
                KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    if selected + 1 < hits.len() {
                        selected += 1;
                    }
                }
                KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    selected = selected.saturating_sub(1);
                }
                KeyCode::Char(c) => {
                    filter.push(c);
                    selected = 0;
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// line prompts
// ---------------------------------------------------------------------------

pub(crate) fn prompt_line(prompt: &str, default: &str) -> String {
    if default.is_empty() {
        print!("{prompt}: ");
    } else {
        print!("{prompt} [{default}]: ");
    }
    let _ = std::io::stdout().flush();
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
    let answer = buf.trim().to_string();
    if answer.is_empty() {
        default.to_string()
    } else {
        answer
    }
}

/// Normalize a base-URL input. `:port` is shorthand for a localhost
/// OpenAI-compatible endpoint (`:8015` → `http://127.0.0.1:8015/v1`);
/// anything else is trimmed of a trailing slash and used as-is.
pub(crate) fn normalize_base_url(input: &str) -> String {
    let s = input.trim();
    if let Some(port) = s.strip_prefix(':') {
        let port = port.trim().trim_end_matches('/');
        if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
            return format!("http://127.0.0.1:{port}/v1");
        }
    }
    trim_slash(s).to_string()
}

/// Drop one trailing-slash run (`https://x/v1/` → `https://x/v1`).
/// The single shared trim behind every base-URL join in the CLI.
pub(crate) fn trim_slash(s: &str) -> &str {
    s.trim_end_matches('/')
}

/// First entry of a comma-stacked key (`k1,k2` → `k1`). Empty when blank.
pub(crate) fn first_key(stacked: &str) -> &str {
    stacked.split(',').next().map(str::trim).unwrap_or("")
}

/// How many non-blank entries a comma-stacked key holds.
pub(crate) fn count_keys(stacked: &str) -> usize {
    stacked.split(',').filter(|k| !k.trim().is_empty()).count()
}

/// Parse `--api-mode openai|openai-compatible|anthropic`. Exits 2 on
/// anything else so both verbs reject the same way.
pub(crate) fn parse_api_mode(raw: Option<&str>) -> ApiMode {
    match raw {
        None => ApiMode::OpenAi,
        Some(m) => match m.trim().to_ascii_lowercase().as_str() {
            "anthropic" => ApiMode::Anthropic,
            "openai" | "openai-compatible" => ApiMode::OpenAi,
            _ => {
                eprintln!("unknown --api-mode {m:?} (openai|anthropic)");
                std::process::exit(2);
            }
        },
    }
}

/// Parse repeatable `--set VAR=value` pairs. Exits 2 on a bad entry so
/// both verbs reject the same way.
pub(crate) fn parse_set_pairs(raw: Vec<String>) -> Vec<(String, String)> {
    raw.into_iter()
        .map(|s| match s.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() && !v.trim().is_empty() => {
                (k.trim().to_string(), v.trim().to_string())
            }
            _ => {
                eprintln!("--set needs VAR=value, got {s:?}");
                std::process::exit(2);
            }
        })
        .collect()
}

pub fn mask_key(key: &str) -> String {
    let key = key.trim();
    if key.is_empty() {
        return "(none)".into();
    }
    // Only hint at the first stacked key. Char-based slicing: keys are
    // ASCII tokens in practice, but never panic on odd input.
    let first = first_key(key);
    let chars: Vec<char> = first.chars().collect();
    if chars.len() <= 8 {
        return "********".into();
    }
    let head: String = chars[..3].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

// ---------------------------------------------------------------------------
// live model fetch
// ---------------------------------------------------------------------------

/// The `anthropic-version` header value. Duplicated from
/// `pantheon_providers::anthropic::ANTHROPIC_VERSION` rather than imported:
/// the CLI does not depend on the provider plane, and this is a pinned wire
/// constant. Keep the two in step.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

/// GET `{base}/models` with the first stacked key. Accepts the OpenAI
/// list shape (`{"data":[{"id"}]}`), `{"models":[...]}` and bare arrays.
///
/// `mode` picks the auth header, because a wire mode is not just a request
/// body dialect: an Anthropic endpoint expects `x-api-key` plus
/// `anthropic-version` and rejects `Authorization: Bearer`. Sending the OpenAI
/// header to an Anthropic-wire endpoint fails auth, which reads as "this
/// endpoint has no models" rather than as a credential problem.
pub fn fetch_models(
    base_url: &str,
    key: &str,
    mode: pantheon_providers::catalog::ApiMode,
) -> Result<Vec<String>, String> {
    let base = trim_slash(base_url.trim());
    let url = format!("{base}/models");
    let first = first_key(key);
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .build();
    let mut req = agent.get(&url);
    if !first.is_empty() {
        match mode {
            pantheon_providers::catalog::ApiMode::Anthropic => {
                req = req
                    .set("x-api-key", first)
                    .set("anthropic-version", ANTHROPIC_VERSION);
            }
            pantheon_providers::catalog::ApiMode::OpenAi => {
                req = req.set("Authorization", &format!("Bearer {first}"));
            }
        }
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(160).collect();
            format!("HTTP {code} from {url}: {snippet}")
        }
        e => format!("{url}: {e}"),
    })?;
    let body = resp.into_string().map_err(|e| format!("read {url}: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse {url}: {e}"))?;
    let arr = v.get("data").or_else(|| v.get("models")).unwrap_or(&v);
    let arr = arr.as_array().ok_or_else(|| {
        format!("unexpected shape from {url}: expected {{\"data\":[{{\"id\"}}]}}")
    })?;
    let mut ids: Vec<String> = arr
        .iter()
        .filter_map(|m| {
            if let Some(s) = m.as_str() {
                return Some(s.to_string());
            }
            m.get("id")
                .or_else(|| m.get("name"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .collect();
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return Err(format!("{url} returned no models"));
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// save
// ---------------------------------------------------------------------------

pub enum Target {
    Default,
    Auxiliary(String),
}

pub struct ModelChoice {
    pub provider: String,
    pub model: String,
    /// Base URL override for custom providers (None = cataloged).
    pub base_url: Option<String>,
    /// Wire mode for custom providers.
    pub api_mode: ApiMode,
    /// Env var holding the key(s).
    pub key_env: String,
    /// Raw comma-separated keys to store in .env (None = keep existing).
    pub raw_keys: Option<String>,
    pub target: Target,
}

fn aux_section_name(target: &Target) -> Option<&str> {
    match target {
        Target::Default => None,
        Target::Auxiliary(k) => Some(k.as_str()),
    }
}

/// Fill a templated base URL's `{var}` values into `.env` (and the
/// process env) and return the resolved URL.
///
/// * `preset`: `--set VAR=value` pairs (non-interactive).
/// * interactive: prompt per var, defaulting to any stored value.
/// * non-interactive without preset or stored value: fail-closed, naming
///   exactly which `--set` flags (or `pantheon model` run) are needed.
pub(crate) fn ensure_template_vars(
    data_dir: &std::path::Path,
    provider_id: &str,
    base: &str,
    preset: &[(String, String)],
    interactive: bool,
) -> Result<String, String> {
    let vars = catalog::template_vars(base);
    if vars.is_empty() {
        return Ok(base.to_string());
    }
    for v in &vars {
        let env_name = catalog::config_env_name(provider_id, v);
        let stored = std::env::var(&env_name)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                super::dotenv::read_dotenv_value(data_dir, &env_name)
                    .filter(|s| !s.trim().is_empty())
            });
        if let Some((_, val)) = preset.iter().find(|(k, _)| k == &env_name || k == v) {
            super::dotenv::persist_dotenv_value(data_dir, &env_name, val.trim())
                .map_err(|e| format!("write .env: {e}"))?;
            continue;
        }
        if interactive {
            let mut tries = 0;
            loop {
                let answer = prompt_line(
                    &format!("{provider_id} {v} [{env_name}]"),
                    stored.as_deref().unwrap_or(""),
                );
                if !answer.trim().is_empty() {
                    super::dotenv::persist_dotenv_value(data_dir, &env_name, answer.trim())
                        .map_err(|e| format!("write .env: {e}"))?;
                    break;
                }
                tries += 1;
                if tries >= 3 {
                    return Err(format!("{v} is required — re-run and provide it"));
                }
                eprintln!("  {v} is required — the endpoint URL contains {{{v}}}");
            }
            continue;
        }
        if stored.is_none() {
            return Err(format!(
                "provider {provider_id:?} needs --set {v}=... (stored as {env_name}); or run `pantheon model` interactively"
            ));
        }
    }
    catalog::resolve_template(provider_id, base)
}
/// Persist the choice: `.env` keys, `[custom_providers.*]` row for custom
/// endpoints, and the `[model]` / aux section. Registers customs in-memory.
pub fn save_choice(data_dir: &std::path::Path, choice: &ModelChoice) -> Result<(), String> {
    if let Some(keys) = &choice.raw_keys {
        super::dotenv::persist_dotenv_value(data_dir, &choice.key_env, keys.trim())
            .map_err(|e| format!("write .env: {e}"))?;
    }
    if let Some(base) = &choice.base_url {
        config_doc::upsert_custom_row(
            data_dir,
            &choice.provider,
            base,
            choice.api_mode,
            &choice.key_env,
        )?;
    }
    // Reload: the row writer above persisted + registered already.
    let mut cfg = Config::load(data_dir).unwrap_or_default();
    match &choice.target {
        Target::Default => {
            let fallbacks = cfg
                .model
                .as_ref()
                .map(|m| m.fallbacks.clone())
                .unwrap_or_default();
            cfg.model = Some(config_doc::ModelSection {
                reasoning_budget: None,
                provider: choice.provider.clone(),
                model: choice.model.clone(),
                api_key_env: Some(choice.key_env.clone()),
                fallbacks,
                // A model pick does not touch reasoning: changing providers
                // must not silently reset an effort level the user chose.
                reasoning: cfg.model.as_ref().and_then(|m| m.reasoning.clone()),
            });
        }
        Target::Auxiliary(kind) => match kind.as_str() {
            "judge" => {
                cfg.judge = Some(JudgeSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
            "compression" => {
                cfg.compression = Some(CompressionSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
            "embeddings" => {
                cfg.embeddings = Some(EmbeddingsSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
            "search_synthesis" => {
                cfg.search_synthesis = Some(SearchSynthesisSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
            "vision" => {
                cfg.vision = Some(VisionSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
            "scheduled" => {
                cfg.scheduled = Some(ScheduledSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
            "mcp_synthesis" => {
                cfg.mcp_synthesis = Some(McpSynthesisSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
            _ => {
                cfg.title_gen = Some(TitleGenSection {
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                });
            }
        },
    }
    cfg.save(data_dir).map_err(|e| e.to_string())?;
    config_doc::register_custom_providers(&cfg);
    Ok(())
}

// ---------------------------------------------------------------------------
// `pantheon model --list`
// ---------------------------------------------------------------------------

pub(crate) fn key_status(env: Option<&str>) -> String {
    match env {
        None => "default PANTHEON_API_KEY path".into(),
        Some(e) => match std::env::var(e) {
            Ok(v) if !v.trim().is_empty() => {
                let n = count_keys(&v);
                format!("{e} set ({} key{})", n, if n == 1 { "" } else { "s" })
            }
            // The .env file is loaded at startup, so this fallback only
            // matters to direct (non-CLI) callers.
            _ => match super::dotenv::read_dotenv_value(&data_dir(), e) {
                Some(v) if !v.trim().is_empty() => format!("{e} set in .env"),
                _ => format!("{e} NOT SET"),
            },
        },
    }
}

fn cmd_list() {
    let cfg = Config::load(&data_dir()).unwrap_or_default();
    match &cfg.model {
        Some(m) => println!(
            "default:   {} / {}  [{}]",
            m.provider,
            m.model,
            key_status(m.api_key_env.as_deref())
        ),
        None => println!("default:   (unset — run `pantheon model`)"),
    }
    let aux = |name: &str, sec: Option<(String, String, Option<String>)>| match sec {
        Some((p, m, k)) => println!("{name:<10} {p} / {m}  [{}]", key_status(k.as_deref())),
        None => println!("{name:<10} auto (default model)"),
    };
    aux(
        "judge",
        cfg.judge
            .clone()
            .map(|d| (d.provider, d.model, d.api_key_env)),
    );
    aux(
        "compression",
        cfg.compression
            .clone()
            .map(|c| (c.provider, c.model, c.api_key_env)),
    );
    aux(
        "title_gen",
        cfg.title_gen
            .clone()
            .map(|t| (t.provider, t.model, t.api_key_env)),
    );
    aux(
        "embeddings",
        cfg.embeddings
            .clone()
            .map(|e| (e.provider, e.model, e.api_key_env)),
    );
    aux(
        "search_syn",
        cfg.search_synthesis
            .clone()
            .map(|s| (s.provider, s.model, s.api_key_env)),
    );
    aux(
        "vision",
        cfg.vision
            .clone()
            .map(|v| (v.provider, v.model, v.api_key_env)),
    );
    aux(
        "scheduled",
        cfg.scheduled
            .clone()
            .map(|s| (s.provider, s.model, s.api_key_env)),
    );
    aux(
        "mcp_synth",
        cfg.mcp_synthesis
            .clone()
            .map(|s| (s.provider, s.model, s.api_key_env)),
    );
    if cfg.custom_providers.is_empty() {
        println!("custom providers: none");
    } else {
        println!("custom providers:");
        let mut names: Vec<&String> = cfg.custom_providers.keys().collect();
        names.sort();
        for n in names {
            let s = &cfg.custom_providers[n];
            println!(
                "  {n}: {} ({})  [{}]",
                s.base_url,
                s.api_mode,
                key_status(s.key_env.as_deref().or(Some(&format!(
                    "PANTHEON_KEY_{}",
                    config_doc::sanitize_env_suffix(n)
                ))))
            );
        }
    }
    let default_provider = cfg
        .model
        .as_ref()
        .map(|m| m.provider.clone())
        .unwrap_or_default();
    print_template_status(&default_provider);
    let mut customs: Vec<&String> = cfg.custom_providers.keys().collect();
    customs.sort();
    for n in customs {
        if *n != default_provider {
            print_template_status(n);
        }
    }
}

/// Endpoint template requirements for one provider id: which
/// `PANTHEON_<PROVIDER>_<VAR>` values exist and which are MISSING.
/// Silent for template-free providers.
pub(crate) fn print_template_status(provider_id: &str) {
    if provider_id.is_empty() {
        return;
    }
    let vars = catalog::required_config_vars(provider_id);
    if vars.is_empty() {
        return;
    }
    let parts: Vec<String> = vars
        .iter()
        .map(|v| {
            let e = catalog::config_env_name(provider_id, v);
            let set = std::env::var(&e)
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false)
                || super::dotenv::read_dotenv_value(&data_dir(), &e)
                    .map(|s| !s.trim().is_empty())
                    .unwrap_or(false);
            format!("{e}={}", if set { "set" } else { "MISSING" })
        })
        .collect();
    println!("  endpoint config ({provider_id}): {}", parts.join(", "));
}

// ---------------------------------------------------------------------------
// interactive flow
// ---------------------------------------------------------------------------

pub(crate) fn valid_provider_id(id: &str) -> bool {
    !id.is_empty() && !id.chars().any(|c| c.is_whitespace() || c == ':') && id.len() <= 64
}

/// A custom name must not shadow a builtin catalog id (the custom row
/// would silently hijack the builtin's base URL at runtime).
pub(crate) fn builtin_provider_id(id: &str) -> bool {
    catalog::providers().iter().any(|p| p.id == id)
}

pub(crate) fn wire_mode_items() -> Vec<PickItem> {
    vec![
        PickItem {
            label: "OpenAI-compatible".into(),
            desc: "/chat/completions".into(),
        },
        PickItem {
            label: "Anthropic".into(),
            desc: "/messages".into(),
        },
    ]
}

fn interactive(target: Target) -> Option<ModelChoice> {
    let scope = match &target {
        Target::Default => "default model",
        Target::Auxiliary(k) => match k.as_str() {
            "judge" => "judge auxiliary",
            "compression" => "compression auxiliary",
            "embeddings" => "embeddings auxiliary",
            "search_synthesis" => "search-synthesis auxiliary",
            "vision" => "vision auxiliary",
            "scheduled" => "scheduled-run auxiliary",
            "mcp_synthesis" => "MCP-synthesis auxiliary",
            _ => "title auxiliary",
        },
    };

    // 1. provider: pure selection. Endpoint management (add/remove)
    // lives in `pantheon provider`; customs created there appear here.
    // Returns the catalog entry and whether the id already has a custom
    // row. URL edits that diverge from a builtin are upgraded to an
    // explicit override row in step 2, so they stay visible + removable.
    let (known, was_custom): (Option<pantheon_providers::catalog::ProviderMeta>, bool) = {
        let all = catalog::all_providers();
        let customs: Vec<String> = Config::load(&data_dir())
            .map(|c| {
                let mut n: Vec<String> = c.custom_providers.keys().cloned().collect();
                n.sort();
                n
            })
            .unwrap_or_default();
        let items: Vec<PickItem> = all
            .iter()
            .map(|p| {
                let n = if p.models.is_empty() {
                    "any model id".into()
                } else {
                    format!("{} models", p.models.len())
                };
                PickItem {
                    label: format!("{} ({})", p.label, p.id),
                    desc: format!("{} · {} · {n}", p.base_url, p.tag),
                }
            })
            .collect();
        let prov_idx = pick(
            &format!("pantheon model — {scope}: pick a provider"),
            &items,
        )?;
        if prov_idx >= all.len() {
            // Defensive: rows are exactly the providers today.
            return None;
        }
        let p = all[prov_idx].clone();
        let was_custom = customs.iter().any(|c| c == &p.id);
        (Some(p), was_custom)
    };

    // 2. base URL + wire mode + provider id. New endpoints are created
    // in `pantheon provider add`; here every row resolves to a catalog
    // entry (builtin or custom).
    let Some(p) = known else {
        // Unreachable: step 1 always breaks with a selection.
        return None;
    };
    let (provider_id, base_url, api_mode, key_env_default, is_custom) = {
        let url = normalize_base_url(&prompt_line("Base URL or :port", &p.base_url));
        let mode_items = wire_mode_items();
        let default_mode = match p.api_mode {
            ApiMode::OpenAi => 0,
            ApiMode::Anthropic => 1,
        };
        println!(
            "Wire mode [{}]:",
            if default_mode == 0 {
                "openai"
            } else {
                "anthropic"
            }
        );
        let mode_idx = pick("wire mode", &mode_items).unwrap_or(default_mode);
        let key_env = if p.key_env.is_empty() {
            format!("PANTHEON_KEY_{}", config_doc::sanitize_env_suffix(&p.id))
        } else {
            p.key_env.clone()
        };
        // An edited URL diverges from the builtin: persist it as an
        // explicit override row (visible in --list, removable) instead of
        // silently dropping the edit.
        let overridden = url != p.base_url;
        if overridden {
            println!("  saving as an override of builtin '{}'", p.id);
        }
        (
            p.id,
            url,
            if mode_idx == 1 {
                ApiMode::Anthropic
            } else {
                ApiMode::OpenAi
            },
            key_env,
            was_custom || overridden,
        )
    };

    // 3. keys → .env.
    let key_env = prompt_line("Key env var", &key_env_default);
    let existing =
        super::dotenv::read_dotenv_value(&data_dir(), &key_env).filter(|v| !v.trim().is_empty());
    if let Some(cur) = &existing {
        println!("  current: {} (empty keeps)", mask_key(cur));
    }
    let keys = prompt_line("API key(s), comma-separated to stack", "");
    // Empty input keeps an existing key, or means keyless when none exists.
    if keys.trim().is_empty() && existing.is_none() {
        println!("  no key stored — keyless endpoints only");
    }
    let raw_keys = if keys.trim().is_empty() {
        None
    } else {
        Some(keys.trim().to_string())
    };
    let effective_keys = raw_keys.clone().or(existing).unwrap_or_default();

    // 3b. endpoint template values (azure resource, bedrock region, ...).
    // Resolves the URL the model fetch below calls; the stored row keeps
    // the template so values stay live in .env.
    let fetch_base = match ensure_template_vars(&data_dir(), &provider_id, &base_url, &[], true) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("endpoint config: {e}");
            std::process::exit(2);
        }
    };

    // 4. fetch + pick model.
    let mut model_ids = match fetch_models(&fetch_base, &effective_keys, api_mode) {
        Ok(ids) => {
            println!("  fetched {} models from {fetch_base}", ids.len());
            ids
        }
        Err(e) => {
            eprintln!("  fetch failed: {e}");
            // Curated catalog rows still let you pick something known-good.
            let curated: Vec<String> = catalog::provider(&provider_id)
                .map(|p| p.models.iter().map(|m| m.model.clone()).collect())
                .unwrap_or_default();
            if !curated.is_empty() {
                println!("  falling back to {} cataloged models", curated.len());
            }
            curated
        }
    };
    let manual = "Type a model id manually...".to_string();
    model_ids.push(manual.clone());
    let model_items: Vec<PickItem> = model_ids
        .iter()
        .map(|m| PickItem {
            label: m.clone(),
            desc: if m == &manual {
                "endpoint has no /models, or id not listed".into()
            } else {
                String::new()
            },
        })
        .collect();
    let model_idx = pick(&format!("pick a model on {provider_id}"), &model_items)?;
    // A hand-typed id is the operator telling us what this endpoint serves, so
    // it gets written down. A picked-from-fetch id does not: it came from the
    // endpoint and will be fetched again next time.
    let manual_typed = model_ids[model_idx] == manual;
    let model = if manual_typed {
        loop {
            let m = prompt_line("Model id", "");
            if !m.is_empty() {
                break m;
            }
        }
    } else {
        model_ids[model_idx].clone()
    };
    if manual_typed && is_custom {
        match config_doc::remember_custom_model(&data_dir(), &provider_id, &model) {
            Ok(true) => println!("  recorded {model} on {provider_id}"),
            Ok(false) => {}
            Err(e) => eprintln!("  could not record {model}: {e}"),
        }
    }

    Some(ModelChoice {
        provider: provider_id,
        model,
        base_url: if is_custom { Some(base_url) } else { None },
        api_mode,
        key_env,
        raw_keys,
        target,
    })
}

// ---------------------------------------------------------------------------
// remove custom provider (picker row + `--remove`)
// ---------------------------------------------------------------------------

/// Sections currently pointing at `name`, for the "still in use" guard.
fn dependents_of(cfg: &Config, name: &str) -> Vec<String> {
    let mut out = Vec::new();
    if cfg
        .model
        .as_ref()
        .map(|m| m.provider == name)
        .unwrap_or(false)
    {
        out.push("default [model]".into());
    }
    for (section, sec) in [
        ("judge", cfg.judge.as_ref().map(|d| d.provider.as_str())),
        (
            "compression",
            cfg.compression.as_ref().map(|c| c.provider.as_str()),
        ),
        (
            "title_gen",
            cfg.title_gen.as_ref().map(|t| t.provider.as_str()),
        ),
        (
            "embeddings",
            cfg.embeddings.as_ref().map(|e| e.provider.as_str()),
        ),
        (
            "search_synthesis",
            cfg.search_synthesis.as_ref().map(|s| s.provider.as_str()),
        ),
        ("vision", cfg.vision.as_ref().map(|v| v.provider.as_str())),
        (
            "scheduled",
            cfg.scheduled.as_ref().map(|s| s.provider.as_str()),
        ),
        (
            "mcp_synthesis",
            cfg.mcp_synthesis.as_ref().map(|s| s.provider.as_str()),
        ),
    ] {
        if sec == Some(name) {
            out.push(format!("[{section}] auxiliary"));
        }
    }
    out
}

/// Effective key env for every section, to decide whether the removed
/// provider's key is still referenced elsewhere.
fn key_envs_in_use(cfg: &Config, except_provider: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |provider: &str, explicit: Option<&str>| {
        if provider != except_provider {
            out.push(config_doc::provider_key_env(
                provider,
                explicit.filter(|s| !s.is_empty()),
            ));
        }
    };
    if let Some(m) = &cfg.model {
        push(&m.provider, m.api_key_env.as_deref());
    }
    if let Some(d) = &cfg.judge {
        push(&d.provider, d.api_key_env.as_deref());
    }
    if let Some(c) = &cfg.compression {
        push(&c.provider, c.api_key_env.as_deref());
    }
    if let Some(t) = &cfg.title_gen {
        push(&t.provider, t.api_key_env.as_deref());
    }
    for sec in [
        cfg.embeddings
            .as_ref()
            .map(|s| (&s.provider, &s.api_key_env)),
        cfg.search_synthesis
            .as_ref()
            .map(|s| (&s.provider, &s.api_key_env)),
        cfg.vision.as_ref().map(|s| (&s.provider, &s.api_key_env)),
        cfg.scheduled
            .as_ref()
            .map(|s| (&s.provider, &s.api_key_env)),
        cfg.mcp_synthesis
            .as_ref()
            .map(|s| (&s.provider, &s.api_key_env)),
    ]
    .into_iter()
    .flatten()
    {
        push(sec.0, sec.1.as_deref());
    }
    for (name, sec) in &cfg.custom_providers {
        if name != except_provider {
            push(name, sec.key_env.as_deref());
        }
    }
    out
}

/// Delete the `[custom_providers.<name>]` row. `delete_key`: `Some(true)` =
/// also drop the key line from `.env`; `Some(false)` = keep it; `None` =
/// ask on a TTY (defaults to keep). Refuses while sections use the provider.
pub(crate) fn remove_custom_provider(
    data_dir: &std::path::Path,
    name: &str,
    delete_key: Option<bool>,
) -> Result<(), String> {
    let mut cfg = Config::load(data_dir).map_err(|e| format!("load config: {e}"))?;
    let sec = cfg
        .custom_providers
        .get(name)
        .ok_or_else(|| format!("no custom provider {name:?} (see `pantheon model --list`)"))?;
    let deps = dependents_of(&cfg, name);
    if !deps.is_empty() {
        return Err(format!(
            "{name:?} is still in use by {} — re-point them first (`pantheon model`)",
            deps.join(", ")
        ));
    }
    let key_env = config_doc::provider_key_env(name, sec.key_env.as_deref());
    let key_orphaned = !key_envs_in_use(&cfg, name).iter().any(|k| k == &key_env);
    cfg.custom_providers.remove(name);
    cfg.save(data_dir).map_err(|e| e.to_string())?;
    let mut note = format!("removed custom provider {name:?}");
    if key_orphaned {
        let drop_it = match delete_key {
            Some(d) => d,
            None => {
                if std::io::stdin().is_terminal() {
                    let a = prompt_line(
                        &format!("key {key_env} is now unused — delete it from .env? (y/N)"),
                        "n",
                    );
                    matches!(a.to_ascii_lowercase().as_str(), "y" | "yes")
                } else {
                    false
                }
            }
        };
        if drop_it {
            match super::dotenv::delete_dotenv_key(data_dir, &key_env) {
                Ok(true) => note.push_str(&format!(" + deleted {key_env} from .env")),
                Ok(false) => note.push_str(&format!(" ({key_env} was not in .env)")),
                Err(e) => note.push_str(&format!(" (could not update .env: {e})")),
            }
        } else {
            note.push_str(&format!(" (kept {key_env} in .env)"));
        }
    }
    println!("{note}");
    Ok(())
}

// ---------------------------------------------------------------------------
// entry
// ---------------------------------------------------------------------------

fn usage() -> ! {
    eprintln!(
        "usage: pantheon model [--auxiliary judge|compression|title_gen|embeddings|search_synthesis|vision|scheduled|mcp_synthesis] [options]"
    );
    eprintln!("       pantheon model --list");
    eprintln!("  interactive (default): provider picker (custom add/remove rows) →");
    eprintln!("    url + wire mode → comma-stacked API keys (→ <data_dir>/.env) →");
    eprintln!("    live /models fetch → model pick → auxiliary step at the bottom");
    eprintln!("  flags (non-interactive):");
    eprintln!(
        "    --provider ID       catalog id or custom name (with --base-url for new endpoints)"
    );
    eprintln!("    --model ID          model id (skips the fetch)");
    eprintln!("    --key K1,K2         raw keys → .env (else keeps existing)");
    eprintln!(
        "    --base-url URL|:port  custom endpoint (stored under provider id; :port = localhost)"
    );
    eprintln!("    --api-mode openai|anthropic   (custom endpoints, default openai)");
    eprintln!("    --api-key-env ENV   key env var (default PANTHEON_KEY_<NAME>)");
    eprintln!(
        "    --set VAR=VAL       endpoint template value, repeatable (region, resource, ...)"
    );
    eprintln!(
        "    --auxiliary KIND    pin judge|compression|title_gen|embeddings|search_synthesis|vision|scheduled|mcp_synthesis instead of default"
    );
    eprintln!("  endpoints: `pantheon provider add|list|remove` (new URLs live there)");
    std::process::exit(2);
}

pub fn cmd_model(args: &[String]) {
    // dotenv + customs so --list key checks and saves see the full picture.
    let dd = data_dir();
    config_doc::init_env_and_catalog(&dd);

    let rest: Vec<String> = args.iter().skip(2).cloned().collect();
    if has_flag(&rest, "--list") {
        cmd_list();
        return;
    }
    if has_flag(&rest, "--remove") || has_flag(&rest, "--delete-key") {
        eprintln!("endpoint management moved to `pantheon provider` (add|list|remove)");
        std::process::exit(2);
    }

    let aux = flag(&rest, "--auxiliary").map(|k| {
        let k = k.trim().to_ascii_lowercase();
        match k.as_str() {
            "judge" | "decision" => "judge".to_string(),
            "compression" => "compression".to_string(),
            "title_gen" | "titlegen" | "title" => "title_gen".to_string(),
            "embeddings" | "embed" => "embeddings".to_string(),
            "search_synthesis" | "search_synth" | "search" => {
                "search_synthesis".to_string()
            }
            "vision" | "images" => "vision".to_string(),
            "scheduled" | "schedule" => "scheduled".to_string(),
            "mcp_synthesis" | "mcp" => "mcp_synthesis".to_string(),
            _ => {
                eprintln!(
                    "unknown --auxiliary {k:?} (judge|compression|title_gen|embeddings|search_synthesis|vision|scheduled|mcp_synthesis)"
                );
                std::process::exit(2);
            }
        }
    });
    let target = match aux {
        Some(k) => Target::Auxiliary(k),
        None => Target::Default,
    };
    let scope: String = aux_section_name(&target).unwrap_or("default").to_string();

    // Non-interactive: --provider + --model (+ optional key/url pieces).
    if let (Some(provider), Some(model)) = (flag(&rest, "--provider"), flag(&rest, "--model")) {
        let provider = provider.trim().to_string();
        let model = model.trim().to_string();
        if provider.is_empty() || model.is_empty() {
            usage();
        }
        let known = catalog::provider(&provider);
        let api_mode = match flag(&rest, "--api-mode").as_deref() {
            None => known
                .as_ref()
                .map(|p| p.api_mode)
                .unwrap_or(ApiMode::OpenAi),
            Some(m) => parse_api_mode(Some(m)),
        };
        let is_custom = known.is_none() || flag(&rest, "--base-url").is_some();
        let base_url = if is_custom {
            match flag(&rest, "--base-url") {
                Some(u) if !u.trim().is_empty() => Some(normalize_base_url(&u)),
                _ => {
                    if known.is_none() {
                        eprintln!(
                            "unknown provider {provider:?}: pass --base-url for custom endpoints"
                        );
                        std::process::exit(2);
                    }
                    None
                }
            }
        } else {
            None
        };
        let key_env = flag(&rest, "--api-key-env").unwrap_or_else(|| {
            config_doc::provider_key_env(&provider, known.as_ref().map(|p| p.key_env.as_str()))
        });
        let raw_keys = flag(&rest, "--key")
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        if is_custom && known.is_some() {
            println!("note: overriding builtin '{provider}' (visible in `pantheon model --list`)");
        }
        // Endpoint template values: persist `--set` pairs, fail closed on
        // anything still missing (the template row itself resolves live).
        let sets = parse_set_pairs(flags(&rest, "--set"));
        let template_base = base_url
            .clone()
            .unwrap_or_else(|| catalog::base_url_for(&provider));
        if let Err(e) = ensure_template_vars(&dd, &provider, &template_base, &sets, false) {
            eprintln!("endpoint config: {e}");
            std::process::exit(2);
        }
        let choice = ModelChoice {
            provider: provider.clone(),
            model: model.clone(),
            base_url,
            api_mode,
            key_env: key_env.clone(),
            raw_keys,
            target,
        };
        match save_choice(&dd, &choice) {
            Ok(()) => println!(
                "{scope} model set: {provider} / {model}  [keys → {}]",
                dd.join(".env").display()
            ),
            Err(e) => {
                eprintln!("model: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if flag(&rest, "--provider").is_some()
        || flag(&rest, "--model").is_some()
        || flag(&rest, "--key").is_some()
        || !flags(&rest, "--set").is_empty()
        || has_flag(&rest, "--yes")
    {
        eprintln!("non-interactive mode needs both --provider and --model");
        usage();
    }

    // Interactive.
    if !std::io::stdin().is_terminal() {
        eprintln!("no TTY: pass --provider + --model (see `pantheon model` usage)");
        std::process::exit(2);
    }
    // `--auxiliary` starts directly at that scope; otherwise the default
    // first, then the auxiliary step lives at the bottom.
    let first_target = target;
    let bottom_aux = matches!(first_target, Target::Default);
    match interactive(first_target) {
        Some(choice) => {
            let summary = format!("{} / {}", choice.provider, choice.model);
            let saved_scope = match &choice.target {
                Target::Default => "default",
                Target::Auxiliary(k) => k.as_str(),
            }
            .to_string();
            match save_choice(&dd, &choice) {
                Ok(()) => println!(
                    "{saved_scope} model set: {summary}  [keys → {}]",
                    dd.join(".env").display()
                ),
                Err(e) => {
                    eprintln!("model: {e}");
                    std::process::exit(1);
                }
            }
            if bottom_aux {
                run_aux_bottom(&dd);
            }
        }
        None => println!("cancelled — nothing changed"),
    }
}

/// Bottom-of-flow auxiliary step: after the default is saved, offer each
/// auxiliary in turn until "done". Cancelling keeps whatever was saved.
fn run_aux_bottom(dd: &std::path::Path) {
    const KINDS: [&str; 7] = [
        "judge",
        "compression",
        "title_gen",
        "embeddings",
        "search_synthesis",
        "vision",
        "scheduled",
    ];
    let descs = [
        "auxiliary judge model (routes + tool gates)",
        "auxiliary compression model",
        "auxiliary session-title model",
        "auxiliary embeddings model (vector search)",
        "auxiliary search-synthesis model",
        "auxiliary vision model",
        "auxiliary model for scheduled runs",
    ];
    let mut items = vec![PickItem {
        label: "Done".into(),
        desc: "keep auxiliaries as they are".into(),
    }];
    items.extend(KINDS.iter().zip(descs).map(|(k, d)| PickItem {
        label: (*k).into(),
        desc: d.into(),
    }));
    loop {
        let Some(idx) = pick("also set an auxiliary? (bottom step)", &items) else {
            println!("auxiliaries unchanged");
            return;
        };
        if idx == 0 {
            return;
        }
        let kind = KINDS[idx - 1].to_string();
        match interactive(Target::Auxiliary(kind.clone())) {
            Some(choice) => {
                let summary = format!("{} / {}", choice.provider, choice.model);
                match save_choice(dd, &choice) {
                    Ok(()) => println!(
                        "{kind} model set: {summary}  [keys → {}]",
                        dd.join(".env").display()
                    ),
                    Err(e) => eprintln!("model: {e}"),
                }
            }
            None => println!("{kind} unchanged"),
        }
    }
}
