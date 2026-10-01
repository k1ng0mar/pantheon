//! Agent profiles (§4): a durable, first-class identity for one agent.
//!
//! # Why this lives in core
//!
//! `[agents.<name>]` was originally declared in the CLI crate
//! (`pantheon-tui::config`). That made the profile *config* readable by
//! the CLI but unreadable by the runtime, which is the only thing that can
//! actually give a session an identity. The type moved here so
//! `pantheon-runtime`, `pantheon-storage`, and `pantheon-agent` can all
//! depend on it without a dependency inversion. `config_doc` re-exports this
//! type so the on-disk spelling (`[agents.<name>]`) and the public Rust name
//! stay a single definition, not two that drift.
//!
//! # What a profile is
//!
//! A named agent with its own instructions (`AGENTS.md`), persona
//! (`SOUL.md`), memory namespace, capability policy, model config, and
//! sessions. The profile is not a CLI flag: it is resolved into an
//! [`AgentProfile`] value that the runtime hands to a `Session`, and every
//! effective value on it records *where it came from*.
//!
//! # Inheritance semantics (explicit, never "merge everything")
//!
//! A profile may declare `inherits = "<parent>"`. The chain is resolved
//! parent-first, and each field has exactly one documented merge rule:
//!
//! | field | rule | rationale |
//! |---|---|---|
//! | `display_name`, `soul_file`, `user_file`, `policy`, `model`, `soul`, `swarm_max_subagents` | **override** — child wins if set, else inherit | a persona is an identity, not a stack of layers |
//! | `agents_file` (AGENTS.md) | **concatenate**, parent first, child last | instructions genuinely layer; a child adds rules, never silently erases the parent's |
//! | `memory_namespace` | **NEVER inherited** — always `agent:<name>` unless set explicitly | inheritance must not merge memory. Two profiles sharing a namespace is a data leak, not a convenience |
//! | sessions, ledger rows, artifacts | **isolated by construction** — keyed by the profile's own id | resuming one agent's run must never replay another's transcript |
//!
//! Because the rules are field-specific rather than uniform, resolution
//! returns provenance: [`Resolved<T>`] records which profile in the chain
//! supplied each value, so "why is this agent using the reader policy?" has
//! an answer that is read from state rather than guessed.
//!
//! # Cycles and missing parents
//!
//! Resolution walks the chain with a visited set. A cycle, a missing parent,
//! an over-deep chain, or a non-slug name is a *load-time* error
//! ([`ProfileError`]), never a silently flattened result. A profile that
//! cannot be resolved is refused; it is never degraded into the default.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Maximum `inherits` chain depth. Bounds resolution cost and turns a
/// pathological config into a clear error rather than a deep walk.
pub const MAX_INHERIT_DEPTH: usize = 8;

/// The profile that exists implicitly: the baseline every other profile may
/// inherit from, and the identity used when no profile is selected.
pub const DEFAULT_PROFILE: &str = "default";

/// Where a resolved value came from.
///
/// Recorded per field so an operator can answer "where did this setting
/// come from?" without reading the whole chain by hand. `Inherited` names
/// the ancestor that supplied the value; `Own` is the profile itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Origin {
    /// Set explicitly on the profile that resolved it.
    Own { profile: String },
    /// Taken from an ancestor because the profile did not set it.
    Inherited { profile: String, ancestor: String },
    /// Supplied by the runtime because no profile set it (e.g. the default
    /// namespace derivation, or the implicit `default` profile).
    Runtime { reason: String },
}

/// A value together with its provenance.
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
    /// The profile that actually *supplied* this value.
    ///
    /// For `Own` that is the profile itself. For `Inherited` it is the
    /// `ancestor` that declared the value — not the `profile` field, which
    /// names who asked. Returning the requester would make every inherited
    /// setting look self-authored, which is precisely the question this
    /// provenance exists to answer.
    pub fn supplied_by(&self) -> Option<&str> {
        match &self.origin {
            Origin::Own { profile } => Some(profile),
            Origin::Inherited { ancestor, .. } => Some(ancestor),
            Origin::Runtime { .. } => None,
        }
    }
    /// The profile that requested resolution, whether or not it supplied
    /// the value.
    pub fn requested_by(&self) -> Option<&str> {
        match &self.origin {
            Origin::Own { profile } | Origin::Inherited { profile, .. } => Some(profile),
            Origin::Runtime { .. } => None,
        }
    }
}

/// A profile exactly as written in `[agents.<name>]`.
///
/// Every optional field means "not set here" and is therefore a candidate
/// for inheritance. This is the *declared* shape, not the effective one;
/// [`ProfileRegistry::resolve`] produces the effective value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    /// Human display name. Defaults to the table name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Persona file (SOUL.md equivalent), by path. Optional: an agent with
    /// no persona file is a blank slate, not an error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soul_file: Option<String>,
    /// Instructions file (AGENTS.md equivalent). Unlike `soul_file` this
    /// one *layers* across an inheritance chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_file: Option<String>,
    /// User-context file (USER.md equivalent), by path. Optional: like
    /// `soul_file` this one *overrides* across an inheritance chain — the
    /// child's file wins if set, else the parent's is inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_file: Option<String>,
    /// Parent profile name (`inherits = "default"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherits: Option<String>,
    /// Long-term memory namespace. Never inherited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_namespace: Option<String>,
    /// Capability policy preset name (`reader` | `coder` | `coder_memory`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    /// Provider/model override for this profile. A profile with no model
    /// pin uses the runtime default; it never picks one itself (locked
    /// decision: agents never choose models).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Provider override paired with `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Profile picture, for the dashboard and mobile app. Purely cosmetic:
    /// the runtime never reads it. Convention is `preset:avatar-N`
    /// (N = 1..=6) for the bundled preset set, or an absolute file path
    /// for a user-uploaded picture. The dashboard serves no file; clients
    /// map the preset to their bundled asset and read the path themselves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avatar: Option<String>,
    /// Per-profile override for the `[swarm]` `max_subagents` cap: how
    /// many sub-agents this profile may spawn. `None` = fall back to the
    /// global `[swarm]` value. Override semantics like `soul_file`: the
    /// child's value wins if set, else the parent's is inherited.
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

/// A fully resolved profile: the effective values plus where each came from.
///
/// Produced by [`ProfileRegistry::resolve`]. The runtime consumes only this
/// type, so an unresolved profile can never reach a `Session`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectiveProfile {
    /// Table name, and the stable identity key. Lowercase slug.
    pub name: String,
    /// Stable internal id used in run ids, leases, and task provenance.
    /// Distinct from `name` so a rename does not silently reassign history.
    pub agent_id: String,
    /// Display name, resolved through the chain.
    pub display_name: Resolved<String>,
    /// Parent profile, if the chain had one.
    pub parent: Option<String>,
    /// Effective memory namespace, with its derivation recorded. Never
    /// inherited: if this value came from a parent, that is a bug and
    /// [`ProfileRegistry::resolve`] refuses it.
    pub memory_namespace: Resolved<String>,
    /// Effective capability policy preset.
    pub policy: Resolved<String>,
    /// Effective persona file path, if any.
    pub soul_file: Resolved<Option<String>>,
    /// Effective user-context file path, if any. Override semantics, like
    /// `soul_file`: the child's file wins if set, else the parent's.
    pub user_file: Resolved<Option<String>>,
    /// Layered instruction files, parent first then child, each tagged with
    /// the profile that contributed it.
    pub agents_files: Vec<(String, String)>,
    /// Effective model pin, if the profile (or an ancestor) set one.
    pub model: Resolved<Option<String>>,
    /// Effective provider pin, if set.
    pub provider: Resolved<Option<String>>,
    /// Effective per-profile sub-agent spawn cap. Override semantics
    /// like `soul_file`: the child's value wins if set, else the
    /// parent's. `None` (with a `Runtime` origin) = no profile in the
    /// chain set one; the caller falls back to the global `[swarm]`
    /// `max_subagents`.
    pub swarm_max_subagents: Resolved<Option<u32>>,
}

impl EffectiveProfile {
    /// One-line identity for logs, TUI headers, and the ledger.
    pub fn identity(&self) -> String {
        match &self.parent {
            Some(p) => format!("{} (inherits {})", self.display_name.value, p),
            None => self.display_name.value.clone(),
        }
    }
    /// The instruction files as a single prompt-ready block, annotated with
    /// the contributing profile. Annotating matters: a file the model reads
    /// should say which agent's rules it is, so a child profile that layers
    /// on a parent is legible in the transcript.
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

/// Load-time validation failure. Always fatal: a profile that cannot be
/// resolved is refused rather than silently replaced by the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileError {
    /// The name is not a slug, or is blank.
    InvalidName { name: String, why: String },
    /// The requested profile is not declared at all. Distinct from
    /// `MissingParent`: this names a profile the caller asked for that does
    /// not exist, while `MissingParent` names a valid profile whose
    /// `inherits` points somewhere undeclared. Conflating them produced the
    /// bug where a rootless profile reported itself as a missing parent.
    UnknownProfile { name: String },
    /// `inherits` names a profile that is not declared.
    MissingParent { profile: String, parent: String },
    /// The `inherits` chain loops.
    Cycle { chain: Vec<String> },
    /// The chain is longer than [`MAX_INHERIT_DEPTH`].
    TooDeep { chain: Vec<String>, max: usize },
    /// Two profiles resolve to the same memory namespace.
    NamespaceClash {
        a: String,
        b: String,
        namespace: String,
    },
    /// A profile explicitly inherited a memory namespace from its parent.
    ///
    /// Kept as its own variant rather than a generic invalid config: this is
    /// the mistake that would leak one agent's memory into another, so it
    /// must be visible by name in an operator-facing message.
    InheritedNamespace { profile: String, namespace: String },
    /// An unknown policy preset name.
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

/// The set of declared profiles, plus the rules for resolving one.
#[derive(Debug, Clone, Default)]
pub struct ProfileRegistry {
    /// Table name -> declared profile.
    profiles: HashMap<String, AgentProfile>,
}

/// Policy preset names the loader accepts. Mirrors
/// `PolicyPreset::as_str` in the CLI; kept as data here because core cannot
/// depend on the CLI, and a duplicate *list of three strings* is a
/// documented, test-pinned mirror rather than a duplicated abstraction.
pub const KNOWN_POLICIES: [&str; 3] = ["reader", "coder", "coder_memory"];

impl ProfileRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare a profile. Rejects an invalid name and a cycle at insert
    /// time, so an invalid config can never be written and then used.
    pub fn insert(&mut self, name: &str, profile: AgentProfile) -> Result<(), ProfileError> {
        if !AgentProfile::is_slug(name) {
            return Err(ProfileError::InvalidName {
                name: name.to_string(),
                why: "must be a slug (letters, digits, - _)".to_string(),
            });
        }
        self.profiles.insert(name.to_string(), profile);
        // A cycle is detectable locally, so catch it at the moment the
        // offending edge is added rather than at every later resolve.
        // Only check for a cycle when this profile actually adds an
        // inheritance edge; re-walking on every unrelated insert would make
        // declaration order-dependent for no reason.
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
        // Missing parents are *not* rejected here: a config may declare
        // profiles in any table order, and `default` may be supplied by the
        // caller rather than declared. Resolution enforces it.
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

    /// Walk `inherits` upward, returning the chain root-first
    /// (`[ancestor, ..., profile]`).
    ///
    /// Termination is three distinct outcomes, and conflating any two of them
    /// is the bug this shape avoids: a declared rootless profile is a
    /// *successful* one-element walk, an undeclared name is
    /// [`ProfileError::UnknownProfile`], and a declared profile naming a
    /// parent that is not declared is [`ProfileError::MissingParent`]. Only
    /// the last two are errors.
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

    /// Resolve a profile into its effective form.
    ///
    /// `default_fallback` supplies the policy used when neither the profile
    /// nor any ancestor names one; the runtime's own default is the honest
    /// source for it. Passing it in keeps core free of a Policy dependency.
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
        // Reverse to root-first so a "first writer wins" scan produces
        // child-overrides-parent naturally.
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
            // OVERRIDE fields: the chain is walked root-first, so the LAST
            // profile that declares a value wins. That is what makes a
            // child override its parent — the child is the last writer. A
            // "first writer wins" scan here would hand precedence to the
            // ancestor, which is the opposite of what `inherits` means.
            if let Some(d) = &decl.display_name {
                display_name = Some(mark_str(d));
            }
            // An explicit `soul_file` overrides; absent means inherit.
            // There is no "explicitly clear it" spelling: `Option<String>`
            // cannot distinguish absent from null, so treating the profile's
            // own absent value as a clear would make a child that simply
            // never mentions a persona silently erase its parent's.
            if decl.soul_file.is_some() {
                soul_file = Some(mark_opt(&decl.soul_file));
            }
            // `user_file` overrides exactly like `soul_file`: the user's
            // context is part of the identity, not a layer.
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
            // `swarm_max_subagents` overrides exactly like `soul_file`:
            // an explicit value on the child wins; absent means inherit.
            if decl.swarm_max_subagents.is_some() {
                let v = &decl.swarm_max_subagents;
                swarm_max_subagents = Some(if is_own {
                    Resolved::own(profile_name, *v)
                } else {
                    Resolved::inherited(name, profile_name, *v)
                });
            }
            // Instructions LAYER: every profile in the chain contributes,
            // parent first. This is the one field where a child adds rather
            // than overrides, because rules compose and erasure is silent.
            if let Some(f) = &decl.agents_file {
                agents_files.push((profile_name.clone(), f.clone()));
            }
            // Memory is read from the profile itself ONLY.
            if is_own {
                own_namespace = decl.memory_namespace.clone();
            }
        }

        // A profile that names someone else's namespace is a config error,
        // caught here rather than at use time.
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

    /// Validate the whole table: every profile resolves, no cycle, no
    /// missing parent, and no two profiles share a memory namespace.
    ///
    /// Namespace clash is checked here and not in `resolve` because it is a
    /// property of the *set*, not of one profile.
    pub fn validate_all(&self, default_policy: &str) -> Result<(), ProfileError> {
        let problems = self.problems(default_policy);
        match problems.into_iter().next() {
            Some(first) => Err(first),
            None => Ok(()),
        }
    }

    /// Every problem in the registry, not just the first.
    ///
    /// `doctor` needs all of them: reporting one broken profile at a time
    /// turns a three-line fix into three round trips. The ordering is
    /// stable (policy names, then per-profile resolution, then namespace
    /// clashes) so the same broken config always reads the same way.
    pub fn problems(&self, default_policy: &str) -> Vec<ProfileError> {
        let mut out = Vec::new();
        let mut by_ns: HashMap<String, String> = HashMap::new();
        // Policy spelling first: an unknown preset usually explains several
        // downstream resolution failures, and it is the cheapest to fix.
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

/// Stable internal id for a profile.
///
/// Prefixed and name-derived so a ledger row is greppable, but a *separate*
/// field from the display name: renaming a profile must not silently
/// reassign a history that belongs to the old identity, so the id recorded
/// on a run is the id, not the label.
pub fn agent_id_for(name: &str) -> String {
    format!("agent:{name}")
}
