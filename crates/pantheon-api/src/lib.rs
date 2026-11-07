//! pantheon-api: commands, events, types - the bottom protocol leaf.
//!
//! This crate is what every layer may depend on and what depends on
//! nothing internal:
//!
//! - **events** - the canonical [`events::Event`](events::Event) enum.
//!   There is exactly one event type; storage appends it, gateways and
//!   extensions observe it, the runtime emits it.
//! - **types** - `message` (transcript/wire shapes), `provenance`
//!   (source/trust of anything persisted), `capability` (the
//!   `Capability`/`Policy`/`Decision` *types*; role maps and resolution
//!   live in `pantheon-agent::capability`), `model` (model *policy* types
//!   default/fallback/auxiliaries; the provider catalog lives in
//!   `pantheon-providers`), `ident` (shared identifier rules).
//! - **errors** - `PantheonError`, the structured value every `Result`
//!   in the workspace carries.
//!
//! The JSON-RPC *server* (`rpc`, `serve`, `transport`, `agui`) is not
//! here: it moved to `pantheon-runtime`, which owns the Runtime API
//! (decision D4 in docs/developer/decisions/0001-workspace-restructure.md). Being
//! the leaf is the point: `pantheon-storage`, `pantheon-gateway`, the
//! scheduler and the extensions depend *on* this crate, never the other
//! way - that is what keeps the dependency graph acyclic.

mod config_group;
mod guard_group;
mod identity_group;
mod types_group;
pub use config_group::config_resolve;
pub use config_group::config_schema;
pub use config_group::dotenv;
pub use config_group::error;
pub use config_group::ident;
pub use guard_group::approval;
pub use guard_group::capability;
pub use guard_group::permission_mode;
pub use identity_group::agent_profile;
pub use identity_group::mode;
pub use identity_group::nightly;
pub use identity_group::workspace;
pub use types_group::events;
pub use types_group::logging;
pub use types_group::mcp_catalog;
pub use types_group::message;
pub use types_group::model;
pub use types_group::provenance;
pub use types_group::temporal;
pub use types_group::todo;
pub mod config;
pub mod ui;
