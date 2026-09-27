//! Pantheon — façade crate.
//!
//! One dependency for "give me Pantheon": every library crate re-exported
//! under a stable, namespaced path. The names mirror the target
//! architecture:
//!
//! | path | crate | owns |
//! |---|---|---|
//! | `pantheon::api` | `pantheon-api` | commands, events, types |
//! | `pantheon::agent` | `pantheon-agent` | agent, profile, inheritance |
//! | `pantheon::runtime` | `pantheon-runtime` | lifecycle, turn, context, state |
//! | `pantheon::swarm` | `pantheon-swarm` | delegation, coordination |
//! | `pantheon::providers` | `pantheon-providers` | catalog, provider, streaming |
//! | `pantheon::capability` | `pantheon-capability` | capability registry, resolution |
//! | `pantheon::tools` | `pantheon-tools` | callable tools + registry |
//! | `pantheon::exec` | `pantheon-exec` | process/fs execution engine |
//! | `pantheon::sandbox` | `pantheon-sandbox` | sandbox levels + policy boundary |
//! | `pantheon::memory` | `pantheon-memory` | memory store, recall, write |
//! | `pantheon::storage` | `pantheon-storage` | SQLite ledger, repository |
//! | `pantheon::migration` | `pantheon-migration` | Hermes/OpenClaw/OMP migration |
//! | `pantheon::mcp` | `pantheon-mcp` | MCP projection |
//! | `pantheon::extensions` | `pantheon-extensions` | hooks, manifests, runners |
//! | `pantheon::gateway` | `pantheon-gateway` | channels, messages, delivery |
//! | `pantheon::scheduler` | `pantheon-scheduler` | cron/interval/webhook jobs |
//! | `pantheon::secrets` | `pantheon-secrets` | vaults + broker |
//! | `pantheon::tui` | `pantheon-tui` | interactive terminal surface |
//!
//! The CLI (`pantheon-cli`, the `pantheon` binary) is deliberately not
//! re-exported: it is the composition root, not library surface.

pub use pantheon_agent as agent;
pub use pantheon_api as api;
pub use pantheon_capability as capability;
pub use pantheon_exec as exec;
pub use pantheon_extensions as extensions;
pub use pantheon_gateway as gateway;
pub use pantheon_mcp as mcp;
pub use pantheon_memory as memory;
pub use pantheon_migration as migration;
pub use pantheon_providers as providers;
pub use pantheon_runtime as runtime;
pub use pantheon_sandbox as sandbox;
pub use pantheon_scheduler as scheduler;
pub use pantheon_secrets as secrets;
pub use pantheon_storage as storage;
pub use pantheon_swarm as swarm;
pub use pantheon_tools as tools;
pub use pantheon_tui as tui;

#[cfg(test)]
mod tests {
    /// Every façade path resolves at compile time; this is the smoke test
    /// that keeps the re-export surface honest.
    #[test]
    fn facade_paths_resolve() {
        let _ = std::any::type_name::<pantheon_agent::AgentLoop>();
        let _ = std::any::type_name::<pantheon_runtime::session::Session>();
        let _ = std::any::type_name::<pantheon_tools::tools::ToolRegistry>();
    }
}
