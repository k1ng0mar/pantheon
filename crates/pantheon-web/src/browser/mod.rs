//! Browser automation behind the `browser_*` tools. The registry picks a
//! backend lazily, so configuring Pantheon does not launch a browser or make
//! a network request.
//!
//! Browser backends are intentionally outside the shell sandbox: they need
//! network access to drive sites. The capability gate is the control here,
//! with `BrowserAct` requiring approval. Local Chrome's `--no-sandbox` flag
//! remains opt-in and is only for environments where Chrome refuses to run.

pub mod backend;
pub mod browserbase;
pub mod camofox;
pub mod cdp;
pub mod error;
pub mod fill;
pub mod lifecycle;
pub mod native;
pub mod playwright;
pub mod proc;
pub mod registry;
pub mod remote;
pub mod steel;
pub mod subprocess;
pub mod tools;

pub use backend::BrowserBackend;
pub use browserbase::{BrowserbaseBackend, BrowserbaseConfig, BROWSERBASE_API_KEY_ENV};
pub use camofox::{CamofoxBackend, CamofoxConfig, CAMOFOX_INSTALL_INSTRUCTIONS};
pub use error::{BrowserError, INSTALL_INSTRUCTIONS};
pub use lifecycle::{sanitize_session_name, SessionManager};
pub use native::{NativeBackend, NativeConfig};
pub use playwright::{PlaywrightBackend, PlaywrightConfig};
pub use registry::{all_backends, build_backend, BackendConfig, BackendInfo, BackendKind};
pub use steel::{SteelBackend, SteelConfig, STEEL_API_KEY_ENV};
pub use subprocess::SubprocessBackend;
pub use tools::{register_browser_tools, BrowserOptions};
