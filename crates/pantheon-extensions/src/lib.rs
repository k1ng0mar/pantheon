//! Extensions: hook spec (Hermes superset), plugin.yaml loader, Python
//! subprocess runner (fail-open), manager, skill doctor.
pub mod doctor;
pub mod hooks;
pub mod manager;
pub mod manifest;
pub mod python_runner;

pub use doctor::{doctor, DoctorReport};
pub use hooks::Hook;
pub use manager::ExtensionManager;
pub use manifest::PluginManifest;
pub use python_runner::{fire_hook, HookInput, PythonPlugin, RunnerConfig};
