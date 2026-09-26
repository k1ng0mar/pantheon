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
//!
//! NOTE — two loops exist by design:
//! - `pantheon-runtime::Session::drive` is the CANONICAL wired path
//!   (typed `Message` transcript + provenance + real provider chain).
//! - `pantheon-agent::AgentLoop` is the TEST HARNESS for the engine
//!   crate: `Vec<String>` transcript, scripted `ModelTurn`s, no network.
//!   Do not add provider/network behavior here; keep it deterministic.
pub mod engine;
pub mod tool;

pub use engine::{AgentLoop, AgentSpawner, Budget, LoopOutcome, ModelTurn, ToolCall, TurnOutcome};
pub use tool::{gate, EventSink, GateOutcome, ToolRunner};
