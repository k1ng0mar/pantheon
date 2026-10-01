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
    Provider,
    Model,
    Reasoning,
    Workspace,
    Execution,
    DockerNetwork,
    Gateways,
    Tools,
    /// The skill-dependencies screen: every third-party package the
    /// skill library needs, detected with install-or-skip per missing
    /// item. Skills are mode-independent, so this screen runs in both
    /// Recommended and Full.
    SkillDeps,
    Browser,
    WebSearch,
    Tts,
    Memory,
    ComputerUse,
    Extensions,
    Fallback,
    ServiceInstall,
    Review,
    Provision,
    Done,
}

impl Section {
    /// Lowercase name used in the `SETUP · NAME` indicator.
    pub fn label(self) -> &'static str {
        match self {
            Section::Entry => "entry",
            Section::Provider => "model",
            Section::Model => "model",
            Section::Reasoning => "reasoning",
            Section::Workspace => "workspace",
            Section::Execution => "execution",
            Section::DockerNetwork => "network",
            Section::Gateways => "gateways",
            Section::Tools => "tools",
            Section::SkillDeps => "skill-deps",
            Section::Browser => "browser",
            Section::WebSearch => "search",
            Section::Tts => "speech",
            Section::Memory => "memory",
            Section::ComputerUse => "computer",
            Section::Extensions => "extensions",
            Section::Fallback => "fallback",
            Section::ServiceInstall => "service",
            Section::Review => "review",
            Section::Provision => "install",
            Section::Done => "ready",
        }
    }
}

/// Which entry mode the user chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Minimum decisions: pick provider+model, everything else gets
    /// recommended defaults.
    Recommended,
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
    pub memory_enabled: bool,
    pub computer_use_enabled: bool,
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
    let mode = a.mode.unwrap_or(Mode::Recommended);
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
        Mode::Recommended => {
            // Fixed provider screens, no Tools screen: the recommended
            // toolset is every group except Voice (STT/TTS are skipped
            // entirely), and the provider screens are driven by it, not
            // by tool answers. Memory native is keyless, so its screen
            // records silently.
            out.extend([
                Section::Provider,
                Section::Model,
                Section::WebSearch,
                Section::Browser,
                Section::Memory,
                Section::ComputerUse,
                // Skills work the same in every mode, so their
                // third-party packages are resolved here too.
                Section::SkillDeps,
                // The service-install permission screen: one question,
                // after every other screen, in both agent modes.
                Section::ServiceInstall,
                Section::Review,
                Section::Provision,
                Section::Done,
            ]);
            return out;
        }
        Mode::Full => {}
    }

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
    if a.gateways_enabled {
        out.push(Section::Gateways);
    }
    // The Tools screen always runs in Full: it is the one place the user
    // sees every capability, and its answers gate the provider screens
    // below. Policy stays the default coder preset — there is no
    // permissions screen anymore.
    out.push(Section::Tools);
    // Skill dependencies come right after the Tools screen: the skills
    // are part of the toolset, so their third-party packages are
    // resolved before the provider screens.
    out.push(Section::SkillDeps);
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
    if a.computer_use_enabled {
        out.push(Section::ComputerUse);
    }
    if a.extensions_enabled {
        out.push(Section::Extensions);
    }
    if a.fallback_enabled {
        out.push(Section::Fallback);
    }
    // The service-install permission screen: the last real screen in
    // Full, after every provider screen.
    out.push(Section::ServiceInstall);
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
