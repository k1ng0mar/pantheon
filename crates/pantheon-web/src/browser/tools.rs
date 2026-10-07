//! Agent-facing `browser_*` tools on a [`ToolRegistry`].
//!
//! Every tool gates on [`Capability::Browser`]; `browser_act` additionally
//! picks up [`Capability::BrowserAct`] (approval-gated in the default
//! presets) when [`BrowserOptions::act_require_approval`] is true - the
//! same `extra_capabilities` pattern the `shell` tool uses for `git push`.
//!
//! The tool surface adapts to the active backend ([`BackendKind`]):
//!
//! * GSD (default): the full 16-tool interactive surface, including
//!   `browser_act` (`act --intent`, verified against gsd-browser 0.1.24),
//!   plus `browser_fill_login` when a login vault is wired
//!   ([`BrowserOptions::login_store`]) - approval-gated credential
//!   autofill on any interactive backend.
//! * Other interactive backends (chromiumoxide, Steel, Browserbase,
//!   Playwright): the interactive surface without `browser_act` - none of
//!   them has gsd's semantic-intent engine, so the tool is not offered.
//! * Lightpanda (extraction-only): `browser_navigate`, `browser_extract`,
//!   `browser_page_source`, `browser_screenshot` only. Interactive
//!   commands are refused by the backend itself too
//!   ([`UnsupportedCommand`](super::error::BrowserError::UnsupportedCommand));
//!   not registering them keeps the model's tool list honest.
//!
//! Search-vs-browse is taught in the tool descriptions: `web_search` is
//! for *looking something up* (facts, docs, prices, error messages);
//! `browser_*` is for *doing things on a live site* - navigate, click,
//! type, extract from JS-heavy pages that snippets cannot cover.

use super::backend::BrowserBackend;

/// Narration sink: invoked with `(session, action, detail)` before every
/// backend call so the app can subtitle what the agent is doing.
pub type ActivitySink = Arc<dyn Fn(&str, &str, &str) + Send + Sync>;
use super::error::BrowserError;
use super::fill::{find_login_fields, host_matches_site, FillError};
use super::lifecycle::{sanitize_session_name, SessionManager, DEFAULT_IDLE_TIMEOUT_SECS};
use super::registry::{build_backend, BackendConfig, BackendKind};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::compact_output;
use pantheon_secrets::LoginStore;
use pantheon_tools::tools::{parse_args, ArgCapabilities, ToolRegistry};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Options for [`register_browser_tools`].
pub struct BrowserOptions {
    /// Master switch. When false, no tools are registered.
    pub enabled: bool,
    /// Which backend drives the tools. Default [`BackendKind::Gsd`].
    pub backend: BackendKind,
    /// Per-backend settings (binaries, base URLs, resolved secrets).
    /// Secrets arrive as resolved values - never logged.
    pub backend_config: BackendConfig,
    /// Gate `browser_act` behind `Capability::BrowserAct` (approval in
    /// the default presets). Default true; when false the tool carries
    /// only `Capability::Browser`. Only meaningful on the GSD backend
    /// (the only one that registers `browser_act`).
    pub act_require_approval: bool,
    /// Seconds of disuse after which a session's daemon is stopped.
    pub idle_timeout_secs: u64,
    /// Produces the current Pantheon run id; mapped to a session name
    /// via [`sanitize_session_name`].
    pub run_id: Arc<dyn Fn() -> String + Send + Sync>,
    /// Optional narration sink: invoked with `(session, action, detail)`
    /// before every backend call so the app can subtitle what the agent
    /// is doing ("Tapping...", "Opening example.com..."). The runtime wires
    /// this to the event ledger; `detail` is display-safe by construction
    /// (see [`activity_of`]) - hosts only, never full URLs or typed text.
    pub on_activity: Option<ActivitySink>,
    /// Website-login vault for [`register_fill_login`]. When `None` the
    /// tool is not registered. The runtime opens it from the data dir
    /// (`logins.json` + `logins.env`); passwords only ever leave through
    /// the fill path - never into model context or error text.
    pub login_store: Option<Arc<LoginStore>>,
}

impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: BackendKind::Gsd,
            backend_config: BackendConfig::default(),
            act_require_approval: true,
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            run_id: Arc::new(|| "default".to_string()),
            on_activity: None,
            login_store: None,
        }
    }
}

fn berr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check tool arguments",
        "",
    )
}

fn arg_str(v: &serde_json::Value, key: &str) -> Result<String, PantheonError> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| berr("TOOL_BAD_ARGS", format!("missing string arg '{key}'")))
}

fn arg_opt_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Normalize a `browser_extract` schema argument for `gsd-browser
/// extract --schema`: the CLI requires a top-level `properties` object
/// (verified live against 0.1.24 - a bare property map is rejected with
/// "schema must have a 'properties' object"), so a bare map is wrapped
/// automatically. Non-JSON input passes through untouched and fails at
/// the backend with its own error.
fn normalize_extract_schema(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(mut map)) if !map.contains_key("properties") => {
            let inner = std::mem::take(&mut map);
            serde_json::json!({"properties": serde_json::Value::Object(inner)}).to_string()
        }
        _ => raw.to_string(),
    }
}

/// Shared per-call context: backend + session manager + run id source.
/// Cloned into every tool closure.
#[derive(Clone)]
struct Ctx {
    backend: Arc<dyn BrowserBackend>,
    sessions: Arc<SessionManager>,
    run_id: Arc<dyn Fn() -> String + Send + Sync>,
    on_activity: Option<ActivitySink>,
    login_store: Option<Arc<LoginStore>>,
    /// Last navigated URL per session name. The canonical command
    /// vocabulary has no "current URL" query, so `browser_fill_login`
    /// defaults its credential match to the current page's host via this
    /// record (tracked on successful `navigate` calls).
    last_url: Arc<Mutex<HashMap<String, String>>>,
}

impl Ctx {
    /// Resolve the current run id to a session name and touch it (GCs
    /// idle sessions as a side effect).
    fn session(&self) -> String {
        let name = sanitize_session_name(&(self.run_id)());
        self.sessions.touch(&name, self.backend.as_ref());
        name
    }

    fn call(&self, argv: Vec<String>) -> Result<serde_json::Value, PantheonError> {
        let session = self.session();
        // Narrate before invoking: the subtitle ("Tapping...") should be
        // live while the action runs, not after it lands.
        if let Some(sink) = &self.on_activity {
            let (action, detail) = activity_of(&argv);
            sink(&session, &action, &detail);
        }
        let result = self
            .backend
            .invoke(&argv, &session)
            .map_err(PantheonError::from);
        // Track the last navigated URL per session so browser_fill_login
        // can default its credential match to the current page's host.
        // The navigate response carries the final (post-redirect) URL on
        // most backends; fall back to the requested URL when it doesn't.
        if argv.first().map(String::as_str) == Some("navigate") {
            let url = result
                .as_ref()
                .ok()
                .and_then(extract_url)
                .or_else(|| argv.get(1).cloned());
            if let (Some(url), Ok(mut map)) = (url, self.last_url.lock()) {
                map.insert(session.clone(), url);
            }
        }
        result
    }

    fn call_text(&self, argv: Vec<String>) -> Result<String, PantheonError> {
        let v = self.call(argv)?;
        serde_json::to_string_pretty(&v).map_err(|e| {
            berr(
                "BROWSER_BAD_OUTPUT",
                format!("serialize browser output: {e}"),
            )
        })
    }

    /// For large outputs (snapshot, page source, extract): compact before
    /// the text reaches model context.
    fn call_compacted(&self, argv: Vec<String>) -> Result<String, PantheonError> {
        Ok(compact_output(&self.call_text(argv)?, &Default::default()).text)
    }

    /// Host of the page this session last navigated to, if any. Used by
    /// `browser_fill_login` to default its credential match to the
    /// current page. No session-manager touch: a read must not keep an
    /// idle session alive.
    fn current_host(&self) -> Option<String> {
        let session = sanitize_session_name(&(self.run_id)());
        let url = self.last_url.lock().ok()?.get(&session)?.clone();
        let host = host_of(&url);
        if host.is_empty() {
            None
        } else {
            Some(host)
        }
    }

    /// Fill a field with a secret value through the canonical `fill-ref`
    /// command. [`BrowserError`] echoes the full argv on failure (Failed,
    /// Timeout, BadOutput) and its `Display` prints it - so the secret
    /// payload is redacted before the error reaches the model. Success
    /// and narration carry no secret either (see [`activity_of`]).
    fn fill_secret(&self, r: &str, secret: &str) -> Result<(), PantheonError> {
        let session = self.session();
        if let Some(sink) = &self.on_activity {
            sink(&session, "fill-ref", "typing into a field");
        }
        self.backend
            .invoke(
                &["fill-ref".to_string(), r.to_string(), secret.to_string()],
                &session,
            )
            .map(|_| ())
            .map_err(|e| PantheonError::from(redact_secret_error(e)))
    }
}

/// Pull a navigated URL out of a backend's `navigate` response. Shapes
/// differ by backend (`{"url": ...}` direct, or wrapped in `"result"`);
/// returns None when the backend reports no URL.
fn extract_url(v: &serde_json::Value) -> Option<String> {
    v.get("url")
        .and_then(|u| u.as_str())
        .or_else(|| {
            v.get("result")
                .and_then(|r| r.get("url"))
                .and_then(|u| u.as_str())
        })
        .map(str::to_string)
}

/// Redact a secret payload from a backend error's echoed argv:
/// `fill-ref <ref> <secret>` becomes `fill-ref <ref> •••`. Without
/// this a failed fill would print the password into the tool error the
/// model reads.
fn redact_secret_error(e: BrowserError) -> BrowserError {
    fn redact_argv(mut argv: Vec<String>) -> Vec<String> {
        if argv.len() > 2 && (argv[0] == "fill-ref" || argv[0] == "type") {
            argv[2] = "•••".to_string();
        }
        argv
    }
    match e {
        BrowserError::Failed { argv, exit, stderr } => BrowserError::Failed {
            argv: redact_argv(argv),
            exit,
            stderr,
        },
        BrowserError::Timeout { argv, secs } => BrowserError::Timeout {
            argv: redact_argv(argv),
            secs,
        },
        BrowserError::BadOutput { argv, detail } => BrowserError::BadOutput {
            argv: redact_argv(argv),
            detail,
        },
        other => other,
    }
}

/// Map a canonical backend argv to narration `(action, detail)`.
/// Pure function - unit-tested. `detail` is display-safe by construction:
/// hosts only for `navigate` (never full URLs with query strings), refs /
/// selectors / key names otherwise, and never typed text (it may contain
/// secrets - `fill-ref`/`type` report a placeholder instead).
pub fn activity_of(argv: &[String]) -> (String, String) {
    let arg = |i: usize| argv.get(i).map(String::as_str).unwrap_or("");
    let action = arg(0);
    let detail = match action {
        "navigate" => host_of(arg(1)),
        "click-ref" | "click" | "hover-ref" => arg(1).to_string(),
        // Typed text may contain secrets: narrate the gesture, not the text.
        "fill-ref" | "type" => "typing into a field".to_string(),
        "press" => arg(1).to_string(),
        "act" => {
            // argv: ["act", "--intent", <intent>, ...]
            let mut intent = "";
            let mut it = argv.iter().map(String::as_str);
            while let Some(a) = it.next() {
                if a == "--intent" {
                    intent = it.next().unwrap_or("");
                    break;
                }
            }
            intent.to_string()
        }
        _ => String::new(),
    };
    (action.to_string(), detail)
}

/// Host portion of a URL for narration (`https://example.com/a?b=c` →
/// `example.com`). Never returns the path, query, or fragment. Public so
/// the dashboard's take-control input path narrates the same way.
pub fn host_of(url: &str) -> String {
    let u = url.trim();
    let after_scheme = u.split("://").nth(1).unwrap_or(u);
    let host_port = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    // Strip userinfo and port; keep it to the bare host.
    let host = host_port.split('@').next_back().unwrap_or(host_port);
    host.split(':').next().unwrap_or(host).to_string()
}

/// Short search-vs-browse blurb appended to interactive tool descriptions.
const SEARCH_VS_BROWSE: &str = "Search-vs-browse: use web_search to look something up (facts, docs, prices); use browser_* only to act on a live site.";

fn str_prop(desc: &str) -> serde_json::Value {
    serde_json::json!({"type": "string", "description": desc})
}

fn schema(
    name: &str,
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
) -> ToolSchema {
    ToolSchema {
        name: name.to_string(),
        description: description.to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": required,
        }),
    }
}

/// Register one browser tool: parse args, build argv, invoke, return text.
#[allow(clippy::too_many_arguments)]
fn register_tool(
    reg: &mut ToolRegistry,
    ctx: &Ctx,
    name: &str,
    description: &str,
    properties: serde_json::Value,
    required: &[&str],
    compact: bool,
    extra: Option<ArgCapabilities>,
    build: impl Fn(&Ctx, serde_json::Value) -> Result<Vec<String>, PantheonError>
        + Send
        + Sync
        + 'static,
) {
    let c = ctx.clone();
    reg.register_with(
        schema(name, description, properties, required),
        Capability::Browser,
        move |args| {
            let v = parse_args(args)?;
            let argv = build(&c, v)?;
            if compact {
                c.call_compacted(argv)
            } else {
                c.call_text(argv)
            }
        },
        extra,
    );
}

/// Register `browser_fill_login`: fill a saved website-login credential
/// into the current page's login form on any interactive backend.
///
/// Flow: resolve the credential from the vault (explicit `login` id, or
/// matched against `site` / the current page's host) → snapshot →
/// locate the username/password fields ([`find_login_fields`]) →
/// fill via canonical `fill-ref`.
///
/// Approval-gated: the tool carries `Capability::BrowserFillLogin` on
/// top of `Capability::Browser`, and the default policies mark it
/// Approval - the run loop parks for a human before the closure runs,
/// so a denied approval aborts without touching the page or the vault.
/// Secrets never reach the model: the result names only the login id
/// and the filled field labels, and backend errors have their argv
/// redacted (see [`redact_secret_error`]).
///
/// The tool does NOT submit the form: the model clicks the submit
/// button itself afterwards with `browser_click_ref`.
fn register_fill_login(reg: &mut ToolRegistry, ctx: &Ctx) {
    let Some(store) = ctx.login_store.clone() else {
        return;
    };
    let c = ctx.clone();
    let extra: Option<ArgCapabilities> =
        Some(Box::new(|_args: &str| vec![Capability::BrowserFillLogin]));
    reg.register_with(
        schema(
            "browser_fill_login",
            "Fill a saved website-login credential into the page's login form. Finds the username/email and password fields from a fresh snapshot and fills them with the vault credential - works on any configured browser backend. APPROVAL-GATED: the run parks for a human before anything is filled; a denial aborts cleanly. Never returns secret values: the result names only the login id and which field labels were filled. Does NOT submit the form - click the submit button yourself afterwards with browser_click_ref.",
            serde_json::json!({
                "login": str_prop("Vault login id (see the app's More → Logins list). If omitted, the login is matched against the site."),
                "site": str_prop("URL or host to match a saved login against. Defaults to the current page's host (tracked from the last browser_navigate)."),
            }),
            &[],
        ),
        Capability::Browser,
        move |args| fill_login(&c, &store, args),
        extra,
    );
}

fn login_err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "add or fix the login in the app under More → Logins (or the dashboard's logins API), then retry",
        "",
    )
}

fn vault_err(e: pantheon_secrets::SecretsError) -> PantheonError {
    PantheonError::new(
        "LOGIN_VAULT_UNAVAILABLE",
        Layer::Execution,
        false,
        format!("login vault unavailable: {e}"),
        "check the data directory is writable and logins.json is valid",
        "",
    )
}

/// Resolve which vault credential to fill: explicit `login` id wins;
/// otherwise match the stored sites against `site` (or the current
/// page's host when omitted).
fn resolve_credential(
    store: &LoginStore,
    login: Option<&str>,
    site: Option<&str>,
    ctx: &Ctx,
) -> Result<pantheon_secrets::LoginCredential, PantheonError> {
    if let Some(id) = login {
        return store.get(id).map_err(vault_err)?.ok_or_else(|| {
            login_err(
                "LOGIN_UNKNOWN",
                format!("unknown login '{id}': no vault entry with that id"),
            )
        });
    }
    let host = match site {
        Some(s) => {
            let h = host_of(s);
            if h.is_empty() {
                return Err(login_err(
                    "LOGIN_NO_HOST",
                    format!("could not read a host from site '{s}': pass a bare host like \"example.com\""),
                ));
            }
            h
        }
        None => ctx.current_host().ok_or_else(|| {
            login_err(
                "LOGIN_NO_HOST",
                "could not determine the current page's host (no browser_navigate recorded this session): pass the site explicitly, e.g. {\"site\": \"example.com\"}".to_string(),
            )
        })?,
    };
    let all = store.list().map_err(vault_err)?;
    let matches: Vec<&pantheon_secrets::LoginCredential> = all
        .iter()
        .filter(|c| host_matches_site(&c.site, &host))
        .collect();
    match matches.len() {
        0 => Err(login_err(
            "LOGIN_NO_MATCH",
            format!("no saved login matches '{host}': add one in the app under More → Logins, or pass an explicit login id"),
        )),
        1 => Ok(matches[0].clone()),
        _ => {
            let ids: Vec<&str> = matches.iter().map(|c| c.id.as_str()).collect();
            Err(login_err(
                "LOGIN_AMBIGUOUS",
                format!(
                    "multiple saved logins match '{host}': {} - pass the login id explicitly",
                    ids.join(", ")
                ),
            ))
        }
    }
}

fn fill_login(ctx: &Ctx, store: &LoginStore, args: &str) -> Result<String, PantheonError> {
    let v = parse_args(args)?;
    let login = arg_opt_str(&v, "login");
    let site = arg_opt_str(&v, "site");

    // 1. Resolve the credential. Only metadata (id/site/username) is
    //    read here; the password leaves the vault straight into the
    //    backend argv in step 3 - never into model-visible text.
    let cred = resolve_credential(store, login.as_deref(), site.as_deref(), ctx)?;
    let password = store
        .password_for(&cred.id)
        .map_err(vault_err)?
        .ok_or_else(|| {
            login_err(
                "LOGIN_NO_PASSWORD",
                format!("login '{}' has no password stored", cred.id),
            )
        })?;

    // 2. Snapshot and locate the fields. Backend-agnostic: every
    //    backend speaks the canonical snapshot vocabulary.
    let snap = ctx.call(vec!["snapshot".into()])?;
    let fields = find_login_fields(&snap).map_err(|e| match e {
        FillError::NoFields => login_err(
            "LOGIN_NO_FIELDS",
            "no username or password fields found on the current page: navigate to the login page first, then retry".to_string(),
        ),
        FillError::AmbiguousPasswordFields(n) => login_err(
            "LOGIN_AMBIGUOUS_FIELDS",
            format!(
                "found {n} password fields - ambiguous which form to fill: snapshot the page and fill the exact refs with browser_fill_ref instead"
            ),
        ),
    })?;

    // 3. Fill. Secrets go only into the backend argv; failures are
    //    redacted by `Ctx::fill_secret`.
    let mut filled = Vec::new();
    if let Some(u) = &fields.username {
        ctx.fill_secret(&u.r, &cred.username)?;
        filled.push(format!(
            "username '{}' → \"{}\" ({})",
            cred.username, u.label, u.r
        ));
    }
    if let Some(p) = &fields.password {
        ctx.fill_secret(&p.r, password.expose())?;
        filled.push(format!("password → \"{}\" ({})", p.label, p.r));
    }

    Ok(format!(
        "Filled saved login '{}' ({}): {}. The form was not submitted.",
        cred.id,
        cred.site,
        filled.join(", ")
    ))
}

/// Register the `browser_*` toolset. When `opts.enabled` is false this
/// registers nothing and returns `Ok`. The tool surface adapts to
/// `opts.backend`: the full interactive surface on GSD, the interactive
/// surface without `browser_act` on the other interactive backends, and
/// the fetch/extract-only surface on Lightpanda.
pub fn register_browser_tools(
    reg: &mut ToolRegistry,
    opts: BrowserOptions,
) -> Result<(), PantheonError> {
    if !opts.enabled {
        return Ok(());
    }
    // Fail fast on misconfiguration (missing API key, no Lightpanda
    // transport, ...) instead of registering dead tools. Construction is
    // lazy: no network, no subprocess, no browser launch happens here
    // backends connect on first `invoke`.
    let backend = build_backend(opts.backend, &opts.backend_config)?;
    let kind = opts.backend;
    let ctx = Ctx {
        backend,
        sessions: Arc::new(SessionManager::new(opts.idle_timeout_secs)),
        run_id: opts.run_id,
        on_activity: opts.on_activity,
        login_store: opts.login_store,
        last_url: Arc::new(Mutex::new(HashMap::new())),
    };
    // Lightpanda never gets the interactive tools (its backend refuses
    // them too - this just keeps the tool list honest).
    let interactive = !kind.is_extraction_only();

    register_tool(
        reg,
        &ctx,
        "browser_navigate",
        &format!("Navigate the browser to a URL. {SEARCH_VS_BROWSE}"),
        serde_json::json!({ "url": str_prop("The URL to navigate to") }),
        &["url"],
        false,
        None,
        |_, v| Ok(vec!["navigate".into(), arg_str(&v, "url")?]),
    );
    // Everything between here and `browser_extract` is the interactive
    // surface: history, snapshots, refs, typing, waiting. Lightpanda
    // (extraction-only) skips it.
    if interactive {
        register_tool(
            reg,
            &ctx,
            "browser_back",
            "Go back one page in the browser history.",
            serde_json::json!({}),
            &[],
            false,
            None,
            |_, _| Ok(vec!["back".into()]),
        );
        register_tool(
            reg,
            &ctx,
            "browser_forward",
            "Go forward one page in the browser history.",
            serde_json::json!({}),
            &[],
            false,
            None,
            |_, _| Ok(vec!["forward".into()]),
        );
        register_tool(
        reg,
        &ctx,
        "browser_reload",
        "Reload the current page. Refs from the previous snapshot are invalidated - snapshot again before interacting.",
        serde_json::json!({}),
        &[],
        false,
        None,
        |_, _| Ok(vec!["reload".into()]),
    );
        register_tool(
        reg,
        &ctx,
        "browser_snapshot",
        &format!(
            "Capture the page's interactive elements as a list of refs like @v1:e1. \
             Always snapshot before clicking/typing: refs are INVALIDATED on any page change \
             (stale refs error - take a fresh snapshot, never retry the old ref). {SEARCH_VS_BROWSE}"
        ),
        serde_json::json!({}),
        &[],
        true,
        None,
        |_, _| Ok(vec!["snapshot".into()]),
    );
        register_tool(
            reg,
            &ctx,
            "browser_click_ref",
            "Click the element identified by a ref from the latest browser_snapshot (e.g. @v1:e1).",
            serde_json::json!({ "ref": str_prop("Ref from the latest snapshot, e.g. @v1:e1") }),
            &["ref"],
            false,
            None,
            |_, v| Ok(vec!["click-ref".into(), arg_str(&v, "ref")?]),
        );
        register_tool(
            reg,
            &ctx,
            "browser_hover_ref",
            "Hover over the element identified by a ref from the latest browser_snapshot.",
            serde_json::json!({ "ref": str_prop("Ref from the latest snapshot, e.g. @v1:e1") }),
            &["ref"],
            false,
            None,
            |_, v| Ok(vec!["hover-ref".into(), arg_str(&v, "ref")?]),
        );
        register_tool(
            reg,
            &ctx,
            "browser_fill_ref",
            "Fill a form field identified by a ref from the latest browser_snapshot with text.",
            serde_json::json!({
                "ref": str_prop("Ref from the latest snapshot, e.g. @v1:e1"),
                "text": str_prop("Text to fill into the field"),
            }),
            &["ref", "text"],
            false,
            None,
            |_, v| {
                Ok(vec![
                    "fill-ref".into(),
                    arg_str(&v, "ref")?,
                    arg_str(&v, "text")?,
                ])
            },
        );
        register_tool(
        reg,
        &ctx,
        "browser_click",
        "Click an element by CSS selector. Prefer browser_snapshot + browser_click_ref when you can see the element - selectors are brittle.",
        serde_json::json!({ "selector": str_prop("CSS selector of the element to click") }),
        &["selector"],
        false,
        None,
        |_, v| Ok(vec!["click".into(), arg_str(&v, "selector")?]),
    );
        register_tool(
        reg,
        &ctx,
        "browser_type",
        "Type text into an element by CSS selector. Prefer browser_snapshot + browser_fill_ref when you can see the field.",
        serde_json::json!({
            "selector": str_prop("CSS selector of the input"),
            "text": str_prop("Text to type"),
        }),
        &["selector", "text"],
        false,
        None,
        |_, v| {
            Ok(vec![
                "type".into(),
                arg_str(&v, "selector")?,
                arg_str(&v, "text")?,
            ])
        },
    );
        register_tool(
            reg,
            &ctx,
            "browser_press",
            "Press a keyboard key (e.g. Enter, Escape, Tab, ArrowDown).",
            serde_json::json!({ "key": str_prop("Key name, e.g. Enter") }),
            &["key"],
            false,
            None,
            |_, v| Ok(vec!["press".into(), arg_str(&v, "key")?]),
        );
        register_tool(
        reg,
        &ctx,
        "browser_wait_for",
        "Wait until a condition holds on the page. Conditions: selector_visible, selector_hidden, url_contains, text_visible, text_hidden, delay (pauses --value milliseconds), network_idle. Use after navigation before snapshotting a JS-heavy page.",
        serde_json::json!({
            "condition": str_prop("Condition to wait for: selector_visible | selector_hidden | url_contains | text_visible | text_hidden | delay | network_idle"),
            "value": str_prop("Condition value: CSS selector, URL substring, text, or milliseconds for delay"),
            "timeout_ms": str_prop("Maximum wait in milliseconds (gsd-browser only)"),
        }),
        &["condition"],
        false,
        None,
        |_, v| {
            let mut argv = vec![
                "wait-for".into(),
                "--condition".into(),
                arg_str(&v, "condition")?,
            ];
            if let Some(val) = arg_opt_str(&v, "value") {
                argv.push("--value".into());
                argv.push(val);
            }
            if let Some(ms) = arg_opt_str(&v, "timeout_ms") {
                argv.push("--timeout".into());
                argv.push(ms);
            }
            Ok(argv)
        },
    );

        // Credential autofill: snapshot → locate fields → fill from the
        // vault. Inside the interactive surface (Lightpanda has no
        // snapshot/fill), and only when a login store is wired - otherwise
        // the tool is not registered at all.
        register_fill_login(reg, &ctx);
    } // end of the interactive surface

    register_tool(
        reg,
        &ctx,
        "browser_extract",
        &format!(
            "Extract structured data from the page using a JSON schema whose properties carry \
             _selector / _attribute hints, e.g. {{\"properties\":{{\"title\":{{\"_selector\":\"h1\"}}}}}}. \
             For JS-heavy pages this sees what web_search snippets cannot. {SEARCH_VS_BROWSE}"
        ),
        serde_json::json!({
            "schema": str_prop("JSON schema with a \"properties\" object; each property is {\"_selector\": \"css\", \"_attribute\": \"href\"?} (a bare property map is wrapped automatically)"),
            "selector": str_prop("Optional CSS selector to scope extraction to part of the page"),
            "multiple": str_prop("Set to \"true\" to extract an array of items in container-selector mode"),
        }),
        &["schema"],
        true,
        None,
        |_, v| {
            let mut argv = vec![
                "extract".to_string(),
                "--schema".to_string(),
                normalize_extract_schema(&arg_str(&v, "schema")?),
            ];
            if let Some(sel) = arg_opt_str(&v, "selector") {
                argv.push("--selector".into());
                argv.push(sel);
            }
            if arg_opt_str(&v, "multiple").as_deref() == Some("true") {
                argv.push("--multiple".into());
            }
            Ok(argv)
        },
    );

    // browser_act: GSD only - it is the one backend with a semantic-intent
    // engine (`act --intent`, verified against gsd-browser 0.1.24).
    // `act` clicks the top intent candidate with NO minimum score
    // threshold upstream. Mirrors the git-push pattern: when
    // act_require_approval is on, the call picks up Capability::BrowserAct
    // so the run loop parks for human approval. The description steers
    // the model to the safer pattern first.
    if kind == BackendKind::Gsd {
        let act_extra: Option<ArgCapabilities> = if opts.act_require_approval {
            Some(Box::new(|_args: &str| vec![Capability::BrowserAct]))
        } else {
            None
        };
        register_tool(
        reg,
        &ctx,
        "browser_act",
        "AUTONOMOUS ACTION - use only as a last resort. Performs a low-confidence semantic action, clicking the top intent candidate with NO minimum score threshold. SAFER PATTERN FIRST: browser_snapshot, verify the target yourself, then browser_click_ref / browser_fill_ref on the exact ref. Approval-gated by policy.",
        serde_json::json!({
            "intent": str_prop("Semantic intent: submit_form | close_dialog | primary_cta | search_field | next_step | dismiss | auth_action | back_navigation"),
            "scope": str_prop("Optional CSS selector to narrow the search area"),
        }),
        &["intent"],
        false,
        act_extra,
        |_, v| {
            let mut argv = vec![
                "act".to_string(),
                "--intent".to_string(),
                arg_str(&v, "intent")?,
            ];
            if let Some(scope) = arg_opt_str(&v, "scope") {
                argv.push("--scope".into());
                argv.push(scope);
            }
            Ok(argv)
        },
    );
    }

    register_tool(
        reg,
        &ctx,
        "browser_screenshot",
        "Capture a screenshot of the current page to a file path.",
        serde_json::json!({
            "path": str_prop("File path to write the screenshot to"),
            "format": str_prop("Image format, e.g. png (default)"),
        }),
        &["path"],
        false,
        None,
        |_, v| {
            Ok(vec![
                "screenshot".into(),
                "--output".into(),
                arg_str(&v, "path")?,
                "--format".into(),
                arg_opt_str(&v, "format").unwrap_or_else(|| "png".to_string()),
            ])
        },
    );
    register_tool(
        reg,
        &ctx,
        "browser_page_source",
        &format!(
            "Return the current page's raw HTML source. Large output is compacted. \
             Prefer browser_extract for readable content; use this when you need markup or scripts. {SEARCH_VS_BROWSE}"
        ),
        serde_json::json!({}),
        &[],
        true,
        None,
        |_, _| Ok(vec!["page-source".into()]),
    );

    Ok(())
}
