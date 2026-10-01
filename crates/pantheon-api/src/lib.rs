//! pantheon-api: commands, events, types — the bottom protocol leaf.
//!
//! This crate is what every layer may depend on and what depends on
//! nothing internal:
//!
//! - **events** — the canonical [`events::Event`](events::Event) enum.
//!   There is exactly one event type; storage appends it, gateways and
//!   extensions observe it, the runtime emits it.
//! - **types** — `message` (transcript/wire shapes), `provenance`
//!   (source/trust of anything persisted), `capability` (the
//!   `Capability`/`Policy`/`Decision` *types*; role maps and resolution
//!   live in `pantheon-agent::capability`), `model` (model *policy* types —
//!   default/fallback/auxiliaries; the provider catalog lives in
//!   `pantheon-providers`), `ident` (shared identifier rules).
//! - **errors** — `PantheonError`, the structured value every `Result`
//!   in the workspace carries.
//!
//! The JSON-RPC *server* (`rpc`, `serve`, `transport`, `agui`) is not
//! here: it moved to `pantheon-runtime`, which owns the Runtime API
//! (decision D4 in docs/developer/decisions/0001-workspace-restructure.md). Being
//! the leaf is the point: `pantheon-storage`, `pantheon-gateway`, the
//! scheduler and the extensions depend *on* this crate, never the other
//! way — that is what keeps the dependency graph acyclic.

pub mod agent_profile;
pub mod approval;
pub mod capability;
pub mod config;
pub mod config_resolve;
pub mod config_schema;
pub mod dotenv;
pub mod error;
pub mod events;
pub mod ident;
pub mod logging;
pub mod mcp_catalog;
pub mod message;
pub mod mode;
pub mod model;
pub mod nightly;
pub mod provenance;
pub mod temporal;
pub mod todo;
