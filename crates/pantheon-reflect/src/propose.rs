//! Signal → proposal. Deterministic templates: the same signals always
//! yield the same proposals, so reflection is reproducible and auditable.
//! Every proposal carries its provenance (the runs/turns it learned from)
//! and, for gated kinds, the eval targets that must pass before approval.

use crate::signals::{Signal, TurnRef};

/// What the reflector wants to change.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProposalKind {
    /// A durable lesson for the memory store. Auto-applies at the `Memory`
    /// trust tier: informative, never authoritative, and it can never
    /// clobber a user-confirmed record.
    MemoryLesson { key: String },
    /// Create or update a SKILL.md under `<data_dir>/skills`. Eval-gated,
    /// then approval-gated.
    Skill { name: String, update: bool },
    /// A tweak to the agent's persona notes (stored as agent-layer memory
    /// under the `persona` namespace, so it shapes future sessions without
    /// editing user config files). Eval-gated, then approval-gated.
    Persona { topic: String },
}

/// Lifecycle state of a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ProposalStatus {
    Proposed,
    EvalPassed,
    EvalFailed,
    Approved,
    Denied,
    Applied,
}

/// One proposed self-improvement, with full provenance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Proposal {
    /// Deterministic id: `rfl_<kind>_<fnv1a(title+body)[..8]>`. The same
    /// signal re-observed in a later pass yields the same id, which makes
    /// "already proposed / already applied" dedupe trivial.
    pub id: String,
    pub kind: ProposalKind,
    pub title: String,
    pub body: String,
    /// Runs this was learned from.
    pub provenance_runs: Vec<String>,
    /// (run, turn) pairs this was learned from.
    pub provenance_turns: Vec<TurnRef>,
    /// pantheon-eval test targets that must pass before this proposal can
    /// be approved. Empty for memory lessons (no evals; auto-applied).
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

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn proposal_id(kind: &str, title: &str, body: &str) -> String {
    format!(
        "rfl_{kind}_{:08x}",
        fnv1a(&format!("{title}\n{body}")) & 0xffff_ffff
    )
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if c == ' ' || c == '_' || c == '-' {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_string();
    if out.is_empty() {
        "skill".to_string()
    } else {
        out
    }
}

fn provenance_of(turns: &[TurnRef]) -> (Vec<String>, Vec<TurnRef>) {
    let mut runs: Vec<String> = turns.iter().map(|t| t.run_id.clone()).collect();
    runs.sort();
    runs.dedup();
    (runs, turns.to_vec())
}

/// Turn signals into proposals, strongest first. Pure and deterministic.
pub fn from_signals(signals: &[Signal]) -> Vec<Proposal> {
    let mut out = Vec::new();
    for signal in signals {
        match signal {
            Signal::RepeatedSequence { tools, hits } => {
                let (runs, turns) = provenance_of(hits);
                let name = slug(&format!("auto-{}", tools.join("-")));
                let title = format!("skill: {} ({}×)", tools.join(" → "), hits.len());
                let mut body = format!(
                    "# {}\n\nLearned from {} successful runs. Use this sequence when the task matches the pattern below.\n\n## Steps\n",
                    title,
                    hits.len()
                );
                for (i, t) in tools.iter().enumerate() {
                    body.push_str(&format!("{}. Call `{t}`\n", i + 1));
                }
                body.push_str(
                    "\n## Notes\n- This skill was proposed automatically by Reflection from repeated successful tool sequences.\n- Refine the trigger conditions after first use.\n",
                );
                out.push(Proposal {
                    id: proposal_id("skill", &title, &body),
                    kind: ProposalKind::Skill {
                        name,
                        update: false,
                    },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    // Skills touch tool execution: gate on the skill and
                    // tool eval targets.
                    eval_tags: vec!["tools_skills".into(), "exec_skills".into()],
                    status: ProposalStatus::Proposed,
                });
            }
            Signal::UserCorrection { text, at } => {
                let (runs, turns) = provenance_of(std::slice::from_ref(at));
                let short: String = text.chars().take(80).collect();
                let title = format!("lesson: user correction — {short}");
                let body = format!(
                    "The user steered a running turn with this guidance: \"{text}\". \
                     Treat it as a standing preference: when the same situation recurs, \
                     follow the corrected direction without needing to be steered."
                );
                let key = format!("reflect-correction-{:08x}", fnv1a(text) & 0xffff_ffff);
                out.push(Proposal {
                    id: proposal_id("lesson", &title, &body),
                    kind: ProposalKind::MemoryLesson { key },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                });
            }
            Signal::ApprovalDenied { scope, at } => {
                let (runs, turns) = provenance_of(std::slice::from_ref(at));
                let title = format!("lesson: approval denied for `{scope}`");
                let body = format!(
                    "The user denied the approval scope `{scope}`. Do not repeat the same request shape: \
                     prefer a narrower scope, explain why the operation is needed first, or find an approach \
                     that stays inside already-granted permissions."
                );
                let key = format!("reflect-denial-{:08x}", fnv1a(scope) & 0xffff_ffff);
                out.push(Proposal {
                    id: proposal_id("lesson", &title, &body),
                    kind: ProposalKind::MemoryLesson { key },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                });
            }
            Signal::RepeatedFailure { tool, count, at } => {
                let (runs, turns) = provenance_of(at);
                let title = format!("lesson: `{tool}` failed {count}× — check preconditions");
                let body = format!(
                    "The tool `{tool}` failed {count} times across recent turns. Before calling it, \
                     verify its preconditions (inputs exist, are well-formed, and the environment is ready) \
                     instead of retrying blindly."
                );
                let key = format!("reflect-prefail-{:08x}", fnv1a(tool) & 0xffff_ffff);
                out.push(Proposal {
                    id: proposal_id("lesson", &title, &body),
                    kind: ProposalKind::MemoryLesson { key },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                });
            }
            Signal::RepeatedPreference {
                topic,
                keyword,
                hits,
            } => {
                let (runs, turns) = provenance_of(hits);
                let title = format!("persona: prefers {topic} ({}×)", hits.len());
                let body = format!(
                    "Standing user preference: the user steered {} turns across {} runs toward \
                     \"{keyword}\". Default to {topic} unless the task clearly needs otherwise. \
                     Proposed automatically by Reflection; it takes effect only after human approval.",
                    hits.len(),
                    runs.len()
                );
                out.push(Proposal {
                    id: proposal_id("persona", &title, &body),
                    kind: ProposalKind::Persona {
                        topic: topic.clone(),
                    },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: turns,
                    // No persona-specific eval targets exist yet; the
                    // approval hold is the gate. Tag them here when they
                    // land in /eval.
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                });
            }
        }
    }
    out
}

// Small deterministic invariant tests only.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::Signal;

    #[test]
    fn correction_becomes_lesson_not_skill() {
        let at = TurnRef {
            run_id: "r1".into(),
            turn_id: "t1".into(),
        };
        let ps = from_signals(&[Signal::UserCorrection {
            text: "use tabs".into(),
            at,
        }]);
        assert_eq!(ps.len(), 1);
        assert!(matches!(ps[0].kind, ProposalKind::MemoryLesson { .. }));
        assert!(ps[0].eval_tags.is_empty());
    }

    #[test]
    fn repeated_preference_becomes_persona() {
        let hits = vec![
            TurnRef {
                run_id: "r1".into(),
                turn_id: "t1".into(),
            },
            TurnRef {
                run_id: "r2".into(),
                turn_id: "t2".into(),
            },
            TurnRef {
                run_id: "r1".into(),
                turn_id: "t3".into(),
            },
        ];
        let ps = from_signals(&[Signal::RepeatedPreference {
            topic: "brevity".into(),
            keyword: "concise".into(),
            hits,
        }]);
        assert_eq!(ps.len(), 1);
        assert!(matches!(
            ps[0].kind,
            ProposalKind::Persona { ref topic } if topic == "brevity"
        ));
        assert_eq!(
            ps[0].provenance_runs,
            vec!["r1".to_string(), "r2".to_string()]
        );
        assert_eq!(ps[0].provenance_turns.len(), 3);
    }

    #[test]
    fn proposal_ids_are_deterministic() {
        let mk = || {
            from_signals(&[Signal::ApprovalDenied {
                scope: "exec:rm".into(),
                at: TurnRef {
                    run_id: "r".into(),
                    turn_id: "t".into(),
                },
            }])[0]
                .id
                .clone()
        };
        assert_eq!(mk(), mk());
    }

    #[test]
    fn skill_proposals_carry_eval_tags() {
        let hits: Vec<TurnRef> = (0..3)
            .map(|i| TurnRef {
                run_id: format!("r{i}"),
                turn_id: format!("t{i}"),
            })
            .collect();
        let ps = from_signals(&[Signal::RepeatedSequence {
            tools: vec!["a".into(), "b".into()],
            hits,
        }]);
        assert_eq!(ps.len(), 1);
        assert!(!ps[0].eval_tags.is_empty());
        assert_eq!(ps[0].provenance_runs.len(), 3);
    }
}
