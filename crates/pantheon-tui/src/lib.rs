//! Pantheon's terminal product.
//!
//! The TUI is the terminal interface. There is no second interactive surface
//! and no line-based fallback: `pantheon` with no arguments opens this, and
//! if there is no terminal it says so and exits rather than degrading into a
//! different product.
//!
//! # Layering
//!
//! ```text
//! app        the event loop, screen stack, and focus
//!   widget   Select / MultiSelect / TextInput / Confirm / SearchList
//!   commands one command registry, no per-surface dispatch tables
//!   session  the conversation view
//!   setup    the setup section graph and its screens
//!       |
//!       v
//!   pantheon-runtime / core / exec / memory
//! ```
//!
//! The dependency arrow only points one way. A widget never touches a
//! `Session`, and `widget.rs` never touches crossterm: it is exercised by
//! calling `handle_key` with a [`Key`] value, so every component test runs
//! without a TTY. Anything that cannot be tested that way is doing too much.
//!
//! # Why one product and not several
//!
//! An earlier shape had a TUI, a REPL, a model picker, a session picker, and
//! a text setup wizard, with two of them able to answer the same command
//! differently. Consolidating means the old surfaces are deleted and their
//! behavior relocated, not wrapped in a shared helper so all five keep
//! existing. What survives a migration is a screen inside this crate; what
//! does not earn a screen is deleted.

pub mod app;
pub mod commands;
pub mod picker;
pub mod session;
pub mod setup;
pub mod widget;

pub use app::TuiApp;
pub use widget::{
    Confirm, Item, Key, KeyResult, MultiSelect, SearchList, Select, Selection, TextInput,
};
