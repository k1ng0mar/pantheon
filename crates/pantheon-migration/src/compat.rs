//! Compat adapter: OpenClaw / OMP extensions -> Pantheon extensions.
//!
//! Both foreign ecosystems ship the same *idea* — a `register(api)` entry
//! point that attaches handlers to named lifecycle events — under different
//! manifests, different spellings, and a much larger event vocabulary. This
//! module is the seam named in `docs/developer/architecture.md` section 8.
//!
//! The contract it upholds is deliberately narrow:
//!
//! - A foreign hook maps to a Pantheon hook **only** when the semantics match.
//!   Nothing is approximated.
//! - An unmappable hook is **reported, never dropped**. A plugin that asks for
//!   `before_tool_call` and gets silence is worse than one that is refused,
//!   so `CompatReport::unsupported` always names what was lost.
//! - Capabilities Pantheon has no equivalent for (model providers, media
//!   generation, gateway methods, HTTP routes) are recorded as `refused`, so
//!   the operator can see the plugin loaded a subset of what it declares.
//! - Credentials are **described, never copied**: the manifest's
//!   `envVars` become a declaration of what the operator must supply through
//!   Pantheon's own secrets plane.

use pantheon_extensions::hooks::Hook;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Foreign manifests
// ---------------------------------------------------------------------------

/// `openclaw.plugin.json`. Only the fields the adapter needs; everything else
/// is ignored so a manifest gaining keys does not break the import.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenClawManifest {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub setup: Option<OpenClawSetup>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenClawSetup {
    #[serde(default)]
    pub providers: Vec<OpenClawProvider>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenClawProvider {
    #[serde(default)]
    pub id: Option<String>,
    /// The env var names this provider needs. A *declaration*, not a value.
    #[serde(default, rename = "envVars")]
    pub env_vars: Vec<String>,
}

/// OMP keeps its manifest in `package.json` under an `omp` or `pi` field.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OmpManifest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub tools: Option<String>,
    #[serde(default)]
    pub hooks: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct PackageJson {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub omp: Option<OmpManifest>,
    #[serde(default)]
    pub pi: Option<OmpManifest>,
}

// ---------------------------------------------------------------------------
// Hook mapping
// ---------------------------------------------------------------------------

/// The outcome of mapping one foreign event onto Pantheon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookMap {
    /// Semantics match; the handler will run at this Pantheon hook.
    Mapped(Hook),
    /// Pantheon has no equivalent. Never silently ignored.
    Unsupported { nearest: Option<Hook> },
}

/// Map a foreign lifecycle event onto a Pantheon hook.
///
/// The mapping is intentionally conservative. A hook is only reported as
/// [`HookMap::Mapped`] when it is both semantically equivalent **and**
/// actually wired (see [`Hook::is_wired`]) — `Mapped` promises the operator
/// that the handler will run, so it must never be handed out for a hook with
/// no fire site. `before_tool_call` now maps for real (to the
/// `pre_tool_call` gate); `pre_gateway_dispatch` is still refused because
/// inbound messages are handled in a crate that cannot reach the manager yet.
pub fn map_hook(event: &str) -> HookMap {
    let norm = event.trim().to_ascii_lowercase().replace(['-', ':'], "_");
    // `Mapped` is a promise; honor it only for wired hooks.
    let mapped = |h: Hook| {
        if h.is_wired() {
            HookMap::Mapped(h)
        } else {
            HookMap::Unsupported { nearest: None }
        }
    };
    match norm.as_str() {
        // Build context/prompt before inference. Direct match.
        "before_prompt_build"
        | "before_llm_call"
        | "pre_llm_call"
        | "prompt_build"
        | "before_prompt" => mapped(Hook::PreLlmCall),
        // Inbound message before the gateway dispatches it. Declared, but
        // unwired: reported unsupported rather than falsely mapped.
        "message_received" | "pre_gateway_dispatch" | "gateway_dispatch" | "before_dispatch" => {
            HookMap::Unsupported { nearest: None }
        }
        "before_api_request" | "pre_api_request" | "api_request" => mapped(Hook::PreApiRequest),
        "after_api_request" | "post_api_request" | "api_response" => mapped(Hook::PostApiRequest),
        // Tool lifecycle. `pre_tool_call` is a real gate (can deny) and
        // `transform_tool_result` a real transform, both fired inline by the
        // host at the tool-execution point.
        "before_tool_call" | "pre_tool_call" | "tool_call_before" => mapped(Hook::PreToolCall),
        "after_tool_call" | "post_tool_call" | "tool_call_after" => mapped(Hook::PostToolCall),
        "transform_tool_result" | "transform_terminal_output" | "transform_llm_output" => {
            mapped(Hook::TransformToolResult)
        }
        "message_sending" | "before_send" | "outbound_message" => {
            HookMap::Unsupported { nearest: None }
        }
        "on_session_start" | "session_start" | "session_started" => mapped(Hook::OnSessionStart),
        "on_session_end" | "session_end" | "session_finalize" => mapped(Hook::OnSessionEnd),
        "subagent_start" | "agent_start" => mapped(Hook::SubagentStart),
        "subagent_stop" | "agent_end" => mapped(Hook::SubagentStop),
        "on_stream_start" => mapped(Hook::OnStreamStart),
        "on_stream_delta" => mapped(Hook::OnStreamDelta),
        "on_stream_end" => mapped(Hook::OnStreamEnd),
        // OMP compaction lifecycle. Pantheon fires one post-facto observer
        // (`Hook::OnCompaction`, off the durable ContextTrimmed /
        // ContextCompressed events), so the start/end pair collapses onto it.
        // Documented on the hook; a pre-compaction gate is not promised.
        "auto_compaction_start" | "auto_compaction_end" | "compaction_start" | "compaction_end" => {
            mapped(Hook::OnCompaction)
        }
        // OMP retry lifecycle (`auto_retry_start/end`,
        // `retry_fallback_applied/succeeded`, `ttsr_triggered`). Pantheon's
        // fallback walk lives in the provider plane (`chain.rs`:
        // `ModelEvent::Fallback`), which never reaches the core event fan-out
        // the bridge fires from — so there is no fire site, and per this
        // file's rule these stay Unsupported rather than mapping onto an
        // observer that would fire at the wrong time. Explicit arms (not the
        // catch-all) so they count as reasoned-about in KNOWN_FOREIGN_EVENTS.
        "auto_retry_start"
        | "auto_retry_end"
        | "retry_fallback_applied"
        | "retry_fallback_succeeded"
        | "ttsr_triggered" => HookMap::Unsupported { nearest: None },
        // Unknown: refused rather than guessed.
        _ => HookMap::Unsupported { nearest: None },
    }
}

/// Every foreign event name the adapter knows how to reason about. Used to
/// tell "this event is unsupported on purpose" from "this event is new to us".
pub const KNOWN_FOREIGN_EVENTS: &[&str] = &[
    "before_prompt_build",
    "before_tool_call",
    "after_tool_call",
    "message_received",
    "message_sending",
    "transform_tool_result",
    "transform_llm_output",
    "on_session_start",
    "on_session_end",
    "subagent_start",
    "subagent_stop",
    "on_stream_start",
    "on_stream_delta",
    "on_stream_end",
    "auto_compaction_start",
    "auto_compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "retry_fallback_applied",
    "retry_fallback_succeeded",
    "ttsr_triggered",
];

// ---------------------------------------------------------------------------
// API surface the runner cannot serve
// ---------------------------------------------------------------------------

/// `api.*` registration methods observed across the OpenClaw extension set
/// that Pantheon has no equivalent for. Recorded so a partial load is visible
/// rather than implied.
pub const REFUSABLE_API_METHODS: &[&str] = &[
    "registerProvider",
    "registerTool",
    "registerCommand",
    "registerCli",
    "registerGatewayMethod",
    "registerHttpRoute",
    "registerService",
    "registerWebSearchProvider",
    "registerImageGenerationProvider",
    "registerVideoGenerationProvider",
    "registerSpeechProvider",
    "registerMediaUnderstandingProvider",
    "registerMemoryEmbeddingProvider",
    "registerMusicGenerationProvider",
    "registerRealtimeTranscriptionProvider",
    "registerModelCatalogProvider",
];

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialRequirement {
    /// The foreign env var name, e.g. `MODELSTUDIO_API_KEY`.
    pub env_var: String,
    /// The provider or plugin that needs it.
    pub provider: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CompatReport {
    /// Display name of the plugin.
    pub name: String,
    /// Which foreign shape it came from.
    pub origin: String,
    /// Foreign events that mapped cleanly.
    pub mapped: Vec<String>,
    /// Foreign events with no Panthey equivalent. Never empty-and-silent.
    pub unsupported: Vec<String>,
    /// `api.*` methods the plugin wanted that we cannot serve.
    pub refused: Vec<String>,
    /// Env vars the plugin declares it needs. Names only, no values.
    pub credentials: Vec<CredentialRequirement>,
    /// Where the generated `plugin.yaml` was written, if it was.
    pub manifest_written: Option<String>,
}

impl CompatReport {
    /// True when the plugin loaded with nothing lost.
    pub fn clean(&self) -> bool {
        self.unsupported.is_empty() && self.refused.is_empty()
    }

    /// One-line operator summary.
    pub fn summary(&self) -> String {
        format!(
            "{} ({}): {} hook(s) mapped, {} unsupported, {} capability(ies) refused",
            self.name,
            self.origin,
            self.mapped.len(),
            self.unsupported.len(),
            self.refused.len()
        )
    }
}

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// What kind of foreign extension a directory holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatKind {
    OpenClaw,
    Omp,
}

impl CompatKind {
    pub fn name(&self) -> &'static str {
        match self {
            CompatKind::OpenClaw => "openclaw",
            CompatKind::Omp => "omp",
        }
    }
}

/// Identify a foreign extension directory, or `None` if it is not one.
pub fn detect_kind(dir: &Path) -> Option<CompatKind> {
    if dir.join("openclaw.plugin.json").is_file() {
        return Some(CompatKind::OpenClaw);
    }
    if let Some(pkg) = read_package(dir) {
        if pkg.omp.is_some() || pkg.pi.is_some() {
            return Some(CompatKind::Omp);
        }
    }
    None
}

fn read_package(dir: &Path) -> Option<PackageJson> {
    let body = std::fs::read_to_string(dir.join("package.json")).ok()?;
    serde_json::from_str(&body).ok()
}

/// The JS entry file a foreign extension registers from.
pub fn entry_file(dir: &Path, kind: CompatKind) -> Option<PathBuf> {
    match kind {
        CompatKind::OpenClaw => {
            for c in ["index.js", "index.mjs", "index.cjs"] {
                if dir.join(c).is_file() {
                    return Some(dir.join(c));
                }
            }
            None
        }
        CompatKind::Omp => {
            let pkg = read_package(dir)?;
            let m = pkg.omp.as_ref().or(pkg.pi.as_ref())?;
            // `hooks` is the registration entry; fall back to the conventional
            // index when the manifest does not name one.
            if let Some(h) = &m.hooks {
                if dir.join(h).is_file() {
                    return Some(dir.join(h));
                }
            }
            for c in ["index.js", "index.mjs", "index.cjs"] {
                if dir.join(c).is_file() {
                    return Some(dir.join(c));
                }
            }
            None
        }
    }
}

/// Parse a foreign extension's manifest into the fields the adapter needs.
pub fn read_manifest(
    dir: &Path,
    kind: CompatKind,
) -> Option<(String, String, Vec<CredentialRequirement>)> {
    match kind {
        CompatKind::OpenClaw => {
            let body = std::fs::read_to_string(dir.join("openclaw.plugin.json")).ok()?;
            let m: OpenClawManifest = serde_json::from_str(&body).ok()?;
            let name = m
                .name
                .clone()
                .or_else(|| m.id.clone())
                .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "openclaw-extension".into());
            let creds = m
                .setup
                .map(|s| {
                    s.providers
                        .into_iter()
                        .flat_map(|p| {
                            let provider = p.id.clone().unwrap_or_else(|| name.clone());
                            p.env_vars.into_iter().map(move |v| CredentialRequirement {
                                env_var: v,
                                provider: provider.clone(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some((name, m.description.unwrap_or_default(), creds))
        }
        CompatKind::Omp => {
            let pkg = read_package(dir)?;
            let m = pkg.omp.clone().or(pkg.pi.clone())?;
            let name = m
                .name
                .clone()
                .or(pkg.name.clone())
                .or_else(|| dir.file_name().map(|n| n.to_string_lossy().to_string()))
                .unwrap_or_else(|| "omp-extension".into());
            let _version = m
                .version
                .clone()
                .or(pkg.version.clone())
                .unwrap_or_default();
            Some((
                name,
                m.description.or(pkg.description).unwrap_or_default(),
                Vec::new(),
            ))
        }
    }
}

/// Scan a foreign extension's JS for the `api.on("<event>"` registrations and
/// `api.<method>(` calls, so the report can name what it will and will not run.
///
/// This is a text scan, not a parse: the extensions ship minified bundles, and
/// a regex over the registration call sites is the honest bound. It can miss a
/// dynamically-computed event name, which is why `CompatReport::unsupported`
/// is a *lower* bound on what was lost and the runner re-checks at load.
pub fn scan_registrations(js: &str) -> (Vec<String>, Vec<String>) {
    let mut events = Vec::new();
    let mut methods = Vec::new();

    // api.on('x' | "x" , ...)
    let mut rest = js;
    while let Some(i) = rest.find("api.on(") {
        let after = &rest[i + "api.on(".len()..];
        let quote = match after.chars().next() {
            Some(q @ ('\'' | '"')) => q,
            _ => {
                rest = &rest[i + 1..];
                continue;
            }
        };
        let after = &after[1..];
        if let Some(end) = after.find(quote) {
            let ev = &after[..end];
            if !events.iter().any(|e| e == ev) {
                events.push(ev.to_string());
            }
        }
        rest = &after[after.len().min(1)..];
    }

    for m in REFUSABLE_API_METHODS {
        if js.contains(&format!("api.{m}(")) {
            methods.push((*m).to_string());
        }
    }
    (events, methods)
}

// ---------------------------------------------------------------------------
// Manifest generation
// ---------------------------------------------------------------------------

/// Render a Pantheon `plugin.yaml` for a foreign extension, listing only the
/// hooks that actually mapped. Written next to the extension so the existing
/// manager can load it unchanged.
pub fn render_plugin_yaml(name: &str, version: &str, description: &str, mapped: &[Hook]) -> String {
    let hooks: Vec<String> = mapped.iter().map(|h| h.name().to_string()).collect();
    format!(
        "# Generated by `pantheon migrate apply` from a foreign extension.\n\
         # Compat adapter: only the hooks below have a Pantheon equivalent.\n\
         # Regenerate rather than hand-editing; unsupported hooks are listed in\n\
         # the migration report, not silently dropped here.\n\
         name: {name}\n\
         version: \"{version}\"\n\
         description: {description:?}\n\
         provides_hooks:\n{}\n",
        hooks
            .iter()
            .map(|h| format!("  - {h}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// Build the full report for a foreign extension directory without writing
/// anything. This is what `migrate show` / `migrate plan` use.
pub fn inspect(dir: &Path) -> Option<CompatReport> {
    let kind = detect_kind(dir)?;
    let (name, _desc, creds) = read_manifest(dir, kind)?;
    let mut report = CompatReport {
        name,
        origin: kind.name().to_string(),
        credentials: creds,
        ..Default::default()
    };

    if let Some(entry) = entry_file(dir, kind) {
        if let Ok(js) = std::fs::read_to_string(&entry) {
            let (events, methods) = scan_registrations(&js);
            for e in events {
                match map_hook(&e) {
                    HookMap::Mapped(_) => {
                        if !report.mapped.contains(&e) {
                            report.mapped.push(e)
                        }
                    }
                    HookMap::Unsupported { .. } => {
                        if !report.unsupported.contains(&e) {
                            report.unsupported.push(e)
                        }
                    }
                }
            }
            report.refused = methods;
        }
    }
    Some(report)
}
