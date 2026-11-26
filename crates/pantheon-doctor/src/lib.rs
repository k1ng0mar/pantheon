//! diagnostics engine behind `pantheon doctor` and `GET /api/health`.
//!
//! checks return [`finding::Finding`]s, they never print. [`registry::Registry`]
//! times them, isolates panics and filters by subsystem; [`render`] formats the
//! [`finding::DiagnosticReport`]; [`health`] builds the `pantheon.health/v1`
//! payload. Both consumers use the same pipeline.

pub mod diagnostics;

pub mod checks {
    pub use crate::diagnostics::checks::*;
}
pub mod finding {
    pub use crate::diagnostics::finding::*;
}
pub mod health {
    pub use crate::diagnostics::health::*;
}
pub mod probe {
    pub use crate::diagnostics::probe::*;
}
pub mod registry {
    pub use crate::diagnostics::registry::*;
}
pub mod render {
    pub use crate::diagnostics::render::*;
}
pub mod skill_deps {
    pub use crate::diagnostics::skill_deps::*;
}

pub use diagnostics::finding::{
    aggregate, summarize, CheckStatus, DiagnosticReport, Finding, OverallHealth, Severity,
    Subsystem,
};
pub use diagnostics::health::{
    build as build_health_response, HealthResponse, SCHEMA as HEALTH_SCHEMA,
};
pub use diagnostics::registry::{CheckDef, DoctorCtx, Registry};
pub use diagnostics::render::{exit_code, render_concise, render_json, render_verbose};
