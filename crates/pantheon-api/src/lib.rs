//! protocol leaf for the whole workspace: events, wire types, ids, capabilities,
//! structured errors. higher layers depend on us, never the other way around,
//! so the graph stays acyclic.
//!
//! runtime JSON-RPC / AG-UI serving lives in `pantheon-runtime`.

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
pub use types_group::depth;
pub use types_group::events;
pub use types_group::goal;
pub use types_group::logging;
pub use types_group::mcp_catalog;
pub use types_group::message;
pub use types_group::model;
pub use types_group::provenance;
pub use types_group::temporal;
pub use types_group::todo;
pub mod config;
pub mod data_dir;
pub mod desktop_tool_contract;
pub mod ui;
