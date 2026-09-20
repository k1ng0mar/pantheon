//! Storage: SQLite-backed event-sourced execution ledger (§12 + §19).
//! Default backend is SQLite (rusqlite, bundled). Postgres later.

pub mod claims;
pub mod ledger;

pub use claims::ClaimStore;
pub use ledger::{Ledger, LedgerEntry};
