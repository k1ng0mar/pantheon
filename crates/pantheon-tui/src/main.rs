//! The `pantheon` binary: one thin shim over the terminal application.
//! All dispatch, command handling, and session behavior live in the library.
fn main() {
    pantheon_tui::terminal::run();
}
