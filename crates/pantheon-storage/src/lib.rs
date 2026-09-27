//! Storage: SQLite-backed event-sourced execution ledger (§12 + §19).
//! Default backend is SQLite (rusqlite, bundled). Postgres later.

pub mod claims;
pub mod leases;
pub mod ledger;
pub mod operations;

pub mod audit;
pub mod collaboration;
pub mod search;
pub use audit::{audit_line, export_jsonl};
pub use claims::ClaimStore;
pub use collaboration::{
    AgentMessage, AgentTask, Collaboration, CollaborationStatus, CollaborationStore, MessageKind,
    TaskConflict, TaskMutationError, TaskStatus,
};
pub use leases::{LostLeaseError, RunLease, RunLeaseStore};
pub use ledger::{Artifact, Ledger, LedgerEntry, RunListing, RunMetrics};
pub use operations::{Operation, OperationConflict, OperationStatus, OperationStore};
pub use search::{recreate_search_index, search_index_health};
pub use search::{SearchHit, SessionChunk, SessionSearch};
