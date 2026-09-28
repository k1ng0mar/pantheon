//! The setup section graph.
//!
//! The graph resolves which sections apply given
//! the answers so far, so the progress indicator can never claim "7 of 12"
//! for a branch that has nine screens.

use std::collections::BTreeMap;

/// One setup section, in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Entry,
    Profile,
    Provider,
    Model,
    Reasoning,
    Workspace,
    Execution,
    DockerNetwork,
    Permissions,
    Gateways,
    Tools,
    Browser,
    WebSearch,
    Tts,
    Memory,
    Extensions,
    Fallback,
    Review,
    Provision,
    Done,
}

impl Section {
    /// Lowercase name used in the `SETUP · NAME` indicator.
    pub fn label(self) -> &'static str {
        match self {
            Section::Entry => "entry",
            Section::Profile => "profile",
            Section::Provider => "model",
            Section::Model => "model",
            Section::Reasoning => "reasoning",
            Section::Workspace => "workspace",
            Section::Execution => "execution",
            Section::DockerNetwork => "network",
            Section::Permissions => "permissions",
            Section::Gateways => "gateways",
            Section::Tools => "tools",
            Section::Browser => "browser",
            Section::WebSearch => "search",
            Section::Tts => "speech",
            Section::Memory => "memory",
            Section::Extensions => "extensions",
            Section::Fallback => "fallback",
            Section::Review => "review",
            Section::Provision => "install",
            Section::Done => "ready",
        }
    }
}

/// Which entry mode the user chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Minimum decisions to get a usable agent.
    Quick,
    /// Complete runtime, skipping irrelevant sections automatically.
    Full,
    /// Create the runtime without configuring agent/model/tooling.
    Blank,
}

/// The decisions that change which sections run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answers {
    pub mode: Option<Mode>,
    pub execution_is_docker: bool,
    /// Tool groups the user enabled. Browser, web search, and speech are
    /// tool groups, so they gate their own provider screens.
    pub browser_enabled: bool,
    pub web_search_enabled: bool,
    pub tts_enabled: bool,
    pub tools_enabled: bool,
    pub memory_enabled: bool,
    pub gateways_enabled: bool,
    pub extensions_enabled: bool,
    pub fallback_enabled: bool,
    /// True when the selected model declares reasoning support.
    pub model_supports_reasoning: bool,
}

/// Resolve the ordered section list for the current answers.
///
/// The point of this function is that the count is *derived*, never
/// hardcoded. Turning off Docker removes a screen; turning off the browser
/// tool removes its provider screen. Any screen whose runtime consumer does
/// not exist yet is omitted, so setup can never offer a choice the runtime
/// cannot honor.
pub fn sections(a: &Answers) -> Vec<Section> {
    let mode = a.mode.unwrap_or(Mode::Quick);
    let mut out = vec![Section::Entry];

    match mode {
        Mode::Blank => {
            // No agent configuration at all. Runtime and workspace only.
            out.extend([
                Section::Workspace,
                Section::Review,
                Section::Provision,
                Section::Done,
            ]);
            return out;
        }
        Mode::Quick => {
            out.extend([
                Section::Profile,
                Section::Provider,
                Section::Model,
                Section::Permissions,
                Section::Memory,
            ]);
            if a.fallback_enabled {
                out.push(Section::Fallback);
            }
            out.extend([Section::Review, Section::Provision, Section::Done]);
            return out;
        }
        Mode::Full => {}
    }

    out.push(Section::Profile);
    out.push(Section::Provider);
    out.push(Section::Model);
    // Reasoning is offered only for a model that declares it, so the screen
    // cannot promise a setting the request pipeline will not honor.
    if a.model_supports_reasoning {
        out.push(Section::Reasoning);
    }
    out.push(Section::Workspace);
    out.push(Section::Execution);
    if a.execution_is_docker {
        out.push(Section::DockerNetwork);
    }
    out.push(Section::Permissions);
    if a.gateways_enabled {
        out.push(Section::Gateways);
    }
    if a.tools_enabled {
        out.push(Section::Tools);
    }
    if a.browser_enabled {
        out.push(Section::Browser);
    }
    if a.web_search_enabled {
        out.push(Section::WebSearch);
    }
    if a.tts_enabled {
        out.push(Section::Tts);
    }
    if a.memory_enabled {
        out.push(Section::Memory);
    }
    if a.extensions_enabled {
        out.push(Section::Extensions);
    }
    if a.fallback_enabled {
        out.push(Section::Fallback);
    }
    out.extend([Section::Review, Section::Provision, Section::Done]);
    out
}

/// `SETUP · TOOLS`, uppercased for the indicator.
pub fn indicator(s: Section) -> String {
    format!("SETUP · {}", s.label().to_uppercase())
}

/// `Tools    6 of 9`, where both numbers come from the resolved branch.
pub fn progress(s: Section, list: &[Section]) -> String {
    let idx = list
        .iter()
        .position(|x| *x == s)
        .map(|i| i + 1)
        .unwrap_or(0);
    format!("{}  {} of {}", s.label(), idx, list.len())
}

/// What a provisioning step can do when it fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepAction {
    Retry,
    Skip,
    Abort,
}

/// The result of one provisioning step. `Skip` is a real outcome, not a
/// failure: it records that the capability is absent so a later health check
/// can say so rather than the agent pretending it has the tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepOutcome {
    Done,
    Skipped { why: String },
    Failed { why: String, action: StepAction },
}

/// Aggregate the steps. Abort means the run never becomes usable; Skipped
/// does not, because an agent without a browser is still an agent.
pub fn provisioning_ok(steps: &[(String, StepOutcome)]) -> bool {
    !steps.iter().any(|(_, o)| {
        matches!(
            o,
            StepOutcome::Failed {
                action: StepAction::Abort,
                ..
            }
        )
    })
}

/// A durable record of what provisioning produced, so `doctor` can report a
/// skipped capability instead of silently having one fewer tool.
pub type ProvisionRecord = BTreeMap<String, StepOutcome>;
