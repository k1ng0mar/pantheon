//! Tool layer: named, schema'd, capability-gated callable operations.
//!
//! Deliberate workspace change (capability ≠ tool):
//!
//! - a **capability** is what a policy *allows or is able to do*
//!   (`filesystem.write`) - its types live in `pantheon-api::capability`,
//!   its role maps and resolution in `pantheon-agent::capability`;
//! - a **tool** is one actual callable operation (`write_file`) - here.
//!
//! The registry is the choke point: `execute_gated` applies the policy
//! before any closure runs, so a new caller gets enforcement by default
//! rather than by remembering. Tools execute *through* `pantheon-exec`
//! (process/fs engines); `pantheon-exec` itself no longer knows about the
//! registry - that is what keeps `capability → tools → exec` acyclic.

pub mod tools;
pub mod tools_group;
