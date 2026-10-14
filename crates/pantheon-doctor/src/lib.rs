//! Structured diagnostics engine behind `pantheon doctor` and
//! `GET /api/health`.
//!
//! Every check is a function returning [`finding::Finding`]s - checks
//! never print. The [`registry::Registry`] orchestrates them (timing,
//! panic isolation, subsystem filtering); [`render`] turns the
//! [`finding::DiagnosticReport`] into concise, verbose, or JSON output;
//! [`health`] builds the `pantheon.health/v1` contract for the HTTP
//! endpoint. Both consumers share this one pipeline.

pub mod checks;
pub mod finding;
pub mod health;
pub mod probe;
pub mod registry;
pub mod render;
pub mod skill_deps;

pub use finding::{
    aggregate, summarize, CheckStatus, DiagnosticReport, Finding, OverallHealth, Severity,
    Subsystem,
};
pub use health::{build as build_health_response, HealthResponse, SCHEMA as HEALTH_SCHEMA};
pub use registry::{CheckDef, DoctorCtx, Registry};
pub use render::{exit_code, render_concise, render_json, render_verbose};
