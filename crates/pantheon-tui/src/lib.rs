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

mod account_group;
mod cli_group;
mod config_group;
mod heal_group;
mod model_group;
mod remote_group;
mod render_group;
mod setup_group;
use account_group::backup;
pub use account_group::checkpoint;
use account_group::oauth;
use account_group::secrets;
pub use account_group::secrets_form;
use cli_group::acp;
use cli_group::agui;
use cli_group::args;
pub use cli_group::cli_surface;
pub use cli_group::commands;
pub use cli_group::consolidate_cli;
pub use cli_group::custom_commands;
use cli_group::entry;
pub use cli_group::mentions;
use cli_group::prompt;
pub use config_group::config;
use config_group::config_schema;
use config_group::config_verb;
use config_group::fallback;
use config_group::gateway;
use config_group::init;
use config_group::logs;
pub use config_group::mcp;
use config_group::migrate;
pub use config_group::notify;
use config_group::pipeline;
use config_group::plugin_toggle;
use config_group::plugins_verb;
use config_group::reset;
pub use config_group::richtext;
use config_group::uninstall;
use config_group::update;
use config_group::worktree;
use heal_group::doctor;
pub use heal_group::nightly_cli;
pub use heal_group::nightly_repair;
pub use heal_group::reflect_cli;
use heal_group::repair;
pub use heal_group::schedule;
use heal_group::schedule_self_heal;
use model_group::model;
use model_group::model_browser;
use model_group::model_catalog;
use model_group::ollama;
use model_group::provider;
pub use model_group::setup_providers;
pub use remote_group::plugin_remote;
pub use remote_group::swarm;
pub use remote_group::swarm_remote;
pub use remote_group::swarm_view;
pub use remote_group::team_remote;
pub use render_group::app;
pub use render_group::codebase;
pub use render_group::desktop;
pub use render_group::diffview;
pub use render_group::render;
pub use render_group::sanitize;
pub use render_group::send;
pub use render_group::session_summary;
use render_group::skills;
pub use render_group::stats;
use render_group::tabs;
pub use render_group::todo_card;
pub use render_group::transcript;
pub use render_group::ui_render;
pub use render_group::widget;
pub use render_group::yank;
pub use setup_group::setup;
pub use setup_group::setup_graph;
use setup_group::setup_wizard;
pub mod auth;
pub mod session;

pub use render_group::app::TuiApp;
pub use render_group::widget::{
    Confirm, Item, Key, KeyResult, MultiSelect, Select, Selection, TextInput,
};

// Terminal command modules: argv dispatch and the non-interactive verbs.
// Private: the binary calls `terminal::run()`; nothing outside this crate
// reaches past it.

pub use pantheon_api::dotenv;
mod markdown;
pub mod terminal;

// TUI-A: session tab bar model + rendering (startup splash lives in terminal).

// Agent todo card: "Working on N to-dos" widget, rendered
// with ratatui only. The visual track wires it into the transcript.

// Native ratatui renderers for the UI protocol (`pantheon_api::ui`):
// each component lowers to transcript lines in the current theme.
