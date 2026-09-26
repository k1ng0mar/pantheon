//! Pantheon core: events, structured errors, capabilities, model policy.
//!
//! Locked decisions:
//! - The model is not the runtime. Agents never choose models.
//! - No model routing. There is a default model, an ordered fallback list
//!   (runtime-controlled, failure-only), and auxiliary models for scoped
//!   capabilities (embeddings, rerank, vision, extraction...). Service
//!   capabilities (STT/TTS, search, browser) are provider-plane, not models.

pub mod capability;
pub mod catalog;
pub mod error;
pub mod events;
pub mod logging;
pub mod message;
pub mod model;
pub mod model_event;
pub mod provenance;
