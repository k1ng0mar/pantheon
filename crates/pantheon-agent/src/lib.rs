//! Agent engine (spec section 2): the brain loop.
//!
//! receive input -> load state -> build context -> model inference ->
//! interpret response -> tool / delegation / response -> execute ->
//! observe result -> update state -> continue.
//!
//! Every transition emits a pantheon_api::events::Event through an
//! `EventSink`, so replay, debugging, and crash recovery come for free.
//! The model is behind `ModelTurn` — swapping providers never touches
//! this loop. Tools are behind `ToolRunner` and gated by
//! pantheon_capability::enforce before anything executes.
//!
//! Two loops exist, and only one runs in production:
//! - `pantheon-runtime::Session::drive` is the CANONICAL wired path
//!   (typed `Message` transcript + provenance + real provider chain).
//! - `pantheon-agent::AgentLoop::run` has no production caller. The
//!   production path builds an `AgentLoop` but drives its own typed
//!   `Message` turn loop over the same policy, budget, and run identity.
//!   `run` is the engine crate's deterministic harness: `Vec<String>`
//!   transcript, scripted `ModelTurn`s, no network. Do not add provider or
//!   network behavior here; keep it deterministic.
//!
//! Known gap: `AgentLoop::run` consults a judge for route selection and gate
//! review, and `Session::drive` does not. `Session` builds its `AgentLoop`
//! with `judge: None`, so a configured `[judge]` section is validated by
//! `doctor` and then never asked anything. Collapsing the two loops is the
//! fix; until then, do not configure `[judge]` expecting routing to change.
pub mod agent_profile;
pub mod engine;
pub mod tool;

pub use engine::{AgentLoop, AgentSpawner, Budget, LoopOutcome, ModelTurn, ToolCall, TurnOutcome};
pub use tool::{gate, EventSink, GateOutcome, ToolRunner};
