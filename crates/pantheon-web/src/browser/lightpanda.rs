//! Lightpanda backend: extraction and fetch only, never interactive.
//!
//! Lightpanda is a from-scratch headless browser engine (own HTML
//! parser/DOM, V8 for JS — not Chromium) built for scraping at ~10x
//! lower memory than Chrome. Pantheon uses it strictly for page
//! fetch/extraction through the shared raw-CDP
//! [`CdpDriver`](super::cdp::CdpDriver): `navigate`, `extract`,
//! `page-source`, and `screenshot`. Any interactive command (`click`,
//! `fill`, `type`, `press`, `act`, waits, history) is rejected up front
//! with [`BrowserError::UnsupportedCommand`] before any connection is
//! made — this is an architectural guardrail, not a runtime limitation.
//!
//! Transport: connect to an already-running Lightpanda CDP server
//! (`lightpanda serve`). Binary launch is deliberately NOT supported:
//! Lightpanda's CLI is not Chromium-flag-compatible, so there is no
//! honest way to drive it through chromiumoxide's `BrowserConfig`.
//! Run `lightpanda serve` yourself and point
//! `browser.lightpanda_cdp_url` at it.

use super::backend::BrowserBackend;
use super::cdp::CdpDriver;
use super::error::BrowserError;

/// Commands Lightpanda is allowed to run. Everything else is refused.
pub const ALLOWED_COMMANDS: &[&str] = &["navigate", "extract", "page-source", "screenshot"];

/// Lightpanda backend configuration.
#[derive(Debug, Clone, Default)]
pub struct LightpandaConfig {
    /// CDP websocket URL of a running Lightpanda server
    /// (`lightpanda serve`). The only transport.
    pub cdp_url: Option<String>,
    /// Command timeout, in seconds.
    pub timeout_secs: u64,
}

impl LightpandaConfig {
    /// Fail fast when no CDP URL is configured.
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self
            .cdp_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        {
            Ok(())
        } else {
            Err(BrowserError::Failed {
                argv: Vec::new(),
                exit: None,
                stderr: "lightpanda backend needs `browser.lightpanda_cdp_url` \
                         pointing at a running `lightpanda serve` CDP endpoint"
                    .to_string(),
            })
        }
    }
}

/// Extraction-only browser backend.
pub struct LightpandaBackend {
    driver: CdpDriver,
}

impl LightpandaBackend {
    pub fn new(config: LightpandaConfig) -> Result<Self, BrowserError> {
        config.validate()?;
        let url = config.cdp_url.expect("validated").trim().to_string();
        let driver =
            CdpDriver::connect_lazy(url, "lightpanda")?.with_op_timeout(config.timeout_secs);
        Ok(Self { driver })
    }

    /// The allowlist check, pure and unit-testable.
    pub fn check_allowed(argv: &[String]) -> Result<(), BrowserError> {
        let cmd = argv.first().map(String::as_str).unwrap_or("");
        if ALLOWED_COMMANDS.contains(&cmd) {
            Ok(())
        } else {
            Err(BrowserError::UnsupportedCommand {
                command: cmd.to_string(),
                backend: "lightpanda".to_string(),
            })
        }
    }
}

impl BrowserBackend for LightpandaBackend {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        // Refuse interactive commands before connecting anywhere.
        Self::check_allowed(argv)?;
        self.driver.execute(argv, session)
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        self.driver.close_session(session);
        Ok(())
    }
}
