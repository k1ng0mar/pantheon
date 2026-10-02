//! Backend registry: the seven-backend lineup behind [`BrowserBackend`].
//!
//! [`BackendKind`] is the stable id selected via `[browser] backend` in
//! config. [`BackendConfig`] carries the resolved per-backend settings
//! (secrets are resolved by the parent through `pantheon-secrets` and
//! arrive here as values - never logged, never re-resolved).
//! [`build_backend`] is the construction seam the tool layer calls.

use super::backend::BrowserBackend;
use super::browserbase::{BrowserbaseBackend, BrowserbaseConfig};
use super::camofox::{CamofoxBackend, CamofoxConfig};
use super::error::BrowserError;
use super::lightpanda::{LightpandaBackend, LightpandaConfig};
use super::native::{NativeBackend, NativeConfig};
use super::playwright::{PlaywrightBackend, PlaywrightConfig};
use super::steel::{SteelBackend, SteelConfig};
use super::subprocess::SubprocessBackend;
use std::path::PathBuf;
use std::sync::Arc;

/// Stable backend ids. These are the `[browser] backend` config values
/// never rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackendKind {
    /// `gsd-browser` CLI subprocess. Default interactive backend.
    Gsd,
    /// Raw CDP via exactly-pinned `chromiumoxide` 0.9.1, local Chrome.
    /// Dependency-free fallback (no CLI, no subprocess).
    ChromiumOxide,
    /// REST session management + raw CDP websocket, through the official
    /// `steel-rs` SDK. Cloud (`https://api.steel.dev`) and self-host
    /// Docker share the API - one implementation, configurable base URL.
    Steel,
    /// Cloud fallback: `POST /v1/sessions` returns a raw CDP `wss://`
    /// URL, driven over that websocket directly.
    Browserbase,
    /// Extraction specialist: CDP to Lightpanda. Fetch/extract workloads
    /// only - never the interactive backend.
    Lightpanda,
    /// Mature fallback: `playwright-cli` (`@playwright/cli`) subprocess.
    Playwright,
    /// Anti-detect fallback: patched-Firefox (Camoufox) through a
    /// long-lived Python shim over JSON-over-stdio. No CDP - the
    /// launcher speaks the patched Juggler protocol.
    Camofox,
}

impl BackendKind {
    /// All backends, in config-picker order (default first).
    pub fn all() -> [BackendKind; 7] {
        [
            BackendKind::Gsd,
            BackendKind::ChromiumOxide,
            BackendKind::Steel,
            BackendKind::Browserbase,
            BackendKind::Lightpanda,
            BackendKind::Playwright,
            BackendKind::Camofox,
        ]
    }

    /// The `[browser] backend` config value.
    pub fn id(self) -> &'static str {
        match self {
            BackendKind::Gsd => "gsd",
            BackendKind::ChromiumOxide => "chromiumoxide",
            BackendKind::Steel => "steel",
            BackendKind::Browserbase => "browserbase",
            BackendKind::Lightpanda => "lightpanda",
            BackendKind::Playwright => "playwright",
            BackendKind::Camofox => "camofox",
        }
    }

    /// Parse a `[browser] backend` value (case-insensitive, trimmed).
    /// Unknown values become `None` so the caller can warn and fall back
    /// to the default instead of failing the whole registration.
    pub fn parse(s: &str) -> Option<BackendKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gsd" | "gsd-browser" => Some(BackendKind::Gsd),
            "chromiumoxide" | "chromium-oxide" | "native" | "cdp" => {
                Some(BackendKind::ChromiumOxide)
            }
            "steel" => Some(BackendKind::Steel),
            "browserbase" => Some(BackendKind::Browserbase),
            "lightpanda" => Some(BackendKind::Lightpanda),
            "playwright" | "playwright-cli" => Some(BackendKind::Playwright),
            // Upstream spells it "Camoufox"; accept both.
            "camofox" | "camoufox" => Some(BackendKind::Camofox),
            _ => None,
        }
    }

    /// True for the extraction-only backend: the tool layer registers the
    /// reduced fetch/extract surface instead of the interactive one.
    pub fn is_extraction_only(self) -> bool {
        matches!(self, BackendKind::Lightpanda)
    }
}

impl std::fmt::Display for BackendKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// One row describing a backend: role and cost honesty for config docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendInfo {
    pub kind: BackendKind,
    /// Human display name.
    pub name: &'static str,
    /// The backend's role in the lineup.
    pub role: &'static str,
    /// What the user must supply to use it.
    pub needs: &'static str,
}

/// Every backend with its role, in picker order.
pub fn all_backends() -> Vec<BackendInfo> {
    vec![
        BackendInfo {
            kind: BackendKind::Gsd,
            name: "GSD Browser",
            role: "Default interactive backend: versioned refs, semantic act (act --intent), vault, stealth.",
            needs: "gsd-browser on PATH (or [browser] binary) plus Chrome/Chromium on the host.",
        },
        BackendInfo {
            kind: BackendKind::ChromiumOxide,
            name: "Native CDP (chromiumoxide)",
            role: "Local raw-CDP fallback: exactly-pinned chromiumoxide 0.9.1, no CLI or Node sidecar.",
            needs: "Chrome/Chromium on the host. Pantheon builds the ref model itself on the AX tree.",
        },
        BackendInfo {
            kind: BackendKind::Steel,
            name: "Steel",
            role: "Hosted or self-hosted browser sessions over REST + CDP via the official steel-rs SDK.",
            needs: "STEEL_API_KEY (cloud) or a self-hosted Steel base URL.",
        },
        BackendInfo {
            kind: BackendKind::Browserbase,
            name: "Browserbase",
            role: "Cloud fallback: session REST API, raw CDP websocket per session.",
            needs: "BROWSERBASE_API_KEY and a Browserbase project id.",
        },
        BackendInfo {
            kind: BackendKind::Lightpanda,
            name: "Lightpanda",
            role: "Extraction specialist only: fetch/extract workloads. Never the interactive backend.",
            needs: "`browser.lightpanda_cdp_url` pointing at a running `lightpanda serve` CDP endpoint.",
        },
        BackendInfo {
            kind: BackendKind::Playwright,
            name: "Playwright CLI",
            role: "Mature fallback: industry-standard engine via the official stateful agent CLI.",
            needs: "`playwright-cli` (@playwright/cli) on PATH; downloads its own browsers.",
        },
        BackendInfo {
            kind: BackendKind::Camofox,
            name: "Camoufox",
            role: "Anti-detect fallback: patched Firefox via the official Python launcher (Juggler, no CDP), driven through a JSON-over-stdio shim.",
            needs: "`pip install \"camoufox[geoip]\"` plus `python -m camoufox fetch`; configure fingerprint in `[browser.camofox]`.",
        },
    ]
}

/// Resolved per-backend settings. Secrets arrive as resolved values from
/// the parent (via `pantheon-secrets`); this struct never resolves or
/// logs them.
#[derive(Debug, Clone, Default)]
pub struct BackendConfig {
    /// `gsd-browser` binary path (`None` = PATH) and resolved vault key.
    pub gsd_binary: Option<PathBuf>,
    /// Resolved `GSD_BROWSER_VAULT_KEY` value. Injected per-spawn, never logged.
    pub gsd_vault_key: Option<String>,
    /// Native CDP settings.
    pub native: NativeConfig,
    /// Steel settings: resolved API key + base URL.
    pub steel: SteelConfig,
    /// Browserbase settings: resolved API key + project id.
    pub browserbase: BrowserbaseConfig,
    /// Lightpanda settings: CDP URL or binary.
    pub lightpanda: LightpandaConfig,
    /// Playwright settings: CLI binary path.
    pub playwright: PlaywrightConfig,
    /// Camoufox settings: Python shim + fingerprint options
    /// (`[browser.camofox]`).
    pub camofox: CamofoxConfig,
    /// Seconds before a backend call is killed. `0` = default (120s).
    pub timeout_secs: u64,
}

impl BackendConfig {
    fn timeout(&self) -> u64 {
        if self.timeout_secs == 0 {
            120
        } else {
            self.timeout_secs
        }
    }

    /// Validate the settings for `kind` before construction: fail fast
    /// with an actionable message instead of failing on first tool call.
    /// Secret *values* are never included in the message - only whether
    /// one was provided.
    pub fn validate(&self, kind: BackendKind) -> Result<(), BrowserError> {
        match kind {
            BackendKind::Gsd | BackendKind::Playwright | BackendKind::ChromiumOxide => Ok(()),
            BackendKind::Steel => self.steel.validate(),
            BackendKind::Browserbase => self.browserbase.validate(),
            BackendKind::Lightpanda => self.lightpanda.validate(),
            BackendKind::Camofox => self.camofox.validate(),
        }
    }
}

/// Build the backend for `kind`. Validation failures are returned here
/// (missing API key etc.) so tool registration can report them instead
/// of registering dead tools. Construction is lazy: no network, no
/// subprocess, no browser launch happens here - backends connect on
/// first `invoke`.
pub fn build_backend(
    kind: BackendKind,
    cfg: &BackendConfig,
) -> Result<Arc<dyn BrowserBackend>, BrowserError> {
    cfg.validate(kind)?;
    let timeout = cfg.timeout();
    let backend: Arc<dyn BrowserBackend> = match kind {
        BackendKind::Gsd => {
            let binary = cfg
                .gsd_binary
                .clone()
                .unwrap_or_else(|| PathBuf::from("gsd-browser"));
            let mut b = SubprocessBackend::new(binary).with_timeout(timeout);
            if let Some(key) = &cfg.gsd_vault_key {
                b.extra_env
                    .insert("GSD_BROWSER_VAULT_KEY".to_string(), key.clone());
            }
            Arc::new(b)
        }
        BackendKind::ChromiumOxide => Arc::new(NativeBackend::new(NativeConfig {
            timeout_secs: timeout,
            ..cfg.native.clone()
        })?),
        BackendKind::Steel => Arc::new(SteelBackend::new(SteelConfig {
            timeout_secs: timeout,
            ..cfg.steel.clone()
        })),
        BackendKind::Browserbase => Arc::new(BrowserbaseBackend::new(BrowserbaseConfig {
            timeout_secs: timeout,
            ..cfg.browserbase.clone()
        })),
        BackendKind::Lightpanda => Arc::new(LightpandaBackend::new(LightpandaConfig {
            timeout_secs: timeout,
            ..cfg.lightpanda.clone()
        })?),
        BackendKind::Playwright => Arc::new(PlaywrightBackend::new(PlaywrightConfig {
            timeout_secs: timeout,
            ..cfg.playwright.clone()
        })),
        BackendKind::Camofox => Arc::new(CamofoxBackend::new(CamofoxConfig {
            timeout_secs: timeout,
            ..cfg.camofox.clone()
        })),
    };
    Ok(backend)
}
