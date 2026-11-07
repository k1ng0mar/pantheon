//! model_group: model, model_browser, model_catalog, provider, ollama, setup_providers in one file.

pub mod model {
    //! `pantheon model`: point pantheon at a provider and pick the model.
    //!
    //! Interactive (a TTY picker):
    //!   1. provider list (builtins + customs) - scroll or type to filter,
    //!      Enter to select. New endpoints are created in `pantheon provider
    //!      add`; URL edits that diverge from a builtin are saved as explicit
    //!      override rows.
    //!   2. base URL (prefilled from the catalog, `:port` = localhost) + wire
    //!      mode (OpenAI-compatible vs Anthropic) picker.
    //!   3. API key prompt - comma-separated to stack multiple keys. Stored in
    //!      `<data_dir>/.env` (`PANTHEON_KEY_X=k1,k2`), never in config.toml.
    //!      The chain rotates stacked keys on 401/403/429.
    //!   4. models are fetched live from `{base}/models`; pick one (or type an
    //!      id manually when the endpoint has no `/models`).
    //!   5. bottom step: a deliberate bottom row - `Set auxiliary` opens the
    //!      slot list, `Done` (or Esc) finishes.
    //!
    //! Bare `pantheon model` configures the default chat model;
    //! `pantheon model --auxiliary KIND` pins one auxiliary instead, where
    //! KIND is one of the 14 `AUX_SLOTS` slots (judge, compression, title_gen,
    //! embeddings, search_synthesis, vision, video, scheduled, mcp_synthesis,
    //! extraction, rerank, planner, repair, verify) plus `reflect`,
    //! `consolidation`, and `nightly` (the `[nightly.model]` pin).
    //! `pantheon model --list` shows the current setup.
    //!
    //! In the auxiliary flow the provider list is key-aware: providers with
    //! a key already on file (process env or `<data_dir>/.env`) sort first,
    //! tagged `key on file`, and picking one offers to reuse the stored key
    //! instead of re-prompting.
    //!
    //! Non-interactive (scripts): pass `--provider` + `--model` (and `--key`,
    //! `--base-url`, `--api-mode`, `--api-key-env`, `--remove` as needed).

    use crate::config::{
        self, AuxSection, Config, ConsolidationSection, NightlySection, ReflectSection,
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
        crate::terminal::data_dir()
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
                    // label/desc can come from plugin manifests: strip
                    // control bytes so the SGR highlight below is the only
                    // escape in this line and a stray CSI cannot cancel it
                    // or move the cursor.
                    println!(
                        "  {marker} \x1b[1m{}\x1b[0m  {}\r",
                        crate::sanitize::strip_control(&it.label),
                        crate::sanitize::strip_control(&it.desc)
                    );
                } else {
                    println!(
                        "  {marker} {}  {}\r",
                        crate::sanitize::strip_control(&it.label),
                        crate::sanitize::strip_control(&it.desc)
                    );
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

    /// Parse `--api-mode openai|anthropic|gemini|auto`. Exits 2 on
    /// anything else so both verbs reject the same way. `gemini` rides the
    /// OpenAI wire through Google's compatibility endpoint; `auto` sniffs
    /// the URL's host (needs the base URL, so callers resolve it after
    /// they know the endpoint).
    pub(crate) fn parse_api_mode(raw: Option<&str>) -> ApiMode {
        match raw {
            None => ApiMode::OpenAi,
            Some(m) => match m.trim().to_ascii_lowercase().as_str() {
                "anthropic" => ApiMode::Anthropic,
                "openai" | "openai-compatible" | "gemini" => ApiMode::OpenAi,
                "auto" => ApiMode::OpenAi,
                _ => {
                    eprintln!("unknown --api-mode {m:?} (openai|anthropic|gemini|auto)");
                    std::process::exit(2);
                }
            },
        }
    }

    /// Whether `--api-mode` asked for host-sniffing. Callers that know the
    /// base URL apply [`sniff_wire_mode`] when this is true.
    pub(crate) fn api_mode_is_auto(raw: Option<&str>) -> bool {
        matches!(
            raw.map(|m| m.trim().to_ascii_lowercase()).as_deref(),
            Some("auto")
        )
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
        format!("{head}...{tail}")
    }

    // ---------------------------------------------------------------------------
    // auxiliary slot registry
    // ---------------------------------------------------------------------------

    /// Canonical auxiliary-slot names pinnable via `--auxiliary` and the
    /// bottom step, in presentation order. Driven from [`AUX_SLOTS`] - the
    /// single source of truth in `pantheon_api::config` - plus the three
    /// model pins that live outside it, so the list cannot drift again.
    ///
    /// * `reflect` - the model pin inside the `[reflect]` table;
    /// * `consolidation` - the model pin inside the `[consolidation]` table;
    /// * `nightly` - the `[nightly.model]` pin (pinning it enables the
    ///   nightly pass unless `enabled = false`).
    pub(crate) fn aux_slot_names() -> Vec<&'static str> {
        let mut names: Vec<&'static str> = config::AUX_SLOTS.iter().map(|s| s.name).collect();
        names.extend(["reflect", "consolidation", "nightly"]);
        names
    }

    /// One-line description per slot, from the `AuxiliaryKind` docs. Every
    /// name from [`aux_slot_names`] has an explicit arm; an unknown name
    /// panics loudly naming the valid set - never a silent wrong label.
    pub(crate) fn aux_slot_desc(name: &str) -> &'static str {
        match name {
            "judge" => "auxiliary judge model (route select + tool gates)",
            "compression" => "auxiliary compression model (transcript overflow)",
            "title_gen" => "auxiliary session-title model",
            "embeddings" => "auxiliary embeddings model (vector search)",
            "search_synthesis" => "auxiliary search-synthesis model (cited web briefs)",
            "vision" => "auxiliary vision model (attached-image descriptions)",
            "video" => "auxiliary video-analysis model (native video input, keyframe fallback)",
            "scheduled" => "auxiliary model for scheduled runs",
            "mcp_synthesis" => "auxiliary MCP tool-result synthesis model",
            "extraction" => "auxiliary structured-extraction model",
            "rerank" => "auxiliary rerank model (search + memory candidates)",
            "planner" => "auxiliary planner model (goal decomposition)",
            "repair" => "auxiliary repair model (nightly fix loop)",
            "verify" => "auxiliary adversarial verifier (delegation claims)",
            "reflect" => "reflection-pass model pin ([reflect])",
            "consolidation" => "consolidation model pin ([consolidation])",
            "nightly" => "nightly-pass model ([nightly.model]; pin enables the pass)",
            other => panic!(
                "no description for aux slot {other:?} (valid: {})",
                aux_slot_names().join("|")
            ),
        }
    }

    /// Scope label for the interactive flow's titles. Explicit arms for
    /// every slot; unknown names panic loudly naming the valid set.
    pub(crate) fn aux_scope_label(kind: &str) -> &'static str {
        match kind {
            "judge" => "judge auxiliary",
            "compression" => "compression auxiliary",
            "title_gen" => "title auxiliary",
            "embeddings" => "embeddings auxiliary",
            "search_synthesis" => "search-synthesis auxiliary",
            "vision" => "vision auxiliary",
            "video" => "video auxiliary",
            "scheduled" => "scheduled-run auxiliary",
            "mcp_synthesis" => "MCP-synthesis auxiliary",
            "extraction" => "extraction auxiliary",
            "rerank" => "rerank auxiliary",
            "planner" => "planner auxiliary",
            "repair" => "repair auxiliary",
            "verify" => "verify auxiliary",
            "reflect" => "reflection auxiliary",
            "consolidation" => "consolidation auxiliary",
            "nightly" => "nightly-pass auxiliary",
            other => panic!(
                "unknown auxiliary slot {other:?} (valid: {})",
                aux_slot_names().join("|")
            ),
        }
    }

    // ---------------------------------------------------------------------------
    // key-aware provider ordering (auxiliary flow)
    // ---------------------------------------------------------------------------

    /// One provider row in the aux picker: `stored_key_env` is `Some` when
    /// a key is already on file for this provider (process env or
    /// `<data_dir>/.env`), naming the env var that holds it.
    pub(crate) struct ProviderRow {
        pub meta: catalog::ProviderMeta,
        pub stored_key_env: Option<String>,
    }

    /// Candidate env vars that could hold `provider_id`'s key: the provider
    /// row's own `key_env` first (`PANTHEON_KEY_<NAME>` when the row names
    /// none - mirroring the interactive flow's default), then any
    /// `api_key_env` already recorded in config (`[model]`, an aux section,
    /// or `[custom_providers.*]`) for that provider. Order is stable and
    /// duplicates are dropped.
    pub(crate) fn provider_key_candidates(
        cfg: &Config,
        provider_id: &str,
        meta_key_env: &str,
    ) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |e: &str| {
            let e = e.trim();
            if !e.is_empty() && !out.iter().any(|x| x == e) {
                out.push(e.to_string());
            }
        };
        if meta_key_env.trim().is_empty() {
            push(&format!(
                "PANTHEON_KEY_{}",
                config::sanitize_env_suffix(provider_id)
            ));
        } else {
            push(meta_key_env);
        }
        let mut consider = |provider: &str, env: Option<&str>| {
            if provider == provider_id {
                if let Some(e) = env {
                    push(e);
                }
            }
        };
        if let Some(m) = &cfg.model {
            consider(&m.provider, m.api_key_env.as_deref());
        }
        // Per-task keys are gone: aux pins name only provider/model,
        // the provider key is the only key considered here.
        if let Some(cp) = cfg.custom_providers.get(provider_id) {
            if let Some(e) = cp.key_env.as_deref() {
                push(e);
            }
        }
        out
    }

    /// First candidate env var that actually has a key stored - process
    /// environment or `<data_dir>/.env`. `None` = no key on file.
    pub(crate) fn stored_key_env(
        data_dir: &std::path::Path,
        candidates: &[String],
    ) -> Option<String> {
        candidates
            .iter()
            .find(|e| {
                std::env::var(e)
                    .map(|v| !v.trim().is_empty())
                    .unwrap_or(false)
                    || crate::dotenv::read_dotenv_value(data_dir, e)
                        .map(|v| !v.trim().is_empty())
                        .unwrap_or(false)
            })
            .cloned()
    }

    /// Read the stored key itself (env first, then `.env`); `None` when the
    /// var is unset or blank. Values are never written to config - only the
    /// var name is saved as `api_key_env`.
    fn read_stored_key(data_dir: &std::path::Path, env: &str) -> Option<String> {
        std::env::var(env)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                crate::dotenv::read_dotenv_value(data_dir, env).filter(|v| !v.trim().is_empty())
            })
    }

    /// Order provider rows for the aux picker: providers with a key on
    /// file first (tagged `key on file`), then the rest in catalog order
    /// (recommended first - `all_providers` already yields that order).
    pub(crate) fn order_providers_key_first(
        data_dir: &std::path::Path,
        cfg: &Config,
        providers: Vec<catalog::ProviderMeta>,
    ) -> Vec<ProviderRow> {
        let mut with_key = Vec::new();
        let mut rest = Vec::new();
        for meta in providers {
            let stored = stored_key_env(
                data_dir,
                &provider_key_candidates(cfg, &meta.id, &meta.key_env),
            );
            let row = ProviderRow {
                meta,
                stored_key_env: stored,
            };
            if row.stored_key_env.is_some() {
                with_key.push(row);
            } else {
                rest.push(row);
            }
        }
        with_key.extend(rest);
        with_key
    }

    /// The outcome of the interactive key step.
    pub(crate) struct KeySelection {
        /// Env var that will hold (or already holds) the key - the value
        /// saved as `api_key_env`, never the key itself.
        pub key_env: String,
        /// Freshly entered keys to persist to `.env` (`None` = keep stored).
        pub raw_keys: Option<String>,
        /// Currently stored keys, for the live `/models` fetch.
        pub existing: Option<String>,
    }

    /// Step 3 of the interactive flow, factored for tests. When a key is
    /// already on file for the provider, offer `use the stored key (ENV)?`
    /// first; "no" (or no stored key) runs the normal key prompts.
    /// `prompt` is injectable so tests can stub it.
    fn select_keys(
        prompt: &dyn Fn(&str, &str) -> String,
        dd: &std::path::Path,
        key_env_default: &str,
        stored: Option<&str>,
    ) -> KeySelection {
        if let Some(env) = stored {
            let answer = prompt(&format!("use the stored key ({env})?"), "y");
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return KeySelection {
                    key_env: env.to_string(),
                    raw_keys: None,
                    existing: read_stored_key(dd, env),
                };
            }
        }
        // No name prompt: the standard name is the answer (custom names via
        // `--api-key-env`). The value prompt below is the only question.
        let key_env = key_env_default.to_string();
        let existing =
            crate::dotenv::read_dotenv_value(dd, &key_env).filter(|v| !v.trim().is_empty());
        if let Some(cur) = &existing {
            println!("  current: {} (empty keeps)", mask_key(cur));
        }
        let keys = prompt("API key(s), comma-separated to stack", "");
        // Empty input keeps an existing key, or means keyless when none exists.
        if keys.trim().is_empty() && existing.is_none() {
            println!("  no key stored - keyless endpoints only");
        }
        let raw_keys = if keys.trim().is_empty() {
            None
        } else {
            Some(keys.trim().to_string())
        };
        KeySelection {
            key_env,
            raw_keys,
            existing,
        }
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
    /// Cached model list, keyed by provider identity (base_url + key hash).
    struct DiscoveryCache {
        key: String,
        etag: Option<String>,
        ids: Vec<String>,
        fetched_at: std::time::Instant,
    }

    static DISCOVERY: std::sync::Mutex<Option<DiscoveryCache>> = std::sync::Mutex::new(None);

    const DISCOVERY_TTL: std::time::Duration = std::time::Duration::from_secs(300);

    fn cache_key(base_url: &str, key: &str) -> String {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        base_url.hash(&mut h);
        key.hash(&mut h);
        format!("{:x}", h.finish())
    }

    pub fn fetch_models(
        base_url: &str,
        key: &str,
        mode: pantheon_providers::catalog::ApiMode,
        provider_id: &str,
    ) -> Result<Vec<String>, String> {
        let base = trim_slash(base_url.trim());
        let url = format!("{base}/models");
        let first = first_key(key);
        let ck = cache_key(base, first);

        // Check cache first: key must match and TTL must not have expired.
        if let Ok(cache) = DISCOVERY.lock() {
            if let Some(ref c) = *cache {
                if c.key == ck && c.fetched_at.elapsed() < DISCOVERY_TTL && !c.ids.is_empty() {
                    return Ok(c.ids.clone());
                }
            }
        }

        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let mut req = agent.get(&url);
        if !first.is_empty() {
            match mode {
                pantheon_providers::catalog::ApiMode::Anthropic => {
                    req = req
                        .header("x-api-key", first)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                }
                pantheon_providers::catalog::ApiMode::Gemini => {
                    req = req.header("x-goog-api-key", first);
                }
                pantheon_providers::catalog::ApiMode::OpenAi => {
                    let header = pantheon_providers::catalog::key_header_for(provider_id);
                    if header == "Authorization" {
                        req = req.header("Authorization", format!("Bearer {first}").as_str());
                    } else {
                        req = req.header(header.as_str(), first);
                    }
                }
            }
        }
        // Send If-None-Match only for this key's ETag: the cache is
        // global, and honouring a previous key's ETag let a 304 serve
        // another key's model list (the 304 path below returns the cached
        // ids without re-checking whose they are).
        if let Ok(cache) = DISCOVERY.lock() {
            if let Some(ref c) = *cache {
                if c.key == ck {
                    if let Some(ref etag) = c.etag {
                        req = req.header("If-None-Match", etag.as_str());
                    }
                }
            }
        }
        let mut resp = req.call().map_err(|e| format!("{url}: {e}"))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            let snippet: String = body.chars().take(160).collect();
            return Err(format!("HTTP {status} from {url}: {snippet}"));
        }
        // 304 Not Modified: keep cached list, but only when the cached
        // entry belongs to this key (defensive twin of the If-None-Match
        // guard above; a mismatched 304 must never serve another key's
        // list, so it falls through to an empty-body parse error instead
        // of returning foreign ids).
        if resp.status() == 304 {
            if let Ok(cache) = DISCOVERY.lock() {
                if let Some(ref c) = *cache {
                    if c.key == ck {
                        drop(cache);
                        if let Ok(mut cache) = DISCOVERY.lock() {
                            if let Some(ref mut c) = *cache {
                                c.fetched_at = std::time::Instant::now();
                            }
                        }
                        if let Ok(cache) = DISCOVERY.lock() {
                            if let Some(ref c) = *cache {
                                return Ok(c.ids.clone());
                            }
                        }
                    }
                }
            }
        }
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp
            .into_body()
            .read_to_string()
            .map_err(|e| format!("read {url}: {e}"))?;
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
        // Update cache.
        if let Ok(mut cache) = DISCOVERY.lock() {
            *cache = Some(DiscoveryCache {
                key: ck.clone(),
                etag,
                ids: ids.clone(),
                fetched_at: std::time::Instant::now(),
            });
        }
        let _ = &ck;
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
                    crate::dotenv::read_dotenv_value(data_dir, &env_name)
                        .filter(|s| !s.trim().is_empty())
                });
            if let Some((_, val)) = preset.iter().find(|(k, _)| k == &env_name || k == v) {
                crate::dotenv::persist_dotenv_value(data_dir, &env_name, val.trim())
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
                        crate::dotenv::persist_dotenv_value(data_dir, &env_name, answer.trim())
                            .map_err(|e| format!("write .env: {e}"))?;
                        break;
                    }
                    tries += 1;
                    if tries >= 3 {
                        return Err(format!("{v} is required - re-run and provide it"));
                    }
                    eprintln!("  {v} is required - the endpoint URL contains {{{v}}}");
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
    /// Pin an auxiliary slot to the chosen provider/model. Every slot has
    /// an explicit arm; an unknown name is a loud error naming the valid
    /// set - never a silent write to the wrong section.
    ///
    /// The 14 [`AUX_SLOTS`] slots are plain `[section]` tables and are
    /// replaced wholesale (like before). `reflect` / `consolidation` mix
    /// behavior knobs with the model pin, so the pin fields are updated in
    /// place and the knobs (`enabled`, `auto_turns`, `cron`, ...) survive.
    /// `nightly` writes the `[nightly.model]` sub-table; a fresh `[nightly]`
    /// comes from its `Default` impl (off unless the pin is present).
    fn pin_aux_slot(cfg: &mut Config, kind: &str, choice: &ModelChoice) -> Result<(), String> {
        // A re-pin replaces provider/model/key, never the timeout the
        // operator set: the old `timeout: None` literal reset every aux
        // timeout on every model pick.
        let pin = |prev_timeout: Option<u64>| AuxSection {
            provider: choice.provider.clone(),
            model: choice.model.clone(),
            timeout: prev_timeout,
        };
        match kind {
            "judge" => {
                let prev = cfg.judge.as_ref().and_then(|s| s.timeout);
                cfg.judge = Some(pin(prev));
            }
            "compression" => {
                // Pinning replaces the model pin; a previously configured
                // target_percent survives the re-pin.
                let target_percent = cfg.compression.as_ref().and_then(|s| s.target_percent);
                let prev = cfg.compression.as_ref().and_then(|s| s.aux.timeout);
                cfg.compression = Some(config::CompressionSection {
                    aux: pin(prev),
                    target_percent,
                })
            }
            "title_gen" => {
                let prev = cfg.title_gen.as_ref().and_then(|s| s.timeout);
                cfg.title_gen = Some(pin(prev));
            }
            "embeddings" => {
                let prev = cfg.embeddings.as_ref().and_then(|s| s.timeout);
                cfg.embeddings = Some(pin(prev));
            }
            "search_synthesis" => {
                let prev = cfg.search_synthesis.as_ref().and_then(|s| s.timeout);
                cfg.search_synthesis = Some(pin(prev));
            }
            "vision" => {
                let prev = cfg.vision.as_ref().and_then(|s| s.timeout);
                cfg.vision = Some(pin(prev));
            }
            "video" => {
                let prev = cfg.video.as_ref().and_then(|s| s.timeout);
                cfg.video = Some(pin(prev));
            }
            "scheduled" => {
                let prev = cfg.scheduled.as_ref().and_then(|s| s.timeout);
                cfg.scheduled = Some(pin(prev));
            }
            "mcp_synthesis" => {
                let prev = cfg.mcp_synthesis.as_ref().and_then(|s| s.timeout);
                cfg.mcp_synthesis = Some(pin(prev));
            }
            "extraction" => {
                let prev = cfg.extraction.as_ref().and_then(|s| s.timeout);
                cfg.extraction = Some(pin(prev));
            }
            "rerank" => {
                let prev = cfg.rerank.as_ref().and_then(|s| s.timeout);
                cfg.rerank = Some(pin(prev));
            }
            "planner" => {
                let prev = cfg.planner.as_ref().and_then(|s| s.timeout);
                cfg.planner = Some(pin(prev));
            }
            "repair" => {
                // Pinning replaces the model pin; a previously configured
                // max_repairs_per_task_per_day survives the re-pin.
                let mut r = cfg.repair.clone().unwrap_or_default();
                let prev = r.aux.timeout;
                r.aux = pin(prev);
                cfg.repair = Some(r);
            }
            "verify" => {
                let prev = cfg.verify.as_ref().and_then(|s| s.timeout);
                cfg.verify = Some(pin(prev));
            }
            "reflect" => {
                let mut r = cfg.reflect.clone().unwrap_or(ReflectSection {
                    // Documented serde defaults (config.rs); doctor.rs uses
                    // the same values. Only used when no [reflect] table
                    // exists yet - an existing table is updated in place.
                    enabled: false,
                    auto_turns: 20,
                    max_proposals: 5,
                    provider: None,
                    model: None,
                    timeout: None,
                });
                r.provider = Some(choice.provider.clone());
                r.model = Some(choice.model.clone());
                // Timeout survives the re-pin (see `pin_aux_slot`).
                cfg.reflect = Some(r);
            }
            "consolidation" => {
                let mut c = cfg.consolidation.clone().unwrap_or(ConsolidationSection {
                    enabled: false,
                    half_life_days: 14.0,
                    min_sessions: 3,
                    min_score: 2.0,
                    cron: "0 3 * * *".to_string(),
                    provider: None,
                    model: None,
                    timeout: None,
                });
                c.provider = Some(choice.provider.clone());
                c.model = Some(choice.model.clone());
                // Timeout survives the re-pin (see `pin_aux_slot`).
                cfg.consolidation = Some(c);
            }
            "nightly" => {
                let prev = cfg
                    .nightly
                    .as_ref()
                    .and_then(|n| n.model.as_ref())
                    .and_then(|m| m.timeout);
                cfg.nightly
                    .get_or_insert_with(NightlySection::default)
                    .model = Some(pin(prev));
            }
            other => {
                return Err(format!(
                    "unknown auxiliary slot {other:?} (valid: {})",
                    aux_slot_names().join("|")
                ));
            }
        }
        Ok(())
    }

    /// Persist the choice: `.env` keys, `[custom_providers.*]` row for custom
    /// endpoints, and the `[model]` / aux section. Registers customs in-memory.
    pub fn save_choice(data_dir: &std::path::Path, choice: &ModelChoice) -> Result<(), String> {
        // Ollama gate: a local provider must be reachable with at least one
        // model and enough RAM before it can become the default. This keeps
        // a dead local endpoint from being saved as the default by accident.
        if choice.provider == "local" || choice.provider == "ollama" {
            let base = choice
                .base_url
                .clone()
                .unwrap_or_else(|| "http://127.0.0.1:11434/v1".to_string());
            let report = crate::ollama::check_local_models(&base);
            if !report.ready {
                return Err(format!(
                    "local provider not ready: reachable={} models={} ram={:?} GiB, need at least {} GiB and one pulled model",
                    report.reachable,
                    report.models.len(),
                    report.ram_gib,
                    crate::ollama::MIN_RAM_GIB
                ));
            }
            // Verified tool call gate: reachable hardware plus a live
            // tool call shape before a local provider can be the default.
            let model_name = choice.model.clone();
            if !crate::ollama::verified_tool_call(&base, &model_name) {
                return Err(format!(
                    "local provider not verified: model {model_name} did not return a tool call on {base}"
                ));
            }
        }
        if let Some(keys) = &choice.raw_keys {
            crate::dotenv::persist_dotenv_value(data_dir, &choice.key_env, keys.trim())
                .map_err(|e| format!("write .env: {e}"))?;
        }
        if let Some(base) = &choice.base_url {
            config::upsert_custom_row(
                data_dir,
                &choice.provider,
                base,
                choice.api_mode,
                &choice.key_env,
            )?;
        }
        // Reload: the row writer above persisted + registered already.
        // Fail closed on a corrupt config: `unwrap_or_default` here used to
        // overwrite a malformed config.toml with defaults plus one model.
        let mut cfg = Config::load_for_save(data_dir).map_err(|e| e.to_string())?;
        match &choice.target {
            Target::Default => {
                let prev = cfg.model.clone();
                let fallbacks = prev
                    .as_ref()
                    .map(|m| m.fallbacks.clone())
                    .unwrap_or_default();
                cfg.model = Some(config::ModelSection {
                    // A model pick does not touch reasoning or the timeout:
                    // changing providers must not silently reset an effort
                    // level or a timeout the user chose. (The old literal
                    // wrote `reasoning_budget: None` / `timeout_ms: None`,
                    // resetting both on every pick.)
                    reasoning_budget: prev.as_ref().and_then(|m| m.reasoning_budget),
                    provider: choice.provider.clone(),
                    model: choice.model.clone(),
                    api_key_env: Some(choice.key_env.clone()),
                    fallbacks,
                    reasoning: prev.as_ref().and_then(|m| m.reasoning.clone()),
                    timeout_ms: prev.as_ref().and_then(|m| m.timeout_ms),
                });
            }
            Target::Auxiliary(kind) => pin_aux_slot(&mut cfg, kind.as_str(), choice)?,
        }
        cfg.save(data_dir).map_err(|e| e.to_string())?;
        config::register_custom_providers(&cfg);
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
                _ => match crate::dotenv::read_dotenv_value(&data_dir(), e) {
                    Some(v) if !v.trim().is_empty() => format!("{e} set in .env"),
                    _ => format!("{e} NOT SET"),
                },
            },
        }
    }

    /// Display triple for an optional model pin (`[reflect]`,
    /// `[consolidation]`, `[nightly.model]`): `None` when nothing is pinned
    /// (reads as "auto"), otherwise provider/model/key-env.
    fn pin_triple(
        provider: Option<String>,
        model: Option<String>,
        key_env: Option<String>,
    ) -> Option<(String, String, Option<String>)> {
        let p = provider.unwrap_or_default();
        let m = model.unwrap_or_default();
        if p.trim().is_empty() && m.trim().is_empty() {
            None
        } else {
            Some((p, m, key_env))
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
            None => println!("default:   (unset - run `pantheon model`)"),
        }
        let aux = |name: &str, sec: Option<(String, String, Option<String>)>| match sec {
            Some((p, m, k)) => println!("{name:<16} {p} / {m}  [{}]", key_status(k.as_deref())),
            None => println!("{name:<16} auto (default model)"),
        };
        // Every AUX_SLOTS section, read through the slot table so the list
        // cannot drift from the registry.
        for slot in config::AUX_SLOTS {
            let sec = (slot.section)(&cfg);
            aux(
                slot.name,
                sec.map(|s| (s.provider.clone(), s.model.clone(), None)),
            );
        }
        // The three pins that live outside AUX_SLOTS. `[reflect]` /
        // `[consolidation]` pins are optional fields (absent = auto);
        // `[nightly.model]` is a sub-table.
        aux(
            "reflect",
            cfg.reflect.as_ref().and_then(|r| {
                pin_triple(r.provider.clone(), r.model.clone(), None)
            }),
        );
        aux(
            "consolidation",
            cfg.consolidation.as_ref().and_then(|c| {
                pin_triple(c.provider.clone(), c.model.clone(), None)
            }),
        );
        aux(
            "nightly",
            cfg.nightly
                .as_ref()
                .and_then(|n| n.model.as_ref())
                .and_then(|m| {
                    pin_triple(
                        Some(m.provider.clone()),
                        Some(m.model.clone()),
                        None,
                    )
                }),
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
                        config::sanitize_env_suffix(n)
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
                    || crate::dotenv::read_dotenv_value(&data_dir(), &e)
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
                label: "Auto-detect".into(),
                desc: "Anthropic host → /messages, otherwise /chat/completions".into(),
            },
            PickItem {
                label: "OpenAI-compatible".into(),
                desc: "/chat/completions".into(),
            },
            PickItem {
                label: "Anthropic".into(),
                desc: "/messages".into(),
            },
            PickItem {
                label: "Gemini".into(),
                desc: "Google's OpenAI-compatible endpoint".into(),
            },
        ]
    }

    /// Guess the wire mode from the base URL's host. Anthropic hosts speak
    /// `/messages`; everything else is assumed OpenAI-compatible (that
    /// includes Google's `.../v1beta/openai` endpoint and all aggregators).
    /// A heuristic for the "Auto-detect" picker row - an explicit pick
    /// always wins over this.
    pub(crate) fn sniff_wire_mode(base_url: &str) -> ApiMode {
        let host = base_url
            .split("://")
            .nth(1)
            .unwrap_or(base_url)
            .split('/')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if host.contains("anthropic") {
            ApiMode::Anthropic
        } else {
            ApiMode::OpenAi
        }
    }

    /// Map a wire-mode picker index onto [`ApiMode`]. `auto_idx` is the
    /// Auto-detect row: it sniffs the URL. Gemini rides the OpenAI wire
    /// through Google's compatibility endpoint - there is no separate
    /// native-Gemini transport, so picking it never writes a mode the
    /// runtime cannot honor.
    pub(crate) fn resolve_wire_mode(idx: usize, base_url: &str) -> ApiMode {
        match idx {
            0 => sniff_wire_mode(base_url),
            2 => ApiMode::Anthropic,
            _ => ApiMode::OpenAi,
        }
    }

    /// The full interactive flow for one scope: provider → base URL + wire
    /// mode → API key → live `/models` fetch → model pick. The auxiliary
    /// flow reuses this exact path (never a degraded variant); only the
    /// provider list differs - it is key-aware for auxiliaries (providers
    /// with a key on file sort first, tagged `key on file`, and offer to
    /// reuse the stored key instead of re-prompting).
    ///
    /// `prompt` is the line prompter (`prompt_line` in production, a stub in
    /// tests).
    fn interactive(target: Target, prompt: &dyn Fn(&str, &str) -> String) -> Option<ModelChoice> {
        let scope = match &target {
            Target::Default => "default model",
            Target::Auxiliary(k) => aux_scope_label(k),
        };
        let dd = data_dir();

        // 1. provider: pure selection. Endpoint management (add/remove)
        // lives in `pantheon provider`; customs created there appear here.
        // Returns the catalog entry, whether the id already has a custom
        // row, and (aux flow) the stored-key env var when one is on file.
        let (known, was_custom, stored_key_env): (
            Option<pantheon_providers::catalog::ProviderMeta>,
            bool,
            Option<String>,
        ) = {
            let cfg = Config::load(&dd).unwrap_or_default();
            let rows: Vec<ProviderRow> = match &target {
                Target::Auxiliary(_) => {
                    order_providers_key_first(&dd, &cfg, catalog::all_providers())
                }
                // Key-aware ordering + status tags: providers that already have
                // a key locally rise to the top, instead of
                // showing a flat `needs [ENV]` wall.
                Target::Default => order_providers_key_first(&dd, &cfg, catalog::all_providers()),
            };
            let customs: Vec<String> = {
                let mut n: Vec<String> = cfg.custom_providers.keys().cloned().collect();
                n.sort();
                n
            };
            let items: Vec<PickItem> = rows
                .iter()
                .map(|row| {
                    let p = &row.meta;
                    let n = if p.models.is_empty() {
                        "any model id".into()
                    } else {
                        format!("{} models", p.models.len())
                    };
                    let status = if row.stored_key_env.is_some() {
                        "● connected"
                    } else if matches!(
                        p.id.as_str(),
                        "local" | "ollama" | "lmstudio" | "ollama_cloud"
                    ) {
                        "● local"
                    } else {
                        "○ needs key"
                    };
                    PickItem {
                        label: format!("{} ({})", p.label, p.id),
                        desc: format!("{} · {} · {} · {status}", p.base_url, p.tag, n),
                    }
                })
                .collect();
            let prov_idx = pick(
                &format!("pantheon model - {scope}: pick a provider"),
                &items,
            )?;
            if prov_idx >= rows.len() {
                // Defensive: rows are exactly the providers today.
                return None;
            }
            let row = &rows[prov_idx];
            let was_custom = customs.iter().any(|c| c == &row.meta.id);
            (
                Some(row.meta.clone()),
                was_custom,
                row.stored_key_env.clone(),
            )
        };

        // 2. base URL + wire mode + provider id. New endpoints are created
        // in `pantheon provider add`; here every row resolves to a catalog
        // entry (builtin or custom).
        let Some(p) = known else {
            // Unreachable: step 1 always breaks with a selection.
            return None;
        };
        let (provider_id, base_url, api_mode, key_env_default, is_custom) = {
            let url = normalize_base_url(&prompt("Base URL or :port", &p.base_url));
            // Untouched preset: the catalog mode is authoritative, no
            // question. Edited URLs and customs still get the picker (Auto
            // first), since the wire is genuinely unknown there.
            let overridden_or_custom = was_custom || url != p.base_url;
            let mode = if !overridden_or_custom {
                p.api_mode
            } else {
                let mode_items = wire_mode_items();
                println!("Wire mode [auto]:");
                let mode_idx = pick("wire mode", &mode_items).unwrap_or(0);
                if mode_idx == 0 {
                    // Auto: sniff the (possibly edited) URL, so a pasted
                    // Anthropic endpoint does not silently get the OpenAI wire.
                    sniff_wire_mode(&url)
                } else {
                    resolve_wire_mode(mode_idx, &url)
                }
            };
            let key_env = if p.key_env.is_empty() {
                format!("PANTHEON_KEY_{}", config::sanitize_env_suffix(&p.id))
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
            (p.id, url, mode, key_env, was_custom || overridden)
        };

        // 3. keys → .env. A provider with a key on file offers to reuse the
        // stored key instead of re-prompting; "no" falls through to the
        // normal prompts and overwrites.
        let stored = stored_key_env.as_deref();
        let ks = select_keys(prompt, &dd, &key_env_default, stored);
        let key_env = ks.key_env;
        let raw_keys = ks.raw_keys;
        let effective_keys = raw_keys.clone().or(ks.existing).unwrap_or_default();

        // 3b. endpoint template values (azure resource, bedrock region, ...).
        // Resolves the URL the model fetch below calls; the stored row keeps
        // the template so values stay live in .env.
        let fetch_base = match ensure_template_vars(&data_dir(), &provider_id, &base_url, &[], true)
        {
            Ok(u) => u,
            Err(e) => {
                eprintln!("endpoint config: {e}");
                std::process::exit(2);
            }
        };

        // 4. fetch + pick model.
        // Curated rows carry context windows and costs, so the table can
        // show metadata even when the endpoint's /models reply is sparse.
        // Curated entries win on metadata; live-only ids keep live pricing.
        let curated_rows: Vec<crate::model_catalog::CatalogModel> = catalog::provider(&provider_id)
            .map(|p| {
                p.models
                    .iter()
                    .map(|m| crate::model_catalog::CatalogModel {
                        id: m.model.clone(),
                        context_limit: m.context_limit.map(u64::from),
                        input_per_mtok_usd: m.cost.input_per_mtok_usd,
                        output_per_mtok_usd: m.cost.output_per_mtok_usd,
                        vision: Some(m.vision),
                        reasoning: Some(m.reasoning),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let live = if effective_keys.is_empty() {
            crate::model_catalog::fetch_live_models(&fetch_base)
        } else {
            crate::model_catalog::fetch_live_models_keyed(&fetch_base, &effective_keys, api_mode)
        };
        let mut model_rows: Vec<crate::model_catalog::ModelRow> = match live {
            Ok(live) => {
                println!("  fetched {} models from {fetch_base}", live.len());
                crate::model_catalog::merge_model_rows(&curated_rows, &live)
            }
            Err(e) => {
                eprintln!("  fetch failed: {e:?}");
                if !curated_rows.is_empty() {
                    println!("  falling back to {} cataloged models", curated_rows.len());
                }
                curated_rows
                    .iter()
                    .cloned()
                    .map(|c| crate::model_catalog::ModelRow {
                        id: c.id,
                        context_limit: c.context_limit,
                        input_per_mtok_usd: c.input_per_mtok_usd,
                        output_per_mtok_usd: c.output_per_mtok_usd,
                        curated: true,
                        vision: c.vision,
                        reasoning: c.reasoning,
                    })
                    .collect()
            }
        };
        // Popular-first, not alphabetical: curated rows (catalog order is the
        // tested order) lead, then live-only ids in endpoint order. The
        // manual entry stays last so it never moves under the cursor.
        model_rows.sort_by_key(|r| !r.curated);
        let manual = "Add custom model...".to_string();
        let manual_idx = model_rows.len();
        model_rows.push(crate::model_catalog::ModelRow {
            id: manual.clone(),
            context_limit: None,
            input_per_mtok_usd: None,
            output_per_mtok_usd: None,
            curated: false,
            vision: None,
            reasoning: None,
        });
        let browse = crate::model_browser::browse(
            &format!("pick a model on {provider_id}"),
            &model_rows,
            Some(manual_idx),
        )?;
        let model_idx = browse.index;
        match browse.action {
            crate::model_browser::ModelAction::SetDefault => {
                let choice = ModelChoice {
                    provider: provider_id.clone(),
                    model: model_rows[model_idx].id.clone(),
                    base_url: if is_custom {
                        Some(base_url.clone())
                    } else {
                        None
                    },
                    api_mode,
                    key_env: key_env.clone(),
                    raw_keys: raw_keys.clone(),
                    target: match &target {
                        Target::Default => Target::Default,
                        Target::Auxiliary(k) => Target::Auxiliary(k.clone()),
                    },
                };
                match crate::model::save_choice(&data_dir(), &choice) {
                    Ok(()) => println!("  set {} on {} as default", choice.model, choice.provider),
                    Err(e) => eprintln!("  could not save default: {e}"),
                }
            }
            crate::model_browser::ModelAction::Use => println!("  selected for this run"),
            crate::model_browser::ModelAction::Test => {
                let test_model = model_rows[model_idx].id.clone();
                println!("  testing {test_model} on {provider_id}...");
                let key = effective_keys.clone();
                match crate::model::fetch_models(&base_url, &key, api_mode, &provider_id) {
                    Ok(models) => {
                        if models.iter().any(|m| m == &test_model) {
                            println!("  connection ok ({test_model} found)");
                        } else {
                            println!(
                                "  connection ok ({test_model} not in {} models)",
                                models.len()
                            );
                        }
                    }
                    Err(e) => eprintln!("  connection failed: {e}"),
                }
            }
        }
        // A hand-typed id is the operator telling us what this endpoint serves, so
        // it gets written down. A picked-from-fetch id does not: it came from the
        // endpoint and will be fetched again next time.
        let manual_typed = model_idx == manual_idx;
        let model = if manual_typed {
            loop {
                let m = prompt("Model id", "");
                if !m.is_empty() {
                    break m;
                }
            }
        } else {
            model_rows[model_idx].id.clone()
        };
        if manual_typed && is_custom {
            match config::remember_custom_model(&data_dir(), &provider_id, &model) {
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
                cfg.compression.as_ref().map(|c| c.aux.provider.as_str()),
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
            ("video", cfg.video.as_ref().map(|v| v.provider.as_str())),
            (
                "scheduled",
                cfg.scheduled.as_ref().map(|s| s.provider.as_str()),
            ),
            (
                "mcp_synthesis",
                cfg.mcp_synthesis.as_ref().map(|s| s.provider.as_str()),
            ),
            (
                "extraction",
                cfg.extraction.as_ref().map(|e| e.provider.as_str()),
            ),
            ("rerank", cfg.rerank.as_ref().map(|r| r.provider.as_str())),
            ("planner", cfg.planner.as_ref().map(|p| p.provider.as_str())),
            ("repair", cfg.repair.as_ref().map(|r| r.provider.as_str())),
            ("verify", cfg.verify.as_ref().map(|v| v.provider.as_str())),
            (
                "reflect",
                cfg.reflect.as_ref().and_then(|r| r.provider.as_deref()),
            ),
            (
                "consolidation",
                cfg.consolidation
                    .as_ref()
                    .and_then(|c| c.provider.as_deref()),
            ),
            (
                "nightly.model",
                cfg.nightly
                    .as_ref()
                    .and_then(|n| n.model.as_ref())
                    .map(|m| m.provider.as_str()),
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
                out.push(config::provider_key_env(
                    provider,
                    explicit.filter(|s| !s.is_empty()),
                ));
            }
        };
        if let Some(m) = &cfg.model {
            push(&m.provider, m.api_key_env.as_deref());
        }
        if let Some(d) = &cfg.judge {
            push(&d.provider, None);
        }
        if let Some(c) = &cfg.compression {
            push(&c.aux.provider, None);
        }
        if let Some(t) = &cfg.title_gen {
            push(&t.provider, None);
        }
        for sec in [
            // (provider, explicit key env) - per-task keys are gone, so
            // explicit is always None and the provider key resolves.
            cfg.embeddings
                .as_ref()
                .map(|s| (s.provider.as_str(), None::<&str>)),
            cfg.search_synthesis
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.vision
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.video
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.scheduled
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.mcp_synthesis
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.extraction
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.rerank
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.planner
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.repair
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.verify
                .as_ref()
                .map(|s| (s.provider.as_str(), None)),
            cfg.reflect
                .as_ref()
                .map(|s| (s.provider.as_deref().unwrap_or(""), None)),
            cfg.consolidation
                .as_ref()
                .map(|s| (s.provider.as_deref().unwrap_or(""), None)),
            cfg.nightly
                .as_ref()
                .and_then(|n| n.model.as_ref())
                .map(|s| (s.provider.as_str(), None)),
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
                "{name:?} is still in use by {} - re-point them first (`pantheon model`)",
                deps.join(", ")
            ));
        }
        let key_env = config::provider_key_env(name, sec.key_env.as_deref());
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
                            &format!("key {key_env} is now unused - delete it from .env? (y/N)"),
                            "n",
                        );
                        matches!(a.to_ascii_lowercase().as_str(), "y" | "yes")
                    } else {
                        false
                    }
                }
            };
            if drop_it {
                match crate::dotenv::delete_dotenv_key(data_dir, &key_env) {
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

    fn print_usage() {
        eprintln!(
            "usage: pantheon model [--auxiliary {}] [options]",
            aux_slot_names().join("|")
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
            "    --auxiliary KIND    pin one auxiliary instead of default ({});",
            aux_slot_names().join("|")
        );
        eprintln!(
            "                      the aux flow reuses the interactive path and is key-aware:"
        );
        eprintln!(
            "                      providers with a key on file sort first (tagged `key on file`)"
        );
        eprintln!("  endpoints: `pantheon provider add|list|remove` (new URLs live there)");
    }

    /// Usage error: print the usage text and exit 2 (wrong flags, missing
    /// values). `--help` instead exits 0 via [`model_help`].
    fn usage() -> ! {
        print_usage();
        std::process::exit(2);
    }

    /// `--help` short-circuit: print the usage text and exit 0 before
    /// anything else runs. The interactive provider picker is the default
    /// path, so without this `--help` would prompt - help must never
    /// prompt or mutate.
    fn model_help() -> ! {
        print_usage();
        std::process::exit(0);
    }

    pub fn cmd_model(args: &[String]) {
        let raw: &[String] = &args[1.min(args.len())..];
        if raw.iter().any(|a| a == "--help" || a == "-h") {
            model_help();
        }
        // Unknown flags used to fall through to the interactive picker
        // (on a TTY) or to a confusing "needs both" error: `--model x
        // --bogus y` never told the operator the flag was dropped.
        // Value flags take their value as the next token, exactly like
        // `flag()` reads them, so a value is never mistaken for a flag.
        // The `--flag=value` form is not read by `flag()` at all, so it
        // is rejected loudly instead of being silently ignored.
        {
            let rest: Vec<String> = args.iter().skip(2).cloned().collect();
            let value_flags = [
                "--auxiliary",
                "--provider",
                "--model",
                "--key",
                "--base-url",
                "--api-mode",
                "--api-key-env",
                "--set",
            ];
            let bool_flags = ["--list", "--remove", "--delete-key", "--yes"];
            let mut j = 0;
            while j < rest.len() {
                let tok = rest[j].as_str();
                let base = tok.split('=').next().unwrap_or(tok);
                if value_flags.contains(&base) {
                    if tok.contains('=') {
                        eprintln!("model: {base}=... is not supported; use `{base} VALUE`");
                        std::process::exit(2);
                    }
                    j += 1; // skip the value token, whatever it looks like
                } else if bool_flags.contains(&base) {
                    // Boolean switch; nothing to skip.
                } else if tok.starts_with('-') {
                    eprintln!("model: unknown flag '{tok}' (see `pantheon model --help`)");
                    std::process::exit(2);
                }
                j += 1;
            }
        }
        // dotenv + customs so --list key checks and saves see the full picture.
        let dd = data_dir();
        config::init_env_and_catalog(&dd);

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
                "search_synthesis" | "search_synth" | "search" => "search_synthesis".to_string(),
                "vision" | "images" => "vision".to_string(),
                "video" => "video".to_string(),
                "scheduled" | "schedule" => "scheduled".to_string(),
                "mcp_synthesis" | "mcp_synth" | "mcp" => "mcp_synthesis".to_string(),
                "extraction" | "extract" => "extraction".to_string(),
                "rerank" => "rerank".to_string(),
                "planner" | "plan" => "planner".to_string(),
                "repair" => "repair".to_string(),
                "verify" => "verify".to_string(),
                "reflect" | "reflection" => "reflect".to_string(),
                "consolidation" | "consol" => "consolidation".to_string(),
                "nightly" => "nightly".to_string(),
                _ => {
                    eprintln!(
                        "unknown --auxiliary {k:?} (valid: {})",
                        aux_slot_names().join("|")
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
            let mode_flag = flag(&rest, "--api-mode");
            let auto_mode = api_mode_is_auto(mode_flag.as_deref());
            let api_mode = match mode_flag.as_deref() {
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
                config::provider_key_env(&provider, known.as_ref().map(|p| p.key_env.as_str()))
            });
            let raw_keys = flag(&rest, "--key")
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty());
            if is_custom && known.is_some() {
                println!(
                    "note: overriding builtin '{provider}' (visible in `pantheon model --list`)"
                );
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
            // `--api-mode auto` sniffs the resolved endpoint host now that the
            // base URL is known.
            let api_mode = if auto_mode {
                sniff_wire_mode(&template_base)
            } else {
                api_mode
            };
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
        match interactive(first_target, &prompt_line) {
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
            None => println!("cancelled - nothing changed"),
        }
    }

    /// Bottom-of-flow auxiliary step: a deliberate bottom row. `Set
    /// auxiliary` opens the slot list; `Done` (or Esc) finishes.
    /// Cancelling keeps whatever was saved.
    fn run_aux_bottom(dd: &std::path::Path) {
        let items = vec![
            PickItem {
                label: "Set auxiliary".into(),
                desc: "pin provider+model per auxiliary slot".into(),
            },
            PickItem {
                label: "Done".into(),
                desc: "keep auxiliaries as they are".into(),
            },
        ];
        loop {
            let Some(idx) = pick("auxiliary models (bottom step)", &items) else {
                println!("auxiliaries unchanged");
                return;
            };
            match idx {
                0 => run_aux_slot_list(dd),
                _ => return,
            }
        }
    }

    /// Slot list opened from "Set auxiliary": every pinnable slot, driven
    /// from [`aux_slot_names`] so it cannot drift. Esc goes back to the
    /// bottom step. Picking a slot runs the same interactive flow as the
    /// default model (provider → URL + wire mode → key → live `/models`
    /// fetch → model pick), then returns here so several slots can be
    /// pinned in a row.
    fn run_aux_slot_list(dd: &std::path::Path) {
        let slots = aux_slot_names();
        let items: Vec<PickItem> = slots
            .iter()
            .map(|name| PickItem {
                label: (*name).into(),
                desc: aux_slot_desc(name).into(),
            })
            .collect();
        loop {
            let Some(idx) = pick("set auxiliary - pick a slot (Esc: back)", &items) else {
                return;
            };
            let kind = slots[idx].to_string();
            match interactive(Target::Auxiliary(kind.clone()), &prompt_line) {
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

    // ---------------------------------------------------------------------------
    // tests
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn save_choice_preserves_timeout_ms_and_reasoning() {
            // A model pick rebuilds `[model]` with the new provider/model;
            // a timeout or reasoning level the user chose must survive it,
            // exactly like the fallbacks do — never silently reset.
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dd = std::env::temp_dir().join(format!(
                "pantheon-model-pick-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dd).unwrap();
            std::fs::write(
                dd.join("config.toml"),
                "[model]\nprovider = \"old-provider\"\nmodel = \"old-model\"\nreasoning = \"high\"\ntimeout_ms = 45000\n",
            )
            .unwrap();
            let choice = ModelChoice {
                provider: "new-provider".to_string(),
                model: "new-model".to_string(),
                base_url: None,
                api_mode: ApiMode::OpenAi,
                key_env: "PANTHEON_API_KEY".to_string(),
                raw_keys: None,
                target: Target::Default,
            };
            save_choice(&dd, &choice).unwrap();
            let cfg = Config::load_for_save(&dd).unwrap();
            let m = cfg.model.expect("model section survives the pick");
            assert_eq!(m.provider, "new-provider");
            assert_eq!(m.model, "new-model");
            assert_eq!(m.reasoning.as_deref(), Some("high"));
            assert_eq!(m.timeout_ms, Some(45000));
            std::fs::remove_dir_all(&dd).ok();
        }
    }
}

pub mod model_browser {
    //! Alternate-screen model browser.
    //!
    //! A richer replacement for the scroll-only `pick` that `pantheon model`
    //! previously used: filter tabs, a four-column table, and a details card.
    //! It deliberately returns a row index plus a chosen action, so the
    //! caller (and not this widget) decides what saving means. The browser
    //! is intentionally a TTY-only, side-effect-free value producer.

    use crate::model_catalog::ModelRow;
    use std::io::Write;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ModelAction {
        Use,
        SetDefault,
        Test,
    }

    #[derive(Debug, Clone)]
    pub struct BrowserResult {
        pub index: usize,
        pub action: ModelAction,
    }

    /// `manual_idx` marks the "Add custom model" row, if present: Enter on
    /// it commits immediately without the details card, since there is no
    /// metadata to show.
    pub fn browse(
        title: &str,
        rows: &[ModelRow],
        manual_idx: Option<usize>,
    ) -> Option<BrowserResult> {
        use crossterm::{
            cursor,
            event::{self, Event, KeyCode},
            execute,
            terminal::{enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        };
        if rows.is_empty() {
            return None;
        }
        enable_raw_mode().ok()?;
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                use crossterm::{cursor, execute, terminal::disable_raw_mode};
                let _ = disable_raw_mode();
                let _ = execute!(std::io::stdout(), cursor::Show);
            }
        }
        let _guard = Guard;
        let mut stdout = std::io::stdout();
        execute!(stdout, EnterAlternateScreen, cursor::Hide).ok()?;
        let leave = |out: &mut std::io::Stdout| {
            let _ = execute!(*out, LeaveAlternateScreen);
        };
        let tabs: [&str; 5] = ["All", "Coding", "Reasoning", "Free", "Vision"];
        let mut tab = 0usize;
        let mut filter = String::new();
        let mut selected = 0usize;
        let mut detail = false;

        // Capability tabs filter on catalog flags, not id substrings:
        // the old Vision tab listed every `claude`/`gemini` id (including
        // text-only rows) and missed vision models whose ids say nothing
        // about vision. A row with unknown flags (live-only, `None`)
        // keeps the old substring heuristic as a fallback — unknown is
        // not false — while a catalogued `Some(false)` is excluded.
        fn tab_match(tab: usize, row: &ModelRow) -> bool {
            let id = row.id.to_lowercase();
            match tab {
                0 => true,
                1 => id.contains("coder") || id.contains("code") || id.contains("codex"),
                2 => match row.reasoning {
                    Some(r) => r,
                    None => {
                        id.contains("reasoning")
                            || id.contains("thinking")
                            || id.starts_with("o1")
                            || id.starts_with("o3")
                            || id.starts_with("o4")
                            || id.contains("-r1")
                            || id.contains(":r1")
                    }
                },
                3 => {
                    matches!(row.input_per_mtok_usd, Some(0.0))
                        && matches!(row.output_per_mtok_usd, Some(0.0))
                }
                4 => match row.vision {
                    Some(v) => v,
                    None => {
                        id.contains("vision")
                            || id.contains("4o")
                            || id.contains("claude")
                            || id.contains("gemini")
                    }
                },
                _ => true,
            }
        }

        loop {
            let mut hits: Vec<usize> = Vec::new();
            for (i, r) in rows.iter().enumerate() {
                if !tab_match(tab, r) {
                    continue;
                }
                let f = filter.to_lowercase();
                if f.is_empty()
                    || r.id.to_lowercase().contains(&f)
                    || crate::model_catalog::row_meta(r)
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&f)
                {
                    hits.push(i);
                }
            }
            if hits.is_empty() {
                selected = 0;
            } else if selected >= hits.len() {
                selected = hits.len().saturating_sub(1);
            }

            execute!(stdout, cursor::MoveTo(0, 0)).ok();
            let _ = crossterm::terminal::Clear(crossterm::terminal::ClearType::All);
            println!("{title}\r");

            print!("  ");
            for (i, t) in tabs.iter().enumerate() {
                if i == tab {
                    print!("\x1b[1;35m[{t}]\x1b[0m  ");
                } else {
                    print!("[{t}]  ");
                }
            }
            println!("filter: {filter}\r");

            if !detail {
                println!("  Name                                 Context     Input      Output\\r");
                if hits.is_empty() {
                    println!("  (no matches)\\r");
                }
                for (row_idx, &idx) in hits.iter().enumerate().take(14) {
                    let r = &rows[idx];
                    let marker = if row_idx == selected { ">" } else { " " };
                    let ctx = r
                        .context_limit
                        .map(|c| format!("{}k", c / 1000))
                        .unwrap_or_else(|| "-".into());
                    let inp = r
                        .input_per_mtok_usd
                        .map(|v| format!("${v:.2}"))
                        .unwrap_or_else(|| "-".into());
                    let out = r
                        .output_per_mtok_usd
                        .map(|v| format!("${v:.2}"))
                        .unwrap_or_else(|| "-".into());
                    let label = crate::sanitize::strip_control(&r.id);
                    let max = 36usize;
                    let label = if label.len() > max {
                        format!("{}...", &label[..max.saturating_sub(3)])
                    } else {
                        format!("{label:<max$}")
                    };
                    if row_idx == selected {
                        println!(
                            "  {marker} \x1b[1;35m{label}\x1b[0m {ctx:>9}  {inp:>9}  {out:>9}\r"
                        );
                    } else {
                        println!("  {marker} {label} {ctx:>9}  {inp:>9}  {out:>9}\r");
                    }
                }
                println!(
                    "\r  {} match{} · Enter details · Tab/1-5 filter · type search · Esc cancel\r",
                    hits.len(),
                    if hits.len() == 1 { "" } else { "es" }
                );
            } else if !hits.is_empty() {
                let r = &rows[hits[selected]];
                println!();
                println!("  MODEL DETAILS\r");
                println!("  Name      {}\r", r.id);
                println!(
                    "  Context   {}\r",
                    r.context_limit
                        .map(|c| format!("{c} tokens"))
                        .unwrap_or_else(|| "unknown".into())
                );
                println!(
                    "  Input     {}\r",
                    r.input_per_mtok_usd
                        .map(|v| format!("${v:.2} / 1M"))
                        .unwrap_or_else(|| "unknown".into())
                );
                println!(
                    "  Output    {}\r",
                    r.output_per_mtok_usd
                        .map(|v| format!("${v:.2} / 1M"))
                        .unwrap_or_else(|| "unknown".into())
                );
                println!(
                    "  Source    {}\r",
                    if r.curated {
                        "curated + live"
                    } else {
                        "live only"
                    }
                );
                println!();
                println!("  [Enter] use this model   [s] set as default   [t] test   [Esc] back\r");
            }
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
                        if detail {
                            detail = false;
                        } else {
                            leave(&mut stdout);
                            return None;
                        }
                    }
                    KeyCode::Enter => {
                        if detail {
                            leave(&mut stdout);
                            return Some(BrowserResult {
                                index: hits[selected],
                                action: ModelAction::Use,
                            });
                        }
                        if hits.is_empty() {
                            continue;
                        }
                        // The manual row has no details card: commit straight
                        // to the free-text prompt.
                        if Some(hits[selected]) == manual_idx {
                            leave(&mut stdout);
                            return Some(BrowserResult {
                                index: hits[selected],
                                action: ModelAction::Use,
                            });
                        }
                        detail = true;
                    }
                    KeyCode::Char('s') if detail => {
                        leave(&mut stdout);
                        return Some(BrowserResult {
                            index: hits[selected],
                            action: ModelAction::SetDefault,
                        });
                    }
                    KeyCode::Char('t') if detail => {
                        leave(&mut stdout);
                        return Some(BrowserResult {
                            index: hits[selected],
                            action: ModelAction::Test,
                        });
                    }
                    KeyCode::Tab | KeyCode::Right => {
                        tab = (tab + 1) % tabs.len();
                        selected = 0;
                        detail = false;
                    }
                    KeyCode::Left => {
                        tab = if tab == 0 { tabs.len() - 1 } else { tab - 1 };
                        selected = 0;
                        detail = false;
                    }
                    KeyCode::Char('1') => {
                        tab = 0;
                        selected = 0;
                        detail = false;
                    }
                    KeyCode::Char('2') => {
                        tab = 1;
                        selected = 0;
                        detail = false;
                    }
                    KeyCode::Char('3') => {
                        tab = 2;
                        selected = 0;
                        detail = false;
                    }
                    KeyCode::Char('4') => {
                        tab = 3;
                        selected = 0;
                        detail = false;
                    }
                    KeyCode::Char('5') => {
                        tab = 4;
                        selected = 0;
                        detail = false;
                    }
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => {
                        if selected + 1 < hits.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = hits.len().saturating_sub(1),
                    KeyCode::Backspace => {
                        filter.pop();
                        selected = 0;
                    }
                    KeyCode::Char('c')
                        if k.modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        leave(&mut stdout);
                        return None;
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
}

pub mod model_catalog {
    //! Live model lists for the wizard's Screen 3 (model picker).
    //!
    //! For OpenAI-compatible providers the picker merges the catalog's
    //! curated entries with the provider's live `{base}/models` list, so the
    //! user sees what actually exists rather than only what the catalog
    //! shipped. The fetch is keyless-first: many OpenAI-style endpoints
    //! (OpenRouter included) serve `/models` without auth. A 401/403 falls
    //! back to the curated entries with a "full list after API key" note;
    //! other failures fall back with a note naming the failure kind. The
    //! wizard never invents models: with no rows at all, free-text entry is
    //! the only path.
    //!
    //! Everything network-adjacent is split so the pure parts are
    //! unit-testable without a socket: [`parse_models_body`] for the JSON
    //! shape, [`classify_status`] for the auth-vs-rest distinction, and
    //! [`merge_model_rows`] for the union.

    use std::time::Duration;

    use pantheon_providers::catalog::ApiMode;

    /// Anthropic `anthropic-version` header value. Kept in step with the
    /// transport in `model.rs`; an Anthropic-wire `/models` rejects the
    /// request without it.
    const ANTHROPIC_VERSION: &str = "2023-06-01";

    /// One live model entry from a `/models` response.
    #[derive(Debug, Clone, PartialEq)]
    pub struct LiveModel {
        pub id: String,
        pub input_per_mtok_usd: Option<f64>,
        pub output_per_mtok_usd: Option<f64>,
        /// `context_length` (or `max_context_length`) reported by the endpoint.
        pub context_limit: Option<u64>,
    }

    /// Why a live fetch failed.
    #[derive(Debug, Clone, PartialEq)]
    pub enum FetchError {
        /// 401/403: the endpoint wants a key. Not fatal - the wizard falls
        /// back to curated entries.
        Auth,
        /// Transport failure, timeout, or a non-auth HTTP status.
        Unreachable(String),
        /// The body was not the expected `{"data":[...]}` shape.
        BadShape(String),
    }

    impl FetchError {
        /// Short kind name for the one-line picker note.
        pub fn kind(&self) -> &'static str {
            match self {
                FetchError::Auth => "auth",
                FetchError::Unreachable(_) => "unreachable",
                FetchError::BadShape(_) => "bad response",
            }
        }
    }

    /// Fetch the live model list from `{base_url}/models`, keyless. One
    /// HTTPS GET with a ~10s timeout.
    pub fn fetch_live_models(base_url: &str) -> Result<Vec<LiveModel>, FetchError> {
        let url = models_url(base_url);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let resp = agent.get(&url).call().map_err(|e| match e {
            ureq::Error::StatusCode(code) => classify_status(code),
            e => FetchError::Unreachable(e.to_string()),
        })?;
        let body = resp
            .into_body()
            .read_to_string()
            .map_err(|e| FetchError::Unreachable(e.to_string()))?;
        parse_models_body(&body)
    }

    /// `{base}/models`, tolerant of a trailing slash on the base.
    fn models_url(base_url: &str) -> String {
        format!("{}/models", base_url.trim_end_matches('/'))
    }

    /// Fetch the live model list **with** an API key, for the wizard's
    /// post-key model step. Same response shape as [`fetch_live_models`]
    /// but it sends the auth header for the wire mode, so a keyed endpoint
    /// (the local router, most hosted providers) returns its real list
    /// instead of a 401 that would drop the wizard back to curated-only.
    ///
    /// An empty key degrades to the keyless call. `mode` picks the header:
    /// OpenAI sends `Authorization: Bearer`, Anthropic sends `x-api-key`
    /// plus `anthropic-version`, because sending the wrong one fails auth
    /// and reads as "this endpoint has no models".
    pub fn fetch_live_models_keyed(
        base_url: &str,
        key: &str,
        mode: ApiMode,
    ) -> Result<Vec<LiveModel>, FetchError> {
        let url = models_url(base_url);
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let mut req = agent.get(&url);
        let key = key.trim();
        if !key.is_empty() {
            match mode {
                ApiMode::Anthropic => {
                    req = req
                        .header("x-api-key", key)
                        .header("anthropic-version", ANTHROPIC_VERSION);
                }
                ApiMode::OpenAi => {
                    req = req.header("Authorization", format!("Bearer {key}").as_str());
                }
                ApiMode::Gemini => {
                    req = req.header("x-goog-api-key", key);
                }
            }
        }
        let resp = req.call().map_err(|e| match e {
            ureq::Error::StatusCode(code) => classify_status(code),
            e => FetchError::Unreachable(e.to_string()),
        })?;
        let body = resp
            .into_body()
            .read_to_string()
            .map_err(|e| FetchError::Unreachable(e.to_string()))?;
        parse_models_body(&body)
    }

    /// Map an HTTP status to the fetch error. Only 401/403 is auth;
    /// everything else reachable-but-failing is grouped with unreachable,
    /// because the wizard's response is the same: curated + a note.
    fn classify_status(code: u16) -> FetchError {
        match code {
            401 | 403 => FetchError::Auth,
            _ => FetchError::Unreachable(format!("http {code}")),
        }
    }

    /// Parse the OpenAI `{"data":[{"id", ...}]}` shape. OpenRouter-style
    /// `pricing: {prompt, completion}` string fields are per-token prices,
    /// converted to per-million-token USD. Entries without a string id are
    /// skipped - they cannot be displayed or selected.
    fn parse_models_body(body: &str) -> Result<Vec<LiveModel>, FetchError> {
        let v: serde_json::Value = serde_json::from_str(body)
            .map_err(|e| FetchError::BadShape(format!("invalid json: {e}")))?;
        let data = v
            .get("data")
            .and_then(|d| d.as_array())
            .ok_or_else(|| FetchError::BadShape("missing data array".to_string()))?;
        let mut out = Vec::with_capacity(data.len());
        for entry in data {
            let Some(id) = entry.get("id").and_then(|i| i.as_str()) else {
                continue;
            };
            let pricing = entry.get("pricing");
            out.push(LiveModel {
                id: id.to_string(),
                input_per_mtok_usd: per_mtok(pricing.and_then(|p| p.get("prompt"))),
                output_per_mtok_usd: per_mtok(pricing.and_then(|p| p.get("completion"))),
                context_limit: entry
                    .get("context_length")
                    .or_else(|| entry.get("max_context_length"))
                    .or_else(|| entry.get("context_limit"))
                    .and_then(|v| v.as_u64()),
            });
        }
        Ok(out)
    }

    /// OpenRouter-style per-token price (string or number) → USD per
    /// million tokens. Non-finite, negative, or unparsable values are
    /// treated as unknown, never as zero.
    fn per_mtok(v: Option<&serde_json::Value>) -> Option<f64> {
        let per_token = match v? {
            serde_json::Value::String(s) => s.parse::<f64>().ok()?,
            serde_json::Value::Number(n) => n.as_f64()?,
            _ => return None,
        };
        if !per_token.is_finite() || per_token < 0.0 {
            return None;
        }
        Some(per_token * 1_000_000.0)
    }

    // ---------------------------------------------------------------------------
    // row merging and tags
    // ---------------------------------------------------------------------------

    /// A curated catalog entry, normalized for the merge.
    #[derive(Debug, Clone)]
    pub struct CatalogModel {
        pub id: String,
        pub context_limit: Option<u64>,
        pub input_per_mtok_usd: Option<f64>,
        pub output_per_mtok_usd: Option<f64>,
        /// Catalog capability flags; `None` on rows built before flags
        /// were threaded (live-only rows stay `None` — unknown, not false).
        pub vision: Option<bool>,
        pub reasoning: Option<bool>,
    }

    /// One picker row: the union of curated and live.
    #[derive(Debug, Clone, PartialEq)]
    pub struct ModelRow {
        pub id: String,
        pub context_limit: Option<u64>,
        pub input_per_mtok_usd: Option<f64>,
        pub output_per_mtok_usd: Option<f64>,
        /// True when the id came from the catalog (curated metadata wins).
        pub curated: bool,
        /// Catalog capability flags (`None` = unknown: live-only rows).
        pub vision: Option<bool>,
        pub reasoning: Option<bool>,
    }

    /// Union by model id. Curated entries win on metadata (context limit,
    /// catalog cost); a live id with no curated entry gets a row with live
    /// pricing; a curated id absent from live still shows. Order is
    /// curated first (catalog order), then live-only ids in live order.
    pub fn merge_model_rows(curated: &[CatalogModel], live: &[LiveModel]) -> Vec<ModelRow> {
        let mut rows: Vec<ModelRow> = curated
            .iter()
            .map(|c| ModelRow {
                id: c.id.clone(),
                context_limit: c.context_limit,
                input_per_mtok_usd: c.input_per_mtok_usd,
                output_per_mtok_usd: c.output_per_mtok_usd,
                curated: true,
                vision: c.vision,
                reasoning: c.reasoning,
            })
            .collect();
        for l in live {
            if rows.iter().any(|r| r.id == l.id) {
                continue;
            }
            rows.push(ModelRow {
                id: l.id.clone(),
                context_limit: l.context_limit,
                input_per_mtok_usd: l.input_per_mtok_usd,
                output_per_mtok_usd: l.output_per_mtok_usd,
                curated: false,
                vision: None,
                reasoning: None,
            });
        }
        rows
    }

    /// Compact price: two decimals for normal values (`$0.20`), more
    /// precision for tiny-but-nonzero ones so they never round to `$0.00`.
    fn fmt_price(v: f64) -> String {
        if v >= 0.01 {
            format!("${v:.2}")
        } else if v > 0.0 {
            let s = format!("{v:.6}");
            format!("${}", s.trim_end_matches('0').trim_end_matches('.'))
        } else {
            "$0.00".to_string()
        }
    }

    /// Context window as a compact string: `131k`. `None` when unknown.
    pub fn context_str(row: &ModelRow) -> Option<String> {
        row.context_limit.map(|l| format!("{}k", l / 1000))
    }

    /// Compact price for the second metadata line: `$0.15 in / $0.60 out`,
    /// `free`, or a single-sided `$0.15 in`. `None` when no pricing is
    /// known, never invented.
    pub fn price_compact(input: Option<f64>, output: Option<f64>) -> Option<String> {
        match (input, output) {
            (Some(i), Some(o)) if i == 0.0 && o == 0.0 => Some("free".to_string()),
            (Some(i), Some(o)) => Some(format!("{} in / {} out", fmt_price(i), fmt_price(o))),
            (Some(i), None) => Some(format!("{} in", fmt_price(i))),
            (None, Some(o)) => Some(format!("{} out", fmt_price(o))),
            (None, None) => None,
        }
    }

    /// The second dim line under a model row: context and price joined
    /// (`131k · $0.15 in / $0.60 out`). This lives on its own line, not the
    /// right-aligned tag, because the combined string is long and clips
    /// against the label on a narrow terminal when it shares the row.
    /// `None` when neither context nor price is known.
    pub fn row_meta(row: &ModelRow) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(c) = context_str(row) {
            parts.push(c);
        }
        if let Some(p) = price_compact(row.input_per_mtok_usd, row.output_per_mtok_usd) {
            parts.push(p);
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn row(ctx: Option<u64>, inp: Option<f64>, out: Option<f64>) -> ModelRow {
            ModelRow {
                id: "m".into(),
                context_limit: ctx,
                input_per_mtok_usd: inp,
                output_per_mtok_usd: out,
                curated: true,
                vision: None,
                reasoning: None,
            }
        }

        #[test]
        fn price_compact_joins_input_and_output() {
            assert_eq!(
                price_compact(Some(0.15), Some(0.60)).as_deref(),
                Some("$0.15 in / $0.60 out")
            );
        }

        #[test]
        fn price_compact_marks_zero_as_free() {
            assert_eq!(price_compact(Some(0.0), Some(0.0)).as_deref(), Some("free"));
        }

        #[test]
        fn price_compact_single_sided() {
            assert_eq!(price_compact(Some(2.50), None).as_deref(), Some("$2.50 in"));
            assert_eq!(
                price_compact(None, Some(0.30)).as_deref(),
                Some("$0.30 out")
            );
        }

        #[test]
        fn price_compact_none_when_unknown() {
            assert_eq!(price_compact(None, None), None);
        }

        #[test]
        fn row_meta_puts_context_and_price_on_one_line() {
            // The whole point of `meta`: context plus price together, ready
            // for its own dim line so it never clips against the label.
            assert_eq!(
                row_meta(&row(Some(131_000), Some(0.15), Some(0.60))).as_deref(),
                Some("131k · $0.15 in / $0.60 out")
            );
        }

        #[test]
        fn row_meta_context_only() {
            assert_eq!(
                row_meta(&row(Some(128_000), None, None)).as_deref(),
                Some("128k")
            );
        }

        #[test]
        fn row_meta_none_when_nothing_known() {
            assert_eq!(row_meta(&row(None, None, None)), None);
        }
    }
}

pub mod provider {
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

    use crate::config::{self, Config};
    use crate::model::{
        api_mode_is_auto, builtin_provider_id, ensure_template_vars, flag, flags, has_flag,
        key_status, normalize_base_url, parse_api_mode, parse_set_pairs, pick,
        print_template_status, prompt_line, resolve_wire_mode, sniff_wire_mode, valid_provider_id,
        wire_mode_items, PickItem,
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
        let interactive = name_flag.is_none()
            && flag(rest, "--base-url").is_none()
            && flag(rest, "--key").is_none();
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
                    &format!("'{name}' is a builtin - override it with this endpoint? (y/N)"),
                    "n",
                );
                if !matches!(a.to_ascii_lowercase().as_str(), "y" | "yes") {
                    println!("cancelled - nothing changed");
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
            let mode = api_mode_flag(rest);
            if api_mode_is_auto(flag(rest, "--api-mode").as_deref()) {
                sniff_wire_mode(&url)
            } else {
                mode
            }
        } else {
            match pick("wire mode", &wire_mode_items()) {
                Some(idx) => resolve_wire_mode(idx, &url),
                None => sniff_wire_mode(&url),
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
            let ke = key_env_default.clone();
            let existing =
                crate::dotenv::read_dotenv_value(&dd, &ke).filter(|v| !v.trim().is_empty());
            if let Some(cur) = &existing {
                println!("  current: {} (empty keeps)", crate::model::mask_key(cur));
            }
            let keys = prompt_line("API key(s), comma-separated to stack", "");
            let raw = if keys.trim().is_empty() {
                if existing.is_none() {
                    println!("  no key stored - keyless endpoints only");
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
            if let Err(e) = crate::dotenv::persist_dotenv_value(&dd, &key_env, keys.trim()) {
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
                "added custom provider '{name}': {url}  [keys → {}]  - select it with `pantheon model`",
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
                    println!("cancelled - nothing changed");
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

    /// `pantheon provider models <name>` - the live model list for one endpoint.
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
        match crate::model::fetch_models(&sec.base_url, &key, mode, name.as_str()) {
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
                let missing: Vec<&String> =
                    remembered.iter().filter(|r| !ids.contains(r)).collect();
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
}

pub mod ollama {
    //! Ollama onboarding: discover, hardware check, verified tool call gate.
    //!
    //! The helper probes the local endpoint, checks total memory from
    //! /proc/meminfo or sysctl, and reports whether a model is ready.
    //! It never downloads a model or changes the default model. The
    //! verified tool call step is a live check the caller runs after
    //! this helper returns ready.

    /// Minimum RAM in GiB recommended for the smallest local recipes.
    pub const MIN_RAM_GIB: u64 = 8;

    pub struct OllamaReport {
        pub reachable: bool,
        pub models: Vec<String>,
        pub ram_gib: Option<u64>,
        pub ready: bool,
    }

    pub fn total_ram_gib() -> Option<u64> {
        #[cfg(target_os = "linux")]
        {
            if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
                for line in text.lines() {
                    let line = line.trim();
                    if let Some(rest) = line.strip_prefix("MemTotal:") {
                        let kb: u64 = rest
                            .split_whitespace()
                            .next()
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(0);
                        return Some((kb / 1024) / 1024);
                    }
                }
            }
        }
        #[cfg(target_os = "macos")]
        {
            if let Ok(out) = std::process::Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
            {
                if out.status.success() {
                    if let Ok(bytes) = String::from_utf8(out.stdout)
                        .unwrap_or_default()
                        .trim()
                        .parse::<u64>()
                    {
                        return Some(bytes / 1024 / 1024 / 1024);
                    }
                }
            }
        }
        None
    }

    pub fn check_local_models(base_url: &str) -> OllamaReport {
        // Reuse the picker discovery path indirectly via a plain GET here to
        // avoid depending on ureq agent setup in this small helper. The
        // caller already has fetch_models for the full picker flow.
        let url = format!("{}/models", base_url.trim_end_matches('/'));
        let mut models = Vec::new();
        let reachable;
        match std::process::Command::new("curl")
            .args(["-s", "--max-time", "5", &url])
            .output()
        {
            Ok(out) if out.status.success() => {
                reachable = true;
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                    if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                        for m in arr {
                            if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                                models.push(id.to_string());
                            }
                        }
                    }
                }
            }
            _ => {
                reachable = false;
            }
        }
        let ram_gib = total_ram_gib();
        let ready =
            reachable && !models.is_empty() && ram_gib.map(|r| r >= MIN_RAM_GIB).unwrap_or(true);
        OllamaReport {
            reachable,
            models,
            ram_gib,
            ready,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn ram_probe_never_panics() {
            let _ = total_ram_gib();
        }

        #[test]
        fn unreachable_endpoint_reports_not_ready() {
            let r = check_local_models("http://127.0.0.1:1");
            assert!(!r.reachable);
            assert!(!r.ready);
        }
    }

    /// Check if a live model on this endpoint can make a tool call.
    /// Sends one tiny chat completion with a required tool definition and
    /// expects the model to return a function call. This is the caller
    /// live check mentioned in the module docs. Returns true when a
    /// tool call shape comes back.
    pub fn verified_tool_call(base_url: &str, model: &str) -> bool {
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "call the ping tool"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "ping",
                    "description": "test tool",
                    "parameters": {"type": "object", "properties": {}}
                }
            }],
            "tool_choice": "required",
            "max_tokens": 50
        });
        let out = match std::process::Command::new("curl")
            .args([
                "-s",
                "--max-time",
                "10",
                "-H",
                "Content-Type: application/json",
                "-d",
                &body.to_string(),
                &url,
            ])
            .output()
        {
            Ok(o) if o.status.success() => o.stdout,
            _ => return false,
        };
        let v: serde_json::Value = match serde_json::from_slice(&out) {
            Ok(v) => v,
            Err(_) => return false,
        };
        v.get("choices")
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("tool_calls"))
            .and_then(|t| t.as_array())
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    }

    #[cfg(test)]
    mod test_verified {
        use super::verified_tool_call;
        #[test]
        fn unreachable_tool_call_is_false() {
            assert!(!verified_tool_call("http://127.0.0.1:1", "any"));
        }
    }
}

pub mod setup_providers {
    //! Generic provider setup for the setup wizard's tool screens.
    //!
    //! Every provider-backed tool group (web search, browser, STT, TTS,
    //! memory) enumerates its providers from the owning crate's registry,
    //! normalized into [`ProviderMeta`]. The flow is identical for all
    //! groups - no provider-specific branching:
    //!
    //! 1. pick a provider: recommended first, preselected on the
    //!    recommended row, provider notes in the descriptions;
    //! 2. follow-up by kind: [`ProviderKind::Keyless`] asks nothing;
    //!    [`ProviderKind::Cloud`] takes the standard env var name silently
    //!    (never the value; the wizard never handles secrets);
    //!    [`ProviderKind::SelfHosted`] asks for the instance URL;
    //!    [`ProviderKind::Local`] detects the binary, and when it is missing
    //!    shows the metadata's dependency notes then offers install-or-skip
    //!    (a known `install_cmd` runs on confirmation, then detection re-runs);
    //! 3. extra fields (voice model, project id, command, and so on) as declared.
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
    use std::collections::HashSet;
    use std::path::Path;

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
        /// unset). Some self-hosted rows can also need auth (a memory bridge
        /// with a key on it, for example): those carry `key_env` - the
        /// standard name is asked silently and the value pasted once, the
        /// same contract Cloud rows follow.
        SelfHosted {
            default_url: String,
            required: bool,
            key_env: Option<String>,
        },
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

    impl ProviderMeta {
        /// The standard key env var this row collects, if any: Cloud rows
        /// always; SelfHosted rows only when they carry one (a memory
        /// bridge with auth on it). The wizard and tests use this to pin
        /// the collect-the-key contract per row.
        pub fn key_env(&self) -> Option<&str> {
            match &self.kind {
                ProviderKind::Cloud { env_var } => Some(env_var),
                ProviderKind::SelfHosted { key_env, .. } => key_env.as_deref(),
                _ => None,
            }
        }
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
    pub fn pick_provider(
        title: &str,
        subtitle: &str,
        providers: Vec<ProviderMeta>,
    ) -> ProviderPick {
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
                // The standard name IS the answer: asking the user to retype
                // it is a flaw. A custom name stays available through flags
                // (`--websearch-key-env` etc.), which pre-fill the answer
                // instead of prompting.
                answer.key_env = Some(env_var.clone());
            }
            ProviderKind::SelfHosted {
                default_url,
                required,
                key_env,
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
                // Populated rows that can need auth: record the standard
                // var NAME here; the value paste happens right after the
                // pick (wizard) or the pick prompt (text setup).
                answer.key_env = key_env.clone();
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
                    ProviderAuth::Local {
                        detect_cmd,
                        install_hint,
                        install_cmd,
                    } => ProviderKind::Local {
                        local: LocalMeta {
                            detect_cmd: detect_cmd.to_string(),
                            install_hint: install_hint.to_string(),
                            install_cmd: install_cmd.map(str::to_string),
                        },
                    },
                    ProviderAuth::SelfHosted { default_url } => ProviderKind::SelfHosted {
                        default_url: default_url.to_string(),
                        required: true,
                        key_env: None,
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
        // Steel Cloud and Steel self-hosted are separate rows: one screen
        // cannot honestly ask for both a cloud key and an instance URL.
        let mut rows: Vec<ProviderMeta> = pantheon_web::browser::all_backends()
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
                        Vec::new(),
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
                    // Local-tooling norm: the install chain bootstraps
                    // everything the backend needs (pip itself included),
                    // so a bare system Python is not a dead end. The fetch
                    // pulls a ~150MB browser binary; headless servers may
                    // also need OS libs (libgtk-3-0, libasound2) - the hint
                    // says so when the one-liner cannot cover it.
                    BackendKind::Camofox => (
                        ProviderKind::Local {
                            local: LocalMeta {
                                detect_cmd: "python3 -c \"import camoufox\"".into(),
                                install_hint: "needs Python 3 with pip (`apt install python3-pip` on Debian/Ubuntu, or `python3 -m ensurepip`), then the package plus its fetched browser (~150MB; headless servers may also need libgtk-3-0 and libasound2)".into(),
                                install_cmd: Some(
                                    "python3 -m pip install \"camoufox[geoip]\" && python3 -m camoufox fetch".into(),
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
            .collect();
        rows.push(ProviderMeta {
            id: "steel_selfhosted".to_string(),
            name: "Steel (self-hosted)".to_string(),
            blurb:
                "Your own Steel instance over REST + CDP. Needs: the instance base URL (no cloud key)."
                    .to_string(),
            recommended: false,
            kind: ProviderKind::SelfHosted {
                default_url: String::new(),
                required: true,
                key_env: None,
            },
            extra: Vec::new(),
        });
        rows
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

    /// Memory backends from the `pantheon-memory` registry, plus any user
    /// memory-plugin manifests under `<data_dir>/memory-plugins` - the same
    /// loader the runtime uses, so the picker shows exactly what
    /// `pantheon memory backend list` would. `native` needs nothing; other
    /// bridge backends are self-hosted rows whose URL and key resolve from
    /// `options.url`/`options.key` or the fixed env convention
    /// `PANTHEON_MEMORY_<NAME>_URL` / `PANTHEON_MEMORY_<NAME>_KEY`. User
    /// plugins ask nothing at all: the manifest already carries its
    /// URL/key/command (and, for stdio, the exec consent), so plugging in
    /// someone's own memory system is pick-and-go. The `fake` backend is a
    /// test double and never offered.
    pub fn memory_providers(data_dir: &Path) -> Vec<ProviderMeta> {
        let default_names: HashSet<String> = pantheon_memory::BackendRegistry::with_defaults()
            .catalog()
            .into_iter()
            .map(|b| b.name)
            .collect();
        // One registry walk serves both needs: which rows are plugin-only
        // (blurb shape below) and the rows themselves.
        let catalog = pantheon_memory::BackendRegistry::with_plugins(data_dir).catalog();
        let plugin_names: HashSet<String> = catalog
            .iter()
            .map(|b| b.name.clone())
            .filter(|n| !default_names.contains(n))
            .collect();
        catalog
            .into_iter()
            .filter(|b| b.name != "fake")
            .map(|b| {
                let native = b.name == "native";
                let is_plugin = plugin_names.contains(&b.name);
                let prefix = b.name.to_uppercase().replace('-', "_");
                // Builtin rows whose auth notes name the standard key var
                // (e.g. hindsight-cloud, mem0, openviking, galaxymem via a
                // bridge) get that var collected; genuinely keyless rows
                // (byterover, hindsight-local, mnemosyne, mnemopi,
                // sharpshooter) never see a key prompt. User plugins never
                // see one either: their key (if any) lives in the manifest.
                let std_key = format!("PANTHEON_MEMORY_{prefix}_KEY");
                let key_env = (!native && b.auth.contains(&std_key)).then_some(std_key);
                let kind = if native || is_plugin {
                    // Native works out of the box; a user plugin is
                    // configured inside its manifest, so both ask nothing.
                    ProviderKind::Keyless
                } else {
                    ProviderKind::SelfHosted {
                        default_url: String::new(),
                        // No compiled default exists for any bridge backend
                        // (the vendor defaults were removed as broken-by-design),
                        // so an empty answer must reprompt rather than write a
                        // URL-less selection the runtime would fail closed on.
                        required: true,
                        key_env,
                    }
                };
                let blurb = if native {
                    b.auth.clone()
                } else if is_plugin {
                    let where_conf =
                        format!("configured in memory-plugins/{}.toml, nothing to enter here", b.name);
                    if b.auth.is_empty() {
                        format!("{}: {where_conf}", b.label)
                    } else {
                        format!("{}: {} ({where_conf})", b.label, b.auth)
                    }
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
        vec![
            ProviderMeta {
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
            },
            ProviderMeta {
                id: "tapp".into(),
                name: "Tapp".into(),
                blurb: "Desktop control through the Tapp MCP server (tapp-mcp). Screenshots, clicks, typing, scrolling, dragging, with an animated cursor overlay showing what the agent is about to do before it does. Built from the tapp workspace; the binary is `tapp-mcp`."
                    .into(),
                recommended: false,
                kind: ProviderKind::Local {
                    local: LocalMeta {
                        detect_cmd: "command -v tapp-mcp".into(),
                        install_hint: "Build the tapp workspace:\n  cargo build --bin tapp-mcp -p tapp-mcp\nThe binary lands in target/debug/. Pin the path under [computer_use].binary if it is not on PATH."
                            .into(),
                        install_cmd: Some("cargo build --bin tapp-mcp -p tapp-mcp".into()),
                    },
                },
                extra: Vec::new(),
            },
        ]
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
}
