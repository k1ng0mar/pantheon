pub mod agent_profile {
    //! agent profiles. config section is `[agents.<name>]`, the type lives here
    //! because runtime/storage/agent all need it and none of them should be
    //! importing the cli crate for it.
    //!
    //! merge rules are per field, not "deep merge everything":
    //!   soul/user/policy/model/display_name/swarm cap -> child overrides
    //!   agents_file -> layers, parent then child
    //!   memory_namespace -> never inherited. this is the one that matters:
    //!     two profiles on one namespace means one agent can read another's
    //!     memory.
    //!
    //! every resolved value carries where it came from (`Resolved<T>`), mostly
    //! because "why is this agent on reader policy" is otherwise unanswerable
    //! without replaying the whole chain by hand.
    //!
    //! bad chains (cycle, missing parent, too deep, bad name) fail at load. no
    //! fallback to default, that would just hide a broken config.

    use serde::{Deserialize, Serialize};
    use std::collections::{HashMap, HashSet};

    /// 8 is arbitrary. deep enough for persona layering, shallow enough that a
    /// typo'd inherits loop fails fast instead of walking forever.
    pub const MAX_INHERIT_DEPTH: usize = 8;

    /// implicit baseline, also what you get when nothing is selected.
    pub const DEFAULT_PROFILE: &str = "default";

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum Origin {
        /// set on the profile itself
        Own { profile: String },
        /// inherited, `ancestor` is whoever set it
        Inherited { profile: String, ancestor: String },
        /// we filled it in, nobody set it
        Runtime { reason: String },
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Resolved<T> {
        pub value: T,
        pub origin: Origin,
    }

    impl<T> Resolved<T> {
        pub fn own(profile: &str, value: T) -> Self {
            Self {
                value,
                origin: Origin::Own {
                    profile: profile.to_string(),
                },
            }
        }
        pub fn inherited(profile: &str, ancestor: &str, value: T) -> Self {
            Self {
                value,
                origin: Origin::Inherited {
                    profile: profile.to_string(),
                    ancestor: ancestor.to_string(),
                },
            }
        }
        pub fn runtime(reason: &str, value: T) -> Self {
            Self {
                value,
                origin: Origin::Runtime {
                    reason: reason.to_string(),
                },
            }
        }
        /// who set it. for inherited that's the ancestor, not whoever asked -
        /// returning the requester makes everything look self-authored.
        pub fn supplied_by(&self) -> Option<&str> {
            match &self.origin {
                Origin::Own { profile } => Some(profile),
                Origin::Inherited { ancestor, .. } => Some(ancestor),
                Origin::Runtime { .. } => None,
            }
        }
        /// who asked for the resolve
        pub fn requested_by(&self) -> Option<&str> {
            match &self.origin {
                Origin::Own { profile } | Origin::Inherited { profile, .. } => Some(profile),
                Origin::Runtime { .. } => None,
            }
        }
    }

    /// the profile as written in config. every Option means "not set here",
    /// which means "look at the parent". not the effective profile.
    #[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
    pub struct AgentProfile {
        /// defaults to the table name
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub display_name: Option<String>,
        /// SOUL.md equivalent
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub soul_file: Option<String>,
        /// AGENTS.md equivalent. layers instead of overriding
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub agents_file: Option<String>,
        /// USER.md equivalent
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub user_file: Option<String>,
        /// parent profile name
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub inherits: Option<String>,
        /// never inherited, see module docs
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub memory_namespace: Option<String>,
        /// reader | coder | coder_memory
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub policy: Option<String>,
        /// model pin. agents never choose their own model
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub provider: Option<String>,
        /// dashboard/mobile only, runtime ignores it. `preset:avatar-N` or an
        /// absolute path
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub avatar: Option<String>,
        /// per-profile override of `[swarm] max_subagents`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub swarm_max_subagents: Option<u32>,
    }

    impl AgentProfile {
        /// A profile's own name is its table name, not a field.
        pub fn name<'a>(&'a self, table: &'a str) -> &'a str {
            self.display_name.as_deref().unwrap_or(table)
        }

        /// The effective memory namespace for a standalone profile: explicit,
        /// else `agent:<name>`. This is the same derivation the config loader
        /// uses to detect namespace collisions, kept here so both agree.
        pub fn namespace(&self, table: &str) -> String {
            self.memory_namespace
                .clone()
                .unwrap_or_else(|| format!("agent:{table}"))
        }

        /// A profile name must be a slug: it appears in table headers, memory
        /// namespaces, and artifact ids. The canonical rule lives in
        /// `pantheon-api::ident` so `pantheon-storage` can check names without
        /// depending on this crate.
        pub fn is_slug(name: &str) -> bool {
            crate::ident::is_slug(name)
        }
    }

    /// effective profile. the runtime only ever sees this, so a half-resolved
    /// profile can't reach a Session.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct EffectiveProfile {
        /// table name, and the stable key
        pub name: String,
        /// internal id. separate from `name` so a rename doesn't steal history
        pub agent_id: String,
        pub display_name: Resolved<String>,
        pub parent: Option<String>,
        pub memory_namespace: Resolved<String>,
        pub policy: Resolved<String>,
        pub soul_file: Resolved<Option<String>>,
        pub user_file: Resolved<Option<String>>,
        /// parent first, then child, tagged with who contributed it
        pub agents_files: Vec<(String, String)>,
        pub model: Resolved<Option<String>>,
        pub provider: Resolved<Option<String>>,
        /// None + Runtime origin means nobody in the chain set it; caller falls
        /// back to `[swarm] max_subagents`
        pub swarm_max_subagents: Resolved<Option<u32>>,
    }

    impl EffectiveProfile {
        /// for logs, tui headers, ledger rows
        pub fn identity(&self) -> String {
            match &self.parent {
                Some(p) => format!("{} (inherits {})", self.display_name.value, p),
                None => self.display_name.value.clone(),
            }
        }
        /// instruction files as one prompt block. each chunk says which profile
        /// it came from, otherwise layered rules are invisible in the transcript.
        pub fn instructions_block(&self, read: &dyn Fn(&str) -> Option<String>) -> String {
            let mut out = String::new();
            for (profile, path) in &self.agents_files {
                match read(path) {
                    Some(body) => {
                        out.push_str(&format!("# instructions from profile: {profile}\n"));
                        out.push_str(body.trim());
                        out.push_str("\n\n");
                    }
                    None => {
                        out.push_str(&format!(
                            "# instructions from profile: {profile}\n# (file {path} could not be read)\n\n"
                        ));
                    }
                }
            }
            out
        }
    }

    /// load-time only, not recoverable. we refuse the config.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ProfileError {
        /// not a slug, or blank
        InvalidName { name: String, why: String },
        /// asked for a profile nobody declared. NOT the same as MissingParent -
        /// conflating those two is what made a rootless profile report itself as
        /// a missing parent.
        UnknownProfile { name: String },
        /// inherits points at something undeclared
        MissingParent { profile: String, parent: String },
        /// inherits loops
        Cycle { chain: Vec<String> },
        /// deeper than MAX_INHERIT_DEPTH
        TooDeep { chain: Vec<String>, max: usize },
        /// two profiles want the same memory namespace
        NamespaceClash {
            a: String,
            b: String,
            namespace: String,
        },
        /// a profile named its parent's namespace. own variant on purpose, this
        /// is the one that leaks memory between agents.
        InheritedNamespace { profile: String, namespace: String },
        /// unknown policy preset
        UnknownPolicy { profile: String, policy: String },
    }

    impl std::fmt::Display for ProfileError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::InvalidName { name, why } => {
                    write!(f, "agent profile {name:?} is invalid: {why}")
                }
                Self::UnknownProfile { name } => {
                    write!(f, "no agent profile named {name:?} is declared")
                }
                Self::MissingParent { profile, parent } => write!(
                    f,
                    "agent profile {profile:?} inherits from {parent:?}, which is not declared"
                ),
                Self::Cycle { chain } => {
                    write!(f, "agent profile inheritance cycle: {}", chain.join(" -> "))
                }
                Self::TooDeep { chain, max } => write!(
                    f,
                    "agent profile inheritance chain is deeper than {max}: {}",
                    chain.join(" -> ")
                ),
                Self::NamespaceClash { a, b, namespace } => write!(
                    f,
                    "agent profiles {a:?} and {b:?} share memory namespace {namespace:?}; \
                     namespaces must be unique or memory is not isolated"
                ),
                Self::InheritedNamespace { profile, namespace } => write!(
                    f,
                    "agent profile {profile:?} cannot inherit memory namespace {namespace:?}; \
                     memory is never inherited between profiles"
                ),
                Self::UnknownPolicy { profile, policy } => write!(
                    f,
                    "agent profile {profile:?} policy {policy:?} is unknown (reader|coder|coder_memory)"
                ),
            }
        }
    }

    impl std::error::Error for ProfileError {}

    #[derive(Debug, Clone, Default)]
    pub struct ProfileRegistry {
        profiles: HashMap<String, AgentProfile>,
    }

    /// has to stay in sync with PolicyPreset::as_str in the cli. it's three
    /// strings and there's a test on it.
    pub const KNOWN_POLICIES: [&str; 3] = ["reader", "coder", "coder_memory"];

    impl ProfileRegistry {
        pub fn new() -> Self {
            Self::default()
        }

        /// rejects a bad name or a cycle right away, so a broken config never
        /// makes it to disk.
        pub fn insert(&mut self, name: &str, profile: AgentProfile) -> Result<(), ProfileError> {
            if !AgentProfile::is_slug(name) {
                return Err(ProfileError::InvalidName {
                    name: name.to_string(),
                    why: "must be a slug (letters, digits, - _)".to_string(),
                });
            }
            self.profiles.insert(name.to_string(), profile);
            // cheap to catch here, and only when this insert actually adds an
            // edge. otherwise every insert walks the whole graph for nothing.
            if self
                .profiles
                .get(name)
                .and_then(|p| p.inherits.as_ref())
                .is_some()
            {
                if let Some(chain) = self.find_cycle(name) {
                    return Err(ProfileError::Cycle { chain });
                }
            }
            // missing parents wait until resolve. profiles can be declared in
            // any table order.
            self.validate_policy_names()?;
            Ok(())
        }

        fn validate_policy_names(&self) -> Result<(), ProfileError> {
            for (name, p) in &self.profiles {
                if let Some(pol) = &p.policy {
                    if !KNOWN_POLICIES.contains(&pol.as_str()) {
                        return Err(ProfileError::UnknownPolicy {
                            profile: name.clone(),
                            policy: pol.clone(),
                        });
                    }
                }
            }
            Ok(())
        }

        pub fn get(&self, name: &str) -> Option<&AgentProfile> {
            self.profiles.get(name)
        }
        pub fn names(&self) -> Vec<String> {
            let mut v: Vec<String> = self.profiles.keys().cloned().collect();
            v.sort();
            v
        }
        pub fn len(&self) -> usize {
            self.profiles.len()
        }
        pub fn is_empty(&self) -> bool {
            self.profiles.is_empty()
        }

        /// walk inherits upward, root first. three ways this stops and they are
        /// not interchangeable: rootless profile = ok, one entry. undeclared
        /// name = UnknownProfile. declared profile pointing at a missing parent
        /// = MissingParent.
        fn chain(&self, name: &str) -> Result<Vec<String>, ProfileError> {
            let Some(start) = self.profiles.get(name) else {
                return Err(ProfileError::UnknownProfile {
                    name: name.to_string(),
                });
            };
            let mut chain: Vec<String> = vec![name.to_string()];
            let mut seen: HashSet<String> = HashSet::new();
            seen.insert(name.to_string());
            // The requested profile exists; start from its declared parent.
            let Some(mut cursor) = start.inherits.clone() else {
                return Ok(chain);
            };
            loop {
                if !seen.insert(cursor.clone()) {
                    chain.push(cursor);
                    return Err(ProfileError::Cycle { chain });
                }
                chain.push(cursor.clone());
                if chain.len() > MAX_INHERIT_DEPTH {
                    return Err(ProfileError::TooDeep {
                        chain,
                        max: MAX_INHERIT_DEPTH,
                    });
                }
                let Some(decl) = self.profiles.get(&cursor) else {
                    return Err(ProfileError::MissingParent {
                        profile: name.to_string(),
                        parent: cursor,
                    });
                };
                match decl.inherits.clone() {
                    Some(next) => cursor = next,
                    // A declared parent with no `inherits` is the chain root.
                    None => return Ok(chain),
                }
            }
        }

        fn find_cycle(&self, start: &str) -> Option<Vec<String>> {
            let mut seen: HashSet<String> = HashSet::new();
            let mut chain: Vec<String> = Vec::new();
            let mut cursor = start.to_string();
            loop {
                if !seen.insert(cursor.clone()) {
                    chain.push(cursor);
                    return Some(chain);
                }
                chain.push(cursor.clone());
                cursor = self
                    .profiles
                    .get(&cursor)
                    .and_then(|p| p.inherits.clone())?;
            }
        }

        /// `default_policy` is what the runtime falls back to when nothing in
        /// the chain sets one. passed in so this crate stays out of policy.
        pub fn resolve(
            &self,
            name: &str,
            default_policy: &str,
        ) -> Result<EffectiveProfile, ProfileError> {
            if !AgentProfile::is_slug(name) {
                return Err(ProfileError::InvalidName {
                    name: name.to_string(),
                    why: "must be a slug (letters, digits, - _)".to_string(),
                });
            }
            // root first, so last writer wins == child overrides parent
            let mut chain = self.chain(name)?;
            chain.reverse();
            let parent = if chain.len() > 1 {
                Some(chain[chain.len() - 2].clone())
            } else {
                None
            };

            let mut display_name: Option<Resolved<String>> = None;
            let mut soul_file: Option<Resolved<Option<String>>> = None;
            let mut user_file: Option<Resolved<Option<String>>> = None;
            let mut policy: Option<Resolved<String>> = None;
            let mut model: Option<Resolved<Option<String>>> = None;
            let mut provider: Option<Resolved<Option<String>>> = None;
            let mut swarm_max_subagents: Option<Resolved<Option<u32>>> = None;
            let mut agents_files: Vec<(String, String)> = Vec::new();
            let mut own_namespace: Option<String> = None;

            for profile_name in &chain {
                let Some(decl) = self.profiles.get(profile_name) else {
                    // Reached only for an undeclared intermediate; the chain
                    // walk already refused a genuinely missing parent.
                    continue;
                };
                let is_own = profile_name == name;
                let mark_str = |v: &str| -> Resolved<String> {
                    if is_own {
                        Resolved::own(profile_name, v.to_string())
                    } else {
                        Resolved::inherited(name, profile_name, v.to_string())
                    }
                };
                let mark_opt = |v: &Option<String>| -> Resolved<Option<String>> {
                    if is_own {
                        Resolved::own(profile_name, v.clone())
                    } else {
                        Resolved::inherited(name, profile_name, v.clone())
                    }
                };
                // override: last writer wins, which is the child
                if let Some(d) = &decl.display_name {
                    display_name = Some(mark_str(d));
                }
                // absent means inherit, not "clear". Option<String> can't tell
                // absent from null and guessing would silently drop a parent's
                // persona.
                if decl.soul_file.is_some() {
                    soul_file = Some(mark_opt(&decl.soul_file));
                }
                // same rule as soul_file
                if decl.user_file.is_some() {
                    user_file = Some(mark_opt(&decl.user_file));
                }
                if let Some(p) = &decl.policy {
                    policy = Some(mark_str(p));
                }
                if decl.model.is_some() {
                    model = Some(mark_opt(&decl.model));
                }
                if decl.provider.is_some() {
                    provider = Some(mark_opt(&decl.provider));
                }
                // same rule again
                if decl.swarm_max_subagents.is_some() {
                    let v = &decl.swarm_max_subagents;
                    swarm_max_subagents = Some(if is_own {
                        Resolved::own(profile_name, *v)
                    } else {
                        Resolved::inherited(name, profile_name, *v)
                    });
                }
                // the one field that layers instead of overriding
                if let Some(f) = &decl.agents_file {
                    agents_files.push((profile_name.clone(), f.clone()));
                }
                // memory only ever comes off the profile itself
                if is_own {
                    own_namespace = decl.memory_namespace.clone();
                }
            }

            // naming an ancestor's namespace is a config error
            if let Some(ns) = &own_namespace {
                if let Some(ancestor) = &parent {
                    if let Some(decl) = self.profiles.get(ancestor) {
                        if decl.namespace(ancestor) == *ns && decl.memory_namespace.is_some() {
                            return Err(ProfileError::InheritedNamespace {
                                profile: name.to_string(),
                                namespace: ns.clone(),
                            });
                        }
                    }
                }
            }

            let namespace = match own_namespace {
                Some(ns) if !ns.trim().is_empty() => Resolved::own(name, ns),
                _ => Resolved::runtime(
                    "default per-profile namespace agent:<name>",
                    format!("agent:{name}"),
                ),
            };

            let policy = policy.unwrap_or_else(|| {
                Resolved::runtime(
                    "no profile or ancestor set a policy",
                    default_policy.to_string(),
                )
            });

            Ok(EffectiveProfile {
                name: name.to_string(),
                agent_id: agent_id_for(name),
                display_name: display_name.unwrap_or_else(|| Resolved::own(name, name.to_string())),
                parent,
                memory_namespace: namespace,
                policy,
                soul_file: soul_file
                    .unwrap_or_else(|| Resolved::runtime("no persona file declared", None)),
                user_file: user_file
                    .unwrap_or_else(|| Resolved::runtime("no user-context file declared", None)),
                agents_files,
                model: model.unwrap_or_else(|| {
                    Resolved::runtime("no model pin; runtime default is used", None)
                }),
                provider: provider.unwrap_or_else(|| {
                    Resolved::runtime("no provider pin; runtime default is used", None)
                }),
                swarm_max_subagents: swarm_max_subagents.unwrap_or_else(|| {
                    Resolved::runtime(
                        "no profile set swarm_max_subagents; falls back to [swarm] max_subagents",
                        None,
                    )
                }),
            })
        }

        /// check the whole table. namespace clash is here and not in `resolve`
        /// because it's a property of the set, not of one profile.
        pub fn validate_all(&self, default_policy: &str) -> Result<(), ProfileError> {
            let problems = self.problems(default_policy);
            match problems.into_iter().next() {
                Some(first) => Err(first),
                None => Ok(()),
            }
        }

        /// all of them, not just the first. doctor needs the full list, else you
        /// fix one thing, rerun, fix the next, repeat. order is stable.
        pub fn problems(&self, default_policy: &str) -> Vec<ProfileError> {
            let mut out = Vec::new();
            let mut by_ns: HashMap<String, String> = HashMap::new();
            // policy name first. a typo there usually explains everything after
            let mut policies: Vec<(&String, &String)> = self
                .profiles
                .iter()
                .filter_map(|(n, p)| p.policy.as_ref().map(|pol| (n, pol)))
                .collect();
            policies.sort();
            for (name, pol) in policies {
                if !KNOWN_POLICIES.contains(&pol.as_str()) {
                    out.push(ProfileError::UnknownPolicy {
                        profile: name.clone(),
                        policy: pol.clone(),
                    });
                }
            }
            for name in self.names() {
                match self.resolve(&name, default_policy) {
                    Err(e) => out.push(e),
                    Ok(eff) => {
                        let ns = eff.memory_namespace.value.clone();
                        if let Some(prev) = by_ns.get(&ns) {
                            out.push(ProfileError::NamespaceClash {
                                a: prev.clone(),
                                b: name,
                                namespace: ns,
                            });
                        } else {
                            by_ns.insert(ns, name);
                        }
                    }
                }
            }
            out
        }
    }

    /// greppable id. separate from the display name so renaming a profile
    /// doesn't hand its old history to whatever takes the name next.
    pub fn agent_id_for(name: &str) -> String {
        format!("agent:{name}")
    }
}

pub mod mode {
    //! build vs plan. plan is a read-only posture the host enforces, not
    //! something we ask the model to respect. tab flips it in the tui, it's
    //! session-scoped, and the gate refuses mutating tools at call time.
    //!
    //! [`Session`]: pantheon_runtime::session::Session
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    pub enum AgentMode {
        #[default]
        Build,
        /// mutating tools get refused at the gate, see PLAN_MODE_REFUSAL
        Plan,
    }

    impl AgentMode {
        /// "build" / "plan"
        pub fn as_str(self) -> &'static str {
            match self {
                AgentMode::Build => "build",
                AgentMode::Plan => "plan",
            }
        }

        /// inverse of as_str, case-insensitive
        pub fn parse(s: &str) -> Option<Self> {
            match s.trim().to_lowercase().as_str() {
                "build" => Some(AgentMode::Build),
                "plan" => Some(AgentMode::Plan),
                _ => None,
            }
        }
    }

    /// what the model sees instead of the tool running. it's a normal tool
    /// result, so the run neither parks nor fails, it just keeps planning.
    pub const PLAN_MODE_REFUSAL: &str = "Plan mode is active: write/execute tools are disabled. \
        Describe your plan instead; do not attempt the change.";

    /// exact match only. we had a prefix rule before and it let `read_write`
    /// through as read-only because it starts with "read". anything not on this
    /// list counts as mutating.
    const READ_ONLY_TOOLS: &[&str] = &[
        "read_file",
        "preview_file",
        "list_dir",
        "list_checkpoints",
        "todo",
        "memory_recall",
        "memory_list",
        "session_search",
        "vault_read",
        "vault_list",
        "vault_search",
        "skill_read",
        "skills_list",
        "web_search",
        "ask_user",
        // Provider-registered read-only tools, listed exactly so similarly-named
        // mutating tools cannot ride a prefix family.
        "glob_files",
        "grep_code",
        "web_fetch_page",
        "get_config",
        "find_refs",
        "think",
        "reflect_step",
    ];

    /// by name only. even `ls` is blocked in plan mode: deciding which shell
    /// commands are harmless means parsing shell, and that's the hole plan mode
    /// exists to close.
    pub fn is_mutating_tool(name: &str) -> bool {
        let n = name.trim().to_lowercase();
        !READ_ONLY_TOOLS.iter().any(|t| *t == n)
    }
}

pub mod nightly {
    //! nightly proposal DTOs and the one LLM seam.
    //!
    //! they sit at the api layer because pantheon-providers has to implement
    //! NightlyLlm and a provider leaf must not depend on a pipeline crate.
    //! pantheon-nightly re-exports them and owns the pipeline.
    use crate::model::{AuxiliaryKind, AuxiliaryModel, ModelPolicy};

    /// (run, turn) is the provenance unit. ts_ms is event time, it feeds the
    /// recency half of memory promotion.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct TurnRef {
        pub run_id: String,
        pub turn_id: String,
        pub ts_ms: i64,
    }

    /// What the nightly pass wants to change.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum ProposalKind {
        /// durable lesson. never auto-applied, and it can't overwrite a
        /// user-confirmed record.
        MemoryLesson { key: String },
        /// create/update a SKILL.md. eval, then replay, then approval.
        Skill { name: String, update: bool },
        /// persona note, stored as agent-layer memory under `persona` so it
        /// doesn't mean editing user config.
        Persona { topic: String },
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum ProposalStatus {
        Proposed,
        EvalPassed,
        EvalFailed,
        ReplayPassed,
        ReplayFailed,
        Approved,
        Denied,
        Applied,
        /// fix loop gave up. someone should look at it, it never gets applied.
        NeedsAttention,
    }

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct Proposal {
        /// `nly_<kind>_<fnv1a(title+body)[..8]>`. same signal in a later pass
        /// gives the same id, which is the whole dedupe story.
        pub id: String,
        pub kind: ProposalKind,
        pub title: String,
        pub body: String,
        /// Runs this was learned from.
        pub provenance_runs: Vec<String>,
        /// (run, turn) pairs this was learned from.
        pub provenance_turns: Vec<TurnRef>,
        /// eval targets that must pass first. empty for memory lessons, they
        /// have no evals.
        pub eval_tags: Vec<String>,
        pub status: ProposalStatus,
    }

    impl Proposal {
        pub fn kind_name(&self) -> &'static str {
            match &self.kind {
                ProposalKind::MemoryLesson { .. } => "lesson",
                ProposalKind::Skill { .. } => "skill",
                ProposalKind::Persona { .. } => "persona",
            }
        }
    }

    /// the one LLM seam for the nightly pass. calls go on the AuxiliaryModel
    /// you're handed, never on the chat model: refiner uses Reflection,
    /// distiller uses Consolidation, repair uses Repair.
    pub trait NightlyLlm: Send + Sync {
        /// polish a draft body. don't change its meaning, kind, or provenance.
        /// Reflection slot.
        fn refine_proposal(
            &self,
            model: &AuxiliaryModel,
            draft: &Proposal,
        ) -> Result<String, String>;

        /// merge staged texts into durable facts. merge and phrase, don't
        /// invent claims. Consolidation slot.
        fn distill_memories(
            &self,
            model: &AuxiliaryModel,
            texts: &[String],
        ) -> Result<Vec<String>, String>;

        /// explain a broken MCP server / scheduled job / tool. advisory only,
        /// the repair actions themselves stay deterministic and bounded.
        /// Repair slot, not Reflection, and not the chat model.
        ///
        /// default is unimplemented and callers fall back to
        /// deterministic-only repair. a pass never fails over a missing model.
        fn diagnose_repair(&self, model: &AuxiliaryModel, prompt: &str) -> Result<String, String> {
            let _ = (model, prompt);
            Err("repair diagnosis not implemented by this backend".to_string())
        }
    }

    /// Some only if the policy has a Reflection entry. caller also checks
    /// config.enabled.
    pub fn resolve_refiner<'a>(
        llm: &'a dyn NightlyLlm,
        policy: &'a ModelPolicy,
    ) -> Option<(&'a dyn NightlyLlm, &'a AuxiliaryModel)> {
        policy
            .auxiliary(&AuxiliaryKind::Reflection)
            .map(|aux| (llm as &'a dyn NightlyLlm, aux))
    }

    /// Some only if the policy has a Repair entry. same gating as above.
    /// None means no revision: the loop retries, then escalates.
    pub fn resolve_repair<'a>(
        llm: &'a dyn NightlyLlm,
        policy: &'a ModelPolicy,
    ) -> Option<(&'a dyn NightlyLlm, &'a AuxiliaryModel)> {
        policy
            .auxiliary(&AuxiliaryKind::Repair)
            .map(|aux| (llm as &'a dyn NightlyLlm, aux))
    }

    // Small deterministic invariant tests only.
}

pub mod workspace {
    //! the session's workspace root. every file path is confined to it and
    //! relative paths resolve against it.
    //!
    //! this used to be the process cwd, captured once at tool-registry build
    //! time. which meant moving a session to another checkout needed a whole
    //! new process, because every tool was still holding the old path.
    //!
    //! so it's swappable now. one handle, shared, read at call time - a move
    //! takes effect on the next call without re-registering tools.

    use std::path::{Path, PathBuf};
    use std::sync::{Arc, RwLock};

    /// cloning is cheap and every clone sees the same root, so a move lands
    /// everywhere at once.
    #[derive(Clone, Debug)]
    pub struct WorkspaceRoot {
        path: Arc<RwLock<PathBuf>>,
    }

    impl WorkspaceRoot {
        pub fn new(path: impl Into<PathBuf>) -> Self {
            Self {
                path: Arc::new(RwLock::new(path.into())),
            }
        }

        /// process cwd. falls back to `.` if that can't be read - still a
        /// confined root, just not canonical.
        pub fn from_cwd() -> Self {
            Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        }

        /// current root. a poisoned lock gets recovered, not propagated - the
        /// path inside is still valid and refusing it kills the session over a
        /// panic in some unrelated thread.
        pub fn path(&self) -> PathBuf {
            self.path
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        /// swap the root, returns the old one. this doesn't check that the new
        /// path exists, the caller does.
        pub fn set(&self, path: impl Into<PathBuf>) -> PathBuf {
            let mut guard = self
                .path
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::replace(&mut *guard, path.into())
        }

        /// lexical, no canonicalization. use it as a fast pre-check only; the
        /// real check is `pantheon_exec::confine::confine`.
        pub fn contains(&self, path: &Path) -> bool {
            let root = self.path();
            path == root || path.starts_with(&root)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::WorkspaceRoot;
        use std::path::{Path, PathBuf};

        #[test]
        fn clones_share_one_root() {
            let root = WorkspaceRoot::new("/tmp/a");
            let clone = root.clone();
            assert_eq!(root.path(), PathBuf::from("/tmp/a"));
            assert_eq!(clone.path(), PathBuf::from("/tmp/a"));
        }

        /// A move is visible to every clone at once: that is the whole
        /// point of the handle over a captured `PathBuf`.
        #[test]
        fn move_is_visible_to_clones() {
            let root = WorkspaceRoot::new("/tmp/a");
            let clone = root.clone();
            let previous = root.set("/tmp/b");
            assert_eq!(previous, PathBuf::from("/tmp/a"));
            assert_eq!(clone.path(), PathBuf::from("/tmp/b"));
            assert_eq!(root.path(), PathBuf::from("/tmp/b"));
        }

        #[test]
        fn contains_covers_the_root_and_children() {
            let root = WorkspaceRoot::new("/tmp/a");
            assert!(root.contains(Path::new("/tmp/a")));
            assert!(root.contains(Path::new("/tmp/a/b/c.rs")));
            assert!(!root.contains(Path::new("/tmp/ab")));
            assert!(!root.contains(Path::new("/tmp")));
        }

        /// A panic while the root was being written must not brick the
        /// session: the poisoned lock still holds a usable path.
        #[test]
        fn poisoned_lock_still_serves_the_root() {
            let root = WorkspaceRoot::new("/tmp/a");
            let clone = root.clone();
            let _ = std::thread::spawn(move || {
                let _ = clone.set("/tmp/b");
                panic!("poison the lock");
            })
            .join();
            // The write happened before the panic, so the root moved and
            // the lock is poisoned.
            assert_eq!(root.path(), PathBuf::from("/tmp/b"));
        }
    }
}
