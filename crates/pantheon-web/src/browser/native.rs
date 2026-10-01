//! Native backend: raw CDP driven locally through exactly-pinned
//! `chromiumoxide` 0.9.1.
//!
//! This is the "raw CDP" entry in the browser lineup: a locally launched
//! Chromium, driven over CDP with no daemon, no CLI, and no model in the
//! loop. Interactive commands run through the shared
//! [`CdpDriver`](super::cdp::CdpDriver); `act --intent` stays
//! unsupported because there is no agent here to interpret intents.

use super::backend::BrowserBackend;
use super::cdp::CdpDriver;
use super::error::BrowserError;
use std::path::PathBuf;

/// Native (raw CDP) backend configuration.
#[derive(Debug, Clone, Default)]
pub struct NativeConfig {
    /// Chrome/Chromium executable. `None` = chromiumoxide's auto-detect.
    pub chrome_binary: Option<PathBuf>,
    /// Headless mode. Defaults to `true`.
    pub headless: Option<bool>,
    /// Command timeout, in seconds.
    pub timeout_secs: u64,
}

impl NativeConfig {
    fn headless(&self) -> bool {
        self.headless.unwrap_or(true)
    }
}

/// Local raw-CDP browser backend.
pub struct NativeBackend {
    driver: CdpDriver,
}

impl NativeBackend {
    pub fn new(config: NativeConfig) -> Result<Self, BrowserError> {
        let timeout = config.timeout_secs.max(1);
        let headless = config.headless();
        let driver = CdpDriver::launch_lazy(config.chrome_binary, headless, "chromiumoxide")
            .with_op_timeout(timeout);
        Ok(Self { driver })
    }
}

impl BrowserBackend for NativeBackend {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        self.driver.execute(argv, session)
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        self.driver.close_session(session);
        Ok(())
    }
}
