//! terminal app for pantheon: interactive sessions and one-shot commands in the
//! same crate, business logic in the lower-level crates
//!
//! widgets don't know about crossterm - keys come in as values so they can be
//! tested without a terminal. `terminal` dispatches, `session` owns the
//! conversation view, `app` runs the screen stack.

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

// argv dispatch + the non-interactive verbs. private on purpose: the binary
// only calls `terminal::run()`

pub use pantheon_api::dotenv;
pub mod logo;
mod markdown;
pub mod image_cmd;
pub mod terminal;

// TUI-A: session tab bar (startup splash still lives in terminal)

// agent todo card ("Working on N to-dos"), ratatui only; the visual track wires it in

// native ratatui renderers for `pantheon_api::ui` - each component becomes
// transcript lines in the current theme
