#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
fn main() {
    // First, while single-threaded: agents and tools inherit a PATH that finds `gh`, `node`…
    pantheon_desktop_lib::prepare_environment();
    if pantheon_desktop_lib::handle_usage_keychain_helper() {
        return;
    }
    // A cloud agent's bridge to its machine (docs/cloud-agents.md); exits when done.
    if pantheon_desktop_lib::handle_cloud_bridge() {
        return;
    }
    pantheon_desktop_lib::run()
}
