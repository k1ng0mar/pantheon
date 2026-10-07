//! Runtime-side tool configuration for browser automation and web search.
//!
//! These are the resolved, concrete configs the agent loop reads at tool
//! registration time. The TUI's `[browser]` / `[websearch]` config
//! sections resolve into these (see `pantheon-tui`'s `BrowserSection` /
//! `WebsearchSection`), with environment variables winning over the file.

use pantheon_web::browser::registry::BackendKind;
use std::path::PathBuf;

/// Runtime config for the `browser_*` tools (gsd-browser subprocess).
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserToolConfig {
    /// Master switch. When false no `browser_*` tools are registered.
    pub enabled: bool,
    /// Which browser backend drives the tools. Default
    /// [`BackendKind::Gsd`].
    pub backend: BackendKind,
    /// Path to the gsd-browser binary. `None` = resolved from PATH.
    pub binary: Option<PathBuf>,
    /// `browser_act` clicks the top semantic-intent candidate with no
    /// minimum score upstream. True = the call carries the `browser.act`
    /// capability and the run parks for human approval.
    pub act_require_approval: bool,
    /// Idle seconds before a run's browser daemon is stopped.
    pub idle_timeout_secs: u64,
    /// Secret name holding the gsd-browser auth-vault key, resolved via
    /// the secrets broker and injected as `GSD_BROWSER_VAULT_KEY`.
    /// `None` = no vault.
    pub vault_key_secret: Option<String>,
    /// Secret name holding the Steel API key, resolved via the secrets
    /// broker. Default "STEEL_API_KEY". `None` = Steel backend unusable.
    pub steel_api_key_secret: Option<String>,
    /// Steel REST base URL override (self-hosted Steel).
    /// `None` = the Steel cloud.
    pub steel_base_url: Option<String>,
    /// Secret name holding the Browserbase API key, resolved via the
    /// secrets broker. Default "BROWSERBASE_API_KEY".
    /// `None` = Browserbase backend unusable.
    pub browserbase_api_key_secret: Option<String>,
    /// Browserbase project id (required by `POST /v1/sessions`).
    pub browserbase_project_id: Option<String>,
    /// CDP websocket URL of a running Lightpanda server. The only
    /// transport; `None` = Lightpanda backend unusable.
    pub lightpanda_cdp_url: Option<String>,
    /// Path to the `playwright-cli` binary. `None` = resolved from PATH.
    pub playwright_binary: Option<PathBuf>,
    /// Path to the Chrome/Chromium binary for the `chromiumoxide`
    /// backend. `None` = chromiumoxide's auto-detect.
    pub chrome_binary: Option<PathBuf>,
    /// Headless mode for the `chromiumoxide` backend.
    pub headless: bool,
    /// Per-command timeout in seconds for browser backends.
    pub timeout_secs: u64,
    /// Camoufox anti-detect settings (`[browser.camofox]`), resolved
    /// with defaults applied.
    pub camofox: CamofoxToolConfig,
}

impl Default for BrowserToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: BackendKind::Gsd,
            binary: None,
            act_require_approval: true,
            idle_timeout_secs: 900,
            vault_key_secret: Some("GSD_BROWSER_VAULT_KEY".to_string()),
            steel_api_key_secret: Some("STEEL_API_KEY".to_string()),
            steel_base_url: None,
            browserbase_api_key_secret: Some("BROWSERBASE_API_KEY".to_string()),
            browserbase_project_id: None,
            lightpanda_cdp_url: None,
            playwright_binary: None,
            chrome_binary: None,
            headless: true,
            timeout_secs: 120,
            camofox: CamofoxToolConfig::default(),
        }
    }
}

/// Runtime config for the Camoufox anti-detect browser backend. These
/// are the resolved `[browser.camofox]` values (defaults applied); the
/// proxy password itself resolves at registration via
/// `proxy_password_secret` and is never stored here.
#[derive(Debug, Clone, PartialEq)]
pub struct CamofoxToolConfig {
    /// Python interpreter for the shim. `None` = `python3` on PATH.
    pub python: Option<PathBuf>,
    /// Launch headless.
    pub headless: bool,
    /// Xvfb "virtual" headless mode instead of true headless (needs
    /// `xvfb`).
    pub headless_virtual: bool,
    /// Fingerprint OS: `windows` | `macos` | `linux`.
    /// `None` = the launcher's BrowserForge default.
    pub os: Option<String>,
    /// Human-like cursor movement, in seconds. `None` = off.
    pub humanize_secs: Option<f64>,
    /// Derive timezone/locale/geolocation from the proxy IP (needs the
    /// `[geoip]` install extra).
    pub geoip: bool,
    /// Explicit locale override, e.g. `"en-US"`.
    pub locale: Option<String>,
    /// Explicit timezone override, e.g. `"America/New_York"`.
    pub timezone: Option<String>,
    /// Proxy server URL, e.g. `"http://proxy:8080"`.
    pub proxy_server: Option<String>,
    /// Proxy username (optional).
    pub proxy_username: Option<String>,
    /// Secret name holding the proxy password, resolved via the secrets
    /// broker at registration. Never a raw value.
    pub proxy_password_secret: Option<String>,
    /// Block image loads (perf).
    pub block_images: bool,
    /// Block WebRTC (IP-leak prevention).
    pub block_webrtc: bool,
    /// Use BrowserForge-backed real fingerprint presets.
    pub fingerprint_preset: bool,
    /// Per-command timeout in seconds.
    pub timeout_secs: u64,
    /// Seconds to wait for the browser to launch on session start.
    /// First launch after a fetch can take minutes.
    pub startup_timeout_secs: u64,
}

impl Default for CamofoxToolConfig {
    /// Matches [`pantheon_web::browser::CamofoxConfig`]'s defaults.
    fn default() -> Self {
        Self {
            python: None,
            headless: true,
            headless_virtual: false,
            os: None,
            humanize_secs: None,
            geoip: false,
            locale: None,
            timezone: None,
            proxy_server: None,
            proxy_username: None,
            proxy_password_secret: None,
            block_images: false,
            block_webrtc: false,
            fingerprint_preset: true,
            timeout_secs: 120,
            startup_timeout_secs: 300,
        }
    }
}

/// Runtime config for the Cloudflare integration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CloudflareToolConfig {
    /// Master switch. `false` = the shell tool never injects a
    /// Cloudflare token into any child, whatever the broker holds.
    pub enabled: bool,
    /// Secret name holding the API token, resolved via the secrets
    /// broker at call time. `None` = `CLOUDFLARE_API_TOKEN`.
    pub api_token_secret: Option<String>,
}

/// Runtime config for the `web_search` tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebsearchToolConfig {
    /// Master switch. The tool is only registered when a key also
    /// resolves - a keyless `web_search` would be a tool that can never
    /// work, so it stays out of the model's tool list.
    pub enabled: bool,
    /// Provider name: a `pantheon-web` registry id (`tavily`, `exa`,
    /// `marginalia`, ...). An unknown id leaves `web_search` unregistered
    /// with a warning.
    pub provider: String,
    /// Secret name holding the provider API key, resolved via the
    /// secrets broker. Never hardcoded. `None` = the provider's own
    /// default (via `pantheon_web::websearch::default_key_env`), so a
    /// provider switch without an explicit secret name still resolves
    /// the right variable instead of a stale Tavily one.
    pub api_key_secret: Option<String>,
    /// Endpoint override for self-hosted providers. `None` = the
    /// provider's default endpoint.
    pub base_url: Option<String>,
    /// Default result count per query.
    pub max_results: u8,
}

/// Runtime config for desktop computer use (the CUA driver).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputerToolConfig {
    /// Master switch, joined with the ComputerUse tool-group toggle.
    /// When false no driver tools are registered.
    pub enabled: bool,
    /// Driver id. Today only `cua-driver`.
    pub driver: String,
    /// Path to the driver binary. `None` = resolved from PATH.
    pub binary: Option<PathBuf>,
}

impl Default for ComputerToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            driver: crate::computer::CUA_DRIVER_ID.to_string(),
            binary: None,
        }
    }
}

impl Default for WebsearchToolConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "tinyfish".to_string(),
            api_key_secret: None,
            base_url: None,
            max_results: 8,
        }
    }
}

/// Runtime config for MCP servers: `[mcp]` + migration declarations,
/// resolved into server specs by the TUI at session startup.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct McpToolConfig {
    /// Master switch. When false the manager holds no specs and no
    /// server is launched, connected, or registered.
    pub enabled: bool,
    /// Resolved server specs (config section first, declarations fill gaps).
    pub servers: Vec<pantheon_mcp::manager::McpServerSpec>,
}

/// Tool-group enablement resolved from `[tools]` in config.toml: which
/// tool groups the runtime registers. A disabled group never appears in
/// the model's tool list - the registration call sites consult this,
/// never the config section directly.
///
/// Session search is deliberately absent: it is default-on and always
/// registered, so there is nothing to resolve for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolEnablement {
    pub web_search: bool,
    pub browser: bool,
    pub terminal: bool,
    pub files: bool,
    pub memory: bool,
    pub skills: bool,
    pub tasks: bool,
    pub delegation: bool,
    pub ask_user: bool,
    pub vault: bool,
    pub voice: bool,
    pub vision: bool,
    pub video_analysis: bool,
    pub computer_use: bool,
    pub plugins: bool,
    pub code_intel: bool,
}

impl Default for ToolEnablement {
    /// Absent `[tools]` section = every group on, matching the
    /// pre-section behavior exactly.
    fn default() -> Self {
        Self {
            web_search: true,
            browser: true,
            terminal: true,
            files: true,
            memory: true,
            skills: true,
            tasks: true,
            delegation: true,
            ask_user: true,
            vault: true,
            voice: true,
            vision: true,
            video_analysis: true,
            computer_use: true,
            plugins: true,
            code_intel: true,
        }
    }
}

impl ToolEnablement {
    /// Every group off. For tests: a disabled group must not appear in
    /// the registry.
    pub fn none() -> Self {
        Self {
            web_search: false,
            browser: false,
            terminal: false,
            files: false,
            memory: false,
            skills: false,
            tasks: false,
            delegation: false,
            ask_user: false,
            vault: false,
            voice: false,
            vision: false,
            video_analysis: false,
            computer_use: false,
            plugins: false,
            code_intel: false,
        }
    }

    /// Resolve from the `[tools]` config section. Absent flags stay on.
    pub fn from_section(s: &pantheon_api::config::ToolsSection) -> Self {
        let mut e = Self::default();
        let get = |v: Option<bool>| v.unwrap_or(true);
        e.web_search = get(s.web_search);
        e.browser = get(s.browser);
        e.terminal = get(s.terminal);
        e.files = get(s.files);
        e.memory = get(s.memory);
        e.skills = get(s.skills);
        e.tasks = get(s.tasks);
        e.delegation = get(s.delegation);
        e.ask_user = get(s.ask_user);
        e.vault = get(s.vault);
        e.voice = get(s.voice);
        e.vision = get(s.vision);
        e.video_analysis = get(s.video_analysis);
        e.computer_use = get(s.computer_use);
        e.plugins = get(s.plugins);
        e.code_intel = get(s.code_intel);
        e
    }

    /// Whether the group registers its tools.
    pub fn is_enabled(&self, group: pantheon_api::config::ToolGroup) -> bool {
        use pantheon_api::config::ToolGroup as G;
        match group {
            G::WebSearch => self.web_search,
            G::Browser => self.browser,
            G::Terminal => self.terminal,
            G::Files => self.files,
            G::Memory => self.memory,
            G::Skills => self.skills,
            G::Tasks => self.tasks,
            G::Delegation => self.delegation,
            G::AskUser => self.ask_user,
            G::Vault => self.vault,
            G::Voice => self.voice,
            G::Vision => self.vision,
            G::VideoAnalysis => self.video_analysis,
            G::ComputerUse => self.computer_use,
            G::Plugins => self.plugins,
            G::CodeIntel => self.code_intel,
        }
    }
}

fn on(value: Option<bool>, default: bool) -> bool {
    value.unwrap_or(default)
}

/// Resolve `[browser]` to the runtime [`BrowserToolConfig`]. Env vars
/// (`PANTHEON_BROWSER_ENABLED`, `PANTHEON_BROWSER_BINARY`,
/// `PANTHEON_BROWSER_BACKEND`) win over the file, matching the runtime
/// defaults.
///
/// Lives here (not in the TUI) so every client of the runtime - the TUI
/// agent loop and the dashboard's browser stream endpoints - resolves the
/// section identically.
pub fn resolve_browser_section(s: &pantheon_api::config::BrowserSection) -> BrowserToolConfig {
    let mut cfg = BrowserToolConfig::default();
    cfg.enabled = std::env::var("PANTHEON_BROWSER_ENABLED")
        .map(|v| v != "0")
        .unwrap_or_else(|_| on(s.enabled, cfg.enabled));
    // Backend selection: env, then file, then the default. Unknown ids
    // warn and fall back to GSD (never fail the session over a typo).
    let backend_id = std::env::var("PANTHEON_BROWSER_BACKEND")
        .ok()
        .or_else(|| s.backend.clone());
    if let Some(id) = backend_id {
        match BackendKind::parse(&id) {
            Some(kind) => cfg.backend = kind,
            None => log_warn!("[pantheon] unknown [browser] backend '{id}'; falling back to 'gsd'"),
        }
    }
    if cfg.binary.is_none() {
        cfg.binary = s.binary.clone().map(std::path::PathBuf::from);
    }
    cfg.act_require_approval = on(s.act_require_approval, cfg.act_require_approval);
    if let Some(secs) = s.idle_timeout_secs.filter(|&s| s > 0) {
        cfg.idle_timeout_secs = secs;
    }
    if let Some(name) = s.vault_key_secret.clone() {
        if !name.trim().is_empty() {
            cfg.vault_key_secret = Some(name);
        }
    }
    if let Some(name) = s.steel_api_key_secret.clone() {
        if !name.trim().is_empty() {
            cfg.steel_api_key_secret = Some(name);
        }
    }
    if let Some(url) = s.steel_base_url.clone() {
        if !url.trim().is_empty() {
            cfg.steel_base_url = Some(url);
        }
    }
    if let Some(name) = s.browserbase_api_key_secret.clone() {
        if !name.trim().is_empty() {
            cfg.browserbase_api_key_secret = Some(name);
        }
    }
    if let Some(id) = s.browserbase_project_id.clone() {
        if !id.trim().is_empty() {
            cfg.browserbase_project_id = Some(id);
        }
    }
    if let Some(url) = s.lightpanda_cdp_url.clone() {
        if !url.trim().is_empty() {
            cfg.lightpanda_cdp_url = Some(url);
        }
    }
    if let Some(bin) = s.playwright_binary.clone() {
        if !bin.trim().is_empty() {
            cfg.playwright_binary = Some(std::path::PathBuf::from(bin));
        }
    }
    if let Some(bin) = s.chrome_binary.clone() {
        if !bin.trim().is_empty() {
            cfg.chrome_binary = Some(std::path::PathBuf::from(bin));
        }
    }
    if let Some(headless) = s.headless {
        cfg.headless = headless;
    }
    if let Some(secs) = s.timeout_secs.filter(|&s| s > 0) {
        cfg.timeout_secs = secs;
    }
    // `[browser.camofox]`: the Camoufox backend's launcher and
    // fingerprint options. Absent = the backend defaults; a
    // camofox-specific timeout wins over the global `[browser]` one.
    if let Some(cs) = &s.camofox {
        let c = &mut cfg.camofox;
        if let Some(p) = cs.python.clone() {
            if !p.trim().is_empty() {
                c.python = Some(std::path::PathBuf::from(p));
            }
        }
        if let Some(headless) = cs.headless {
            c.headless = headless;
        }
        if let Some(hv) = cs.headless_virtual {
            c.headless_virtual = hv;
        }
        if let Some(os) = cs.os.clone() {
            if !os.trim().is_empty() {
                c.os = Some(os);
            }
        }
        if let Some(hs) = cs.humanize_secs {
            c.humanize_secs = Some(hs);
        }
        if let Some(geoip) = cs.geoip {
            c.geoip = geoip;
        }
        if let Some(locale) = cs.locale.clone() {
            if !locale.trim().is_empty() {
                c.locale = Some(locale);
            }
        }
        if let Some(tz) = cs.timezone.clone() {
            if !tz.trim().is_empty() {
                c.timezone = Some(tz);
            }
        }
        if let Some(server) = cs.proxy_server.clone() {
            if !server.trim().is_empty() {
                c.proxy_server = Some(server);
            }
        }
        if let Some(user) = cs.proxy_username.clone() {
            if !user.trim().is_empty() {
                c.proxy_username = Some(user);
            }
        }
        if let Some(name) = cs.proxy_password_secret.clone() {
            if !name.trim().is_empty() {
                c.proxy_password_secret = Some(name);
            }
        }
        if let Some(block) = cs.block_images {
            c.block_images = block;
        }
        if let Some(block) = cs.block_webrtc {
            c.block_webrtc = block;
        }
        if let Some(fp) = cs.fingerprint_preset {
            c.fingerprint_preset = fp;
        }
        if let Some(secs) = cs.timeout_secs.filter(|&s| s > 0) {
            c.timeout_secs = secs;
        } else {
            c.timeout_secs = cfg.timeout_secs;
        }
        if let Some(secs) = cs.startup_timeout_secs.filter(|&s| s > 0) {
            c.startup_timeout_secs = secs;
        }
    } else {
        // No `[browser.camofox]` section: keep the backend's timeout in
        // line with the global `[browser] timeout_secs`.
        cfg.camofox.timeout_secs = cfg.timeout_secs;
    }
    cfg
}

/// Build the [`BackendConfig`] for a resolved [`BrowserToolConfig`],
/// resolving secret names through `secrets`.
///
/// Pure mapping (no I/O, no browser launch): the agent loop uses this at
/// tool-registration time, and the dashboard's browser stream/input
/// endpoints use it to drive the identical backend.
pub fn browser_backend_config(
    cfg: &BrowserToolConfig,
    secrets: &pantheon_secrets::SecretsBroker,
) -> pantheon_web::browser::registry::BackendConfig {
    use pantheon_web::browser as b;
    let resolve_secret = |name: Option<&str>| {
        name.and_then(|n| secrets.resolve(n).ok().flatten())
            .map(|v| v.expose().to_owned())
    };
    let timeout = cfg.timeout_secs;
    b::registry::BackendConfig {
        gsd_binary: cfg.binary.clone(),
        gsd_vault_key: resolve_secret(cfg.vault_key_secret.as_deref()),
        native: b::NativeConfig {
            chrome_binary: cfg.chrome_binary.clone(),
            headless: Some(cfg.headless),
            timeout_secs: timeout,
        },
        steel: b::SteelConfig {
            api_key: resolve_secret(cfg.steel_api_key_secret.as_deref()),
            base_url: cfg.steel_base_url.clone(),
            timeout_secs: timeout,
        },
        browserbase: b::BrowserbaseConfig {
            api_key: resolve_secret(cfg.browserbase_api_key_secret.as_deref()),
            project_id: cfg.browserbase_project_id.clone(),
            base_url: None,
            timeout_secs: timeout,
        },
        lightpanda: b::LightpandaConfig {
            cdp_url: cfg.lightpanda_cdp_url.clone(),
            timeout_secs: timeout,
        },
        playwright: b::PlaywrightConfig {
            binary: cfg.playwright_binary.clone(),
            timeout_secs: timeout,
        },
        // `[browser.camofox]` arrives resolved (defaults applied). The
        // proxy password resolves here, at build time, like the other
        // secret values - never stored, never logged.
        camofox: b::CamofoxConfig {
            python: cfg.camofox.python.clone(),
            headless: cfg.camofox.headless,
            headless_virtual: cfg.camofox.headless_virtual,
            os: cfg.camofox.os.clone(),
            humanize_secs: cfg.camofox.humanize_secs,
            geoip: cfg.camofox.geoip,
            locale: cfg.camofox.locale.clone(),
            timezone: cfg.camofox.timezone.clone(),
            proxy_server: cfg.camofox.proxy_server.clone(),
            proxy_username: cfg.camofox.proxy_username.clone(),
            proxy_password: resolve_secret(cfg.camofox.proxy_password_secret.as_deref()),
            block_images: cfg.camofox.block_images,
            block_webrtc: cfg.camofox.block_webrtc,
            fingerprint_preset: cfg.camofox.fingerprint_preset,
            timeout_secs: cfg.camofox.timeout_secs,
            startup_timeout_secs: cfg.camofox.startup_timeout_secs,
            ..Default::default()
        },
        timeout_secs: timeout,
    }
}

/// Build the configured browser backend from a resolved
/// [`BrowserToolConfig`], resolving secret names through `secrets`.
///
/// Returns `None` when the browser tool is disabled - callers treat that
/// as "browser not available", not an error. Secrets resolve here (build
/// time), like at tool registration - they are never stored, never
/// logged.
pub fn build_browser_backend(
    cfg: &BrowserToolConfig,
    secrets: &pantheon_secrets::SecretsBroker,
) -> Result<
    Option<std::sync::Arc<dyn pantheon_web::browser::BrowserBackend>>,
    pantheon_api::error::PantheonError,
> {
    if !cfg.enabled {
        return Ok(None);
    }
    let backend_config = browser_backend_config(cfg, secrets);
    pantheon_web::browser::build_backend(cfg.backend, &backend_config)
        .map(Some)
        .map_err(pantheon_api::error::PantheonError::from)
}
