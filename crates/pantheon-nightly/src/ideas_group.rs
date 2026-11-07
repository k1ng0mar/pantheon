//! ideas_group: audit, ideas, llm, persona, propose, report, signals, state in one file.

pub mod audit {
    //! Audit: every nightly lifecycle event, one JSONL line each.
    //!
    //! The audit log is the single source of truth for what the nightly pass
    //! did and why. Events are append-only JSONL at
    //! `<data_dir>/nightly/nightly.jsonl`.

    use std::path::{Path, PathBuf};

    pub fn audit_path(data_dir: &Path) -> PathBuf {
        data_dir.join("nightly").join("nightly.jsonl")
    }

    /// Lifecycle events worth auditing. `Debug` formatting is the human
    /// line; the JSON payload is the machine one.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "event", rename_all = "snake_case")]
    pub enum NightlyEvent {
        PassStarted {
            runs_scanned: usize,
            at_ms: i64,
        },
        SignalObserved {
            kind: String,
            hits: usize,
            at_ms: i64,
        },
        ProposalMade {
            id: String,
            kind: String,
            title: String,
            at_ms: i64,
        },
        EvalPassed {
            id: String,
            summary: String,
            at_ms: i64,
        },
        EvalRejected {
            id: String,
            reason: String,
            at_ms: i64,
        },
        /// Queueing for approval failed (IO, corrupt queue file). Deliberately
        /// NOT EvalRejected: a queue failure is an infrastructure failure,
        /// not a judgement that the proposal was bad, and counting it as
        /// "rejected" inflated the rejection count.
        QueueFailed {
            id: String,
            reason: String,
            at_ms: i64,
        },
        ReplayPassed {
            id: String,
            summary: String,
            at_ms: i64,
        },
        ReplayFailed {
            id: String,
            reason: String,
            at_ms: i64,
        },
        LessonApplied {
            id: String,
            key: String,
            at_ms: i64,
        },
        QueuedForApproval {
            id: String,
            kind: String,
            at_ms: i64,
        },
        ApprovalDecided {
            id: String,
            approved: bool,
            at_ms: i64,
        },
        Applied {
            id: String,
            kind: String,
            at_ms: i64,
        },
        /// An approved proposal could not be applied (disk error, bad skill
        /// name). The proposal stays in the pending queue so the human can
        /// retry or deny it; nothing is silently dropped.
        ApplyFailed {
            id: String,
            reason: String,
            at_ms: i64,
        },
        /// One iteration of the bounded fix loop (see `crate::fixloop`):
        /// a deterministic repair was attempted and validation re-ran.
        FixAttempt {
            id: String,
            attempt: usize,
            phase: String,
            detail: String,
            at_ms: i64,
        },
        /// The fix loop gave up: the proposal failed validation after the
        /// maximum attempts (or hit an unrepairable infrastructure failure).
        /// Marked `NeedsAttention`; surfaced in `nightly-escalated.json`
        /// and the pass report. Never queued for approval.
        Escalated {
            id: String,
            reason: String,
            attempts: usize,
            at_ms: i64,
        },
        PassFinished {
            proposals: usize,
            applied: usize,
            pending: usize,
            at_ms: i64,
        },
        /// Token/cost accounting for one successful auxiliary model call made
        /// during a nightly pass. Kept in this audit because nightly work has no
        /// interactive session id to attribute in `pantheon stats`.
        ModelUsage {
            provider: String,
            model: String,
            input_tokens: u64,
            output_tokens: u64,
            total_tokens: u64,
            cost_usd: Option<f64>,
            at_ms: i64,
        },
    }

    /// Append one event to the audit log.
    pub fn audit(data_dir: &Path, event: &NightlyEvent) -> Result<(), String> {
        let path = audit_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let mut line = serde_json::to_string(event).map_err(|e| format!("encode event: {e}"))?;
        line.push('\n');
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        f.write_all(line.as_bytes())
            .map_err(|e| format!("append {}: {e}", path.display()))
    }

    /// Read every audit event (most recent last); unparseable lines are
    /// skipped so one corrupt line never breaks status views.
    pub fn read_events(data_dir: &Path) -> Vec<NightlyEvent> {
        let text = std::fs::read_to_string(audit_path(data_dir)).unwrap_or_default();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// One-line summary of the most recent pass, for status surfaces.
    /// Counts lifecycle events since the last `PassStarted`.
    pub fn last_run_summary(data_dir: &Path) -> Option<String> {
        let events = read_events(data_dir);
        let start_idx = events
            .iter()
            .rposition(|e| matches!(e, NightlyEvent::PassStarted { .. }))?;
        let started_ms = match events.get(start_idx)? {
            NightlyEvent::PassStarted { at_ms, .. } => *at_ms,
            _ => return None,
        };
        let finished_ms = events[start_idx..]
            .iter()
            .find_map(|e| match e {
                NightlyEvent::PassFinished { at_ms, .. } => Some(*at_ms),
                _ => None,
            })
            // No PassFinished yet means the pass is still running or died
            // mid-pass: bound the window at now, not i64::MAX, or usage
            // from every later pass would be folded into this one.
            .unwrap_or_else(|| pantheon_api::logging::now_ms());
        let mut proposed = 0;
        let mut applied = 0;
        let mut queued = 0;
        let mut rejected = 0;
        let mut escalated = 0;
        for e in &events[start_idx..] {
            match e {
                NightlyEvent::ProposalMade { .. } => proposed += 1,
                NightlyEvent::LessonApplied { .. } | NightlyEvent::Applied { .. } => applied += 1,
                NightlyEvent::QueuedForApproval { .. } => queued += 1,
                NightlyEvent::EvalRejected { .. } | NightlyEvent::ReplayFailed { .. } => {
                    rejected += 1
                }
                NightlyEvent::Escalated { .. } => escalated += 1,
                _ => {}
            }
        }
        // Usage callbacks can append before the buffered lifecycle events are
        // flushed at pass end, so select by their durable timestamp, not JSONL
        // position. Rewinds do not apply to these maintenance model calls.
        let usage = events
            .iter()
            .fold((0u64, 0u64, 0u64, 0.0f64), |mut totals, e| {
                if let NightlyEvent::ModelUsage {
                    total_tokens,
                    cost_usd,
                    at_ms,
                    ..
                } = e
                {
                    if *at_ms >= started_ms && *at_ms <= finished_ms {
                        totals.0 += 1;
                        totals.1 += *total_tokens;
                        if let Some(cost) = cost_usd {
                            totals.2 += 1;
                            totals.3 += *cost;
                        }
                    }
                }
                totals
            });
        let usage_summary = if usage.0 == 0 {
            String::new()
        } else if usage.2 == 0 {
            format!(", {} model calls, {} tokens, cost n/a", usage.0, usage.1)
        } else {
            format!(
                ", {} model calls, {} tokens, ${:.4} estimated",
                usage.0, usage.1, usage.3
            )
        };
        Some(format!(
            "{proposed} proposed, {applied} applied, {queued} awaiting approval, {rejected} rejected, {escalated} escalated{usage_summary}"
        ))
    }

    // Small deterministic invariant tests only.
}

pub mod ideas {
    //! Nightly-gated proactive ideas (the Ideas page backend).
    //!
    //! Two flavors, both deterministic and rule-based:
    //!
    //! - **General ideas** - repair opportunities and observations from the
    //!   pass: repair targets the repair phase contained but could not fix,
    //!   and tools that failed repeatedly in the scan window.
    //! - **Suggested scheduled tasks** - mined from repeating session
    //!   patterns: a tool sequence run on several separate days becomes a
    //!   "you do X most mornings - want a daily task?" proposal, with the
    //!   cron set to the median hour (UTC) the sequence ran.
    //!
    //! Daily refresh: at most one batch per calendar day (a second pass the
    //! same day mints nothing), and unanswered `pending` ideas older than
    //! [`PENDING_KEEP_DAYS`] roll off. Feedback tuning: a topic the user
    //! dismissed twice with no accept is never proposed again.
    //!
    //! This phase runs only inside an enabled pass - `run_pass` is never
    //! invoked when the nightly master switch is off, so no second flag is
    //! invented here.

    use crate::repair_targets::{RepairOutcome, RepairReport};
    use crate::signals::Signal;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_storage::{IdeaKind, IdeaStore, NewIdea, ScheduleSpec, TopicSignals};

    /// Max ideas minted per calendar day.
    pub const MAX_IDEAS_PER_DAY: usize = 5;
    /// Unanswered pending ideas older than this many days roll off.
    pub const PENDING_KEEP_DAYS: i64 = 3;
    /// A topic dismissed this many times (with zero accepts) is downranked:
    /// generation skips it.
    const DISMISS_DOWNRANK: i64 = 2;
    /// Repeated-failure signals below this count are noise, not an idea.
    const MIN_FAILURE_COUNT: usize = 3;
    /// A sequence must span this many distinct days to count as a habit
    /// worth scheduling (one burst is not a routine).
    const MIN_HABIT_DAYS: usize = 2;

    fn err(cause: String) -> PantheonError {
        PantheonError::new(
            "NLY_IDEA",
            Layer::Runtime,
            false,
            cause,
            "check the data dir and ideas.db health (`pantheon doctor`)",
            String::new(),
        )
    }

    /// One generated idea, before the store assigns its id.
    #[derive(Debug, Clone)]
    pub struct GeneratedIdea {
        pub id: String,
        pub title: String,
        pub description: String,
        pub includes: Vec<String>,
        pub kind: IdeaKind,
        pub topic: String,
        pub schedule: Option<ScheduleSpec>,
    }

    // ---------------------------------------------------------------------------
    // Calendar helpers (pure, UTC, no chrono dependency)
    // ---------------------------------------------------------------------------

    /// `YYYY-MM-DD` (UTC) for an epoch-millis timestamp.
    pub fn today_utc(at_ms: i64) -> String {
        let (y, m, d) = civil_from_days(at_ms.div_euclid(86_400_000));
        format!("{y:04}-{m:02}-{d:02}")
    }

    /// Add (or subtract) days from a `YYYY-MM-DD` date. `None` on bad input.
    pub fn add_days(day: &str, delta: i64) -> Option<String> {
        let (y, m, d) = parse_day(day)?;
        let (y, m, d) = civil_from_days(days_from_civil(y, m, d)? + delta);
        Some(format!("{y:04}-{m:02}-{d:02}"))
    }

    fn parse_day(day: &str) -> Option<(i64, u32, u32)> {
        let mut parts = day.split('-');
        let y: i64 = parts.next()?.parse().ok()?;
        let m: u32 = parts.next()?.parse().ok()?;
        let d: u32 = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        Some((y, m, d))
    }

    // Howard Hinnant's civil <-> days algorithms.
    fn days_from_civil(y: i64, m: u32, d: u32) -> Option<i64> {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let mp = (m as i64 + 9) % 12;
        let doy = (153 * mp + 2) / 5 + d as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Some(era * 146097 + doe - 719468)
    }

    fn civil_from_days(z: i64) -> (i64, u32, u32) {
        let z = z + 719468;
        let era = z.div_euclid(146097);
        let doe = z.rem_euclid(146097);
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        (if m <= 2 { y + 1 } else { y }, m, d)
    }

    /// Median hour of day (UTC, 0-23) across timestamps. Lower median on
    /// even counts; deterministic.
    pub fn median_hour_utc(timestamps_ms: &[i64]) -> u32 {
        let mut hours: Vec<u32> = timestamps_ms
            .iter()
            .map(|ts| (ts.div_euclid(3_600_000) % 24) as u32)
            .collect();
        hours.sort_unstable();
        hours.get(hours.len() / 2).copied().unwrap_or(0)
    }

    /// Distinct UTC calendar days spanned by timestamps.
    pub fn distinct_days(timestamps_ms: &[i64]) -> usize {
        let mut days: Vec<i64> = timestamps_ms
            .iter()
            .map(|ts| ts.div_euclid(86_400_000))
            .collect();
        days.sort_unstable();
        days.dedup();
        days.len()
    }

    /// Feedback tuning: the user keeps dismissing this topic and never
    /// accepted it - stop proposing it.
    pub fn downranked(signals: &TopicSignals) -> bool {
        signals.dismissed >= DISMISS_DOWNRANK && signals.accepted == 0
    }

    // ---------------------------------------------------------------------------
    // Candidate builders (pure)
    // ---------------------------------------------------------------------------

    fn slug(s: &str) -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.len() <= max {
            return s.to_string();
        }
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &s[..end])
    }

    /// A repair target the pass contained but could not fix: suggest the
    /// user finish the job by hand.
    pub fn general_from_repair(report: &RepairReport) -> Option<GeneratedIdea> {
        match report.outcome {
            RepairOutcome::Contained => {}
            // Repaired/healthy need no idea; dry-run detections are audited
            // elsewhere and must not mint user-facing ideas.
            _ => return None,
        }
        let topic = format!("repair:{}", slug(&report.target));
        Some(GeneratedIdea {
            id: String::new(),
            title: format!("Repair couldn't finish: {}", report.target),
            description: format!(
                "The nightly repair pass tried to fix {} but could only contain it. {}",
                report.target,
                truncate(&report.detail, 300)
            ),
            includes: vec![
                "Read the nightly repair report for the failed step".into(),
                "Fix the root cause by hand (config, credentials, path)".into(),
                "Re-enable or unpause the target once it works".into(),
            ],
            kind: IdeaKind::General,
            topic,
            schedule: None,
        })
    }

    /// A tool that failed repeatedly in the scan window.
    pub fn general_from_failure(tool: &str, count: usize) -> Option<GeneratedIdea> {
        if count < MIN_FAILURE_COUNT {
            return None;
        }
        Some(GeneratedIdea {
            id: String::new(),
            title: format!("The \"{tool}\" tool keeps failing"),
            description: format!(
                "{count} failed \"{tool}\" invocations in the last scan window. \
                 Something about its setup may be broken."
            ),
            includes: vec![
                "Look at the recent error output for the tool".into(),
                "Fix the underlying cause (config, credentials, path)".into(),
                "Dismiss this idea if the failures are expected".into(),
            ],
            kind: IdeaKind::General,
            topic: format!("fail:{}", slug(tool)),
            schedule: None,
        })
    }

    /// A tool sequence run like a habit: propose it as a scheduled task at
    /// the median hour (UTC) it ran. `None` when it is a one-off burst.
    pub fn scheduled_from_sequence(
        tools: &[String],
        hits: &[crate::signals::TurnRef],
    ) -> Option<GeneratedIdea> {
        if hits.len() < MIN_FAILURE_COUNT {
            return None;
        }
        let ts: Vec<i64> = hits.iter().map(|h| h.ts_ms).collect();
        if distinct_days(&ts) < MIN_HABIT_DAYS {
            return None;
        }
        let hour = median_hour_utc(&ts);
        let days = distinct_days(&ts);
        let seq = tools.join(" → ");
        let prompt = format!(
            "Run the recurring \"{seq}\" workflow you usually do around {hour:02}:00 UTC and report the result."
        );
        Some(GeneratedIdea {
            id: String::new(),
            title: format!("Recurring task: {seq}"),
            description: format!(
                "You've run this sequence on {days} separate days, mostly around {hour:02}:00 UTC. \
                 A scheduled task could run it for you."
            ),
            includes: vec![
                "Review the proposed daily schedule".into(),
                "Accept to create the scheduled task".into(),
                "Dismiss if this was a one-off burst".into(),
            ],
            kind: IdeaKind::ScheduledTask,
            topic: format!(
                "seq:{}",
                tools.iter().map(|t| slug(t)).collect::<Vec<_>>().join("+")
            ),
            schedule: Some(ScheduleSpec {
                cron: format!("0 {hour} * * *"),
                deliver: "home".into(),
                prompt,
            }),
        })
    }

    // ---------------------------------------------------------------------------
    // The phase
    // ---------------------------------------------------------------------------

    /// Mint today's ideas: expire stale pending ideas, then generate one
    /// batch per calendar day. Returns the number minted. A no-op in dry-run
    /// mode (the pass mutates nothing then).
    pub fn run_ideas_phase(
        store: &IdeaStore,
        signals: &[Signal],
        repairs: &[RepairReport],
        dry_run: bool,
        today: &str,
        _now_ms: i64,
    ) -> Result<usize, PantheonError> {
        if dry_run {
            return Ok(0);
        }
        // Daily refresh, first half: unanswered pending ideas older than
        // PENDING_KEEP_DAYS roll off instead of piling up forever.
        let cutoff = add_days(today, -PENDING_KEEP_DAYS)
            .ok_or_else(|| err(format!("bad day string '{today}'")))?;
        store.expire_pending_before(&cutoff)?;
        // Daily refresh, second half: one batch per day; a second pass the
        // same day mints nothing.
        if store.has_day(today)? {
            return Ok(0);
        }

        let mut candidates: Vec<GeneratedIdea> = Vec::new();
        for report in repairs {
            if let Some(g) = general_from_repair(report) {
                candidates.push(g);
            }
        }
        for signal in signals {
            match signal {
                Signal::RepeatedFailure { tool, count, .. } => {
                    if let Some(g) = general_from_failure(tool, *count) {
                        candidates.push(g);
                    }
                }
                Signal::RepeatedSequence { tools, hits } => {
                    if let Some(g) = scheduled_from_sequence(tools, hits) {
                        candidates.push(g);
                    }
                }
                // User corrections, denied approvals, and preferences already
                // feed the memory/persona pipelines; ideas must not duplicate
                // them.
                _ => {}
            }
        }

        let day_tag = today.replace('-', "");
        let mut minted = 0usize;
        for (i, mut g) in candidates.into_iter().enumerate() {
            if minted >= MAX_IDEAS_PER_DAY {
                break;
            }
            // Feedback tuning: skip topics the user keeps dismissing, and
            // never pile up a second pending idea for an unanswered topic.
            if downranked(&store.topic_signals(&g.topic)?) {
                continue;
            }
            if store.pending_topic_exists(&g.topic)? {
                continue;
            }
            g.id = format!("idea_{day_tag}_{i}");
            let new = NewIdea {
                id: g.id.clone(),
                title: g.title,
                description: g.description,
                includes: g.includes,
                kind: g.kind,
                topic: g.topic,
                created_day: today.to_string(),
                schedule: g.schedule,
            };
            if store.mint(&new)? {
                minted += 1;
            }
        }
        Ok(minted)
    }
}

pub mod llm {
    //! The single LLM-gating contract for the nightly pass.
    //!
    //! Three LLM touchpoints - proposal refinement, memory distillation, and
    //! fix-loop draft revision - go through this one trait, and every call is
    //! routed through an explicitly resolved [`AuxiliaryModel`]: the proposal
    //! refiner uses the `Reflection` slot, the memory distiller uses the
    //! `Consolidation` slot, and ALL fix-loop draft revision (eval-reject and
    //! replay-reject paths) uses the `Repair` slot - never Reflection. None
    //! ever touches the chat/default model. All LLM steps are OFF unless the
    //! pass config has `enabled = true` AND a model policy resolves the slot;
    //! anything else is a deterministic-only pass with zero model calls.

    // The trait and its DTOs live at the API layer (`pantheon_api::nightly`)
    // so providers can implement the contract without depending on this
    // pipeline crate. Re-exported here so `pantheon_nightly::NightlyLlm`
    // keeps resolving.
    use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, ModelPolicy};
    pub use pantheon_api::nightly::{resolve_refiner, resolve_repair, NightlyLlm};

    /// A [`DistillBackend`] bound to one resolved auxiliary model: the
    /// adapter `run_pass` hands to the weigh phase. Construction is the only
    /// place the `Consolidation` slot is resolved, so every distill call in
    /// the pipeline provably routes through it.
    pub struct ResolvedDistill<'a> {
        llm: &'a dyn NightlyLlm,
        aux: &'a AuxiliaryModel,
    }

    /// Backend trait for the weigh phase: takes candidate texts, returns
    /// durable facts. The default is [`DeterministicDistill`] (identity).
    pub trait DistillBackend: Send + Sync {
        fn distill(&self, texts: &[String]) -> Result<Vec<String>, String>;
        fn name(&self) -> &'static str {
            "distill"
        }
    }

    impl DistillBackend for ResolvedDistill<'_> {
        fn distill(&self, texts: &[String]) -> Result<Vec<String>, String> {
            self.llm.distill_memories(self.aux, texts)
        }
        fn name(&self) -> &'static str {
            "auxiliary"
        }
    }

    /// No-model distiller: identity. Used whenever LLM steps are disabled
    /// zero model calls.
    pub struct DeterministicDistill;

    impl DistillBackend for DeterministicDistill {
        fn distill(&self, texts: &[String]) -> Result<Vec<String>, String> {
            Ok(texts.to_vec())
        }
        fn name(&self) -> &'static str {
            "deterministic"
        }
    }

    /// Resolve the memory distiller: `Some` only when the policy carries a
    /// `Consolidation` entry. The caller additionally gates on
    /// `config.enabled`.
    pub fn resolve_distill<'a>(
        llm: &'a dyn NightlyLlm,
        policy: &'a ModelPolicy,
    ) -> Option<ResolvedDistill<'a>> {
        policy
            .auxiliary(&AuxiliaryKind::Consolidation)
            .map(|aux| ResolvedDistill { llm, aux })
    }

    // Small deterministic invariant tests only.
}

pub mod persona {
    //! Approved persona notes → live session injection.
    //!
    //! Persona proposals never auto-apply: they pass eval-gating and
    //! replay-gating, then wait for explicit human approval. Approval writes
    //! the note to the memory store's `persona` namespace (agent layer, key
    //! `nightly-persona-<topic>`). This module is the read half: the
    //! runtime loads those notes into the system prompt of fresh runs, so an
    //! approved persona actually shapes future sessions.
    //!
    //! Trust-tier rule, enforced by construction: [`crate::apply::decide`]
    //! with `approve = true` is the only writer to this namespace, so
    //! everything read here passed evals, passed replay validation, and got
    //! a human yes. The injector itself stays dumb on purpose.

    use pantheon_memory::MemoryBackend;

    /// Memory namespace holding approved persona notes.
    pub const PERSONA_NAMESPACE: &str = "persona";

    /// Key prefix for nightly-written persona notes.
    pub const PERSONA_KEY_PREFIX: &str = "nightly-persona-";

    /// Approved persona notes in stable write order: (key, text). Empty
    /// notes are dropped; a read failure degrades to empty (fail-open
    /// a broken memory read must never block a turn). Reads through the
    /// session's active backend, whatever it is.
    pub fn approved_notes(store: &dyn MemoryBackend) -> Vec<(String, String)> {
        store
            .list_agent(PERSONA_NAMESPACE)
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| k.starts_with(PERSONA_KEY_PREFIX))
            .filter(|(_, v)| !v.trim().is_empty())
            .collect()
    }

    /// Format approved persona notes as a system-prompt block. Empty string
    /// when there are no notes - the caller appends nothing.
    pub fn overlay_block(notes: &[(String, String)]) -> String {
        if notes.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "<nightly_persona>\nThe operator has approved these persona notes, learned from past sessions. Treat them as standing guidance:\n",
        );
        for (key, text) in notes {
            let topic = key.strip_prefix(PERSONA_KEY_PREFIX).unwrap_or(key);
            out.push_str(&format!("- [{topic}] {text}\n"));
        }
        out.push_str("</nightly_persona>");
        out
    }

    // Small deterministic invariant tests only.
}

pub mod propose {
    //! Signal → proposal. Deterministic templates: the same signals always
    //! yield the same proposals, so the nightly pass is reproducible and
    //! auditable. Every proposal carries its provenance (the runs/turns it
    //! learned from).
    //!
    //! Memory promotion uses a plain frequency + recency rule - no decay
    //! curves. A candidate promotes when it was observed in at least
    //! `min_sessions` distinct runs and its newest observation is within
    //! `max_age_days`. The old exponential half-life formula
    //! (`0.5^(age/half_life)`) is gone: it was plausible-looking math with
    //! no empirical basis, and "seen N times recently" says the same thing
    //! honestly.

    use crate::signals::{normalize, CandidateKind, MemoryCandidate, Signal, Source};
    pub use pantheon_api::nightly::{Proposal, ProposalKind, ProposalStatus, TurnRef};
    use sha2::{Digest, Sha256};
    use std::collections::{HashMap, HashSet};

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
            "nly_{kind}_{:08x}",
            fnv1a(&format!("{title}\n{body}")) & 0xffff_ffff
        )
    }

    /// Deterministic idempotency key for a memory candidate's normalized text.
    pub fn candidate_key(normalized: &str) -> String {
        let mut h = Sha256::new();
        h.update(normalized.as_bytes());
        let digest = h.finalize();
        format!(
            "nightly:{}",
            digest
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
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

    /// True when `turns` meet the memory promotion rule: observed in at
    /// least `min_sessions` distinct runs, newest observation within
    /// `max_age_days`. Frequency + recency, no decay curves.
    fn recurrence_met(
        turns: &[TurnRef],
        min_sessions: usize,
        max_age_days: i64,
        now_ms: i64,
    ) -> bool {
        let mut runs: Vec<&str> = turns.iter().map(|t| t.run_id.as_str()).collect();
        runs.sort_unstable();
        runs.dedup();
        if runs.len() < min_sessions {
            return false;
        }
        let newest = turns.iter().map(|t| t.ts_ms).max().unwrap_or(0);
        (now_ms - newest).max(0) as f64 / 86_400_000.0 <= max_age_days as f64
    }

    /// Turn behavior signals into proposals, strongest first. Pure and
    /// deterministic.
    ///
    /// Memory lessons only come from signals that meet the recurrence rule
    /// (`min_sessions` distinct runs, newest within `max_age_days`): a
    /// one-off correction or denial is noise, not a lesson. (Recurring user
    /// corrections also flow through the staged-candidate path in
    /// `weigh_candidates`; this function deliberately does NOT emit a
    /// lesson per correction, so the same steering can't mint two memory
    /// keys.)
    ///
    /// Design note: memory uses recurrence, not replay, as its gate. Replay
    /// would need a headless agent that can load a candidate lesson into
    /// context and re-run held-out tasks; Pantheon ships no such runner, so
    /// recurrence across real sessions is the implemented memory gate
    /// ("replay where meaningful; otherwise recurrence"). Skill and Persona
    /// proposals are NOT recurrence-gated here - their guard is the replay
    /// gate's strict-improvement requirement.
    pub fn from_signals(
        signals: &[Signal],
        min_sessions: usize,
        max_age_days: i64,
        now_ms: i64,
    ) -> Vec<Proposal> {
        // Group one-per-event signals so the recurrence rule can see
        // repeats of the same scope across runs.
        let mut denials: HashMap<String, Vec<TurnRef>> = HashMap::new();
        for signal in signals {
            match signal {
                Signal::UserCorrection { .. } => {
                    // No direct proposal: recurring steering is promoted via
                    // the staged-candidate path (`weigh_candidates`), which
                    // enforces the same recurrence rule. Emitting one here
                    // too would mint a second memory key for the same text.
                }
                Signal::ApprovalDenied { scope, at } => {
                    denials.entry(scope.clone()).or_default().push(at.clone());
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        // First-seen index per denial scope, so each scope emits one lesson.
        let mut denial_first: HashMap<&str, usize> = HashMap::new();
        for (i, signal) in signals.iter().enumerate() {
            if let Signal::ApprovalDenied { scope, .. } = signal {
                denial_first.entry(scope.as_str()).or_insert(i);
            }
        }
        for (i, signal) in signals.iter().enumerate() {
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
                        "\n## Notes\n- This skill was proposed automatically from repeated successful tool sequences.\n- Refine the trigger conditions after first use.\n",
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
                Signal::UserCorrection { .. } => {
                    // Handled by the staged-candidate path; see the grouping
                    // loop above. The signal is still audited (SignalObserved).
                }
                Signal::ApprovalDenied { scope, .. } => {
                    // One lesson per scope, only on recurrence: emit for
                    // the first-seen signal of each scope.
                    if denial_first.get(scope.as_str()) != Some(&i) {
                        continue;
                    }
                    let hits = &denials[scope];
                    if !recurrence_met(hits, min_sessions, max_age_days, now_ms) {
                        continue;
                    }
                    let (runs, turns) = provenance_of(hits);
                    let title = format!("lesson: approval denied for `{scope}`");
                    let body = format!(
                        "The user denied the approval scope `{scope}`. Do not repeat the same request shape: \
                         prefer a narrower scope, explain why the operation is needed first, or find an approach \
                         that stays inside already-granted permissions."
                    );
                    let key = format!("nightly-denial-{:08x}", fnv1a(scope) & 0xffff_ffff);
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
                    // The scan already required `min_failure_repeats`
                    // turns; a lesson additionally needs the failures to
                    // recur across distinct runs within the window.
                    if !recurrence_met(at, min_sessions, max_age_days, now_ms) {
                        continue;
                    }
                    let (runs, turns) = provenance_of(at);
                    let title = format!("lesson: `{tool}` failed {count}× - check preconditions");
                    let body = format!(
                        "The tool `{tool}` failed {count} times across recent turns. Before calling it, \
                         verify its preconditions (inputs exist, are well-formed, and the environment is ready) \
                         instead of retrying blindly."
                    );
                    let key = format!("nightly-prefail-{:08x}", fnv1a(tool) & 0xffff_ffff);
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
                         Proposed automatically; it takes effect only after human approval.",
                        hits.len(),
                        runs.len()
                    );
                    out.push(Proposal {
                        id: proposal_id("persona", &title, &body),
                        kind: ProposalKind::Persona {
                            topic: topic.to_string(),
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

    /// A memory candidate after the promotion decision.
    #[derive(Debug, Clone)]
    pub struct WeighedCandidate {
        /// Display text (distilled when the LLM step ran, else raw).
        pub text: String,
        pub kind: CandidateKind,
        pub distinct_runs: usize,
        pub sources: Vec<Source>,
        pub promote: bool,
        /// Deterministic idempotency key: `nightly:<sha256-16>`.
        pub key: String,
    }

    /// Max length of one promoted fact. Longer texts are truncated at a word
    /// boundary; the memory layer budget is the real ceiling.
    pub const MAX_FACT_CHARS: usize = 500;

    fn bound_fact(text: &str) -> String {
        let t = text.trim();
        if t.chars().count() <= MAX_FACT_CHARS {
            return t.to_string();
        }
        let mut out: String = t.chars().take(MAX_FACT_CHARS).collect();
        if let Some(i) = out.rfind(' ') {
            out.truncate(i);
        }
        out
    }

    /// Decide promotion for memory candidates and distill their text.
    ///
    /// The rule is deliberately plain: promote when the candidate was seen
    /// in at least `min_sessions` distinct runs and its newest observation
    /// is within `max_age_days`. Frequency without recency doesn't promote;
    /// recency without frequency doesn't either. No decay curves.
    ///
    /// `distill` merges near-duplicate texts when the operator enabled LLM
    /// steps; otherwise grouping is by exact normalized text and the raw
    /// text is kept. Distill output is validated: non-empty, single line,
    /// bounded - the model phrases, it never invents, because promotion
    /// still requires the group's staged ledger sources.
    pub fn weigh_candidates(
        candidates: Vec<MemoryCandidate>,
        min_sessions: usize,
        max_age_days: i64,
        now: i64,
        distill: Option<&dyn crate::llm::DistillBackend>,
    ) -> Vec<WeighedCandidate> {
        // Group by (kind, normalized text).
        let mut groups: HashMap<(String, String), Vec<MemoryCandidate>> = HashMap::new();
        for c in candidates {
            let key = (c.kind.label().to_string(), normalize(&c.text));
            groups.entry(key).or_default().push(c);
        }

        let mut weighed = Vec::new();
        for ((_kind_label, norm), members) in groups {
            let kind = members[0].kind.clone();
            let mut sources = Vec::new();
            for m in &members {
                sources.extend(m.sources.iter().cloned());
            }
            sources.sort_by(|a, b| (&a.run_id, a.ts_ms).cmp(&(&b.run_id, b.ts_ms)));
            sources.dedup_by(|a, b| a.run_id == b.run_id && a.ts_ms == b.ts_ms);

            let texts: Vec<String> = if let Some(d) = distill {
                let raws: Vec<String> = members.iter().map(|m| m.text.clone()).collect();
                match d.distill(&raws) {
                    Ok(facts) => facts
                        .into_iter()
                        .map(|f| bound_fact(&f))
                        .filter(|f| !f.is_empty() && !f.contains('\n'))
                        .collect(),
                    // A failed distill degrades to deterministic.
                    Err(_) => raws,
                }
            } else {
                members
                    .iter()
                    .map(|m| bound_fact(&m.text))
                    .collect::<Vec<_>>()
            };
            let mut seen = HashSet::new();
            let mut texts: Vec<String> = texts
                .into_iter()
                .filter(|t| seen.insert(normalize(t)))
                .collect();
            if texts.is_empty() {
                continue;
            }
            let display = texts.remove(0);

            let distinct_runs: usize = {
                let mut runs: Vec<&str> = sources.iter().map(|s| s.run_id.as_str()).collect();
                runs.sort_unstable();
                runs.dedup();
                runs.len()
            };
            let newest_age_days = sources
                .iter()
                .map(|s| (now - s.ts_ms).max(0) as f64 / 86_400_000.0)
                .fold(f64::INFINITY, f64::min);
            let promote = distinct_runs >= min_sessions && newest_age_days <= max_age_days as f64;

            weighed.push(WeighedCandidate {
                text: display,
                kind,
                distinct_runs,
                sources,
                promote,
                key: candidate_key(&norm),
            });
        }

        // Deterministic order: most runs first, then key.
        weighed.sort_by(|a, b| {
            b.distinct_runs
                .cmp(&a.distinct_runs)
                .then_with(|| a.key.cmp(&b.key))
        });
        weighed
    }

    /// Turn promoted memory candidates into MemoryLesson proposals. The
    /// promotion rule (frequency + recency) marks which candidates are
    /// strong enough to *surface*; each becomes a proposal parked in the
    /// approval queue with its evidence (provenance runs + observed-count).
    /// An operator grant is what dures the record - the unattended pass
    /// never writes it itself.
    pub fn memory_proposals(weighed: &[WeighedCandidate]) -> Vec<Proposal> {
        weighed
            .iter()
            .filter(|w| w.promote)
            .map(|w| {
                let runs: Vec<String> = {
                    let mut r: Vec<&str> = w.sources.iter().map(|s| s.run_id.as_str()).collect();
                    r.sort_unstable();
                    r.dedup();
                    r.into_iter().map(|s| s.to_string()).collect()
                };
                let title = format!("memory: {}", w.text.chars().take(80).collect::<String>());
                let body = format!(
                    "Durable fact ({}): {}. Observed across {} runs.",
                    w.kind.label(),
                    w.text,
                    w.distinct_runs
                );
                Proposal {
                    id: proposal_id("lesson", &title, &body),
                    kind: ProposalKind::MemoryLesson { key: w.key.clone() },
                    title,
                    body,
                    provenance_runs: runs,
                    provenance_turns: Vec::new(),
                    eval_tags: Vec::new(),
                    status: ProposalStatus::Proposed,
                }
            })
            .collect()
    }

    // Small deterministic invariant tests only.
}

pub mod report {
    //! Human-readable nightly report.
    //!
    //! One markdown page per pass at `<data_dir>/nightly/nightly-report.md`
    //! (also returned by `run_pass`). Written for the operator who runs the
    //! pass by hand and wants to see what happened without reading JSONL.

    use crate::audit::NightlyEvent;
    use crate::propose::Proposal;
    use crate::repair_targets::{RepairOutcome, RepairReport};
    use crate::state::NightlyState;

    /// The report label for a repair outcome: exactly `ok` | `skipped` |
    /// `failed`, never the Rust variant name (`Contained`, `SkippedDryRun`)
    /// and never a vague word. A contained target was disabled/paused with
    /// an escalation: from the operator's view that target failed.
    fn repair_label(outcome: &RepairOutcome) -> &'static str {
        match outcome {
            RepairOutcome::Healthy | RepairOutcome::Repaired => "ok",
            RepairOutcome::SkippedDryRun => "skipped",
            RepairOutcome::Contained => "failed",
        }
    }

    pub fn render(
        state: &NightlyState,
        proposals: &[Proposal],
        repairs: &[RepairReport],
        events: &[NightlyEvent],
    ) -> String {
        let mut md = String::from("# Nightly report\n\n");
        md.push_str(&format!(
            "- Last run: {}\n- Outcome: {}{}\n- Runs scanned: {}\n- Signals: {}\n- Proposals: {}\n- Applied: {}\n- Pending approval: {}\n- Repairs fixed: {}\n- Repairs contained: {}\n- Dry run: {}\n\n",
            fmt_ms(state.last_run_ms),
            state.last_outcome,
            if state.last_outcome_reason.is_empty() {
                String::new()
            } else {
                format!(" ({})", state.last_outcome_reason)
            },
            state.last_runs_scanned,
            state.last_signals,
            state.last_proposals,
            state.last_applied,
            state.last_pending,
            state.last_repairs_fixed,
            state.last_repairs_contained,
            state.last_dry_run,
        ));

        md.push_str("## Proposals\n\n");
        if proposals.is_empty() {
            md.push_str("No proposals this pass.\n\n");
        }
        for p in proposals {
            md.push_str(&format!(
                "### {} - {}\n- Kind: {}\n- Status: {:?}\n- Provenance: {}\n\n{}\n\n",
                p.id,
                p.title,
                p.kind_name(),
                p.status,
                p.provenance_runs.join(", "),
                p.body,
            ));
        }

        md.push_str("## Repairs\n\n");
        let acted: Vec<&RepairReport> = repairs
            .iter()
            .filter(|r| r.outcome != RepairOutcome::Healthy)
            .collect();
        if acted.is_empty() {
            md.push_str("No broken targets this pass.\n\n");
        }
        for r in acted {
            let detail = if r.detail.is_empty() {
                match r.outcome {
                    RepairOutcome::SkippedDryRun => "dry run: detected, not changed",
                    _ => "(no detail)",
                }
            } else {
                r.detail.as_str()
            };
            md.push_str(&format!(
                "- `{}` - {}: {}\n",
                r.target,
                repair_label(&r.outcome),
                detail
            ));
        }
        md.push('\n');

        md.push_str("## Lifecycle\n\n");
        for e in events {
            md.push_str(&format!("- {e:?}\n"));
        }
        md
    }

    fn fmt_ms(ms: i64) -> String {
        if ms <= 0 {
            return "never".into();
        }
        // A real UTC date, not "Ns since epoch": the old rendering forced
        // the operator to convert epoch seconds by hand to know when a
        // pass ran. Civil-from-days (Hinnant's algorithm), std-only.
        let secs = ms.div_euclid(1000);
        let days = secs.div_euclid(86_400);
        let tod = secs.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
            tod / 3600,
            (tod % 3600) / 60,
            tod % 60
        )
    }

    // Small deterministic invariant tests only.

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fmt_ms_renders_utc_date_not_epoch_seconds() {
            assert_eq!(fmt_ms(0), "never");
            assert_eq!(fmt_ms(-5), "never");
            // 1790449200s == 2026-09-26 19:00:00 UTC.
            assert_eq!(fmt_ms(1_790_449_200_000), "2026-09-26 19:00:00 UTC");
            assert_eq!(fmt_ms(1_000), "1970-01-01 00:00:01 UTC");
        }

        /// Repair lines carry exactly ok / skipped / failed, never the
        /// Rust variant names and never a vague word.
        #[test]
        fn repair_labels_are_explicit() {
            assert_eq!(repair_label(&RepairOutcome::Healthy), "ok");
            assert_eq!(repair_label(&RepairOutcome::Repaired), "ok");
            assert_eq!(repair_label(&RepairOutcome::SkippedDryRun), "skipped");
            assert_eq!(repair_label(&RepairOutcome::Contained), "failed");
        }

        /// The header line names the pass outcome (with its reason), so
        /// the report never leaves the operator guessing whether the pass
        /// itself succeeded.
        #[test]
        fn report_header_names_outcome() {
            let mut state = NightlyState::default();
            state.last_run_ms = 1_790_449_200_000;
            state.last_outcome = "failed".to_string();
            state.last_outcome_reason = "ledger unreadable".to_string();
            let md = render(&state, &[], &[], &[]);
            assert!(md.contains("- Outcome: failed (ledger unreadable)"), "{md}");
            state.last_outcome = "ok".to_string();
            state.last_outcome_reason = String::new();
            let md = render(&state, &[], &[], &[]);
            assert!(md.contains("- Outcome: ok\n"), "{md}");
        }
    }
}

pub mod signals {
    //! Unified ledger scan: one pass over recent runs yields both behavior
    //! signals (proposal mining) and memory candidates (promotion).
    //!
    //! Replaces the old duplicated scans in `pantheon-reflect::signals` and
    //! `pantheon-consolidate::stage`. The scan is bounded (`max_runs` runs,
    //! events at or after `since_ms`) and deterministic: the same ledger
    //! state always yields the same signals in the same order.

    use pantheon_api::capability::Policy;
    use pantheon_api::error::PantheonError;
    use pantheon_api::events::Event;
    use pantheon_memory::{LayerKind, MemoryBackend, Recalled};
    use pantheon_storage::Ledger;
    use std::collections::HashMap;

    /// Knobs for the unified scan.
    #[derive(Debug, Clone)]
    pub struct CollectPolicy {
        /// A tool sequence needs this many successful repetitions to count.
        pub min_sequence_repeats: usize,
        /// A tool needs this many attributed failures to count.
        pub min_failure_repeats: usize,
        /// A preference keyword needs this many hits to count.
        pub min_preference_hits: usize,
    }

    impl Default for CollectPolicy {
        fn default() -> Self {
            Self {
                min_sequence_repeats: 3,
                min_failure_repeats: 3,
                min_preference_hits: 3,
            }
        }
    }

    /// Minimum tool calls in a turn for the turn to count as a "sequence".
    const MIN_SEQUENCE_LEN: usize = 2;

    // `TurnRef` lives at the API layer (see `pantheon_api::nightly`): it is
    // part of the `Proposal` DTO shared with providers. Re-exported here so
    // `pantheon_nightly::TurnRef` keeps resolving.
    pub use pantheon_api::nightly::TurnRef;

    /// Behavior signals mined from the ledger.
    #[derive(Debug, Clone)]
    pub enum Signal {
        /// A tool sequence succeeded in N turns - skill material.
        RepeatedSequence {
            tools: Vec<String>,
            hits: Vec<TurnRef>,
        },
        /// The user steered a turn mid-flight.
        UserCorrection { text: String, at: TurnRef },
        /// An approval was denied.
        ApprovalDenied { scope: String, at: TurnRef },
        /// A tool failed repeatedly - precondition-lesson material.
        RepeatedFailure {
            tool: String,
            count: usize,
            at: Vec<TurnRef>,
        },
        /// A preference keyword recurred across turns - persona material.
        RepeatedPreference {
            topic: &'static str,
            keyword: &'static str,
            hits: Vec<TurnRef>,
        },
    }

    /// Where a memory candidate came from.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct Source {
        pub run_id: String,
        pub ts_ms: i64,
    }

    /// What kind of memory a candidate wants to become.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum CandidateKind {
        /// A steering correction from a user - standing guidance.
        Steering,
        /// Something the agent wrote at session scope worth keeping.
        SessionMemory,
    }

    impl CandidateKind {
        pub fn label(&self) -> &'static str {
            match self {
                CandidateKind::Steering => "steering",
                CandidateKind::SessionMemory => "session-memory",
            }
        }
    }

    /// A candidate durable memory with provenance.
    #[derive(Debug, Clone)]
    pub struct MemoryCandidate {
        pub text: String,
        pub kind: CandidateKind,
        pub sources: Vec<Source>,
    }

    /// What one ledger scan produced.
    pub struct ScanResult {
        /// How many runs were actually walked.
        pub runs_scanned: usize,
        pub signals: Vec<Signal>,
        pub candidates: Vec<MemoryCandidate>,
        /// Per-tool invocation health over the scanned window: total calls
        /// (`ToolStarted`) and errors attributed via failed turns (the same
        /// attribution as the `RepeatedFailure` signal). Feeds the nightly
        /// repair loop's broken-tool detection.
        pub tool_stats: Vec<crate::repair_targets::ToolCallStats>,
    }

    /// One pass over the ledger: behavior signals AND memory candidates.
    ///
    /// Events before `since_ms` are skipped; at most `max_runs` runs are
    /// scanned (most recent first). Memory candidates also need the memory
    /// backend: the ledger only carries memory-write keys, so the value is
    /// read back by exact (layer, namespace, key) - never a fuzzy neighbor.
    pub fn collect(
        ledger: &Ledger,
        backend: &dyn MemoryBackend,
        policy: &Policy,
        collect_policy: &CollectPolicy,
        since_ms: i64,
        max_runs: usize,
    ) -> Result<ScanResult, PantheonError> {
        let mut signals = Vec::new();
        let mut runs_scanned = 0usize;
        let mut candidates: HashMap<(String, CandidateKindKey), Vec<Source>> = HashMap::new();
        let mut candidate_texts: HashMap<(String, CandidateKindKey), String> = HashMap::new();

        // turn key -> ordered tool names started in that turn
        let mut turn_tools: HashMap<(String, String), Vec<String>> = HashMap::new();
        // tool name -> total invocations started in the window
        let mut tool_calls: HashMap<String, u64> = HashMap::new();
        // turn key -> did the turn complete successfully
        let mut turn_ok: HashMap<(String, String), bool> = HashMap::new();
        // turn key -> when the turn started (for signal recency)
        let mut turn_ts: HashMap<(String, String), i64> = HashMap::new();
        let mut failures: HashMap<String, Vec<TurnRef>> = HashMap::new();
        // (steering text, where) for preference grouping below.
        let mut corrections: Vec<(String, TurnRef)> = Vec::new();

        for listing in ledger.list_runs(max_runs)? {
            let run_id = listing.0.clone();
            runs_scanned += 1;
            let mut current_turn: Option<String> = None;
            for entry in ledger.replay(&run_id)? {
                if entry.ts_ms < since_ms {
                    continue;
                }
                match &entry.event {
                    Event::TurnStarted { turn_id, .. } => {
                        current_turn = Some(turn_id.clone());
                        turn_ts
                            .entry((run_id.clone(), turn_id.clone()))
                            .or_insert(entry.ts_ms);
                    }
                    Event::ToolStarted { tool, .. } => {
                        *tool_calls.entry(tool.clone()).or_default() += 1;
                        if let Some(t) = &current_turn {
                            turn_tools
                                .entry((run_id.clone(), t.clone()))
                                .or_default()
                                .push(tool.clone());
                        }
                    }
                    Event::TurnCompleted { turn_id, .. } => {
                        turn_ok.insert((run_id.clone(), turn_id.clone()), true);
                    }
                    Event::TurnFailed { turn_id, .. } => {
                        turn_ok.insert((run_id.clone(), turn_id.clone()), false);
                        // Attribute the failure to the last tool started in
                        // the turn, if any: the most likely culprit.
                        if let Some(tools) = turn_tools.get(&(run_id.clone(), turn_id.clone())) {
                            if let Some(tool) = tools.last() {
                                failures.entry(tool.clone()).or_default().push(TurnRef {
                                    run_id: run_id.clone(),
                                    turn_id: turn_id.clone(),
                                    ts_ms: entry.ts_ms,
                                });
                            }
                        }
                    }
                    Event::SteeringProvided { text, .. } => {
                        let at = TurnRef {
                            run_id: run_id.clone(),
                            turn_id: current_turn.clone().unwrap_or_default(),
                            ts_ms: entry.ts_ms,
                        };
                        corrections.push((text.clone(), at.clone()));
                        signals.push(Signal::UserCorrection {
                            text: text.clone(),
                            at: at.clone(),
                        });
                        push_candidate(
                            &mut candidates,
                            &mut candidate_texts,
                            CandidateKind::Steering,
                            text.clone(),
                            Source {
                                run_id: run_id.clone(),
                                ts_ms: entry.ts_ms,
                            },
                        );
                    }
                    Event::RunProgress { detail, .. } => {
                        if let Some((layer_name, ns, key)) = parse_memory_write(detail) {
                            // Only session-scoped writes are consolidation
                            // material; Agent/Project rows are already durable.
                            if parse_layer(&layer_name) != Some(LayerKind::TaskSession) {
                                continue;
                            }
                            // Read the value back from the store: the ledger
                            // only carries the key. Recall with the key's
                            // tokens as the query, then take the exact
                            // (layer, namespace, key) hit - never a fuzzy
                            // neighbor. A missing record (expired,
                            // forgotten) is not a candidate: we never invent
                            // text.
                            if let Ok(hits) = backend.recall(
                                policy,
                                &[ns.as_str()],
                                &[LayerKind::TaskSession],
                                &key_query(&key),
                                10,
                            ) {
                                let exact = hits.iter().find(|h: &&Recalled| {
                                    h.record.layer == LayerKind::TaskSession
                                        && h.record.namespace == ns
                                        && h.record.key == key
                                });
                                if let Some(hit) = exact {
                                    let text = hit.record.value.trim();
                                    if !text.is_empty() {
                                        push_candidate(
                                            &mut candidates,
                                            &mut candidate_texts,
                                            CandidateKind::SessionMemory,
                                            text.to_string(),
                                            Source {
                                                run_id: run_id.clone(),
                                                ts_ms: entry.ts_ms,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Event::ApprovalDenied { scope, .. } => {
                        let at = TurnRef {
                            run_id: run_id.clone(),
                            turn_id: current_turn.clone().unwrap_or_default(),
                            ts_ms: entry.ts_ms,
                        };
                        signals.push(Signal::ApprovalDenied {
                            scope: scope.clone(),
                            at,
                        });
                    }
                    _ => {}
                }
            }
        }

        // Group successful turns by their exact tool sequence.
        let mut seq_hits: HashMap<Vec<String>, Vec<TurnRef>> = HashMap::new();
        for ((run_id, turn_id), tools) in &turn_tools {
            if tools.len() < MIN_SEQUENCE_LEN {
                continue;
            }
            if turn_ok.get(&(run_id.clone(), turn_id.clone())) != Some(&true) {
                continue;
            }
            seq_hits.entry(tools.clone()).or_default().push(TurnRef {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
                ts_ms: turn_ts
                    .get(&(run_id.clone(), turn_id.clone()))
                    .copied()
                    .unwrap_or(0),
            });
        }
        let mut seqs: Vec<(Vec<String>, Vec<TurnRef>)> = seq_hits
            .into_iter()
            .filter(|(_, hits)| hits.len() >= collect_policy.min_sequence_repeats)
            .collect();
        // Deterministic order: most hits first, then lexicographic.
        seqs.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        for (tools, hits) in seqs {
            signals.push(Signal::RepeatedSequence { tools, hits });
        }

        // Error counts per tool for the repair loop, captured before the
        // `failures` map is consumed below.
        let tool_errors: HashMap<String, u64> = failures
            .iter()
            .map(|(k, v)| (k.clone(), v.len() as u64))
            .collect();
        let mut fails: Vec<(String, Vec<TurnRef>)> = failures
            .into_iter()
            .filter(|(_, at)| at.len() >= collect_policy.min_failure_repeats)
            .collect();
        fails.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
        for (tool, at) in fails {
            signals.push(Signal::RepeatedFailure {
                count: at.len(),
                tool,
                at,
            });
        }

        // Standing preferences: corrections sharing one preference keyword,
        // grouped by (keyword, topic). Deterministic order: topic, keyword.
        let mut pref_hits: HashMap<(&str, &str), Vec<TurnRef>> = HashMap::new();
        for (text, at) in &corrections {
            let ws = words(text);
            for (word, topic) in PREFERENCE_KEYWORDS {
                if ws.iter().any(|w| w == word) {
                    pref_hits.entry((word, topic)).or_default().push(at.clone());
                }
            }
        }
        let mut prefs: Vec<((&str, &str), Vec<TurnRef>)> = pref_hits
            .into_iter()
            .filter(|(_, hits)| hits.len() >= collect_policy.min_preference_hits)
            .collect();
        prefs.sort_by(|a, b| (a.0 .1, a.0 .0).cmp(&(b.0 .1, b.0 .0)));
        for ((keyword, topic), hits) in prefs {
            signals.push(Signal::RepeatedPreference {
                topic,
                keyword,
                hits,
            });
        }

        // Memory candidates, deterministic order: most sources first, then
        // (kind, normalized text).
        let mut out: Vec<MemoryCandidate> = candidates
            .into_iter()
            .map(|((norm, kind_key), sources)| MemoryCandidate {
                text: candidate_texts
                    .remove(&(norm, kind_key.clone()))
                    .unwrap_or_default(),
                kind: kind_key.to_kind(),
                sources,
            })
            .collect();
        out.sort_by(|a, b| {
            b.sources
                .len()
                .cmp(&a.sources.len())
                .then(a.kind.label().cmp(b.kind.label()))
                .then(normalize(&a.text).cmp(&normalize(&b.text)))
        });

        // Per-tool invocation health for the repair loop: total calls plus
        // errors attributed via failed turns (the same attribution as the
        // `RepeatedFailure` signal).
        let mut tool_stats: Vec<crate::repair_targets::ToolCallStats> = tool_calls
            .iter()
            .map(|(name, calls)| crate::repair_targets::ToolCallStats {
                name: name.clone(),
                calls: *calls,
                errors: tool_errors.get(name).copied().unwrap_or(0),
            })
            .collect();
        tool_stats.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(ScanResult {
            runs_scanned,
            signals,
            candidates: out,
            tool_stats,
        })
    }

    /// Hashable stand-in for [`CandidateKind`].
    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    enum CandidateKindKey {
        Steering,
        SessionMemory,
    }

    impl CandidateKindKey {
        fn to_kind(&self) -> CandidateKind {
            match self {
                CandidateKindKey::Steering => CandidateKind::Steering,
                CandidateKindKey::SessionMemory => CandidateKind::SessionMemory,
            }
        }
    }

    fn push_candidate(
        candidates: &mut HashMap<(String, CandidateKindKey), Vec<Source>>,
        texts: &mut HashMap<(String, CandidateKindKey), String>,
        kind: CandidateKind,
        text: String,
        source: Source,
    ) {
        let norm = normalize(&text);
        if norm.is_empty() {
            return;
        }
        let key = match kind {
            CandidateKind::Steering => CandidateKindKey::Steering,
            CandidateKind::SessionMemory => CandidateKindKey::SessionMemory,
        };
        let entry = (norm, key);
        texts.entry(entry.clone()).or_insert(text);
        candidates.entry(entry).or_default().push(source);
    }

    /// Normalize text for dedupe: lowercase, collapse whitespace.
    pub fn normalize(text: &str) -> String {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    fn words(text: &str) -> Vec<String> {
        text.split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .map(|w| w.to_lowercase())
            .collect()
    }

    /// (keyword, topic) pairs for preference mining.
    const PREFERENCE_KEYWORDS: &[(&str, &str)] = &[
        ("concise", "concise answers"),
        ("concisely", "concise answers"),
        ("brief", "brief answers"),
        ("detailed", "detailed answers"),
        ("verbose", "detailed answers"),
        ("tabs", "tabs over spaces"),
        ("spaces", "spaces over tabs"),
        ("rust", "rust"),
        ("python", "python"),
        ("dark", "dark mode"),
        ("light", "light mode"),
    ];

    /// Parse a `memory_write layer=.. ns=.. key=..` RunProgress annotation.
    fn parse_memory_write(detail: &str) -> Option<(String, String, String)> {
        let rest = detail.strip_prefix("memory_write ")?;
        let mut layer = None;
        let mut ns = None;
        let mut key = None;
        for tok in rest.split_whitespace() {
            let (k, v) = tok.split_once('=')?;
            match k {
                "layer" => layer = Some(v.to_string()),
                "ns" => ns = Some(v.to_string()),
                "key" => key = Some(v.to_string()),
                _ => {}
            }
        }
        Some((layer?, ns?, key?))
    }

    /// Map the Debug-format layer name from the RunProgress annotation.
    fn parse_layer(name: &str) -> Option<LayerKind> {
        match name {
            "Global" => Some(LayerKind::Global),
            "Agent" => Some(LayerKind::Agent),
            "Project" => Some(LayerKind::Project),
            "TaskSession" => Some(LayerKind::TaskSession),
            "EphemeralTurn" => Some(LayerKind::EphemeralTurn),
            _ => None,
        }
    }

    /// Build a recall query from a memory key: its alphanumeric tokens.
    fn key_query(key: &str) -> String {
        key.split(|c: char| !c.is_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    // Small deterministic invariant tests only.
}

pub mod state {
    //! Durable pass state: what ran, when, and what changed.
    //!
    //! `<data_dir>/nightly/nightly-state.json` records the last pass so the
    //! TUI and the dashboard can show "nightly last ran at ..., proposed N,
    //! applied M" without re-running the pipeline.

    use std::path::{Path, PathBuf};

    pub fn state_path(data_dir: &Path) -> PathBuf {
        data_dir.join("nightly").join("nightly-state.json")
    }

    /// The last nightly pass, in durable form.
    #[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
    pub struct NightlyState {
        pub last_run_ms: i64,
        pub last_runs_scanned: usize,
        pub last_signals: usize,
        pub last_proposals: usize,
        pub last_applied: usize,
        pub last_pending: usize,
        pub last_dry_run: bool,
        /// Repair targets contained (disabled/paused) by the last pass.
        #[serde(default)]
        pub last_repairs_contained: usize,
        /// Repair targets fixed by the last pass.
        #[serde(default)]
        pub last_repairs_fixed: usize,
        /// Last pass outcome, exactly one of `ok` | `skipped` | `failed`
        /// (never a vague label). `skipped` = dry run; `failed` = the pass
        /// errored before finishing. Empty in state files written before
        /// this field existed; [`NightlyState::load`] normalizes those
        /// (a recorded pass completed, so it was `ok`, or `skipped` for a
        /// recorded dry run) instead of surfacing a blank label.
        #[serde(default)]
        pub last_outcome: String,
        /// Why the last pass was `skipped` or `failed`; empty for `ok`.
        #[serde(default)]
        pub last_outcome_reason: String,
    }

    impl NightlyState {
        /// Load the last-pass state. A torn file loads as the default, with
        /// a loud warning: state is written atomically (see [`Self::save`]),
        /// so a torn file can only predate atomic writes, and a state file
        /// that errors forever would hide every later pass from status
        /// surfaces. The summary is recomputed next pass; nothing durable
        /// besides the summary lives here.
        pub fn load(data_dir: &Path) -> Result<Self, String> {
            match std::fs::read_to_string(state_path(data_dir)) {
                Ok(text) => match serde_json::from_str(&text) {
                    Ok(state) => Ok(Self::normalize(state)),
                    Err(e) => {
                        eprintln!(
                            "nightly: state {} is torn ({e}); treating as never-run",
                            state_path(data_dir).display()
                        );
                        Ok(Self::default())
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
                Err(e) => Err(format!("read nightly state: {e}")),
            }
        }

        /// Fill in the outcome for state files written before the field
        /// existed. Only a completed pass ever wrote state (failures are
        /// recorded separately since this change), so a recorded pass with
        /// no outcome was a success - or a dry run, which is `skipped`.
        fn normalize(mut state: Self) -> Self {
            if state.last_outcome.is_empty() && state.last_run_ms > 0 {
                if state.last_dry_run {
                    state.last_outcome = "skipped".to_string();
                    state.last_outcome_reason = "dry run: no changes applied".to_string();
                } else {
                    state.last_outcome = "ok".to_string();
                }
            }
            state
        }

        /// Save atomically (temp file + rename), so a crash mid-write never
        /// leaves a torn state file for [`Self::load`] to trip over.
        pub fn save(&self, data_dir: &Path) -> Result<(), String> {
            let path = state_path(data_dir);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            let text = serde_json::to_string_pretty(self)
                .map_err(|e| format!("encode nightly state: {e}"))?;
            crate::apply::atomic_write(&path, &text)
                .map_err(|e| format!("write {}: {e}", path.display()))
        }
    }

    // Small deterministic invariant tests only.

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Legacy state files (no outcome field) normalize to the only
        /// honest label: a recorded pass completed, so `ok` - or
        /// `skipped` for a recorded dry run. A fresh default (never ran)
        /// stays blank, and the dashboard renders "no passes yet" for it.
        #[test]
        fn normalize_fills_legacy_outcome() {
            let mut legacy = NightlyState::default();
            legacy.last_run_ms = 1_790_449_200_000;
            let norm = NightlyState::normalize(legacy);
            assert_eq!(norm.last_outcome, "ok");
            assert!(norm.last_outcome_reason.is_empty());

            let mut dry = NightlyState::default();
            dry.last_run_ms = 1_790_449_200_000;
            dry.last_dry_run = true;
            let norm = NightlyState::normalize(dry);
            assert_eq!(norm.last_outcome, "skipped");
            assert_eq!(norm.last_outcome_reason, "dry run: no changes applied");

            let never = NightlyState::normalize(NightlyState::default());
            assert!(never.last_outcome.is_empty());
        }
    }
}
