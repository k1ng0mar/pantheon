//! Agent engine (spec section 2): the brain loop.
//!
//! receive input -> load state -> build context -> model inference ->
//! interpret response -> tool / delegation / response -> execute ->
//! observe result -> update state -> continue.
//!
//! Every transition emits a pantheon_core::events::Event through an
//! `EventSink`, so replay, debugging, and crash recovery come for free.
//! The model is behind `ModelTurn` — swapping providers never touches
//! this loop. Tools are behind `ToolRunner` and gated by
//! pantheon_capability::enforce before anything executes.
pub mod engine;
pub mod tool;

pub use engine::{AgentLoop, AgentSpawner, Budget, LoopOutcome, ModelTurn, TurnOutcome, ToolCall};
pub use tool::{EventSink, GateOutcome, ToolRunner};
