//! Extensions: the hook spec, the `plugin.yaml` loader, the Python and
//! JavaScript subprocess runners (both fail-open), the manager, the skill
//! doctor, and the section 8 compat adapter that lets an OpenClaw or OMP
//! extension register against Pantheon hooks.
pub mod compat;
pub mod doctor;
pub mod event_bridge;
pub mod hooks;
pub mod js_runner;
pub mod manager;
pub mod manifest;
pub mod python_runner;

pub use compat::{
    detect_kind, entry_file, inspect, map_hook, read_manifest, render_plugin_yaml, CompatKind,
    CompatReport, CredentialRequirement, HookMap,
};
pub use doctor::{doctor, DoctorReport};
pub use event_bridge::{dispatch, dispatch_stream_edges, HookDispatcher, HookFire};
pub use hooks::{Hook, HookClass};
pub use js_runner::{fire_hook as fire_js_hook, fire_js_hook_full, JsPlugin, JsRunnerConfig};
pub use manager::{ExtensionManager, GateDecision};
pub use manifest::PluginManifest;
pub use python_runner::{
    fire_hook, fire_hook_full, HookDirective, HookInput, HookOutput, PythonPlugin, RunnerConfig,
};
