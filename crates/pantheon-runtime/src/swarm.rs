//! Swarm (spec section 3): recursive delegation with runtime-owned caps.
//!
//! The runtime controls maximum depth, maximum concurrent agents, token
//! budget, tool-call budget, model restrictions, and cost. Recursive
//! spawning is allowed; accidental agent explosions are not. Every cap
//! violation is a structured error, never a silent no-op.
//!
//! This module lives in `pantheon-runtime` because the caps are owned by
//! the runtime (the delegation path that enforces them is
//! `Session::drive`).
use pantheon_api::error::{Layer, PantheonError};

fn serr(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Agent, false, cause, remediation, "")
}

/// Caps owned by the runtime. Defaults are deliberately small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    pub max_depth: u32,
    pub max_concurrent: u32,
    pub max_total_agents: u32,
    /// Per-agent spawn cap: how many children one agent may spawn before
    /// further spawns from that agent are refused. Mirrors the `[swarm]`
    /// `max_subagents` config key (see `Caps::from_swarm_section`); an
    /// `[agents.<name>]` `swarm_max_subagents` override narrows it for
    /// that profile.
    pub max_subagents: u32,
    /// When false, a child agent (depth >= 1) that attempts to delegate
    /// gets a structured refusal instead of a grandchild. Mirrors the
    /// `[swarm]` `allow_child_spawn` config key.
    pub allow_child_spawn: bool,
    pub token_budget: u64,
    pub tool_call_budget: u64,
    /// Models a spawned agent may use; empty means inherit the default.
    pub allowed_models: Vec<String>,
    pub cost_budget_micros: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_concurrent: 4,
            max_total_agents: 8,
            max_subagents: 4,
            allow_child_spawn: true,
            token_budget: 200_000,
            tool_call_budget: 64,
            allowed_models: Vec::new(),
            cost_budget_micros: 500_000,
        }
    }
}

impl Caps {
    /// Build caps from the `[swarm]` config section. `max_subagents`
    /// maps to the per-agent spawn cap; budgets, the total-agent cap,
    /// and the model allowlist keep their built-in defaults (they have
    /// no config spelling yet).
    pub fn from_swarm_section(s: &pantheon_api::config::SwarmSection) -> Self {
        Self {
            max_depth: s.max_depth,
            max_concurrent: s.max_concurrent,
            max_subagents: s.max_subagents,
            allow_child_spawn: s.allow_child_spawn,
            ..Self::default()
        }
    }

    /// Effective per-agent spawn cap for one profile: the profile's own
    /// `swarm_max_subagents` override wins when set, else the global
    /// `[swarm]` value.
    pub fn for_profile(s: &pantheon_api::config::SwarmSection, profile_max: Option<u32>) -> Self {
        let mut caps = Self::from_swarm_section(s);
        if let Some(m) = profile_max {
            caps.max_subagents = m;
        }
        caps
    }
}

/// One live sub-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHandle {
    pub name: String,
    pub depth: u32,
    pub model: String,
    /// Agent that spawned this child ("": unknown). Lets `complete`
    /// release the per-parent slot the spawn took.
    pub parent: String,
}

/// Why a spawn was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRefusal {
    Depth {
        requested: u32,
        max: u32,
    },
    Concurrency {
        live: u32,
        max: u32,
    },
    TotalAgents {
        spawned: u32,
        max: u32,
    },
    Tokens {
        used: u64,
        budget: u64,
    },
    ToolCalls {
        used: u64,
        budget: u64,
    },
    Cost {
        used_micros: u64,
        budget_micros: u64,
    },
    ModelNotAllowed {
        requested: String,
    },
    /// One agent spawned more children than its per-agent cap allows.
    PerAgentCap {
        parent: String,
        spawned: u32,
        max: u32,
    },
    /// `allow_child_spawn = false` and a child (depth >= 1) tried to delegate.
    ChildSpawnDenied {
        parent_depth: u32,
    },
}

impl SpawnRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            SpawnRefusal::Depth { .. } => "SWARM_MAX_DEPTH",
            SpawnRefusal::Concurrency { .. } => "SWARM_MAX_CONCURRENT",
            SpawnRefusal::TotalAgents { .. } => "SWARM_MAX_AGENTS",
            SpawnRefusal::Tokens { .. } => "SWARM_TOKEN_BUDGET",
            SpawnRefusal::ToolCalls { .. } => "SWARM_TOOL_BUDGET",
            SpawnRefusal::Cost { .. } => "SWARM_COST_BUDGET",
            SpawnRefusal::ModelNotAllowed { .. } => "SWARM_MODEL_NOT_ALLOWED",
            SpawnRefusal::PerAgentCap { .. } => "SWARM_PER_AGENT_CAP",
            SpawnRefusal::ChildSpawnDenied { .. } => "SWARM_CHILD_SPAWN_DENIED",
        }
    }
    pub fn message(&self) -> String {
        match self {
            SpawnRefusal::Depth { requested, max } => {
                format!("depth {requested} exceeds max {max}")
            }
            SpawnRefusal::Concurrency { live, max } => format!("{live} live agents, max {max}"),
            SpawnRefusal::TotalAgents { spawned, max } => {
                format!("{spawned} spawned total, max {max}")
            }
            SpawnRefusal::Tokens { used, budget } => format!("{used} tokens used, budget {budget}"),
            SpawnRefusal::ToolCalls { used, budget } => {
                format!("{used} tool calls used, budget {budget}")
            }
            SpawnRefusal::Cost {
                used_micros,
                budget_micros,
            } => format!("{used_micros} micros used, budget {budget_micros}"),
            SpawnRefusal::ModelNotAllowed { requested } => {
                format!("model {requested} not in allowed list")
            }
            SpawnRefusal::PerAgentCap {
                parent,
                spawned,
                max,
            } => format!("agent {parent} spawned {spawned}, per-agent cap {max}"),
            SpawnRefusal::ChildSpawnDenied { parent_depth } => format!(
                "child at depth {parent_depth} may not delegate (allow_child_spawn = false)"
            ),
        }
    }
}

/// Live swarm bookkeeping. All counters live here, not in the agents.
pub struct Swarm {
    caps: Caps,
    live: Vec<AgentHandle>,
    spawned_total: u32,
    /// Children spawned per parent agent name: enforces the per-agent
    /// `max_subagents` cap. Counts *live* children only - `complete`
    /// releases the parent's slot, so a long session delegating many
    /// small tasks cannot deadlock against its own history.
    spawned_by: std::collections::HashMap<String, u32>,
    tokens_used: u64,
    tool_calls_used: u64,
    cost_used_micros: u64,
}

impl Swarm {
    pub fn new(caps: Caps) -> Self {
        Self {
            caps,
            live: Vec::new(),
            spawned_total: 0,
            spawned_by: std::collections::HashMap::new(),
            tokens_used: 0,
            tool_calls_used: 0,
            cost_used_micros: 0,
        }
    }

    /// Check every cap before spawning. Returns the refusal that fires first
    /// in a deterministic order, or Ok.
    ///
    /// `parent` names the delegating agent ("" when unknown): its own
    /// spawn count is checked against the per-agent cap, and when
    /// `allow_child_spawn` is false a parent at depth >= 1 (itself a
    /// child) is refused outright.
    pub fn check_spawn(
        &self,
        parent: &str,
        parent_depth: u32,
        model: &str,
    ) -> Result<(), SpawnRefusal> {
        let requested = parent_depth + 1;
        if requested > self.caps.max_depth {
            return Err(SpawnRefusal::Depth {
                requested,
                max: self.caps.max_depth,
            });
        }
        if !self.caps.allow_child_spawn && parent_depth >= 1 {
            return Err(SpawnRefusal::ChildSpawnDenied { parent_depth });
        }
        if self.live.len() as u32 >= self.caps.max_concurrent {
            return Err(SpawnRefusal::Concurrency {
                live: self.live.len() as u32,
                max: self.caps.max_concurrent,
            });
        }
        if self.spawned_total >= self.caps.max_total_agents {
            return Err(SpawnRefusal::TotalAgents {
                spawned: self.spawned_total,
                max: self.caps.max_total_agents,
            });
        }
        let by_parent = self.spawned_by.get(parent).copied().unwrap_or(0);
        if by_parent >= self.caps.max_subagents {
            return Err(SpawnRefusal::PerAgentCap {
                parent: parent.to_string(),
                spawned: by_parent,
                max: self.caps.max_subagents,
            });
        }
        if self.tokens_used >= self.caps.token_budget {
            return Err(SpawnRefusal::Tokens {
                used: self.tokens_used,
                budget: self.caps.token_budget,
            });
        }
        if self.tool_calls_used >= self.caps.tool_call_budget {
            return Err(SpawnRefusal::ToolCalls {
                used: self.tool_calls_used,
                budget: self.caps.tool_call_budget,
            });
        }
        if self.cost_used_micros >= self.caps.cost_budget_micros {
            return Err(SpawnRefusal::Cost {
                used_micros: self.cost_used_micros,
                budget_micros: self.caps.cost_budget_micros,
            });
        }
        if !self.caps.allowed_models.is_empty()
            && !self.caps.allowed_models.iter().any(|m| m == model)
        {
            return Err(SpawnRefusal::ModelNotAllowed {
                requested: model.to_string(),
            });
        }
        Ok(())
    }

    /// Spawn, or refuse with a structured error. `parent` names the
    /// delegating agent for the per-agent cap ("" when unknown).
    pub fn spawn_for(
        &mut self,
        parent: &str,
        name: &str,
        parent_depth: u32,
        model: &str,
    ) -> Result<AgentHandle, PantheonError> {
        if let Err(r) = self.check_spawn(parent, parent_depth, model) {
            return Err(serr(
                r.code(),
                r.message(),
                "raise the cap explicitly or reduce delegation depth",
            ));
        }
        self.live.push(AgentHandle {
            name: name.to_string(),
            depth: parent_depth + 1,
            model: model.to_string(),
            parent: parent.to_string(),
        });
        self.spawned_total += 1;
        *self.spawned_by.entry(parent.to_string()).or_insert(0) += 1;
        Ok(self.live.last().expect("just pushed").clone())
    }

    /// Spawn without a known parent (per-agent cap tracked under "").
    pub fn spawn(
        &mut self,
        name: &str,
        parent_depth: u32,
        model: &str,
    ) -> Result<AgentHandle, PantheonError> {
        self.spawn_for("", name, parent_depth, model)
    }

    /// Retire a sub-agent and fold its usage into the swarm counters.
    /// Also releases the parent's per-agent slot: the cap counts live
    /// children, so a settled task frees its slot for the next spawn.
    pub fn complete(&mut self, name: &str, tokens: u64, tool_calls: u64, cost_micros: u64) {
        if let Some(pos) = self.live.iter().position(|a| a.name == name) {
            let handle = self.live.remove(pos);
            if let Some(count) = self.spawned_by.get_mut(&handle.parent) {
                *count = count.saturating_sub(1);
            }
        }
        self.tokens_used = self.tokens_used.saturating_add(tokens);
        self.tool_calls_used = self.tool_calls_used.saturating_add(tool_calls);
        self.cost_used_micros = self.cost_used_micros.saturating_add(cost_micros);
    }
}

/// How a sub-agent's work ended, from the child's own report.
///
/// Parsed in Rust from the child's return text - never by the LLM. When
/// the child does not return the envelope, the status is `Unknown` and
/// the parent must treat the result as unverified, not as done.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    /// The child reports the task fully done.
    Completed,
    /// Done in part; `open_questions` / `followups` say what remains.
    Partial,
    /// The child reports it could not do the task.
    Failed,
    /// The child did not return the envelope (free text, or nothing
    /// parseable). Fail-closed: this is NOT `Completed`.
    #[default]
    Unknown,
}

impl<'de> serde::Deserialize<'de> for ChildStatus {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Lenient on purpose: a child that writes "done" or "COMPLETE"
        // still degrades to a known status instead of failing the parse.
        // Anything unrecognized is Unknown - never Completed.
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "completed" | "complete" | "done" | "success" => ChildStatus::Completed,
            "partial" | "partly" | "incomplete" => ChildStatus::Partial,
            "failed" | "failure" | "error" => ChildStatus::Failed,
            _ => ChildStatus::Unknown,
        })
    }
}

/// Fixed machine-parseable return contract for a spawned sub-agent.
///
/// The spawn prompt tells the child to wrap its final answer in a
/// ```child-result fenced JSON block with exactly these fields.
/// `parse_child_result` extracts it; anything else degrades to
/// `status: unknown` with the raw text as `summary` - never an error,
/// never a silent pass.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildResult {
    #[serde(default)]
    pub status: ChildStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followups: Vec<String>,
}

impl ChildResult {
    /// The graceful-degradation result: non-conforming child output is
    /// wrapped, not rejected. Status stays `Unknown` so the parent can
    /// never mistake it for a completed task.
    pub fn unknown(summary: &str) -> Self {
        Self {
            status: ChildStatus::Unknown,
            files_changed: Vec::new(),
            summary: summary.trim().to_string(),
            decisions: Vec::new(),
            open_questions: Vec::new(),
            followups: Vec::new(),
        }
    }

    /// Canonical JSON for the parent transcript: stable field order,
    /// no extra whitespace. The parent model and any later Rust
    /// consumer parse the same bytes.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            "{\"status\":\"unknown\",\"summary\":\"serialization failed\"}".to_string()
        })
    }

    /// True only when the child positively reported completion. Every
    /// other status - including `Unknown` - means the parent must not
    /// treat the delegation as done.
    pub fn is_completed(&self) -> bool {
        self.status == ChildStatus::Completed
    }
}

/// Fence name the child wraps its envelope in: ```child-result ... ```.
pub const RESULT_FENCE: &str = "child-result";

/// Extract the envelope from a child's return text. Pure Rust, no LLM.
///
/// Accepts (in order): a fenced ```child-result block, a fenced
/// ```json block, or the whole text as bare JSON. Missing fields take
/// their defaults; an unrecognized `status` string degrades to
/// `Unknown`. Anything unparseable becomes `ChildResult::unknown`
/// the raw text preserved as `summary`, never dropped, never an error.
pub fn parse_child_result(text: &str) -> ChildResult {
    if let Some(block) = fenced_block(text, RESULT_FENCE)
        .or_else(|| fenced_block(text, "json"))
        .or_else(|| {
            let t = text.trim();
            (!t.is_empty()).then_some(t.to_string())
        })
    {
        if let Ok(mut parsed) = serde_json::from_str::<ChildResult>(&block) {
            parsed.summary = parsed.summary.trim().to_string();
            return parsed;
        }
    }
    ChildResult::unknown(text)
}

/// The contract text inlined verbatim into every spawn prompt (see
/// `pantheon_runtime`'s delegate-session builder). The child cannot
/// inherit it from the parent's context, so it travels with the task.
pub fn result_contract() -> &'static str {
    "When you finish, your FINAL message must be a machine-readable result envelope, \
     wrapped in a fenced block like this:\n\
     ```child-result\n\
     {\"status\": \"completed\", \"files_changed\": [\"path/one.rs\"], \
     \"summary\": \"one or two sentences on what was actually done\", \
     \"decisions\": [\"chose X because Y\"], \
     \"open_questions\": [\"anything unresolved\"], \
     \"followups\": [\"suggested next steps\"]}\n\
     ```\n\
     `status` is one of: completed | partial | failed | unknown. \
     Use completed ONLY if the task is fully done; partial if part remains \
     (say what in open_questions/followups); failed if you could not do it. \
     If you cannot produce this envelope, write your answer normally - \
     it will be treated as unverified, not as done."
}

/// Extract the first fenced code block with the given info string.
/// The fence may carry trailing attributes (```child-result json);
/// matching is on the first whitespace-separated token, case-insensitive.
/// Returns the block's owned text (without the fences).
fn fenced_block(text: &str, fence: &str) -> Option<String> {
    let mut lines = text.lines();
    loop {
        let line = lines.next()?;
        let t = line.trim_start();
        if !t.starts_with("```") {
            continue;
        }
        let info = t[3..].split_whitespace().next().unwrap_or("");
        if !info.eq_ignore_ascii_case(fence) {
            continue;
        }
        let mut block = String::new();
        for line in lines.by_ref() {
            if line.trim_start().starts_with("```") {
                return Some(block);
            }
            block.push_str(line);
            block.push('\n');
        }
        return None;
    }
}
