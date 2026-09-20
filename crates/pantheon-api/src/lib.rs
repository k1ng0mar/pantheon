//! JSON-RPC command surface (spec section 18).
//!
//! The runtime answers commands — `agent.run/pause/resume/stop`,
//! `task.create/cancel`, `memory.search/propose`, `tool.list/execute`,
//! `model.list/select`, `schedule.create` — over a transport-agnostic
//! [`ApiTransport`]. The wire format is JSON-RPC 2.0: one request id maps to
//! one response id, notifications (id `null`) are executed but never
//! answered, and every protocol failure becomes a structured error response,
//! never a crash.
//!
//! Handlers attach to a [`Dispatcher`]; a transport serves a dispatcher.
//! The Unix-socket transport is first; WebSocket comes later behind the same
//! trait. Events stream out as `pantheon_core::events::Event` — there is no
//! second event type.

pub mod rpc;
pub mod transport;

pub use rpc::{Dispatcher, Id, MethodHandler, Request, Response, RpcError};
pub use transport::{ApiTransport, UnixSocketTransport};