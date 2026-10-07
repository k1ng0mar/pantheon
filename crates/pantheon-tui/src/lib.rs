//! Pantheon's terminal product: the one terminal application.
//!
//! Bare `pantheon` opens the interactive session. With a verb (`setup`,
//! `doctor`, `run --say ...`) it runs one non-interactive command instead.
//! Both live here now: there is no second terminal crate and no line-based
//! fallback. If there is no terminal, `pantheon` says so and exits rather
//! than degrading into a different product.
//!
//! # Layering
//!
//! ```text
//! terminal   argv dispatch + non-interactive command modules (agui, doctor,
//!            setup, model, memory via terminal, ...). thin: parses args,
//!            calls into crates, never owns business logic.
//! entry      interactive entrypoint: configured? → session : setup wizard
//! session    the conversation view: transcript state, overlays, slash
//!            commands, streaming event handling, permission card
//! app        the screen-stack event loop (setup wizard, pickers)
//!   widget   Select / MultiSelect / TextInput / Confirm
//!   render   the same widgets, drawn. no terminal imports in widget.rs
//!   commands the slash-command registry (metadata; handlers live in session)
//!   setup_graph the setup section graph
//!   setup_wizard the interactive setup screens over app/widget
//!   setup    flag parsing + config writer (`pantheon setup [--yes]`)
//!   prompt   run one widget on the real terminal, return the answer
//!       |
//!       v
//!   pantheon-runtime / exec / memory / providers / storage / ...
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
//! differently - and for a while a whole second terminal crate
//! (`pantheon-cli`) holding a duplicate session implementation. That crate
//! is gone: its dispatch and command modules moved here unchanged in
//! behavior, and its session implementation became `session.rs`. What
//! survives a migration is a module inside this crate; what does not earn
//! one is deleted.

pub mod app;
pub mod checkpoint;
pub mod commands;
pub mod render;
pub mod sanitize;
pub mod session;
pub mod setup;
pub mod setup_graph;
pub mod setup_providers;
pub mod widget;

pub use app::TuiApp;
pub use widget::{Confirm, Item, Key, KeyResult, MultiSelect, Select, Selection, TextInput};

// Terminal command modules: argv dispatch and the non-interactive verbs.
// Private: the binary calls `terminal::run()`; nothing outside this crate
// reaches past it.
mod agui;
mod args;
mod backup;
pub mod cloudflare_verb;
pub mod config;
mod config_schema;
mod config_verb;
pub mod consolidate_cli;
pub mod diffview;
mod doctor;
mod uninstall;
pub use pantheon_api::dotenv;
mod entry;
mod fallback;
mod gateway;
mod init;
mod logs;
mod markdown;
pub mod mcp;
pub mod mentions;
mod migrate;
mod model;
mod model_catalog;
pub mod nightly_cli;
pub mod nightly_repair;
pub mod notify;
mod pipeline;
pub mod plugin_remote;
mod plugin_toggle;
mod plugins_verb;
mod prompt;
mod provider;
pub mod reflect_cli;
mod repair;
mod reset;
pub mod richtext;
pub mod schedule;
mod schedule_self_heal;
pub mod send;
pub mod session_summary;
mod setup_wizard;
mod skill_deps;
mod skills;
pub mod stats;
pub mod swarm;
pub mod swarm_remote;
pub mod swarm_view;
pub mod team_remote;
pub mod terminal;
pub mod transcript;
mod update;
pub mod yank;

// TUI-A: session tab bar model + rendering (startup splash lives in terminal).
mod tabs;

// Agent todo card: opencode-style "Working on N to-dos" widget, rendered
// with ratatui only. The visual track wires it into the transcript.
pub mod todo_card;
