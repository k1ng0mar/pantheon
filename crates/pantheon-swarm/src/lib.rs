//! Swarm (spec section 3): recursive delegation with runtime-owned caps.
//!
//! The runtime controls maximum depth, maximum concurrent agents, token
//! budget, tool-call budget, model restrictions, and cost. Recursive
//! spawning is allowed; accidental agent explosions are not. Every cap
//! violation is a structured error, never a silent no-op.
use pantheon_core::error::{Layer, PantheonError};

fn serr(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Agent, false, cause, remediation, "")
}

/// Caps owned by the runtime. Defaults are deliberately small.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Caps {
    pub max_depth: u32,
    pub max_concurrent: u32,
    pub max_total_agents: u32,
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
            token_budget: 200_000,
            tool_call_budget: 64,
            allowed_models: Vec::new(),
            cost_budget_micros: 500_000,
        }
    }
}

/// One live sub-agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentHandle {
    pub name: String,
    pub depth: u32,
    pub model: String,
}

/// Why a spawn was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnRefusal {
    Depth { requested: u32, max: u32 },
    Concurrency { live: u32, max: u32 },
    TotalAgents { spawned: u32, max: u32 },
    Tokens { used: u64, budget: u64 },
    ToolCalls { used: u64, budget: u64 },
    Cost { used_micros: u64, budget_micros: u64 },
    ModelNotAllowed { requested: String },
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
        }
    }
    pub fn message(&self) -> String {
        match self {
            SpawnRefusal::Depth { requested, max } => format!("depth {requested} exceeds max {max}"),
            SpawnRefusal::Concurrency { live, max } => format!("{live} live agents, max {max}"),
            SpawnRefusal::TotalAgents { spawned, max } => format!("{spawned} spawned total, max {max}"),
            SpawnRefusal::Tokens { used, budget } => format!("{used} tokens used, budget {budget}"),
            SpawnRefusal::ToolCalls { used, budget } => format!("{used} tool calls used, budget {budget}"),
            SpawnRefusal::Cost { used_micros, budget_micros } => format!("{used_micros} micros used, budget {budget_micros}"),
            SpawnRefusal::ModelNotAllowed { requested } => format!("model {requested} not in allowed list"),
        }
    }
}

/// Live swarm bookkeeping. All counters live here, not in the agents.
pub struct Swarm {
    caps: Caps,
    live: Vec<AgentHandle>,
    spawned_total: u32,
    tokens_used: u64,
    tool_calls_used: u64,
    cost_used_micros: u64,
}

impl Swarm {
    pub fn new(caps: Caps) -> Self {
        Self { caps, live: Vec::new(), spawned_total: 0, tokens_used: 0,
            tool_calls_used: 0, cost_used_micros: 0 }
    }

    pub fn live(&self) -> &[AgentHandle] { &self.live }

    /// Check every cap before spawning. Returns the refusal that fires first
    /// in a deterministic order, or Ok.
    pub fn check_spawn(&self, parent_depth: u32, model: &str) -> Result<(), SpawnRefusal> {
        let requested = parent_depth + 1;
        if requested > self.caps.max_depth {
            return Err(SpawnRefusal::Depth { requested, max: self.caps.max_depth });
        }
        if self.live.len() as u32 >= self.caps.max_concurrent {
            return Err(SpawnRefusal::Concurrency { live: self.live.len() as u32, max: self.caps.max_concurrent });
        }
        if self.spawned_total >= self.caps.max_total_agents {
            return Err(SpawnRefusal::TotalAgents { spawned: self.spawned_total, max: self.caps.max_total_agents });
        }
        if self.tokens_used >= self.caps.token_budget {
            return Err(SpawnRefusal::Tokens { used: self.tokens_used, budget: self.caps.token_budget });
        }
        if self.tool_calls_used >= self.caps.tool_call_budget {
            return Err(SpawnRefusal::ToolCalls { used: self.tool_calls_used, budget: self.caps.tool_call_budget });
        }
        if self.cost_used_micros >= self.caps.cost_budget_micros {
            return Err(SpawnRefusal::Cost {
                used_micros: self.cost_used_micros, budget_micros: self.caps.cost_budget_micros });
        }
        if !self.caps.allowed_models.is_empty()
            && !self.caps.allowed_models.iter().any(|m| m == model) {
            return Err(SpawnRefusal::ModelNotAllowed { requested: model.to_string() });
        }
        Ok(())
    }

    /// Spawn, or refuse with a structured error.
    pub fn spawn(&mut self, name: &str, parent_depth: u32, model: &str)
        -> Result<&AgentHandle, PantheonError> {
        if let Err(r) = self.check_spawn(parent_depth, model) {
            return Err(serr(r.code(), r.message(),
                "raise the cap explicitly or reduce delegation depth"));
        }
        self.live.push(AgentHandle {
            name: name.to_string(), depth: parent_depth + 1, model: model.to_string() });
        self.spawned_total += 1;
        Ok(self.live.last().expect("just pushed"))
    }

    /// Retire a sub-agent and fold its usage into the swarm counters.
    pub fn complete(&mut self, name: &str, tokens: u64, tool_calls: u64, cost_micros: u64) {
        if let Some(pos) = self.live.iter().position(|a| a.name == name) {
            self.live.remove(pos);
        }
        self.tokens_used = self.tokens_used.saturating_add(tokens);
        self.tool_calls_used = self.tool_calls_used.saturating_add(tool_calls);
        self.cost_used_micros = self.cost_used_micros.saturating_add(cost_micros);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_cap_blocks_level_two() {
        let mut s = Swarm::new(Caps { max_depth: 1, max_concurrent: 8,
            max_total_agents: 16, ..Caps::default() });
        assert!(s.spawn("researcher", 0, "sonnet").is_ok());
        let err = s.spawn("source-hunter", 1, "sonnet").unwrap_err();
        assert_eq!(err.code, "SWARM_MAX_DEPTH");
    }

    #[test]
    fn concurrency_cap_blocks_extra_agents() {
        let mut s = Swarm::new(Caps { max_concurrent: 2, max_depth: 5,
            max_total_agents: 16, ..Caps::default() });
        s.spawn("a", 0, "sonnet").unwrap();
        s.spawn("b", 0, "sonnet").unwrap();
        let err = s.spawn("c", 0, "sonnet").unwrap_err();
        assert_eq!(err.code, "SWARM_MAX_CONCURRENT");
    }

    #[test]
    fn budget_cap_fires_once_usage_is_folded_in() {
        let mut s = Swarm::new(Caps { token_budget: 1_000, max_total_agents: 16,
            ..Caps::default() });
        s.spawn("a", 0, "sonnet").unwrap();
        s.complete("a", 1_000, 0, 0);
        let err = s.spawn("b", 0, "sonnet").unwrap_err();
        assert_eq!(err.code, "SWARM_TOKEN_BUDGET");
    }

    #[test]
    fn model_restriction_is_enforced() {
        let mut s = Swarm::new(Caps { allowed_models: vec!["kimi".into()],
            ..Caps::default() });
        let err = s.spawn("a", 0, "sonnet").unwrap_err();
        assert_eq!(err.code, "SWARM_MODEL_NOT_ALLOWED");
        assert!(s.spawn("b", 0, "kimi").is_ok());
    }

    #[test]
    fn complete_frees_a_slot() {
        let mut s = Swarm::new(Caps { max_concurrent: 1, max_total_agents: 4,
            max_depth: 4, ..Caps::default() });
        s.spawn("a", 0, "sonnet").unwrap();
        assert!(s.spawn("b", 0, "sonnet").is_err());
        s.complete("a", 0, 0, 0);
        assert!(s.spawn("c", 0, "sonnet").is_ok());
    }
}
