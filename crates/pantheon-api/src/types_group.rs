//! types_group: events, message, model, provenance, temporal, todo, logging, mcp_catalog in one file.

pub mod events {
    //! Agent-engine event enum. Every meaningful transition becomes an event,
    //! giving replayability, debugging, and crash recovery.

    use crate::message::Message;
    use crate::model::DecisionPoint;
    use crate::provenance::Provenance;
    use crate::todo::TodoItem;
    use serde::{Deserialize, Serialize};

    /// Compact summary of a decision answer for ledger events.
    /// Full type lives in model.rs; this is the ledger-friendly version.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "type")]
    pub enum DecisionAnswerSummary {
        Route {
            choice: String,
            confidence: f32,
        },
        Gate {
            verdict: String,
            score: f32,
            confidence: f32,
        },
        Binary {
            accepted: bool,
            confidence: f32,
        },
        Threshold {
            passed: bool,
            value: f32,
        },
    }

    /// Compact summary of what the host actually did with a decision.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "type")]
    pub enum DecisionActionSummary {
        Accepted,
        /// Parked awaiting an operator decision (approval or input) - the
        /// run is neither accepted nor denied. Recording a park as
        /// `Accepted` hid blocked runs from every ledger consumer.
        Parked {
            scope: String,
        },
        Overridden {
            fallback_used: String,
        },
        Denied {
            reason: String,
        },
    }

    /// Canonical runtime events (§2 agent engine + §18 runtime API).
    /// NOTE: no Eq derive - DecisionMade events carry f32 confidence scores.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub enum Event {
        RunStarted {
            run_id: String,
        },
        RunProgress {
            run_id: String,
            detail: String,
        },
        RunCompleted {
            run_id: String,
        },
        RunFailed {
            run_id: String,
            code: String,
        },
        /// Cancellation is terminal but distinct from failure: a user requested
        /// it, so replay and metrics can tell the two apart.
        RunCanceled {
            run_id: String,
            reason: String,
        },
        RunRecovered {
            run_id: String,
        },
        /// A run is bound to an agent profile identity.
        ///
        /// Separate from `RunStarted` on purpose: a run row can be created
        /// before any profile is resolved (a CLI pre-start, a TUI that opens a
        /// conversation, a resumed pre-profiles run), and the binding is what
        /// makes that run *belong* to an agent. Emitted once, and refused by
        /// the ledger if the run is already bound to a different agent, so a
        /// run can never silently change identity mid-life.
        AgentBound {
            run_id: String,
            agent_id: String,
            profile: String,
        },
        /// A user-initiated turn inside a durable conversation. A run may contain
        /// many turns over its lifetime; each turn owns its model/tool activity.
        TurnStarted {
            run_id: String,
            turn_id: String,
        },
        /// The raw user prompt that opened the turn. Recorded so a turn can be
        /// retried (`POST /api/runs/:id/retry`) without the client resending
        /// the text; display paths redact it like any other user content.
        UserMessage {
            run_id: String,
            text: String,
        },
        /// The turn yielded to an external continuation such as human approval.
        TurnParked {
            run_id: String,
            turn_id: String,
            reason: String,
        },
        TurnCompleted {
            run_id: String,
            turn_id: String,
            outcome: String,
        },
        TurnFailed {
            run_id: String,
            turn_id: String,
            code: String,
        },
        /// The operator rewound this turn (TUI double-Esc). History is never
        /// rewritten: the marker is appended and `Ledger::replay` excludes the
        /// named turn and everything after it up to this marker, so resume and
        /// the transcript rebuild behave as if the rewound turns never happened.
        /// Turns started after the marker replay normally.
        TurnRewound {
            run_id: String,
            turn_id: String,
        },
        /// The operator snapshotted the session at this turn (`/checkpoint`).
        /// A durable, named marker: `/checkpoints` lists them and `/restore`
        /// rewinds to one by emitting `TurnRewound` at the turn that follows
        /// the checkpoint's turn. Like rewind markers, this is a read-time
        /// projection aid - raw history is never rewritten.
        CheckpointCreated {
            run_id: String,
            turn_id: String,
            name: String,
        },
        /// The operator steered the running turn (`/steer`): mid-turn guidance
        /// injected without canceling or restarting. The guidance also lands in
        /// the transcript as a marked user message (see `rebuild_messages`), so
        /// resume stays faithful; this row is the durable steering record.
        SteeringProvided {
            run_id: String,
            text: String,
        },
        /// A message from another agent was drained out of the collaboration
        /// mailbox into the running turn. It also lands in the transcript as a
        /// marked, Untrusted-tier user message (see `rebuild_messages`), so resume
        /// stays faithful; this row is the durable delivery record and the replay
        /// source. `kind` is a string because the `MessageKind` enum lives in
        /// storage, which this crate cannot depend on.
        AgentMessageDelivered {
            run_id: String,
            message_id: String,
            sender: String,
            kind: String,
            task_id: Option<String>,
            correlation_id: Option<String>,
            content: String,
        },
        ModelRequested {
            run_id: String,
            model: String,
        },
        ModelDelta {
            run_id: String,
            delta: String,
        },
        ModelCompleted {
            run_id: String,
        },
        ToolRequested {
            run_id: String,
            tool: String,
        },
        ToolStarted {
            run_id: String,
            /// Stable per-call id (`call_{turn}_{i}`); resume keys grants on this.
            call_id: String,
            tool: String,
            /// Tool arguments as issued (persisted so resume re-executes exactly).
            args: String,
            /// Which entity/action issued this tool call (user, model, memory).
            provenance: Provenance,
        },
        ToolOutput {
            run_id: String,
            call_id: String,
            tool: String,
            truncated: bool,
            /// Provenance of the output (tool-derived, always Untrusted tier).
            provenance: Provenance,
        },
        ToolCompleted {
            run_id: String,
            call_id: String,
            tool: String,
            /// Provenance of the completion boundary.
            provenance: Provenance,
        },
        /// A tool call that started but never completed before its run was
        /// settled by recovery (crash, or an explicit repair). This is the
        /// first-class provenance record for an unfinished outcome: it names
        /// the call, repeats the issued arguments, and says what was already
        /// confirmed (`output_seen`: a `ToolOutput` row exists;
        /// `result_seen`: a `ToolMessage` row exists) — and, by its existence,
        /// that the outcome itself was never confirmed. It is deliberately
        /// NOT a success and NOT a failure: the tool may or may not have
        /// had its side effect, and no consumer may assume either. A call
        /// with this event is settled, never pending: recovery must not
        /// re-run it, and a second recovery pass must not write it again.
        ToolInterrupted {
            run_id: String,
            call_id: String,
            tool: String,
            args: String,
            output_seen: bool,
            result_seen: bool,
            /// Fixed explanation, not a tool result: the call was interrupted
            /// and its outcome is unconfirmed.
            detail: String,
        },
        /// The agent replaced its todo list via the `todo` tool. The items
        /// ride on the event so transcript observers (TUI card, gateway) see
        /// every change as it happens; the durable per-run snapshot lives in
        /// the ledger's `todos` table, reloaded on session resume.
        TodosUpdated {
            run_id: String,
            items: Vec<TodoItem>,
        },
        /// The agent published structured UI content (see `crate::ui`). The
        /// document rides the event so transcript observers (TUI block,
        /// gateway frames) see it as it happens; the ledger persists it like
        /// any other event, so resume and replay rebuild it. Clients render
        /// it through their own native renderers after [`crate::ui::degrade_for`].
        UiContent {
            run_id: String,
            turn_id: String,
            ui_id: String,
            document: crate::ui::UiDocument,
        },
        /// A revision of a published document (same `ui_id`, bumped
        /// `revision`). The full replacement document rides the event - no
        /// JSON-patch dialect to version. Clients apply it only when
        /// `revision` exceeds the one they hold, so out-of-order replays
        /// cannot regress a live view.
        UiUpdated {
            run_id: String,
            ui_id: String,
            revision: u64,
            document: crate::ui::UiDocument,
        },
        /// The runtime parked the turn on a UI approval card (see
        /// `crate::ui::ApprovalCard`): the operator's approve/deny decision
        /// is awaited, exactly like `UserInputRequested` parks for `ask_user`.
        /// Resolved by [`Event::UiActionSubmitted`].
        UiActionRequested {
            run_id: String,
            ui_id: String,
            call_id: String,
        },
        /// The operator submitted a UI action. This is the audit record and
        /// the idempotency key: `(ui_id, nonce)` replays are rejected.
        /// Params are deliberately NOT persisted (form `Secret` fields must
        /// never land in the ledger); the triggered tool call gets its own
        /// `ToolStarted` event like any other execution.
        UiActionSubmitted {
            run_id: String,
            ui_id: String,
            revision: u64,
            node_id: String,
            action_id: String,
            nonce: String,
        },
        /// Assistant turn persisted so resume rebuilds messages without a model call.
        AssistantMessage {
            run_id: String,
            message: Message,
        },
        /// Tool result persisted so resume rebuilds messages without re-running tools.
        ToolMessage {
            run_id: String,
            message: Message,
        },
        /// A reasoning trace captured at import time (session migration).
        ///
        /// Foreign agents persist their thinking in transcript files; Pantheon
        /// keeps it durable so an imported session shows its original
        /// deliberation. Read-only: `rebuild_messages` skips it (it is never
        /// sent to the model as a conversation message); the TUI surfaces it
        /// through `rebuild_transcript` as a Thinking block.
        ImportedReasoning {
            run_id: String,
            turn_id: String,
            text: String,
        },
        AgentSpawned {
            run_id: String,
            agent: String,
            /// The child run this delegation created, when the spawn path
            /// knows it: the durable parent→child link transcript clients
            /// use to fetch the child's own detail. `None` on rows written
            /// before the link existed and on spawn paths with no child run
            /// in hand.
            #[serde(default)]
            child_run_id: Option<String>,
            /// The parent tool call that delegated, when the delegation
            /// came through a tool call: keys the link to one step in the
            /// parent transcript.
            #[serde(default)]
            call_id: Option<String>,
        },
        AgentMessage {
            run_id: String,
            agent: String,
        },
        AgentCompleted {
            run_id: String,
            agent: String,
            /// See [`Event::AgentSpawned`]: the child run that finished.
            #[serde(default)]
            child_run_id: Option<String>,
            /// See [`Event::AgentSpawned`]: the delegating parent tool call.
            #[serde(default)]
            call_id: Option<String>,
        },
        MemoryProposed {
            run_id: String,
        },
        ApprovalRequested {
            run_id: String,
            scope: String,
        },
        ApprovalGranted {
            run_id: String,
            scope: String,
        },
        /// A denial is scoped to one approval request. It must not turn the
        /// entire run into a failed run; another tool call may still proceed.
        ApprovalDenied {
            run_id: String,
            scope: String,
        },
        /// Pre-state snapshot recorded when a file-writing tool call parks for
        /// approval. The scope matches the ApprovalRequested scope. On resume,
        /// the runtime re-reads the file and compares hashes; a mismatch means
        /// the file changed between plan and apply, so the grant is stale and
        /// the call is rejected rather than blindly applied.
        PreStateRecorded {
            run_id: String,
            scope: String,
            /// Absolute or relative path of the target file.
            path: String,
            /// SHA-256 hex of the file's contents at park time. Empty string
            /// means the file did not exist yet.
            sha256: String,
        },
        /// The agent asked the operator a question via the `ask_user` tool.
        /// Parks the turn exactly like an approval: the run waits for
        /// [`Event::UserInputProvided`] and resumes with the answer available
        /// as the tool result. Rendered as the clarify card (cyan `?`, never
        /// amber) - visually distinct from approvals.
        UserInputRequested {
            run_id: String,
            call_id: String,
            question: String,
            options: Vec<String>,
        },
        /// The operator's answer to a [`Event::UserInputRequested`]. The host
        /// also appends the answer as the `ask_user` tool result so the
        /// resumed turn sees it in the ledger like any completed tool call.
        UserInputProvided {
            run_id: String,
            call_id: String,
            answer: String,
        },
        /// A decision-layer model was consulted at a specific insertion point.
        DecisionRequested {
            run_id: String,
            point: DecisionPoint,
            model: String,
        },
        /// The decision model produced a typed answer (Choice/Score/Noul).
        DecisionMade {
            run_id: String,
            point: DecisionPoint,
            model: String,
            answer: DecisionAnswerSummary,
        },
        /// The host validated the decision against live state and acted.
        DecisionRecorded {
            run_id: String,
            point: DecisionPoint,
            model: String,
            action: DecisionActionSummary,
        },
        /// The assembled context exceeded the model's window; the host trimmed
        /// it before the provider call (oldest tool rows re-compacted, oldest
        /// exchanges dropped). Ephemeral - ledger history is untouched, only
        /// what the model sees this turn. Persisted so `pantheon logs` shows why
        /// earlier turns are absent from the prompt.
        ContextTrimmed {
            run_id: String,
            /// Estimated input tokens after trimming.
            estimated: u32,
            /// Usable input window the fit targeted.
            window: u32,
            /// Rows dropped (oldest whole exchanges, oldest first).
            dropped_rows: u32,
            /// Tool rows re-compacted to the floor.
            compacted_rows: u32,
        },
        /// The compression aux model summarized the oldest exchanges during an
        /// overflow-triggered fit. The summary row is ephemeral prompt state
        /// (memory tier); ledger history is untouched. Emitted even when the
        /// subsequent deterministic fit still has to drop rows.
        ContextCompressed {
            run_id: String,
            /// The compression aux model (identifier for the audit trail).
            model: String,
            /// Exchanges absorbed into the summary.
            exchanges: u32,
            /// Rows removed (replaced by one summary row).
            rows: u32,
            /// Rendered transcript size in chars.
            chars_before: u32,
            /// Summary row size in chars.
            chars_after: u32,
            /// Summary text persisted so resumed turns reuse it.
            #[serde(default)]
            summary: String,
        },
        /// The title-gen auxiliary (or the deterministic fallback) named this
        /// conversation from its first user prompt. The latest `SessionTitled`
        /// event is the run's display title - a later `/name` (manual) or a
        /// successful model call simply overwrites the previous one. Cosmetic:
        /// never affects execution, replay, or the transcript.
        SessionTitled {
            run_id: String,
            /// The bounded single-line title (at most TITLE_MAX_CHARS).
            title: String,
            /// Which model produced it (identifier for the audit trail);
            /// "deterministic" for the prompt-derived fallback.
            model: String,
            /// "model" when the aux call succeeded, "fallback" when the host
            /// derived the title locally from the first prompt.
            source: String,
        },
        /// One model call's token/cost accounting, persisted. `ModelEvent::Usage`
        /// used to be dropped by `to_event`, so historical spend was invisible;
        /// this event makes the event ledger the source of truth for
        /// `pantheon stats`. The model/provider are captured from the most
        /// recent `ModelRequested`/`Attempt` seen by the persisting sink.
        UsageRecorded {
            run_id: String,
            /// Model identifier, e.g. `anthropic:claude-opus-4-6` (provider
            /// prefix when known); "unknown" when the sink saw no request.
            model: String,
            /// Provider name, e.g. `anthropic`; empty when unknown.
            provider: String,
            input_tokens: u64,
            output_tokens: u64,
            total_tokens: u64,
            /// USD cost for this call, when the catalog priced it.
            cost_usd: Option<f64>,
        },
        /// Browser narration: what the agent (or dashboard take-control) just
        /// did in a browser session. Powers the live "Tapping... / Typing... /
        /// Opening example.com..." subtitle in the app's browser screen.
        ///
        /// `action` is the canonical command (`navigate`, `snapshot`,
        /// `click`, `type`, `press`, `scroll`, `tap`, `back`, ...); `detail`
        /// is display-safe by construction - a host for `navigate`, a ref or
        /// key for `click`/`press`, and never typed text (it may contain
        /// secrets). The dashboard's take-control path uses an empty `run_id`
        /// (no run context there); consumers key on `session`.
        BrowserActivity {
            run_id: String,
            session: String,
            action: String,
            detail: String,
        },
        /// A scheduled task's run failed. Emitted by the scheduler's
        /// outcome sink - the durable, user-visible alert for a failed
        /// scheduled run. `error` is the failure detail; `detail` carries
        /// what was found and done (investigation summary when self-heal
        /// ran, otherwise a note that self-heal is disabled or was skipped).
        ScheduledTaskFailed {
            run_id: String,
            job_id: String,
            error: String,
            detail: String,
        },
        /// A scheduled task recovered: the self-heal investigation fixed the
        /// cause and the single retry run succeeded.
        ScheduledTaskRecovered {
            run_id: String,
            job_id: String,
            detail: String,
        },
    }
    impl Event {
        /// A copy of this event addressed to a different run.
        ///
        /// Used by session fork: the forked run's history is the source run's
        /// event prefix verbatim, re-addressed. Every field except `run_id`
        /// is preserved, so replay, metrics, and resume see the fork as if
        /// the conversation had always lived there.
        pub fn with_run_id(&self, run_id: &str) -> Event {
            match self.clone() {
                Event::RunStarted { run_id: _ } => Event::RunStarted {
                    run_id: run_id.to_string(),
                },
                Event::RunProgress { run_id: _, detail } => Event::RunProgress {
                    run_id: run_id.to_string(),
                    detail,
                },
                Event::RunCompleted { run_id: _ } => Event::RunCompleted {
                    run_id: run_id.to_string(),
                },
                Event::RunFailed { run_id: _, code } => Event::RunFailed {
                    run_id: run_id.to_string(),
                    code,
                },
                Event::RunCanceled { run_id: _, reason } => Event::RunCanceled {
                    run_id: run_id.to_string(),
                    reason,
                },
                Event::RunRecovered { run_id: _ } => Event::RunRecovered {
                    run_id: run_id.to_string(),
                },
                Event::AgentBound {
                    run_id: _,
                    agent_id,
                    profile,
                } => Event::AgentBound {
                    run_id: run_id.to_string(),
                    agent_id,
                    profile,
                },
                Event::TurnStarted { run_id: _, turn_id } => Event::TurnStarted {
                    run_id: run_id.to_string(),
                    turn_id,
                },
                Event::UserMessage { run_id: _, text } => Event::UserMessage {
                    run_id: run_id.to_string(),
                    text,
                },
                Event::TurnParked {
                    run_id: _,
                    turn_id,
                    reason,
                } => Event::TurnParked {
                    run_id: run_id.to_string(),
                    turn_id,
                    reason,
                },
                Event::TurnCompleted {
                    run_id: _,
                    turn_id,
                    outcome,
                } => Event::TurnCompleted {
                    run_id: run_id.to_string(),
                    turn_id,
                    outcome,
                },
                Event::TurnFailed {
                    run_id: _,
                    turn_id,
                    code,
                } => Event::TurnFailed {
                    run_id: run_id.to_string(),
                    turn_id,
                    code,
                },
                Event::TurnRewound { run_id: _, turn_id } => Event::TurnRewound {
                    run_id: run_id.to_string(),
                    turn_id,
                },
                Event::CheckpointCreated {
                    run_id: _,
                    turn_id,
                    name,
                } => Event::CheckpointCreated {
                    run_id: run_id.to_string(),
                    turn_id,
                    name,
                },
                Event::SteeringProvided { run_id: _, text } => Event::SteeringProvided {
                    run_id: run_id.to_string(),
                    text,
                },
                Event::AgentMessageDelivered {
                    run_id: _,
                    message_id,
                    sender,
                    kind,
                    task_id,
                    correlation_id,
                    content,
                } => Event::AgentMessageDelivered {
                    run_id: run_id.to_string(),
                    message_id,
                    sender,
                    kind,
                    task_id,
                    correlation_id,
                    content,
                },
                Event::ModelRequested { run_id: _, model } => Event::ModelRequested {
                    run_id: run_id.to_string(),
                    model,
                },
                Event::ModelDelta { run_id: _, delta } => Event::ModelDelta {
                    run_id: run_id.to_string(),
                    delta,
                },
                Event::ModelCompleted { run_id: _ } => Event::ModelCompleted {
                    run_id: run_id.to_string(),
                },
                Event::ToolRequested { run_id: _, tool } => Event::ToolRequested {
                    run_id: run_id.to_string(),
                    tool,
                },
                Event::ToolStarted {
                    run_id: _,
                    call_id,
                    tool,
                    args,
                    provenance,
                } => Event::ToolStarted {
                    run_id: run_id.to_string(),
                    call_id,
                    tool,
                    args,
                    provenance,
                },
                Event::ToolOutput {
                    run_id: _,
                    call_id,
                    tool,
                    truncated,
                    provenance,
                } => Event::ToolOutput {
                    run_id: run_id.to_string(),
                    call_id,
                    tool,
                    truncated,
                    provenance,
                },
                Event::ToolCompleted {
                    run_id: _,
                    call_id,
                    tool,
                    provenance,
                } => Event::ToolCompleted {
                    run_id: run_id.to_string(),
                    call_id,
                    tool,
                    provenance,
                },
                Event::ToolInterrupted {
                    run_id: _,
                    call_id,
                    tool,
                    args,
                    output_seen,
                    result_seen,
                    detail,
                } => Event::ToolInterrupted {
                    run_id: run_id.to_string(),
                    call_id,
                    tool,
                    args,
                    output_seen,
                    result_seen,
                    detail,
                },
                Event::TodosUpdated { run_id: _, items } => Event::TodosUpdated {
                    run_id: run_id.to_string(),
                    items,
                },
                Event::UiContent {
                    run_id: _,
                    turn_id,
                    ui_id,
                    document,
                } => Event::UiContent {
                    run_id: run_id.to_string(),
                    turn_id,
                    ui_id,
                    document,
                },
                Event::UiUpdated {
                    run_id: _,
                    ui_id,
                    revision,
                    document,
                } => Event::UiUpdated {
                    run_id: run_id.to_string(),
                    ui_id,
                    revision,
                    document,
                },
                Event::UiActionRequested {
                    run_id: _,
                    ui_id,
                    call_id,
                } => Event::UiActionRequested {
                    run_id: run_id.to_string(),
                    ui_id,
                    call_id,
                },
                Event::UiActionSubmitted {
                    run_id: _,
                    ui_id,
                    revision,
                    node_id,
                    action_id,
                    nonce,
                } => Event::UiActionSubmitted {
                    run_id: run_id.to_string(),
                    ui_id,
                    revision,
                    node_id,
                    action_id,
                    nonce,
                },
                Event::AssistantMessage { run_id: _, message } => Event::AssistantMessage {
                    run_id: run_id.to_string(),
                    message,
                },
                Event::ToolMessage { run_id: _, message } => Event::ToolMessage {
                    run_id: run_id.to_string(),
                    message,
                },
                Event::ImportedReasoning {
                    run_id: _,
                    turn_id,
                    text,
                } => Event::ImportedReasoning {
                    run_id: run_id.to_string(),
                    turn_id,
                    text,
                },
                Event::AgentSpawned {
                    run_id: _,
                    agent,
                    child_run_id,
                    call_id,
                } => Event::AgentSpawned {
                    run_id: run_id.to_string(),
                    agent,
                    child_run_id,
                    call_id,
                },
                Event::AgentMessage { run_id: _, agent } => Event::AgentMessage {
                    run_id: run_id.to_string(),
                    agent,
                },
                Event::AgentCompleted {
                    run_id: _,
                    agent,
                    child_run_id,
                    call_id,
                } => Event::AgentCompleted {
                    run_id: run_id.to_string(),
                    agent,
                    child_run_id,
                    call_id,
                },
                Event::MemoryProposed { run_id: _ } => Event::MemoryProposed {
                    run_id: run_id.to_string(),
                },
                Event::ApprovalRequested { run_id: _, scope } => Event::ApprovalRequested {
                    run_id: run_id.to_string(),
                    scope,
                },
                Event::ApprovalGranted { run_id: _, scope } => Event::ApprovalGranted {
                    run_id: run_id.to_string(),
                    scope,
                },
                Event::ApprovalDenied { run_id: _, scope } => Event::ApprovalDenied {
                    run_id: run_id.to_string(),
                    scope,
                },
                Event::PreStateRecorded {
                    run_id: _,
                    scope,
                    path,
                    sha256,
                } => Event::PreStateRecorded {
                    run_id: run_id.to_string(),
                    scope,
                    path,
                    sha256,
                },
                Event::UserInputRequested {
                    run_id: _,
                    call_id,
                    question,
                    options,
                } => Event::UserInputRequested {
                    run_id: run_id.to_string(),
                    call_id,
                    question,
                    options,
                },
                Event::UserInputProvided {
                    run_id: _,
                    call_id,
                    answer,
                } => Event::UserInputProvided {
                    run_id: run_id.to_string(),
                    call_id,
                    answer,
                },
                Event::DecisionRequested {
                    run_id: _,
                    point,
                    model,
                } => Event::DecisionRequested {
                    run_id: run_id.to_string(),
                    point,
                    model,
                },
                Event::DecisionMade {
                    run_id: _,
                    point,
                    model,
                    answer,
                } => Event::DecisionMade {
                    run_id: run_id.to_string(),
                    point,
                    model,
                    answer,
                },
                Event::DecisionRecorded {
                    run_id: _,
                    point,
                    model,
                    action,
                } => Event::DecisionRecorded {
                    run_id: run_id.to_string(),
                    point,
                    model,
                    action,
                },
                Event::ContextTrimmed {
                    run_id: _,
                    estimated,
                    window,
                    dropped_rows,
                    compacted_rows,
                } => Event::ContextTrimmed {
                    run_id: run_id.to_string(),
                    estimated,
                    window,
                    dropped_rows,
                    compacted_rows,
                },
                Event::ContextCompressed {
                    run_id: _,
                    model,
                    exchanges,
                    rows,
                    chars_before,
                    chars_after,
                    summary,
                } => Event::ContextCompressed {
                    run_id: run_id.to_string(),
                    model,
                    exchanges,
                    rows,
                    chars_before,
                    chars_after,
                    summary,
                },
                Event::SessionTitled {
                    run_id: _,
                    title,
                    model,
                    source,
                } => Event::SessionTitled {
                    run_id: run_id.to_string(),
                    title,
                    model,
                    source,
                },
                Event::UsageRecorded {
                    run_id: _,
                    model,
                    provider,
                    input_tokens,
                    output_tokens,
                    total_tokens,
                    cost_usd,
                } => Event::UsageRecorded {
                    run_id: run_id.to_string(),
                    model,
                    provider,
                    input_tokens,
                    output_tokens,
                    total_tokens,
                    cost_usd,
                },
                Event::BrowserActivity {
                    run_id: _,
                    session,
                    action,
                    detail,
                } => Event::BrowserActivity {
                    run_id: run_id.to_string(),
                    session,
                    action,
                    detail,
                },
                Event::ScheduledTaskFailed {
                    run_id: _,
                    job_id,
                    error,
                    detail,
                } => Event::ScheduledTaskFailed {
                    run_id: run_id.to_string(),
                    job_id,
                    error,
                    detail,
                },
                Event::ScheduledTaskRecovered {
                    run_id: _,
                    job_id,
                    detail,
                } => Event::ScheduledTaskRecovered {
                    run_id: run_id.to_string(),
                    job_id,
                    detail,
                },
            }
        }
    }

    #[cfg(test)]
    mod agent_link_tests {
        use super::*;

        /// Ledger rows written before the link fields existed must keep
        /// deserializing, with the link absent rather than guessed.
        #[test]
        fn legacy_agent_events_deserialize_without_link_fields() {
            let spawned: Event =
                serde_json::from_str(r#"{"AgentSpawned":{"run_id":"parent","agent":"child"}}"#)
                    .expect("legacy AgentSpawned");
            assert!(matches!(
                spawned,
                Event::AgentSpawned {
                    child_run_id: None,
                    call_id: None,
                    ..
                }
            ));
            let completed: Event =
                serde_json::from_str(r#"{"AgentCompleted":{"run_id":"parent","agent":"child"}}"#)
                    .expect("legacy AgentCompleted");
            assert!(matches!(
                completed,
                Event::AgentCompleted {
                    child_run_id: None,
                    call_id: None,
                    ..
                }
            ));
        }

        /// Re-homing an event onto another run (fork) keeps the link: the
        /// forked history is identical, so it still points at the child
        /// whose work it records.
        #[test]
        fn with_run_id_preserves_run_id_on_denied_and_prestate() {
            let denied = Event::ApprovalDenied {
                run_id: "src".into(),
                scope: "s".into(),
            }
            .with_run_id("fork");
            assert!(matches!(
                denied,
                Event::ApprovalDenied { run_id, .. } if run_id == "fork"
            ));
            let pre = Event::PreStateRecorded {
                run_id: "src".into(),
                scope: "s".into(),
                path: "/tmp/x".into(),
                sha256: "abc".into(),
            }
            .with_run_id("fork");
            assert!(matches!(
                pre,
                Event::PreStateRecorded { run_id, .. } if run_id == "fork"
            ));
        }

        #[test]
        fn with_run_id_preserves_link_fields() {
            let e = Event::AgentCompleted {
                run_id: "old".into(),
                agent: "child".into(),
                child_run_id: Some("kid".into()),
                call_id: Some("call_1".into()),
            }
            .with_run_id("parent");
            match e {
                Event::AgentCompleted {
                    run_id,
                    child_run_id,
                    call_id,
                    ..
                } => {
                    assert_eq!(run_id, "parent");
                    assert_eq!(child_run_id.as_deref(), Some("kid"));
                    assert_eq!(call_id.as_deref(), Some("call_1"));
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
    }
}

pub mod message {
    //! Canonical conversation messages (OpenAI wire shape).
    //!
    //! One message type for the whole runtime. Roles: system | user | assistant | tool.
    //! Strict alternation is enforced by the loop, not this type. Tool results ride
    //! `role: "tool"` rows with their tool_call_id, never a synthetic user message.

    use crate::provenance::Provenance;
    use serde::{Deserialize, Serialize};

    /// One conversation message.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Message {
        pub role: Role,
        pub content: String,
        /// Image parts riding alongside `content` (vision). Empty on every row
        /// written before vision existed (`#[serde(default)]`), and skipped on
        /// the wire when empty - so text-only rows serialize byte-identically
        /// to before, and every existing `.content` accessor keeps working.
        /// The base64 payload is stored on the ledger row itself so history
        /// replay re-sends the picture, not a stale disk path.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub images: Vec<ImagePart>,
        /// Present on assistant rows that requested tools.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub tool_calls: Vec<ToolCallRef>,
        /// Present on tool rows; matches the assistant tool_call id.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub tool_call_id: Option<String>,
        /// Structured provenance. `None` on legacy rows (treated as the role's
        /// default tier); set on every row the harness builds from now on.
        /// Skipped on the wire for rows without it, so legacy providers and
        /// fixtures keep working.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub provenance: Option<Provenance>,
        /// Unix millis when the message was created (server time). `None` on
        /// rows written before timestamps existed; omitted on the wire so
        /// legacy consumers never see it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub ts_ms: Option<u64>,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum Role {
        System,
        User,
        Assistant,
        Tool,
    }

    /// A tool request from the model.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ToolCallRef {
        pub id: String,
        pub name: String,
        /// JSON-encoded arguments string (wire shape; parsed by the executor).
        pub arguments: String,
    }

    impl Message {
        pub fn system(content: impl Into<String>) -> Self {
            Self {
                role: Role::System,
                content: content.into(),
                images: vec![],
                tool_calls: vec![],
                tool_call_id: None,
                provenance: None,
                ts_ms: Some(crate::logging::now_ms() as u64),
            }
        }
        pub fn user(content: impl Into<String>) -> Self {
            Self {
                role: Role::User,
                content: content.into(),
                images: vec![],
                tool_calls: vec![],
                tool_call_id: None,
                provenance: None,
                ts_ms: Some(crate::logging::now_ms() as u64),
            }
        }
        pub fn assistant(content: impl Into<String>) -> Self {
            Self {
                role: Role::Assistant,
                content: content.into(),
                images: vec![],
                tool_calls: vec![],
                tool_call_id: None,
                provenance: None,
                ts_ms: Some(crate::logging::now_ms() as u64),
            }
        }
        pub fn assistant_tool_calls(calls: Vec<ToolCallRef>) -> Self {
            Self::assistant_tool_calls_text(calls, String::new())
        }

        /// Assistant tool-calls row carrying the model's accompanying text
        /// (see `TurnOutcome::Tools.text`): the text must reach the next
        /// turn's transcript, not just the live TUI/ledger deltas.
        pub fn assistant_tool_calls_text(calls: Vec<ToolCallRef>, text: String) -> Self {
            Self {
                role: Role::Assistant,
                content: text,
                images: vec![],
                tool_calls: calls,
                tool_call_id: None,
                provenance: None,
                ts_ms: Some(crate::logging::now_ms() as u64),
            }
        }
        pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
            Self {
                role: Role::Tool,
                content: content.into(),
                images: vec![],
                tool_calls: vec![],
                tool_call_id: Some(tool_call_id.into()),
                provenance: None,
                ts_ms: Some(crate::logging::now_ms() as u64),
            }
        }

        /// Attach provenance to this message. Builder-style so call sites read
        /// as `Message::tool(id, out).with_provenance(...)`.
        pub fn with_provenance(mut self, provenance: Provenance) -> Self {
            self.provenance = Some(provenance);
            self
        }

        /// Recalled memory as its own row type. Memory is context, not
        /// instruction, so it never borrows System role: a System row with no
        /// provenance is authoritative by definition, which would let a record
        /// written from untrusted tool output speak with the harness's voice.
        /// A User row carrying Memory-tier provenance gets the provider's
        /// `[provenance: ...]` envelope and is treated as data.
        pub fn recall(content: impl Into<String>, source: impl Into<String>) -> Self {
            Self {
                role: Role::User,
                content: content.into(),
                images: vec![],
                tool_calls: vec![],
                tool_call_id: None,
                provenance: Some(Provenance::memory(source)),
                ts_ms: Some(crate::logging::now_ms() as u64),
            }
        }

        /// Attach image parts to this message. Builder-style:
        /// `Message::user("see this").with_images(parts)`.
        pub fn with_images(mut self, images: Vec<ImagePart>) -> Self {
            self.images = images;
            self
        }

        /// Whether this message carries any image parts.
        pub fn has_images(&self) -> bool {
            !self.images.is_empty()
        }

        /// Text content plus one `[image: <name> (<mime>, <n> bytes)]` note per
        /// image part. For text-only renderers that cannot carry image parts
        /// (compression input, transcript export): the pictures are named, never
        /// silently dropped.
        pub fn text_with_image_notes(&self) -> String {
            if self.images.is_empty() {
                return self.content.clone();
            }
            let mut out = self.content.clone();
            for img in &self.images {
                out.push('\n');
                out.push_str(&img.note());
            }
            out
        }
    }

    /// Hard cap on one image's raw bytes: 10 MiB. Base64 inflates stored rows
    /// by ~4/3 on top of this, and providers cap per-image payloads too - so
    /// anything bigger is rejected at the attach site, not mid-request.
    pub const MAX_IMAGE_BYTES: usize = 25 * 1024 * 1024;
    /// Mimes the wire encoders know how to label. Sniffed from magic bytes,
    /// never trusted from the client.
    pub const ALLOWED_IMAGE_MIMES: &[&str] =
        &["image/png", "image/jpeg", "image/gif", "image/webp"];
    /// Max images on one message: bounds the worst-case request
    /// (10 images x 10 MiB x 4/3 base64). Enforced by the provider chain.
    pub const MAX_IMAGES_PER_MESSAGE: usize = 10;

    /// One image riding a message: base64 payload for the wire.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ImagePart {
        /// Original filename (diagnostic; shown in transcripts).
        pub name: String,
        /// Mime type, sniffed from magic bytes at attach time.
        pub mime: String,
        /// Raw bytes, base64 (standard alphabet). Stored on the ledger row so
        /// history replay re-sends the picture, not a stale disk path.
        pub data: String,
    }

    /// Why an image could not be attached.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ImageError {
        /// Raw bytes exceeded [`MAX_IMAGE_BYTES`]; carries the actual size.
        TooLarge(usize),
        /// Magic bytes did not match any of [`ALLOWED_IMAGE_MIMES`].
        UnsupportedMime,
    }

    impl std::fmt::Display for ImageError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                ImageError::TooLarge(n) => write!(
                    f,
                    "image is {n} bytes, over the {} MiB limit",
                    MAX_IMAGE_BYTES / 1024 / 1024
                ),
                ImageError::UnsupportedMime => write!(
                    f,
                    "not a recognized image (allowed: {})",
                    ALLOWED_IMAGE_MIMES.join(", ")
                ),
            }
        }
    }

    impl ImagePart {
        /// Build from raw bytes: sniffs the mime from magic bytes, enforces
        /// [`MAX_IMAGE_BYTES`], and base64-encodes. The claimed mime is never
        /// trusted - a renamed `.exe` fails the sniff and is rejected loudly.
        pub fn from_bytes(name: &str, bytes: &[u8]) -> Result<Self, ImageError> {
            if bytes.len() > crate::config::limit_or_env("image_bytes", None, MAX_IMAGE_BYTES) {
                return Err(ImageError::TooLarge(bytes.len()));
            }
            let mime = sniff_image_mime(bytes).ok_or(ImageError::UnsupportedMime)?;
            Ok(Self {
                name: name.to_string(),
                mime: mime.to_string(),
                data: b64encode(bytes),
            })
        }

        /// `data:` URL for OpenAI-style `image_url` parts.
        pub fn data_url(&self) -> String {
            format!("data:{};base64,{}", self.mime, self.data)
        }

        /// One-line `[image: <name> (<mime>, <n> bytes)]` note for text-only
        /// renderers (compression input, transcript export, tool rows).
        pub fn note(&self) -> String {
            format!(
                "[image: {} ({}, {} bytes)]",
                self.name,
                self.mime,
                self.data.len() * 3 / 4
            )
        }
    }

    /// Mime from magic bytes. PNG / JPEG / GIF / WEBP only - anything else is
    /// not an image Pantheon will put on the wire.
    fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
            Some("image/png")
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            Some("image/jpeg")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            Some("image/gif")
        } else if bytes.len() >= 12 && bytes[0..4] == *b"RIFF" && bytes[8..12] == *b"WEBP" {
            Some("image/webp")
        } else {
            None
        }
    }

    /// Longest edge, in pixels, an image may have before [`downscale_image`]
    /// shrinks it. Rationale: providers cap per-image payloads and charge per
    /// image token by dimension, so an unbounded phone photo is a cost bug;
    /// 1568 keeps plenty of detail for description/QA. Mirrors the Anthropic
    /// 5 MB-per-image ceiling with headroom after JPEG re-encode.
    pub const VISION_DOWNSCALE_MAX_EDGE: u32 = 1568;
    /// JPEG quality for downscaled re-encodes: visually lossless for
    /// description work, far smaller than a phone's q95+ original.
    const DOWNSCALE_JPEG_QUALITY: u8 = 85;

    /// Shrink `raw` image bytes so the longest edge fits
    /// [`VISION_DOWNSCALE_MAX_EDGE`], returning the re-encoded bytes.
    /// Returns `None` when no work is needed (already small enough) or the
    /// bytes cannot be decoded with the available codecs (JPEG/PNG) - the
    /// caller passes the original bytes through; magic sniffing in
    /// [`ImagePart::from_bytes`] already validated the shape.
    ///
    /// Re-encode is JPEG quality 85, except a PNG source with real
    /// transparency (any pixel alpha < 255), which stays PNG so the alpha
    /// channel survives. GIF/WebP sources are never decoded here (no codec
    /// enabled) and pass through untouched.
    pub fn downscale_image(raw: &[u8]) -> Option<Vec<u8>> {
        let fmt = image::guess_format(raw).ok()?;
        let img = image::load_from_memory(raw).ok()?;
        let (w, h) = (img.width(), img.height());
        let longest = w.max(h);
        if longest <= VISION_DOWNSCALE_MAX_EDGE || longest == 0 {
            return None;
        }
        let scale = VISION_DOWNSCALE_MAX_EDGE as f32 / longest as f32;
        let (nw, nh) = (
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
        );
        let small = img.resize(nw, nh, image::imageops::FilterType::Triangle);
        let keep_png =
            fmt == image::ImageFormat::Png && small.to_rgba8().pixels().any(|p| p[3] < 255);
        let mut out = Vec::new();
        if keep_png {
            small
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .ok()?;
        } else {
            let rgb = small.to_rgb8();
            let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut out,
                DOWNSCALE_JPEG_QUALITY,
            );
            enc.encode_image(&rgb).ok()?;
        }
        Some(out)
    }

    /// Minimal base64 encoder (standard alphabet, padded). Mirrors the decoder
    /// in `pantheon-dashboard`'s uploads; std-only, no new dep.
    pub fn b64encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let n = (chunk[0] as u32) << 16
                | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
                | chunk.get(2).copied().unwrap_or(0) as u32;
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 {
                ALPHABET[(n >> 6) as usize & 63] as char
            } else {
                '='
            });
            out.push(if chunk.len() > 2 {
                ALPHABET[n as usize & 63] as char
            } else {
                '='
            });
        }
        out
    }

    /// Parse a `[attachments]` block (as emitted by the dashboard's
    /// `send_message`) for `image/*` entries and build `ImagePart`s from the
    /// referenced files, so images attached to a chat turn reach the provider
    /// as picture parts instead of dying as file-path text.
    ///
    /// Each entry has the shape `- <name> (<mime>, <size>, id: <upl_...>): <abs path>`.
    /// Only entries whose mime starts with `image/` are considered. Every path
    /// is canonicalized and must stay inside `uploads_dir` - the block is text
    /// the model can influence, so a `../` escape or an absolute path to
    /// anywhere else is skipped, never read. Files that fail to read or sniff
    /// are skipped too: the block text still names them, so the model sees the
    /// filename and can try its file tools - nothing is silently hidden.
    /// Capped at [`MAX_IMAGES_PER_MESSAGE`].
    pub fn attachment_images(text: &str, uploads_dir: &std::path::Path) -> Vec<ImagePart> {
        let uploads_dir = match uploads_dir.canonicalize() {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let mut in_block = false;
        let mut out = Vec::new();
        for line in text.lines() {
            if !in_block {
                if line.trim() == "[attachments]" {
                    in_block = true;
                }
                continue;
            }
            let line = line.trim_end();
            if !line.starts_with("- ") {
                // First non-entry line ends the block (the trailer copy).
                break;
            }
            if out.len()
                >= crate::config::limit_or_env("images_per_message", None, MAX_IMAGES_PER_MESSAGE)
            {
                break;
            }
            // Split `- name (mime, size, id: xxx): /abs/path` on the last "): ".
            let Some(sep) = line.rfind("): ") else {
                continue;
            };
            let head = &line[2..sep];
            let path = line[sep + 3..].trim();
            if !head.contains("(image/") {
                continue;
            }
            let name = head.split(" (").next().unwrap_or("image").trim();
            let name = if name.is_empty() { "image" } else { name };
            // Never trust the path: it must resolve inside the uploads dir.
            let Ok(canon) = std::path::Path::new(path).canonicalize() else {
                continue;
            };
            if !canon.starts_with(&uploads_dir) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&canon) else {
                continue;
            };
            // One shared downscale for every path that sends pixels: a 12 MP
            // phone photo becomes a ~1568px JPEG before base64, never raw.
            let bytes = downscale_image(&bytes).unwrap_or(bytes);
            if let Ok(part) = ImagePart::from_bytes(name, &bytes) {
                out.push(part);
            }
        }
        out
    }

    /// Cap on videos parsed from one `[attachments]` block: videos are
    /// expensive (frames × describe calls), so the block beyond two is left
    /// for the model to open with its file tools.
    pub const MAX_VIDEOS_PER_MESSAGE: usize = 5;

    /// One video referenced by an `[attachments]` block: validated path only,
    /// never bytes - a 25 MiB upload must not be read fully into memory.
    /// Frame extraction (ffmpeg) streams what it needs.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct VideoAttachment {
        /// Original filename (diagnostic; shown in transcripts).
        pub name: String,
        /// Canonicalized absolute path, verified inside `uploads_dir`.
        pub path: std::path::PathBuf,
    }

    /// Parse the dashboard `[attachments]` block (appended by `send_message`)
    /// for `video/*` entries and return their validated paths, so attached
    /// videos reach the video-analysis pass instead of dying as file-path
    /// text.
    ///
    /// Entry shape and trust rules mirror [`attachment_images`]: `- <name>
    /// (<mime>, <size>, id: <upl_...>): <abs path>`, canonicalized, must stay
    /// inside `uploads_dir`, capped at [`MAX_VIDEOS_PER_MESSAGE`]. Bytes are
    /// never read here - extraction happens downstream via ffmpeg.
    pub fn attachment_videos(text: &str, uploads_dir: &std::path::Path) -> Vec<VideoAttachment> {
        let uploads_dir = match uploads_dir.canonicalize() {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        let mut in_block = false;
        let mut out = Vec::new();
        for line in text.lines() {
            if !in_block {
                if line.trim() == "[attachments]" {
                    in_block = true;
                }
                continue;
            }
            let line = line.trim_end();
            if !line.starts_with("- ") {
                break;
            }
            if out.len()
                >= crate::config::limit_or_env("videos_per_message", None, MAX_VIDEOS_PER_MESSAGE)
            {
                break;
            }
            let Some(sep) = line.rfind("): ") else {
                continue;
            };
            let head = &line[2..sep];
            let path = line[sep + 3..].trim();
            if !head.contains("(video/") {
                continue;
            }
            let name = head.split(" (").next().unwrap_or("video").trim();
            let name = if name.is_empty() { "video" } else { name };
            let Ok(canon) = std::path::Path::new(path).canonicalize() else {
                continue;
            };
            if !canon.starts_with(&uploads_dir) {
                continue;
            }
            out.push(VideoAttachment {
                name: name.to_string(),
                path: canon,
            });
        }
        out
    }

    /// Tool schema in OpenAI wire format: {"type":"function","function":{...}}.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ToolSchema {
        pub name: String,
        pub description: String,
        /// JSON Schema for the parameters object.
        pub parameters: serde_json::Value,
    }

    impl ToolSchema {
        pub fn to_wire(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": self.name,
                    "description": self.description,
                    "parameters": self.parameters,
                }
            })
        }
    }
}

pub mod model {
    //! Model policy: NO routing. Default + ordered fallbacks (failure-only,
    //! runtime-controlled) + auxiliary models for scoped capabilities.
    //!
    //! One configured default, a fallback chain, and
    //! auxiliary models for helpers (embeddings, rerank, vision, extraction...).
    //! The agent never selects. The runtime does.
    //!
    //! NOTE: this list is for things that ARE models - an endpoint you call
    //! with a prompt or tensors and get inference back. Service capabilities
    //! (STT, TTS, search, browser) belong to the provider plane: they are
    //! swappable services or local binaries selected by capability, not
    //! entries in the model policy (ARCHITECTURE §14).

    use crate::error::PantheonError;
    use serde::{Deserialize, Serialize};

    /// Which model a run uses. Single configured default.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct DefaultModel {
        pub provider: String,
        pub model: String,
        /// Env var name holding this entry's API key. Only meaningful on
        /// fallback entries: a fallback may be a different provider with
        /// a different key, and this names that key. `None` = resolve via
        /// the provider's own catalog/env credential (the existing
        /// behaviour). The default entry's key travels separately via the
        /// chain's configured key (`[model].api_key_env`), never through
        /// this field, so a fallback can never accidentally inherit the
        /// primary provider's secret - and an absent fallback key_env
        /// resolves per-provider, it does NOT fall back to the default
        /// entry's key value (sending the primary secret to another
        /// provider's endpoint would leak it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub key_env: Option<String>,
    }

    /// Ordered fallback chain. Used ONLY on retryable default-model failure
    /// (unavailable, quota, 5xx, timeout). Never agent-chosen, never scored.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct FallbackChain {
        pub fallbacks: Vec<DefaultModel>,
    }

    impl FallbackChain {
        pub fn next_after(&self, failed_index: Option<usize>) -> Option<(usize, &DefaultModel)> {
            let i = match failed_index {
                Some(i) => i + 1,
                None => 0,
            };
            self.fallbacks.get(i).map(|m| (i, m))
        }
    }

    /// Auxiliary model for a scoped capability (NOT a chat substitute).
    ///
    /// Aids that have a config section (`[judge]`, `[embeddings]`, ...)
    /// resolve to the pinned target; the rest default to `auto` = the run's
    /// default model. Exceptions where `auto` would be wrong are documented
    /// on the variant (embeddings: absent = local embedder, never chat).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum AuxiliaryKind {
        Embeddings,
        /// Vision model (config `[vision]`): describes and answers about
        /// images attached to a turn. Host-orchestrated; never chat. Absent =
        /// `auto`: the run's default model. Consulted by
        /// `Session::vision_aux_pass` before an image-bearing turn reaches the
        /// chat model, and gated by `[tools] vision`.
        Vision,
        /// Video-analysis model (config `[video]`): understands video attached
        /// to a turn - scene description, temporal question answering,
        /// keyframe summarization. Host-orchestrated; never chat. Absent =
        /// `auto`: the run's default model.
        ///
        /// Consulted by `Session::video_aux_pass`: when a turn carries video,
        /// the pass samples keyframes (`extract_frames`, ffmpeg) and calls this
        /// slot, never chat. The `[tools]` `video_analysis` toggle gates it, so
        /// a configured `[video]` cannot be consulted by accident. Long video
        /// goes through the same summarization shape as `[search_synthesis]`
        /// (chunk -> note -> context), so the window never fills with raw
        /// frames.
        Video,
        /// Search-synthesis model (config `[search_synthesis]`): turns raw
        /// web-search results into a short cited brief before they enter
        /// context. Host-orchestrated; never chat. Absent = `auto`: the run's
        /// default model.
        ///
        /// Wired: `Session::durable_tool` consults this slot for `web_search`
        /// results and prepends the cited brief above the raw results. Any
        /// failure keeps the raw results unchanged.
        SearchSynthesis,
        /// Judge model (config `[judge]`): classifies, routes, scores. Never
        /// generates chat. Used for route selection, tool gating, verification
        /// thresholds. Replaces the old `DecisionRouter` name.
        ///
        /// Wired: `Session::build_judge` resolves this slot and
        /// `judge_verdict_for_call` consults it in `PermissionMode::Smart`, so a
        /// configured judge actually adjudicates approval-gated calls. Every
        /// failure path (no entry, unresolvable key, transport error,
        /// unparseable answer) yields `None`, and `PermissionMode::resolve`
        /// turns `None` into a park - an unreachable judge degrades to asking a
        /// human, never to running unattended.
        Judge,
        /// MCP tool-result synthesis model (config `[mcp_synthesis]`): bounds
        /// large MCP tool results into a note before they enter context.
        ///
        /// Wired: `Session::durable_tool` consults this slot for large
        /// tool results (MCP tools register with bare names, so the
        /// trigger is result size, not tool name). Any failure keeps the
        /// raw result.
        McpSynthesis,
        /// Model for scheduled (background) runs (config `[scheduled]`): when a
        /// scheduled job executes an agent turn it uses this model instead of
        /// the interactive default - cheap background runs. `auto` = default.
        Scheduled,
        /// Context-compression model: summarizes the oldest exchanges when the
        /// transcript overflows the window. Host-orchestrated; never chat.
        Compression,
        /// Session-title model: names a conversation from its first user prompt.
        /// Host-orchestrated, fire-and-forget beside the first turn; never chat.
        /// Absent entry = `auto`: the runtime falls back to the default model.
        TitleGen,
        /// Reflection-pass model (config `[reflect]`): the auxiliary model for
        /// LLM-backed reflection steps (proposal enrichment, and any future
        /// model-generated proposers). Host-orchestrated; never chat. Absent =
        /// `auto`: the run's default model. Every reflection LLM call resolves
        /// through this slot so background self-improvement stays cheap and
        /// never borrows the interactive model's context directly.
        Reflection,
        /// Consolidation-pass model (config `[consolidation]`): the auxiliary
        /// model for LLM-backed consolidation steps (candidate distillation
        /// merging near-duplicate staged candidates into single well-phrased
        /// facts). Host-orchestrated; never chat. Absent = `auto`: the run's
        /// default model. Every consolidation LLM call resolves through this
        /// slot so nightly memory consolidation stays cheap and never borrows
        /// the interactive model's context directly.
        Consolidation,
        /// Repair model (config `[repair]`): revises drafts and diagnoses
        /// broken things in the nightly self-improvement loop (eval-reject
        /// and replay-reject draft revision today; broken MCP servers,
        /// scheduled tasks, and tools next). Host-orchestrated; never chat.
        /// Absent = OFF: no policy entry, and fix-loop draft revision is
        /// unavailable. Every fix-loop revision resolves through this slot
        /// (never the Reflection slot), so repair work stays on a model
        /// pinned for the job.
        Repair,
        /// Structured-extraction model (config `[extraction]`): pulls fields
        /// and records out of prose and tool outputs (dates, amounts, names,
        /// entities) into typed values the runtime can act on.
        /// Host-orchestrated; never chat. Absent = `auto`: the run's default
        /// model.
        ///
        /// Wired: `Session::durable_tool` consults this slot for large tool
        /// results and appends the extracted fields. Any failure keeps the
        /// raw result.
        Extraction,
        /// Rerank model (config `[rerank]`): scores and orders search and
        /// memory-retrieval candidates before they enter context, so the best
        /// evidence wins the window instead of the loudest keyword match.
        /// Host-orchestrated; never chat. Absent = `auto`: the run's default
        /// model.
        ///
        /// Wired: `Session::durable_tool` consults this slot for
        /// `session_search` and `vault_search` results and reorders the
        /// hit blocks. Any failure keeps the retrieval order.
        Rerank,
        /// Planner model (config `[planner]`): decomposes a goal into a task
        /// plan for a future planner/worker split, where planning and execution
        /// run on different models. Host-orchestrated; never chat. Absent =
        /// `auto`: the run's default model.
        ///
        /// Wired: `Session::chat_turn` resolves this slot at turn start
        /// when a session goal is active, and rides the plan steps in
        /// the system prompt. Unconfigured or on error there is no plan
        /// block, never a fabricated one.
        Planner,
        /// Adversarial verifier model (config `[verify]`): after a delegated
        /// sub-agent completes, it takes the task's goal plus the child's
        /// claimed result, assumes the goal was missed, and tries to falsify
        /// the claim from the evidence. Fail-closed: a falsified claim errors
        /// the delegation, an inconclusive one is marked unverified - neither
        /// counts as done. Host-orchestrated; never chat. Absent = the slot is
        /// OFF entirely (no entry in the policy), so verification only runs
        /// when the operator explicitly pins a cheap model here.
        Verify,
        Other(String),
    }

    impl AuxiliaryKind {
        /// Built-in per-capability request timeout in seconds, used when the
        /// aux section sets no explicit `timeout`. Matches the historical
        /// client timeouts so an unconfigured slot behaves exactly as before.
        pub fn default_timeout_secs(&self) -> u64 {
            match self {
                AuxiliaryKind::TitleGen => 10,
                AuxiliaryKind::Judge => 10,
                AuxiliaryKind::Embeddings => 15,
                AuxiliaryKind::Compression => 30,
                AuxiliaryKind::Verify => 30,
                AuxiliaryKind::Reflection => 60,
                AuxiliaryKind::Consolidation => 60,
                AuxiliaryKind::Repair => 60,
                _ => 120,
            }
        }
    }

    /// The kind of decision being routed through a Judge aux model.
    /// Each maps to a narrow insertion point in the host code.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum DecisionPoint {
        /// Before a chat turn: which provider/model to use for this request.
        RouteSelect,
        /// Before a tool executes: score risk + decide allow/deny/approve.
        ToolGate,
        /// After a stage completes: verify output quality.
        TaskVerify,
        /// Before spawning a sub-agent: pick the right specialist.
        DelegateSelect,
        /// Custom decision point.
        Other(String),
    }

    /// A decision model's typed output. Never free text.
    ///
    /// ADVISORY-ONLY CONTRACT (locked): the decision model proposes, the host
    /// validates against live state and enforces. Specifically:
    /// - Route answers may only select from the caller-supplied `choices`
    ///   (the configured default + ordered fallbacks). Anything else is
    ///   recorded as `Overridden` and ignored.
    /// - Gate answers may only ESCALATE: Allow -> Approval/Deny is honored;
    ///   Deny -> Allow is never honored. The deterministic `Policy` is the
    ///   floor; the classifier can raise the bar, never lower it.
    /// - Confidence is a routing/escalation signal, never a permission.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub enum DecisionAnswer {
        /// A model/provider identifier for route selection.
        /// Must match one entry of the caller's `choices`; else overridden.
        Route { choice: String, confidence: f32 },
        /// A risk score and gate verdict for tool gating.
        Gate {
            verdict: GateVerdict,
            confidence: f32,
            score: f32,
        },
        /// A binary accept/reject with confidence.
        Binary { accepted: bool, confidence: f32 },
        /// A numeric threshold check.
        Threshold { passed: bool, value: f32 },
    }

    impl DecisionAnswer {
        /// Validate a route choice against the allowed set (default + fallbacks
        /// supplied as `choices` by the caller). Returns the choice if allowed.
        pub fn validated_route(&self, allowed: &[String]) -> Option<String> {
            match self {
                DecisionAnswer::Route { choice, .. } => {
                    if allowed.iter().any(|a| a == choice) {
                        Some(choice.clone())
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    }

    /// What the gate decided for a tool call.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub enum GateVerdict {
        Allow,
        Deny { reason: String },
        NeedsApproval { reason: String },
    }

    impl GateVerdict {
        /// Escalation order: Allow < NeedsApproval < Deny.
        /// The classifier may only move UP this ladder relative to the
        /// deterministic host policy. Moving down is dropped by the host.
        pub fn escalation_level(&self) -> u8 {
            match self {
                GateVerdict::Allow => 0,
                GateVerdict::NeedsApproval { .. } => 1,
                GateVerdict::Deny { .. } => 2,
            }
        }
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AuxiliaryModel {
        pub kind: AuxiliaryKind,
        pub provider: String,
        pub model: String,
        /// Per-aux request timeout in seconds. Resolved from the aux
        /// section's `timeout_secs` (default 120); call sites apply it to
        /// the aux HTTP transport.
        pub timeout_secs: u64,
        /// Compression only: summary target as a percentage (1-100) of the
        /// absorbed transcript chars (`[compression] target_percent`).
        /// `None` = the historic default (≈12%). Ignored by every other
        /// auxiliary kind.
        #[serde(default)]
        pub target_percent: Option<u8>,
    }

    /// Full model config for a run/policy.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ModelPolicy {
        pub default: DefaultModel,
        pub fallbacks: FallbackChain,
        pub auxiliaries: Vec<AuxiliaryModel>,
        /// Reasoning effort for chat turns. `Off` sends no effort param and
        /// every provider behaves exactly as before. Applies to the default
        /// and to fallback attempts alike; aux turns (title, judge,
        /// compression) always run `Off` so helpers stay fast and cheap.
        /// `#[serde(default)]` keeps configs written before this existed
        /// loading as `Off`.
        #[serde(default)]
        pub reasoning: ReasoningLevel,
        /// Exact thinking budget in tokens, overriding the level mapping on
        /// budget wires (Anthropic). Ignored on effort-string wires (OpenAI).
        /// `None` (the default) means the level mapping decides; `Some(0)`
        /// disables thinking entirely, even with a level set.
        #[serde(default)]
        pub reasoning_budget: Option<u32>,
    }

    /// Reasoning effort for one turn. Levels are portable names; each wire
    /// mode maps them to its own param at request time (OpenAI:
    /// `reasoning_effort`, including GPT-5's `minimal` and Codex's `xhigh`;
    /// Anthropic: a thinking budget). For exact control, pair a level with
    /// `ModelPolicy::reasoning_budget`, which overrides the mapped budget on
    /// budget wires and is ignored elsewhere. Endpoints that do not
    /// understand the param ignore or reject it - setting effort is an
    /// explicit request, and a rejection surfaces as a structured provider
    /// error naming the endpoint.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum ReasoningLevel {
        /// No effort param sent. The provider default. Always safe.
        #[default]
        Off,
        /// GPT-5's lightest setting. Budget wires floor it at their minimum.
        Minimal,
        Low,
        Medium,
        High,
        /// Codex's top setting, sent verbatim on effort-string wires.
        Xhigh,
        /// As much as the window allows (budget wires: max_tokens − 1).
        /// Sent verbatim as `"max"` on effort-string wires; endpoints that
        /// do not know it reject with a structured error.
        Max,
    }

    impl ReasoningLevel {
        /// Parse a config/CLI spelling. Accepts case-insensitive `off`
        /// (`none`), `minimal` (`min`), `low`, `medium` (`med`), `high`,
        /// `xhigh` (`xhi`), `max`; anything else is `None`. Named `parse`,
        /// not `from_str`,
        /// so it can never be confused with the `FromStr` trait method.
        pub fn parse(s: &str) -> Option<Self> {
            match s.trim().to_ascii_lowercase().as_str() {
                "off" | "none" => Some(Self::Off),
                "minimal" | "min" => Some(Self::Minimal),
                "low" => Some(Self::Low),
                "medium" | "med" => Some(Self::Medium),
                "high" => Some(Self::High),
                "xhigh" | "xhi" => Some(Self::Xhigh),
                "max" => Some(Self::Max),
                _ => None,
            }
        }

        pub fn as_str(&self) -> &'static str {
            match self {
                Self::Off => "off",
                Self::Minimal => "minimal",
                Self::Low => "low",
                Self::Medium => "medium",
                Self::High => "high",
                Self::Xhigh => "xhigh",
                Self::Max => "max",
            }
        }
    }

    impl ModelPolicy {
        pub fn auxiliary(&self, kind: &AuxiliaryKind) -> Option<&AuxiliaryModel> {
            self.auxiliaries.iter().find(|a| &a.kind == kind)
        }
    }

    /// What the host tells the decision model: enough context to make the call.
    /// The model returns a Choice/Score/Noul typed answer, not free text.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct DecisionRequest {
        pub run_id: String,
        pub point: DecisionPoint,
        /// The specific question for this decision point.
        pub query: String,
        /// Options the model can choose from, when the point is a choice problem.
        /// For scoring/gate decisions, this carries the items being scored.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub choices: Vec<String>,
        /// Optional context: the transcript prefix, tool args, etc.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub context: Option<String>,
    }

    /// Host-side interface for the judge model. Provider-agnostic:
    /// implementations may talk to any typed classifier (hosted API, local
    /// endpoint, small specialized model) - the host picks one via the
    /// `Judge` auxiliary and validates every answer against live state
    /// before acting.
    pub trait Judge: Send + Sync {
        /// Returns the model identifier for logging/audit trail.
        fn model_name(&self) -> &str {
            "aux-judge-model"
        }

        /// Ask the judge a typed question. Returns a typed answer,
        /// never free text.
        fn decide(&self, req: &DecisionRequest) -> Result<DecisionAnswer, PantheonError>;
    }

    /// What the host asks a compression model to do: summarize `transcript`
    /// (the oldest exchanges, pre-rendered and row-capped by the host) into a
    /// handoff note of roughly `target_chars`.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CompressionRequest {
        pub run_id: String,
        /// Pre-rendered oldest exchanges, role-tagged, per-row capped.
        pub transcript: String,
        /// Soft cap for the summary length in chars. The host hard-bounds the
        /// result regardless of what the model returns.
        pub target_chars: usize,
    }

    /// A compression model's output: the summary text, nothing else.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct CompressionResult {
        pub summary: String,
    }

    /// Host-side interface for a context-compression model. Provider-agnostic
    /// like `Judge`: the host renders the transcript, bounds the
    /// summary, and falls back to deterministic dropping when this errors.
    pub trait ContextCompressor: Send + Sync {
        /// Model identifier for logging/audit.
        fn model_name(&self) -> &str {
            "aux-compression-model"
        }

        /// Summarize the transcript. On `Err` the host proceeds with the
        /// deterministic fit - compression is an optimization, never a
        /// requirement for correctness.
        fn compress(&self, req: &CompressionRequest) -> Result<CompressionResult, PantheonError>;
    }

    /// Soft cap for a session title. Models overshoot; the host truncates at a
    /// char boundary so list rows stay readable in the CLI, TUI, and gateways.
    pub const TITLE_MAX_CHARS: usize = 60;

    /// What the host asks a title model to do: name the conversation whose
    /// first user prompt is `prompt`. One shot, no transcript - a title is
    /// derived from the opening message, not the whole exchange.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TitleRequest {
        pub run_id: String,
        /// The first user prompt of the session (host-capped before sending).
        pub prompt: String,
    }

    /// A title model's output: the title text, nothing else.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TitleResult {
        pub title: String,
    }

    /// Host-side interface for the session-title model. Provider-agnostic like
    /// `ContextCompressor`: the host picks the target (config `[title_gen]`,
    /// else `auto` = the run's default model), bounds the output, and falls
    /// back to [`fallback_title`] when this errors. Titles are cosmetic - a
    /// failed call never affects the conversation.
    pub trait TitleGenerator: Send + Sync {
        /// Model identifier for logging/audit.
        fn model_name(&self) -> &str {
            "aux-title-model"
        }

        /// Name the session. On `Err` the host derives a deterministic title
        /// from the prompt instead.
        fn title(&self, req: &TitleRequest) -> Result<TitleResult, PantheonError>;
    }

    /// Normalize any raw text (a model's reply, or the first prompt itself in
    /// the fallback path) into a single-line title: first non-empty line,
    /// wrapping quotes stripped, a leading `Title:` label dropped, whitespace
    /// collapsed, hard-truncated at `max_chars` on a char boundary.
    /// Returns `""` when nothing usable remains.
    pub fn bound_title(raw: &str, max_chars: usize) -> String {
        let mut s = raw
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string();
        // Strip one layer of wrapping quotes (ASCII or curly).
        for (a, b) in [('"', '"'), ('"', '"'), ('\'', '\'')] {
            if s.chars().count() >= 2 && s.starts_with(a) && s.ends_with(b) {
                let inner = s.strip_prefix(a).and_then(|t| t.strip_suffix(b));
                if let Some(inner) = inner {
                    s = inner.trim().to_string();
                }
                break;
            }
        }
        // Models love to answer "Title: ..." despite instructions.
        for label in ["title:", "session title:", "name:"] {
            if let Some(head) = s.get(..label.len()) {
                if head.eq_ignore_ascii_case(label) {
                    s = s[label.len()..].trim_start().to_string();
                    break;
                }
            }
        }
        // Collapse remaining whitespace runs (including any inner newlines).
        let mut out = String::with_capacity(s.len());
        let mut prev_ws = false;
        for c in s.chars() {
            if c.is_whitespace() {
                if !prev_ws {
                    out.push(' ');
                }
                prev_ws = true;
            } else {
                out.push(c);
                prev_ws = false;
            }
        }
        let out = out.trim();
        if out.chars().count() <= max_chars {
            return out.to_string();
        }
        // Truncate on a char boundary at `max_chars` characters (not bytes
        // a byte cap would butcher multibyte scripts).
        let end = out
            .char_indices()
            .nth(max_chars)
            .map(|(i, _)| i)
            .unwrap_or(out.len());
        out[..end].trim_end().to_string()
    }

    /// Deterministic title for a session: the first prompt, bounded. This is
    /// the fallback when the title aux model is absent, errors, or times out
    /// history always shows something meaningful, and a later successful call
    /// can replace it (last title wins).
    pub fn fallback_title(prompt: &str) -> String {
        bound_title(prompt, TITLE_MAX_CHARS)
    }
}

pub mod provenance {
    //! Provenance and trust tiers for untrusted-data boundaries.
    //!
    //! Every piece of content that reaches the model carries a trust tier
    //! describing where it came from. The tier is structural data on the
    //! `Message`, not a prompt convention: it survives serialization into the
    //! ledger, and providers render it as a provenance envelope so the model
    //! can tell instructions apart from fetched data.
    //!
    //! The core invariant: content never gains trust by being copied.
    //! `derived_trust <= source_trust` unless an explicit trusted actor
    //! (the user, via CLI or an approval) promotes it.

    use serde::{Deserialize, Serialize};

    /// Where content came from, coarse-grained for policy decisions.
    /// Fine distinctions (which tool, which URL) live on `Provenance.source`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum TrustTier {
        /// Authoritative control: system prompts, harness-generated denials,
        /// recovery scaffolding. The model must obey it.
        System,
        /// Direct instructions from the operator. The model must obey it.
        User,
        /// Recalled context. Informative, never authoritative: the model may
        /// use it as background but must not treat it as an instruction.
        Memory,
        /// Anything that originated outside Pantheon's trust boundary: tool
        /// output, fetched pages, plugin context. Data to analyze, never
        /// instructions to follow. The `source` string carries the finer
        /// distinction (local calculator vs remote web page).
        Untrusted,
    }

    impl TrustTier {
        pub fn as_str(&self) -> &'static str {
            match self {
                TrustTier::System => "system",
                TrustTier::User => "user",
                TrustTier::Memory => "memory",
                TrustTier::Untrusted => "untrusted",
            }
        }

        pub fn parse(s: &str) -> Option<Self> {
            match s.trim() {
                "system" => Some(TrustTier::System),
                "user" => Some(TrustTier::User),
                "memory" => Some(TrustTier::Memory),
                "untrusted" => Some(TrustTier::Untrusted),
                _ => None,
            }
        }

        /// Ordering for the `derived_trust <= source_trust` invariant.
        /// System(3) > User(2) > Memory(1) > Untrusted(0).
        pub fn rank(&self) -> u8 {
            match self {
                TrustTier::Untrusted => 0,
                TrustTier::Memory => 1,
                TrustTier::User => 2,
                TrustTier::System => 3,
            }
        }
    }

    /// Structured provenance attached to a message or memory record.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct Provenance {
        pub trust: TrustTier,
        /// Fine-grained origin: tool name (`shell`, `web_fetch`), `user`,
        /// `memory:<namespace>`, `cli`, `import`, `pantheon` (harness-generated).
        pub source: String,
        /// Ledger seq of the originating event, when known. Lets an auditor
        /// trace a memory record back to the exact tool output it derived from.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub origin_seq: Option<u64>,
    }

    impl Provenance {
        pub fn system(source: impl Into<String>) -> Self {
            Self {
                trust: TrustTier::System,
                source: source.into(),
                origin_seq: None,
            }
        }

        pub fn user(source: impl Into<String>) -> Self {
            Self {
                trust: TrustTier::User,
                source: source.into(),
                origin_seq: None,
            }
        }

        pub fn untrusted(source: impl Into<String>) -> Self {
            Self {
                trust: TrustTier::Untrusted,
                source: source.into(),
                origin_seq: None,
            }
        }

        /// Recalled or derived context (memory writes, compressed history):
        /// informative, never authoritative.
        pub fn memory(source: impl Into<String>) -> Self {
            Self {
                trust: TrustTier::Memory,
                source: source.into(),
                origin_seq: None,
            }
        }

        /// Render the envelope prefix providers put in front of untrusted and
        /// memory-tier content so the model sees provenance inline.
        pub fn envelope_prefix(&self) -> String {
            format!(
                "[provenance: source={} trust={}]",
                self.source,
                self.trust.as_str()
            )
        }
    }
}

pub mod temporal {
    //! Tacit temporal awareness: time, but only when it matters.
    //!
    //! The model is never timestamped per message. Before a turn's first model
    //! call, the pipeline measures how long the conversation has been idle
    //! from the durable ledger, so the reading is restart-safe by construction
    //! and when the gap is meaningful it appends one coarse, human-friendly
    //! hint to the outgoing user message. The hint is ephemeral: it rides the
    //! API call only, is never written to the ledger or the transcript, rides
    //! the user turn (never the system prompt, so prompt caching is unaffected),
    //! and gets coarser as the gap grows.
    //!
    //! All decision logic here is pure ([`temporal_hint`]): hand it the last
    //! assistant turn's ledger timestamp, now, a timezone, and the config, and
    //! it returns the hint or `None`. The pipeline hook in pantheon-runtime
    //! only wires ledger reads around it and fails open.

    use chrono::TimeZone;
    use chrono_tz::Tz;
    use serde::{Deserialize, Serialize};

    fn default_true() -> bool {
        true
    }

    fn default_min_gap_secs() -> u64 {
        7200
    }

    /// `[temporal]` behavior knobs. Zero tokens by construction - the hint is
    /// pure string injection - so the feature defaults to on.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct TemporalConfig {
        /// Master switch. Default true.
        #[serde(default = "default_true")]
        pub enabled: bool,
        /// Idle seconds before an elapsed-gap hint fires. Default 7200 (2h).
        /// `0` disables the elapsed-gap trigger; the date-rollover trigger
        /// still works.
        #[serde(default = "default_min_gap_secs")]
        pub min_gap_secs: u64,
        /// Fire a hint when the local calendar date rolled over since the
        /// last turn, even on a short gap. Default true.
        #[serde(default = "default_true")]
        pub notify_date_change: bool,
        /// IANA timezone name, e.g. `"Africa/Lagos"`. Absent = the system
        /// local timezone; unparseable = system local, then UTC. Never fails.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub timezone: Option<String>,
    }

    impl Default for TemporalConfig {
        fn default() -> Self {
            Self {
                enabled: true,
                min_gap_secs: 7200,
                notify_date_change: true,
                timezone: None,
            }
        }
    }

    /// Resolve the effective timezone: the explicit config name wins, then
    /// the OS local zone, then UTC. Total - never fails, never panics.
    pub fn resolve_tz(cfg: &TemporalConfig) -> Tz {
        if let Some(name) = cfg.timezone.as_deref() {
            if let Ok(tz) = name.parse::<Tz>() {
                return tz;
            }
        }
        if let Ok(name) = iana_time_zone::get_timezone() {
            if let Ok(tz) = name.parse::<Tz>() {
                return tz;
            }
        }
        Tz::UTC
    }

    /// Coarse inner sentence for an elapsed gap, e.g.
    /// `"about 5 hours have passed"`. Deliberately imprecise - never exact
    /// wall-clock time, and coarser as the gap grows.
    fn gap_sentence(gap_secs: i64) -> String {
        const MIN: i64 = 60;
        const HOUR: i64 = 3600;
        const DAY: i64 = 86400;
        if gap_secs < 90 * MIN {
            // Only reachable when the operator lowers `min_gap_secs`; with
            // the default the elapsed trigger already implies hours or more.
            let m = (gap_secs as f64 / MIN as f64).round().max(1.0) as i64;
            format!("about {m} minutes have passed")
        } else if gap_secs < DAY {
            let h = (gap_secs as f64 / HOUR as f64).round().max(1.0) as i64;
            format!("about {h} hours have passed")
        } else if gap_secs < 2 * DAY {
            "about a day has passed".to_string()
        } else if gap_secs < 7 * DAY {
            let d = (gap_secs as f64 / DAY as f64).round().max(2.0) as i64;
            format!("about {d} days have passed")
        } else if gap_secs < 14 * DAY {
            "about a week has passed".to_string()
        } else if gap_secs < 30 * DAY {
            let w = (gap_secs as f64 / (7 * DAY) as f64).round().max(2.0) as i64;
            format!("about {w} weeks have passed")
        } else if gap_secs < 60 * DAY {
            "about a month has passed".to_string()
        } else {
            let mo = (gap_secs as f64 / (30 * DAY) as f64).round().max(2.0) as i64;
            format!("about {mo} months have passed")
        }
    }

    /// Decide whether the idle gap since the last assistant turn deserves a
    /// hint. Pure: no I/O, no clock reads, trivially testable.
    ///
    /// `last_assistant_ts_ms` is `None` for a new session (or when the caller
    /// could not find a prior assistant turn) - always silent. A non-positive
    /// gap (clock skew, same-millisecond turns) is silent too.
    ///
    /// When both triggers fire, the elapsed-gap wording wins: a multi-day gap
    /// already implies the date changed, so the wordings never stack.
    pub fn temporal_hint(
        last_assistant_ts_ms: Option<i64>,
        now_ms: i64,
        tz: &Tz,
        cfg: &TemporalConfig,
    ) -> Option<String> {
        if !cfg.enabled {
            return None;
        }
        let last_ms = last_assistant_ts_ms?;
        let gap_ms = now_ms.saturating_sub(last_ms);
        if gap_ms <= 0 {
            return None;
        }
        let gap_secs = gap_ms / 1000;

        // Elapsed-gap trigger.
        if cfg.min_gap_secs > 0 && gap_secs >= cfg.min_gap_secs as i64 {
            return Some(format!(
                "[temporal: {} since the previous exchange]",
                gap_sentence(gap_secs)
            ));
        }
        // Date-rollover trigger: the local calendar date changed even though
        // the gap is short. Only reachable when the elapsed trigger stayed
        // silent, so a sub-`min_gap_secs` gap implies at most one midnight and
        // "yesterday" is literally correct.
        if cfg.notify_date_change {
            let last_day = tz
                .timestamp_millis_opt(last_ms)
                .single()
                .map(|dt| dt.date_naive());
            let now_day = tz
                .timestamp_millis_opt(now_ms)
                .single()
                .map(|dt| dt.date_naive());
            if let (Some(a), Some(b)) = (last_day, now_day) {
                if a != b {
                    return Some("[temporal: the previous exchange was yesterday]".to_string());
                }
            }
        }
        None
    }
}

pub mod todo {
    //! Canonical todo types: the agent's per-session task list.
    //!
    //! The `todo` tool (pantheon-tools) replaces the whole list in one call,
    //! the event ledger (pantheon-storage) persists it per run, and the TUI
    //! renders it as a card. This module is the single definition they all
    //! share. Status strings are `pending` / `in_progress` / `completed` on
    //! the wire; `activeForm` is the optional
    //! present-tense phrasing shown while an item is in progress.

    use serde::{Deserialize, Serialize};

    /// Tool name, capability token, and ledger key prefix for the todo list.
    pub const TODO_TOOL_NAME: &str = "todo";

    /// One todo's lifecycle state.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum TodoStatus {
        Pending,
        InProgress,
        Completed,
    }

    impl TodoStatus {
        /// The wire string: `pending` / `in_progress` / `completed`.
        pub fn as_str(self) -> &'static str {
            match self {
                TodoStatus::Pending => "pending",
                TodoStatus::InProgress => "in_progress",
                TodoStatus::Completed => "completed",
            }
        }

        /// Parse a model-supplied status string. Anything else is `None` so
        /// the tool layer rejects it with `TOOL_BAD_ARGS` instead of guessing.
        pub fn parse(s: &str) -> Option<Self> {
            match s.trim() {
                "pending" => Some(TodoStatus::Pending),
                "in_progress" => Some(TodoStatus::InProgress),
                "completed" => Some(TodoStatus::Completed),
                _ => None,
            }
        }
    }

    /// One entry on the agent's todo list.
    ///
    /// Wire format is camelCase (`activeForm`), matching the tool schema
    /// the model sees.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    pub struct TodoItem {
        /// The task, imperative phrasing ("Read the auth module").
        pub content: String,
        pub status: TodoStatus,
        /// Present-tense phrasing shown while the item is in progress
        /// ("Reading the auth module"). Optional; only meaningful on
        /// `in_progress` items.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub active_form: Option<String>,
    }

    /// Why a replacement list was rejected.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TodoValidationError {
        /// An item's content was empty or whitespace-only (index into the
        /// proposed list).
        EmptyContent(usize),
        /// More than one item claimed `in_progress`.
        MultipleInProgress,
    }

    impl std::fmt::Display for TodoValidationError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                TodoValidationError::EmptyContent(i) => {
                    write!(f, "todos[{i}]: content must not be empty")
                }
                TodoValidationError::MultipleInProgress => write!(
                    f,
                    "at most one todo may be in_progress; finish or pause the current one first"
                ),
            }
        }
    }

    /// The whole per-session list. Replace-only: there is no per-item update
    /// call; the tool hands over the full new list and [`TodoList::replace`]
    /// validates it as a unit. Replacing is the update mechanism.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct TodoList {
        pub items: Vec<TodoItem>,
    }

    impl TodoList {
        /// Replace the list, enforcing the invariants: every item has
        /// non-empty content and at most one item is `in_progress`.
        /// On error the list is left untouched.
        pub fn replace(&mut self, items: Vec<TodoItem>) -> Result<(), TodoValidationError> {
            let mut in_progress = 0usize;
            for (i, item) in items.iter().enumerate() {
                if item.content.trim().is_empty() {
                    return Err(TodoValidationError::EmptyContent(i));
                }
                if item.status == TodoStatus::InProgress {
                    in_progress += 1;
                }
            }
            if in_progress > 1 {
                return Err(TodoValidationError::MultipleInProgress);
            }
            self.items = items;
            Ok(())
        }

        /// The single item currently being worked, if any.
        pub fn in_progress(&self) -> Option<&TodoItem> {
            self.items
                .iter()
                .find(|i| i.status == TodoStatus::InProgress)
        }

        pub fn len(&self) -> usize {
            self.items.len()
        }

        pub fn is_empty(&self) -> bool {
            self.items.is_empty()
        }
    }
}

pub mod logging {
    //! Structured file logging, std-only.
    //!
    //! The runtime had no logging at all: no `tracing`, no `log`, no files. Every
    //! failure was a line on stderr that vanished when the process exited, so
    //! "why did that turn die" was unanswerable after the fact and `pantheon logs`
    //! had nothing real to read.
    //!
    //! This is deliberately dependency-free. The workspace has no logging crate and
    //! adding one means a global subscriber, per-crate span instrumentation, and a
    //! subscriber that has to be initialised before anything can log - all of it
    //! invisible in review. A writer behind one function, called where the
    //! interesting thing happens, is greppable and cannot silently no-op.
    //!
    //! Two files, split by severity, because they answer different questions:
    //!
    //! - `agent.log` (DEBUG+) - what the runtime did, in order.
    //! - `errors.log` (WARNING+) - what went wrong, for grepping.
    //!
    //! Each line is `TIMESTAMP LEVEL [component] message`, which is the shape
    //! `hermes logs` parses back out with a regex. Keeping the two in agreement is
    //! what makes the reader possible at all.
    //!
    //! Every line passes through `redact()` before it is written, so API keys
    //! and bearer tokens never reach disk even when a caller logs a raw request
    //! dump. Files rotate at 10 MiB, keeping 5 generations (`agent.log.1`..=
    //! `agent.log.5`), enforced on the write path so no call site can forget.

    use std::fmt;
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
    pub enum Level {
        Debug = 0,
        Info = 1,
        Warning = 2,
        Error = 3,
    }

    impl Level {
        pub fn as_str(self) -> &'static str {
            match self {
                Level::Debug => "DEBUG",
                Level::Info => "INFO",
                Level::Warning => "WARNING",
                Level::Error => "ERROR",
            }
        }

        /// Parse a level name, case-insensitively. `None` for anything else, so a
        /// bad `--level` is rejected rather than silently meaning DEBUG.
        pub fn parse(s: &str) -> Option<Level> {
            match s.to_ascii_lowercase().as_str() {
                "debug" => Some(Level::Debug),
                "info" => Some(Level::Info),
                "warn" | "warning" => Some(Level::Warning),
                "error" => Some(Level::Error),
                _ => None,
            }
        }
    }

    impl fmt::Display for Level {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.as_str())
        }
    }

    /// The log files a reader can address by name.
    pub const AGENT_LOG: &str = "agent.log";
    pub const ERRORS_LOG: &str = "errors.log";
    pub const GATEWAY_LOG: &str = "gateway.log";

    /// All known files, for `logs list`. This is a closed set because a reader
    /// cannot enumerate a data dir for `*.log` and stay correct: the backup copies
    /// `repair` writes (`ledger.db.<stamp>.bak`) and any editor swap file would
    /// show up as a log.
    pub const KNOWN_LOGS: &[&str] = &[AGENT_LOG, ERRORS_LOG, GATEWAY_LOG];

    /// Size at which a log file is rotated. 10 MiB is small enough that a
    /// `logs tail` never chokes and large enough that rotation is rare: a
    /// busy turn writes kilobytes, so one generation covers weeks.
    pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

    /// Rotated generations kept per log file: `agent.log.1` ..= `agent.log.5`.
    /// `.1` is always the newest. Oldest is deleted on rotation, never archived,
    /// so a forgotten debug session cannot fill a disk.
    pub const LOG_ROTATE_GENERATIONS: u32 = 5;

    /// Where the logs live, and the threshold. Set once from the CLI entry point
    /// at startup; until then logging is a no-op, so a library test or an embedded
    /// use never writes files it did not ask for.
    static SINK: OnceLock<Sink> = OnceLock::new();

    struct Sink {
        dir: PathBuf,
        min: Level,
    }

    pub fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    pub fn stamp(ms: i64) -> String {
        // UTC, `YYYY-MM-DD HH:MM:SS.mmm`. Hand-rolled rather than pulling a date
        // crate for one format, and it matches the prefix `hermes logs` expects.
        let secs = ms / 1000;
        let millis = ms % 1000;
        let days = secs.div_euclid(86_400);
        let tod = secs.rem_euclid(86_400);
        let (h, mi, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
        // Civil-from-days (Howard Hinnant's algorithm), valid for any i64 count.
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
        format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{millis:03}")
    }

    /// Point the logger at `dir/logs` with a minimum level. Call once, early.
    ///
    /// A second call is ignored rather than an error: the gateway and a CLI command
    /// can both reach this, and whichever runs second must not silently retarget
    /// the first one's files.
    pub fn init(dir: &Path, min: Level) {
        let _ = SINK.set(Sink {
            dir: dir.join("logs"),
            min,
        });
    }

    /// The configured log directory, if logging was initialised.
    pub fn log_dir() -> Option<PathBuf> {
        SINK.get().map(|s| s.dir.clone())
    }

    /// Append one line to `file`, and to `errors.log` too when the level is a
    /// warning or worse. Failures are swallowed on purpose: a log write must never
    /// take down the operation being logged, and a missing log line is far less
    /// damaging than a failed turn.
    ///
    /// The message is redacted before writing, so an API key or bearer token in
    /// a request dump never reaches the file even when the caller forgot to
    /// sanitize. Rotation is enforced on the way in, so the files cannot grow
    /// forever no matter who writes to them.
    ///
    /// Takes a full path and reads no global, so the formatting and the
    /// warn+ mirroring are testable without racing for the process-wide `SINK`.
    pub(crate) fn append(file: &Path, level: Level, component: &str, msg: &str) {
        // Redact first: this is the single choke point every log line passes
        // through, so secrets cannot reach disk via a forgotten call site.
        let redacted = redact(msg);
        // Escape newlines before writing. A caller passing a multi-line tool
        // result is the ordinary case, and an unescaped one would let the message
        // forge additional log lines - including ones that look like a different
        // component's ERROR. The reader is line-oriented, so this is what keeps a
        // record one record.
        let flat = redacted.replace('\r', "\\r").replace('\n', "\\n");
        let line = format!("{} {level} [{component}] {flat}\n", stamp(now_ms()));
        if let Ok(mut f) = open_append(file) {
            let _ = f.write_all(line.as_bytes());
        }
        if level >= Level::Warning {
            // Derived from `file`, not from the sink, so a gateway-logged warning
            // lands in the same errors file instead of a second one.
            let errors = file.with_file_name(ERRORS_LOG);
            if let Ok(mut f) = open_append(&errors) {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    fn open_append(path: &Path) -> std::io::Result<File> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Enforce the size cap before every write. Best-effort: a failed rotation
        // degrades to the old unbounded behaviour rather than losing the line.
        rotate_if_needed(path);
        OpenOptions::new().create(true).append(true).open(path)
    }

    /// Rotate `path` when it has reached `LOG_ROTATE_BYTES`: `path` becomes
    /// `path.1`, the old `path.1` becomes `path.2`, and so on up to
    /// `LOG_ROTATE_GENERATIONS`, at which point the oldest generation is deleted.
    /// Called before every append, so all three log files rotate no matter which
    /// entry point wrote them.
    ///
    /// Best-effort by design. Two processes can race a rename, and a full disk
    /// fails every op inside; either way logging must never take down the turn
    /// being logged. Every failure here is swallowed.
    fn rotate_if_needed(path: &Path) {
        rotate_sized(path, LOG_ROTATE_BYTES, LOG_ROTATE_GENERATIONS);
    }

    fn rotate_sized(path: &Path, limit_bytes: u64, generations: u32) {
        let too_big = std::fs::metadata(path)
            .map(|m| m.len() >= limit_bytes)
            .unwrap_or(false);
        if !too_big {
            return;
        }
        rotate_log(path, generations);
    }

    /// Unconditionally rotate `path` through `generations` generations. Public
    /// so a CLI `logs rotate` or a startup sweep can rotate files the append
    /// path has not seen yet; the append path calls it only when the size cap
    /// is hit.
    pub fn rotate_log(path: &Path, generations: u32) {
        let name = path.to_string_lossy();
        // Drop the oldest generation first so the chain of renames has a free
        // slot; a missing file anywhere in the chain is fine (first rotation).
        let _ = std::fs::remove_file(format!("{name}.{generations}"));
        for i in (1..generations).rev() {
            let _ = std::fs::rename(format!("{name}.{i}"), format!("{name}.{}", i + 1));
        }
        let _ = std::fs::rename(path, format!("{name}.1"));
    }

    /// The one place level filtering and file routing happen, so no call site can
    /// forget the threshold or write to a file the reader does not know about.
    fn emit(file: &str, level: Level, component: &str, msg: &str) {
        let Some(sink) = SINK.get() else { return };
        if level < sink.min {
            return;
        }
        append(&sink.dir.join(file), level, component, msg);
    }

    /// Record something in `agent.log` at INFO.
    pub fn info(component: &str, msg: impl AsRef<str>) {
        emit(AGENT_LOG, Level::Info, component, msg.as_ref());
    }

    pub fn debug(component: &str, msg: impl AsRef<str>) {
        emit(AGENT_LOG, Level::Debug, component, msg.as_ref());
    }

    pub fn warn(component: &str, msg: impl AsRef<str>) {
        emit(AGENT_LOG, Level::Warning, component, msg.as_ref());
    }

    pub fn error(component: &str, msg: impl AsRef<str>) {
        let Some(sink) = SINK.get() else { return };
        append(
            &sink.dir.join(AGENT_LOG),
            Level::Error,
            component,
            msg.as_ref(),
        );
    }

    // --- redaction -----------------------------------------------------------

    /// Redact secrets from a message before it reaches a log file.
    ///
    /// The redaction pipeline is fail-closed: when a secret pattern is detected,
    /// the entire match is replaced with `[REDACTED]`. This is deliberately
    /// aggressive - a partial leak is still a leak.
    ///
    /// Patterns redacted:
    /// - `sk-or-v1-...` (OpenRouter keys)
    /// - `sk-...` (OpenAI-style keys)
    /// - `AIza...` (Google API keys)
    /// - `ghp_...`, `gho_...`, `ghu_...`, `ghs_...`, `ghr_...` (GitHub tokens)
    /// - `glpat-...` (GitLab tokens), `xoxb-...` / `xoxp-...` (Slack tokens)
    /// - `Bearer <value>` (Authorization headers)
    /// - `api-key: <value>` (API key headers)
    /// - `PANTHEON_SECRET_<name>=<value>` (env-style secrets)
    /// - values under sensitive keys (`password`, `token`, `api_key`,
    ///   `secret`, `authorization`, and close variants) in JSON - whether
    ///   the whole input parses as JSON or the JSON is embedded in a log
    ///   line (`request: {"password": "..."}`). The embedded case used to
    ///   leak: the JSON pass only ran when the whole input parsed, so a
    ///   secret inside a larger line reached disk despite the "never reach
    ///   disk" claim.
    pub fn redact(msg: &str) -> String {
        let mut out = msg.to_string();
        out = redact_prefix(&out, "sk-or-v1-");
        out = redact_prefix(&out, "sk-");
        out = redact_prefix(&out, "AIza");
        out = redact_prefix(&out, "ghp_");
        out = redact_prefix(&out, "gho_");
        out = redact_prefix(&out, "ghu_");
        out = redact_prefix(&out, "ghs_");
        out = redact_prefix(&out, "ghr_");
        out = redact_prefix(&out, "glpat-");
        out = redact_prefix(&out, "xoxb-");
        out = redact_prefix(&out, "xoxp-");
        out = redact_prefix(&out, "Bearer ");
        out = redact_prefix(&out, "api-key: ");
        out = redact_prefix(&out, "api-key=");
        out = redact_prefix(&out, "PANTHEON_SECRET_");
        out = redact_embedded_json(&out);
        out = redact_json_values(&out);
        out
    }

    /// True when a JSON object key names a credential. Matching is on the
    /// lowercased key with `-`/space folded to `_`: the known credential
    /// names, plus any `*_token` / `*_secret` / `*_password` compound
    /// (`access_token`, `client_secret`, `db_password`, ...). Deliberately
    /// narrower than a substring scan - `token_count` is telemetry, not a
    /// secret, and redacting it would gut usage logs.
    fn is_sensitive_key(key: &str) -> bool {
        let k = key.to_lowercase().replace(['-', ' '], "_");
        matches!(
            k.as_str(),
            "password"
                | "passwd"
                | "pwd"
                | "token"
                | "api_key"
                | "apikey"
                | "secret"
                | "authorization"
                | "auth"
                | "bearer"
                | "private_key"
                | "client_secret"
                | "access_token"
                | "refresh_token"
                | "id_token"
                | "auth_token"
                | "session_token"
                | "webhook_secret"
                | "signing_secret"
        ) || k.ends_with("_token")
            || k.ends_with("_secret")
            || k.ends_with("_password")
    }

    /// Replace every value under a sensitive key, at any depth. Returns
    /// true when anything changed.
    fn redact_json_node(v: &mut serde_json::Value) -> bool {
        let mut changed = false;
        match v {
            serde_json::Value::Object(map) => {
                for (key, val) in map.iter_mut() {
                    if is_sensitive_key(key) {
                        if val.as_str() != Some("[REDACTED]") {
                            *val = serde_json::Value::String("[REDACTED]".to_string());
                            changed = true;
                        }
                    } else {
                        changed |= redact_json_node(val);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items.iter_mut() {
                    changed |= redact_json_node(item);
                }
            }
            _ => {}
        }
        changed
    }

    /// JSON-aware pass over a log line that is not itself JSON: scan for
    /// JSON objects/arrays embedded in the line and redact values under
    /// sensitive keys inside each one, leaving the surrounding text
    /// untouched. A `{` that does not start a parseable JSON value is
    /// emitted verbatim and scanning resumes after it, so Rust-Debug-style
    /// braces and truncated fragments pass through. A value that parses
    /// with nothing sensitive inside is copied verbatim, never
    /// re-serialized, so clean log lines keep their formatting.
    fn redact_embedded_json(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(pos) = rest.find('{') {
            out.push_str(&rest[..pos]);
            let candidate = &rest[pos..];
            let mut stream =
                serde_json::Deserializer::from_str(candidate).into_iter::<serde_json::Value>();
            if let Some(Ok(mut v)) = stream.next() {
                let consumed = stream.byte_offset();
                if redact_json_node(&mut v) {
                    if let Ok(redacted) = serde_json::to_string(&v) {
                        out.push_str(&redacted);
                        rest = &candidate[consumed..];
                        continue;
                    }
                }
                // Parsed but clean: copy verbatim, continue after it.
                out.push_str(&candidate[..consumed]);
                rest = &candidate[consumed..];
            } else {
                out.push('{');
                rest = &candidate[1..];
            }
        }
        out.push_str(rest);
        out
    }

    /// JSON-aware pass over [`redact`]: when the whole input parses as
    /// JSON, values under sensitive keys are replaced wholesale. Input
    /// that does not parse, or parses with nothing sensitive in it, is
    /// returned byte-for-byte - clean JSON keeps its original formatting,
    /// and only a message that actually carried a secret is re-serialized.
    fn redact_json_values(input: &str) -> String {
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(input) else {
            return input.to_string();
        };
        if !redact_json_node(&mut v) {
            return input.to_string();
        }
        serde_json::to_string(&v).unwrap_or_else(|_| input.to_string())
    }

    /// Redact a secret that starts with a known prefix. The secret runs until
    /// the next whitespace or end of string.
    fn redact_prefix(input: &str, prefix: &str) -> String {
        if !input.contains(prefix) {
            return input.to_string();
        }
        let mut out = String::with_capacity(input.len());
        let mut rest = input;
        while let Some(pos) = rest.find(prefix) {
            out.push_str(&rest[..pos]);
            out.push_str("[REDACTED]");
            rest = &rest[pos + prefix.len()..];
            // Consume the secret value: everything up to the next whitespace
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    /// Gateway-scoped records go to their own file so `logs gateway` is not mostly
    /// agent turns.
    pub fn gateway(level: Level, component: &str, msg: impl AsRef<str>) {
        emit(GATEWAY_LOG, level, component, msg.as_ref());
    }

    #[cfg(test)]
    mod redact_tests {
        use super::redact;

        #[test]
        fn prefix_scan_still_redacts_non_json_text() {
            let out = redact("key sk-or-v1-abcdef and Bearer xyz");
            assert!(!out.contains("abcdef"), "prefix secret leaked: {out}");
            assert!(!out.contains("xyz"), "bearer value leaked: {out}");
            assert_eq!(redact("nothing secret here"), "nothing secret here");
        }

        #[test]
        fn json_values_under_sensitive_keys_are_redacted() {
            let out = redact(r#"{"user": "umar", "password": "hunter2"}"#);
            let v: serde_json::Value = serde_json::from_str(&out).expect("still valid JSON");
            assert_eq!(v["password"], "[REDACTED]");
            assert_eq!(v["user"], "umar", "non-sensitive values pass through");
            assert!(!out.contains("hunter2"));
        }

        #[test]
        fn json_redaction_covers_variants_nesting_and_non_string_values() {
            let out = redact(
                r#"{"access_token": "tok-1", "attempts": 2, "nested": {"api-key": "k", "token": 4242}, "list": [{"client_secret": "s"}]}"#,
            );
            let v: serde_json::Value = serde_json::from_str(&out).expect("still valid JSON");
            assert_eq!(v["access_token"], "[REDACTED]");
            assert_eq!(v["nested"]["api-key"], "[REDACTED]");
            assert_eq!(v["nested"]["token"], "[REDACTED]");
            assert_eq!(v["list"][0]["client_secret"], "[REDACTED]");
            assert_eq!(v["attempts"], 2, "non-sensitive values pass through");
            for leaked in ["tok-1", "4242", "\"k\"", "client_secret\": \"s"] {
                assert!(!out.contains(leaked), "leaked {leaked}: {out}");
            }
        }

        #[test]
        fn clean_json_and_telemetry_keys_pass_through_byte_for_byte() {
            let clean = "{ \"command\": \"ls -la\", \"count\": 3 }";
            assert_eq!(redact(clean), clean, "clean JSON is not re-serialized");
            let telemetry = r#"{"token_count": 1200, "model": "gpt"}"#;
            assert_eq!(redact(telemetry), telemetry);
        }

        /// A secret inside JSON embedded in a log line is redacted, not
        /// leaked to disk: the old whole-input-only pass let
        /// `request: {"password": ...}` straight through.
        #[test]
        fn embedded_json_secrets_are_redacted() {
            let out = redact(r#"request: {"password": "hunter2", "user": "bob"} done"#);
            assert!(!out.contains("hunter2"), "embedded password leaked: {out}");
            assert!(
                out.contains("bob"),
                "non-sensitive value must survive: {out}"
            );
            assert!(
                out.starts_with("request: "),
                "surrounding text survives: {out}"
            );
            assert!(out.ends_with(" done"), "trailing text survives: {out}");
            let nested = redact(r#"upstream said {"outer": {"api_key": "k-secret"}} ok"#);
            assert!(
                !nested.contains("k-secret"),
                "nested embedded secret leaked: {nested}"
            );
            let in_array = redact(r#"got [{"token": "tok-9"}] done"#);
            assert!(
                !in_array.contains("tok-9"),
                "array-embedded secret leaked: {in_array}"
            );
        }

        /// Embedded clean JSON keeps its formatting; non-JSON braces pass
        /// through untouched.
        #[test]
        fn embedded_clean_json_and_braces_pass_through() {
            let clean = r#"request: {"command": "ls", "count": 3} done"#;
            assert_eq!(redact(clean), clean);
            let debug = "value {not json} here";
            assert_eq!(redact(debug), debug);
        }

        /// Provider key prefixes beyond `sk-` are redacted.
        #[test]
        fn new_key_prefixes_are_redacted() {
            for secret in [
                "AIzaSyAbc123Xyz",
                "ghp_abc123def456",
                "gho_oauth789",
                "glpat-xyz123",
                "xoxb-1234567890",
                "xoxp-0987654321",
            ] {
                let out = redact(&format!("saw key {secret} end"));
                assert!(!out.contains(secret), "{secret} leaked: {out}");
            }
        }
    }
}

pub mod mcp_catalog {
    //! The canonical bundled MCP server catalog: the ONE recipe definition
    //! every surface consumes.
    //!
    //! ## Why the canonical recipe lives here (not in `pantheon-mcp`)
    //!
    //! The launcher owns `pantheon-mcp`, but the dependency edges run
    //! `pantheon-mcp -> pantheon-tools -> pantheon-api`: the agent's
    //! `enable_mcp` tool (in `pantheon-tools`) needs the catalog names, pins,
    //! and the file-persisting [`set_enabled`] write. Placing the canonical
    //! recipe in `pantheon-mcp` would force a `tools -> mcp` edge - a
    //! dependency cycle, since the edge runs `mcp -> tools`. So the recipe
    //! data lives in the leaf both depend on (`pantheon-api`), and
    //! `pantheon_mcp::bundled` is a thin facade re-exporting
    //! [`BundledMcpServer`] plus the launcher-side pure helpers. There is
    //! exactly one copy of the recipe data; the old "two copies + drift
    //! test" arrangement is gone by construction.
    //!
    //! ## What this module guarantees
    //!
    //! - [`BundledMcpServer`] rows: the single source of truth for package
    //!   pins, launch shapes, secret names, and placement notes.
    //! - [`BundledMcpServer::to_config_entry`]: the single materialization
    //!   of a recipe into a `[mcp.servers.<name>]` entry (disabled by
    //!   default).
    //! - [`materialize_recipe_table`]: the single TOML-table writer. Both
    //!   the agent path ([`set_enabled`]) and the dashboard path use it, so
    //!   one canonical recipe always materializes to byte-identical tables
    //!   no matter which surface enabled the server first
    //!   first-writer-wins divergence is impossible. `pinned_version` is
    //!   stamped only when the recipe carries a real package pin; unpinned
    //!   (remote) recipes get no stamp - a fabricated pin would lie about
    //!   what code is approved to run.
    //! - [`set_enabled`] / [`is_enabled`]: the single enablement state
    //!   `[mcp.servers.<name>]` in `<data_dir>/config.toml` - rejecting
    //!   non-catalog names. This is the no-arbitrary-MCP-command boundary
    //!   for the agent path: the agent can never get an unvetted binary onto
    //!   the command line through it.
    //!
    //! ## MCPs vs plugins: the trust distinction
    //!
    //! (Mirrors `pantheon_extensions::bundled`.) A bundled MCP server is an
    //! out-of-process integration: Pantheon spawns or connects to it and
    //! speaks the MCP protocol. Pantheon never executes the server's code,
    //! which may run on another machine entirely - trust is in the endpoint
    //! and its configuration (command, URL, env), not in shipped code.
    //! Bundled MCPs are first-party, so they skip the third-party approval
    //! store ([`crate::approval`]); they do **not** skip enablement. A custom
    //! (non-catalog) MCP server additionally needs operator consent recorded
    //! in `<data_dir>/mcp/.approvals.json` before the launcher connects.
    //!
    //! ## Content-hash binding
    //!
    //! The launcher's content hash binds the package spec (`name@pin`) plus
    //! the full argument list for launcher-shim servers (`npx`, `uvx`,
    //! `docker`, ...), so a pin bump or an arg change lapses approval. For
    //! shims the hash binds the *requested* spec, not the bytes the registry
    //! served - registry-fetched code can change under a pin, so treat the
    //! approval as binding intent, not bytes.

    use crate::config::{Config, McpServerEntry};
    use crate::error::{Layer, PantheonError};
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use std::path::Path;

    fn merr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Extension,
            false,
            cause,
            "check the server name against the bundled MCP catalog",
            "",
        )
    }

    /// One MCP server Pantheon ships: the canonical bundled recipe. All
    /// entries are disabled by default - [`is_enabled`] is false for a name
    /// with no `[mcp.servers.<name>]` entry.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct BundledMcpServer {
        /// Catalog name. Matches the `[mcp.servers.<name>]` config key.
        pub name: String,
        /// Display label for the pin, e.g. `@playwright/mcp@0.0.83`.
        /// Remote-only recipes (no package) use `"remote <url>"`.
        pub version: String,
        /// The true pinned package spec, e.g. `@playwright/mcp@0.0.83`.
        /// `None` when there is no bundleable package (Cloudflare: remote
        /// only, the npm package is stale and deprecated upstream).
        /// This - and only this - is what `pinned_version` stamps.
        pub package: Option<String>,
        /// One-paragraph operator-facing summary.
        pub description: String,
        /// `stdio`, `sse`, or `http`.
        pub transport: String,
        /// Program to spawn (stdio only).
        pub command: Option<String>,
        /// Arguments for the program (stdio only). Pins live here too, so a
        /// pin bump changes the materialized entry (and lapses approval via
        /// the content hash).
        pub args: Vec<String>,
        /// Endpoint URL (sse/http only).
        pub url: Option<String>,
        /// Environment variable names the server needs. Materialized as
        /// `env = { NAME = "env:NAME" }` so secrets resolve from the
        /// operator's environment at spawn time - never stored in config.
        pub requires_env: Vec<String>,
        /// Non-secret env vars Pantheon sets by default when launching the
        /// server (e.g. telemetry opt-outs). Stored as literal values.
        pub default_env: Vec<(String, String)>,
        /// Operator-facing setup notes: accounts, OAuth clients, scopes,
        /// prerequisites the recipe alone does not capture.
        pub setup_notes: String,
        /// True when the server process runs on the operator's machine and
        /// calls only cloud APIs; false when the server itself is hosted by a
        /// third party (Cloudflare Code Mode).
        pub local_only: bool,
        /// Upstream deprecation/sunset warnings the operator should see
        /// before enabling.
        pub deprecated_warning: Option<String>,
        /// What enabling this server actually authorizes, in plain terms.
        /// Shown by approval surfaces before the operator decides.
        pub privilege_notes: String,
        /// Remote auth mode materialized into the config entry, e.g.
        /// `Some("oauth")` for Cloudflare's hosted endpoint. `None` means
        /// no auth (cloudflare-docs is public and correctly needs none).
        pub auth: Option<String>,
    }

    impl BundledMcpServer {
        /// The `[mcp.servers.<name>]` table this catalog row materializes.
        /// Disabled by default: enabling is always an explicit write through
        /// [`set_enabled`] (agent path) or the dashboard path - both funnel
        /// through [`materialize_recipe_table`], so the entry is identical
        /// whichever surface wrote it.
        ///
        /// Secrets become `env:NAME` placeholders (resolved from the
        /// operator's environment at spawn time; values are never stored).
        /// `default_env` pairs are stored as literal values.
        pub fn to_config_entry(&self) -> McpServerEntry {
            let mut env = HashMap::new();
            for (key, value) in &self.default_env {
                env.insert(key.clone(), value.clone());
            }
            for name in &self.requires_env {
                env.insert(name.clone(), format!("env:{name}"));
            }
            McpServerEntry {
                transport: self.transport.clone(),
                command: self.command.clone(),
                args: self.args.clone(),
                env,
                url: self.url.clone(),
                enabled: false,
                timeout_secs: None,
                headers: Default::default(),
                auth: self.auth.clone(),
                scopes: Vec::new(),
                client_id: None,
            }
        }
    }

    /// One catalog row. Pins verified 2026-09-29 against official
    /// docs/registries (`docs/bundled-mcp-research.md`).
    ///
    /// Canonical decisions, made once here (previously the two copies
    /// disagreed): every `npx` invocation carries `-y` (without it npx
    /// prompts on piped stdin and hangs); every registry package is pinned
    /// in the arg list (an unpinned arg would float to latest under a pinned
    /// claim); `google-workspace` runs `workspace-mcp==1.30.0`.
    fn catalog_rows() -> Vec<BundledMcpServer> {
        vec![
            BundledMcpServer {
                name: "github".to_string(),
                version: "ghcr.io/github/github-mcp-server:v1.12.2".to_string(),
                package: Some("ghcr.io/github/github-mcp-server:v1.12.2".to_string()),
                description: "GitHub's official MCP server: repos, issues, PRs, actions, \
                    packages and more, driven through toolsets. Runs as a local Docker \
                    container calling the GitHub API."
                    .to_string(),
                transport: "stdio".to_string(),
                command: Some("docker".to_string()),
                args: vec![
                    "run".to_string(),
                    "-i".to_string(),
                    "--rm".to_string(),
                    "-e".to_string(),
                    "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    "ghcr.io/github/github-mcp-server:v1.12.2".to_string(),
                ],
                url: None,
                requires_env: vec![
                    "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    "GITHUB_OAUTH_CALLBACK_PORT".to_string(),
                    "GITHUB_HOST".to_string(),
                ],
                default_env: vec![],
                setup_notes: "A classic PAT needs at minimum `repo`, `read:org` and \
                    `read:packages` scopes; grant only what your toolsets need. \
                    Alternatively use the browser OAuth flow: set \
                    GITHUB_OAUTH_CALLBACK_PORT=8085 and pass -p 127.0.0.1:8085:8085 \
                    to docker (the official image bundles app credentials; the \
                    resulting token is kept in memory only). For GitHub Enterprise \
                    Server or *.ghe.com data residency set GITHUB_HOST. Use \
                    GITHUB_TOOLSETS / GITHUB_TOOLS to restrict the exposed tools; \
                    --read-only is available for audit-safe use. No-Docker fallback: \
                    build from source (go build ./cmd/github-mcp-server) and run \
                    with args [\"stdio\"]."
                    .to_string(),
                local_only: true,
                deprecated_warning: None,
                privilege_notes: "Spawns a Docker container running GitHub's official \
                    MCP server (pinned image v1.12.2). It can read and act on your \
                    GitHub account with whatever scopes your personal access token \
                    grants - grant only the scopes the task needs. First-party \
                    bundle entry; the container image is third-party code run by \
                    Docker, not by Pantheon."
                    .to_string(),
                auth: None,
            },
            BundledMcpServer {
                name: "google-workspace".to_string(),
                version: "workspace-mcp==1.30.0".to_string(),
                package: Some("workspace-mcp==1.30.0".to_string()),
                description: "Community Google Workspace MCP server \
                    (taylorwilsdon/google_workspace_mcp): one local server covering \
                    Gmail, Drive, Calendar, Docs, Sheets, Slides, Forms, Tasks, \
                    Contacts, Chat and more, calling the Google cloud APIs."
                    .to_string(),
                transport: "stdio".to_string(),
                command: Some("uvx".to_string()),
                args: vec!["workspace-mcp==1.30.0".to_string()],
                url: None,
                requires_env: vec![
                    "GOOGLE_OAUTH_CLIENT_ID".to_string(),
                    "GOOGLE_OAUTH_CLIENT_SECRET".to_string(),
                ],
                default_env: vec![],
                setup_notes: "You must create your own OAuth client in your own \
                    Google Cloud project and enable the API for each service you use; \
                    the redirect URI must match, e.g. \
                    http://localhost:${WORKSPACE_MCP_PORT}/oauth2callback. Launch \
                    tiers: uvx workspace-mcp --tool-tier core|extended|complete, or \
                    cherry-pick --tools gmail drive calendar; --read-only and \
                    per-service --permissions are available. Maintainer caution: \
                    emails/docs can carry hidden prompt-injection instructions - be \
                    deliberate with write tools. Google Chat needs a one-time Chat \
                    app configuration and a Workspace account (not free Gmail). Do \
                    NOT install the CLI via `uvx workspace-cli` - that PyPI name is \
                    squatted by an abandoned package."
                    .to_string(),
                local_only: true,
                deprecated_warning: None,
                privilege_notes: "Spawns the community workspace-mcp server (pinned \
                    PyPI release 1.30.0) via uvx. It acts on your Google account \
                    through your own OAuth client: mail, calendar, drive files, \
                    and documents are all in reach of its tools. Treat its outputs \
                    as untrusted - emails and docs can carry hidden instructions."
                    .to_string(),
                auth: None,
            },
            BundledMcpServer {
                name: "chrome-devtools".to_string(),
                version: "chrome-devtools-mcp@1.10.1".to_string(),
                package: Some("chrome-devtools-mcp@1.10.1".to_string()),
                description: "Chrome DevTools MCP: drive a local Chrome for automation, \
                    debugging, performance traces and screenshots. No secrets; needs \
                    Chrome and Node.js LTS on this machine."
                    .to_string(),
                transport: "stdio".to_string(),
                command: Some("npx".to_string()),
                args: vec!["-y".to_string(), "chrome-devtools-mcp@1.10.1".to_string()],
                url: None,
                requires_env: vec![],
                default_env: vec![
                    (
                        "CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS".to_string(),
                        "1".to_string(),
                    ),
                    (
                        "CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS".to_string(),
                        "1".to_string(),
                    ),
                ],
                setup_notes: "Requires a current stable Google Chrome installed on \
                    the same machine plus Node.js LTS; the server auto-starts Chrome \
                    on the first tool call that needs a browser. Privacy: usage \
                    statistics and npm update checks are ON upstream by default - \
                    Pantheon disables both via the bundled env above. Performance \
                    tools call the Google CrUX API for field data (add --no-performance-crux \
                    to disable). Useful flags: --headless, --slim (3 tools only), \
                    --browser-url=http://127.0.0.1:9222 to attach to a running \
                    Chrome, --user-data-dir, --channel, --executablePath."
                    .to_string(),
                local_only: true,
                deprecated_warning: None,
                privilege_notes: "Spawns the official Chrome DevTools MCP server \
                    (pinned npm release 1.10.1) via npx. It drives your installed \
                    Chrome: navigation, script execution, screenshots. Usage \
                    statistics and update checks phone home to Google unless \
                    disabled; performance tools call the CrUX API."
                    .to_string(),
                auth: None,
            },
            BundledMcpServer {
                name: "cloudflare".to_string(),
                version: "remote https://mcp.cloudflare.com/mcp".to_string(),
                package: None,
                description: "Cloudflare Code Mode MCP: broad Cloudflare API coverage \
                    via Cloudflare's own hosted remote endpoint. Auth is a per-user \
                    browser OAuth flow - there is no static secret to configure."
                    .to_string(),
                transport: "http".to_string(),
                command: None,
                args: vec![],
                url: Some("https://mcp.cloudflare.com/mcp".to_string()),
                requires_env: vec![],
                default_env: vec![],
                setup_notes: "Fully remote and cloud-hosted by Cloudflare: nothing \
                    runs on this machine. First connect opens a browser OAuth flow \
                    where you pick the Cloudflare account; some features require a \
                    paid Workers plan. Domain-specific servers also exist \
                    (*.mcp.cloudflare.com/mcp: docs, bindings, builds, \
                    observability, containers, browser, logs, ai-gateway, autorag, \
                    dns-analytics, dex, casb, radar, blog). The npm package \
                    @cloudflare/mcp-server-cloudflare is stale (0.2.0, March 2025) \
                    and the local server implementations are deprecated upstream - \
                    they are deliberately NOT bundled. Clients without remote \
                    support can shim with npx mcp-remote <url>."
                    .to_string(),
                local_only: false,
                deprecated_warning: None,
                privilege_notes: "Connects to Cloudflare's hosted MCP endpoint \
                    (streamable HTTP). Browser OAuth on first connect; its tools \
                    act on your Cloudflare account (Workers, DNS, zones). No \
                    local code runs - trust is in the endpoint and your account."
                    .to_string(),
                auth: Some("oauth".to_string()),
            },
            BundledMcpServer {
                name: "cloudflare-docs".to_string(),
                version: "remote https://docs.mcp.cloudflare.com/mcp".to_string(),
                package: None,
                description: "Cloudflare's official documentation MCP server: up-to-date \\\
                    docs and reference lookups across every Cloudflare product."
                    .to_string(),
                transport: "http".to_string(),
                command: None,
                args: vec![],
                url: Some("https://docs.mcp.cloudflare.com/mcp".to_string()),
                requires_env: vec![],
                default_env: vec![],
                setup_notes: "Fully remote and public: no authentication, no token, \\\
                    nothing runs on this machine. Safe to leave enabled everywhere. \\\
                    Pair with the cloudflare server (OAuth) for account operations, \\\
                    or with the cf CLI + CLOUDFLARE_API_TOKEN for the same surface \\\
                    through the shell."
                    .to_string(),
                local_only: false,
                deprecated_warning: None,
                privilege_notes: "Read-only documentation lookups against Cloudflare's \\\
                    hosted endpoint. No account access: the server is public and \\\
                    unauthenticated."
                    .to_string(),
                auth: None,
            },
            BundledMcpServer {
                name: "playwright".to_string(),
                version: "@playwright/mcp@0.0.83".to_string(),
                package: Some("@playwright/mcp@0.0.83".to_string()),
                description: "Playwright's official browser MCP: structured web \
                    automation over Chromium/Firefox/WebKit running locally. No \
                    secrets; Node.js 18+ required."
                    .to_string(),
                transport: "stdio".to_string(),
                command: Some("npx".to_string()),
                args: vec!["-y".to_string(), "@playwright/mcp@0.0.83".to_string()],
                url: None,
                requires_env: vec![],
                default_env: vec![],
                setup_notes: "Requires Node.js 18+. Browsers (Chromium by default) \
                    are downloaded by Playwright on the first run. Options: \
                    --browser chrome|firefox|webkit|msedge, --caps vision,pdf,devtools, \
                    --cdp-endpoint <url> to drive an external browser, --user-data-dir \
                    for persistent profiles. Filesystem access is restricted to \
                    workspace roots by default (--allow-unrestricted-file-access \
                    relaxes); host/origin allow-lists via --allowed-hosts / \
                    --allowed-origins (PLAYWRIGHT_MCP_* env equivalents exist for \
                    every flag)."
                    .to_string(),
                local_only: true,
                deprecated_warning: None,
                privilege_notes: "Spawns the official Playwright MCP server \
                    (pinned npm release 0.0.83) via npx. It drives a real \
                    browser (Chromium by default) on your machine: pages it \
                    visits run arbitrary web content. Filesystem access is \
                    restricted to workspace roots unless relaxed."
                    .to_string(),
                auth: None,
            },
            BundledMcpServer {
                name: "notion".to_string(),
                version: "@notionhq/notion-mcp-server@2.5.2".to_string(),
                package: Some("@notionhq/notion-mcp-server@2.5.2".to_string()),
                description: "Official Notion MCP server: query and edit pages and \
                    data sources through an internal integration token. Local server \
                    calling the Notion API."
                    .to_string(),
                transport: "stdio".to_string(),
                command: Some("npx".to_string()),
                args: vec![
                    "-y".to_string(),
                    "@notionhq/notion-mcp-server@2.5.2".to_string(),
                ],
                url: None,
                requires_env: vec!["NOTION_TOKEN".to_string()],
                default_env: vec![],
                setup_notes: "Create an internal integration at \
                    notion.so/profile/integrations, then explicitly share pages and \
                    databases with it (integration Access tab, or per-page \
                    \"Connect to integration\") - unshared content is invisible to \
                    the server. v2.0.0 is a breaking release (Notion API \
                    2025-09-03): database tools were renamed to data-source tools, \
                    e.g. post-database-query -> query-data-source; 22 tools total."
                    .to_string(),
                local_only: true,
                deprecated_warning: Some(
                    "Upstream maintainers state this repository is no longer actively \
                    maintained or supported and may be sunset in the future in favor \
                    of the hosted Remote Notion MCP. Bundled at 2.5.2 with eyes open; \
                    plan a remote-Notion path."
                        .to_string(),
                ),
                privilege_notes: "Spawns Notion's official MCP server (pinned npm \
                    release 2.5.2) via npx. It reads and writes the Notion pages \
                    and databases you shared with its integration, using your \
                    integration token. Note: Notion has marked this repo \
                    unmaintained and may sunset it - plan a remote-Notion path."
                    .to_string(),
                auth: None,
            },
        ]
    }

    /// Every MCP server Pantheon ships, sorted by name. Deterministic order
    /// so dashboard, app, and wizard render the same list.
    pub fn bundled_catalog() -> Vec<BundledMcpServer> {
        let mut out = catalog_rows();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// The catalog entry for `name`, or `None` when it is not bundled.
    /// Only catalog names are toggleable - this is what keeps the agent
    /// from proposing (or any surface from flipping) an arbitrary command.
    pub fn find(name: &str) -> Option<BundledMcpServer> {
        bundled_catalog().into_iter().find(|s| s.name == name)
    }

    /// Fill an empty `[mcp.servers.<name>]` TOML table from the one canonical
    /// recipe. This is the single writer both the agent path ([`set_enabled`])
    /// and the dashboard path use: one canonical recipe in, byte-identical
    /// table out, regardless of which surface enabled the server first.
    ///
    /// `pinned_version` is stamped only when the recipe carries a real
    /// package pin ([`BundledMcpServer::package`]); unpinned (remote) recipes
    /// get no stamp - a fabricated pin would lie about what code is approved
    /// to run. The stamp is informational (the launcher ignores unknown
    /// keys).
    ///
    /// The caller passes the table empty; keys the operator already set are
    /// never overwritten because materialization only runs on empty tables.
    pub fn materialize_recipe_table(
        table: &mut toml::map::Map<String, toml::Value>,
        recipe: &BundledMcpServer,
    ) {
        debug_assert!(
            table.is_empty(),
            "materialize_recipe_table expects an empty table"
        );
        let entry = recipe.to_config_entry();
        table.insert(
            "transport".to_string(),
            toml::Value::String(entry.transport),
        );
        if let Some(command) = entry.command {
            table.insert("command".to_string(), toml::Value::String(command));
        }
        if !entry.args.is_empty() {
            table.insert(
                "args".to_string(),
                toml::Value::Array(entry.args.into_iter().map(toml::Value::String).collect()),
            );
        }
        if let Some(url) = entry.url {
            table.insert("url".to_string(), toml::Value::String(url));
        }
        if !entry.env.is_empty() {
            let mut env_table = toml::map::Map::new();
            for (k, v) in entry.env {
                env_table.insert(k, toml::Value::String(v));
            }
            table.insert("env".to_string(), toml::Value::Table(env_table));
        }
        if let Some(pin) = &recipe.package {
            table.insert(
                "pinned_version".to_string(),
                toml::Value::String(pin.clone()),
            );
        }
    }

    /// Is the bundled server `name` enabled? Pure read over the config:
    /// absent `[mcp.servers.<name>]` (or an entry without `enabled`) =
    /// false. Unknown names are false too - inert entries never enable
    /// anything.
    pub fn is_enabled(config: &Config, name: &str) -> bool {
        config.mcp_server_enabled(name)
    }

    /// Load the config for enablement decisions without
    /// [`Config::load_or_report`]'s process-exit behavior: a library must
    /// never `exit(2)` the host because the config has a typo. A missing
    /// config means every bundled server is disabled (fail closed); a config
    /// that exists but does not parse is reported on stderr and also fails
    /// closed - a typo must never silently flip a server on.
    pub fn load_config(data_dir: &Path) -> Option<Config> {
        match Config::load(data_dir) {
            Ok(c) => Some(c),
            Err(e) if e.code == "CONFIG_OPEN" => None,
            Err(e) => {
                eprintln!("pantheon: bundled MCP servers disabled: {}", e.cause);
                eprintln!("pantheon: fix: {}", e.remediation);
                None
            }
        }
    }

    /// Write the single enablement state: set `[mcp.servers.<name>].enabled`
    /// in `<data_dir>/config.toml`, materializing the canonical catalog row
    /// when the table does not exist yet. Only bundled-catalog names are
    /// accepted (`MCP_UNKNOWN_SERVER` otherwise) - this is the
    /// no-arbitrary-command boundary: the agent can never get an unvetted
    /// binary onto the command line through this path.
    ///
    /// Enabling stamps the recipe's real package pin (`pinned_version` is
    /// informational - the launcher ignores unknown keys); unpinned recipes
    /// get no stamp. Disabling keeps the row and flips the flag. The
    /// document is edited as TOML rather than re-serialized from the
    /// [`Config`] struct, so unknown keys survive; comments and original key
    /// order do not (the TOML document model drops comments and sorts keys).
    pub fn set_enabled(data_dir: &Path, name: &str, enabled: bool) -> Result<(), PantheonError> {
        let server = find(name).ok_or_else(|| {
            merr(
                "MCP_UNKNOWN_SERVER",
                format!(
                    "'{name}' is not a bundled MCP server; only catalog servers can be toggled"
                ),
            )
        })?;
        let path = Config::path(data_dir);
        let mut doc: toml::Value = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| merr("MCP_CONFIG_PARSE", format!("parse {}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(toml::map::Map::new())
            }
            Err(e) => {
                return Err(merr(
                    "MCP_CONFIG_READ",
                    format!("read {}: {e}", path.display()),
                ))
            }
        };
        if !doc.is_table() {
            return Err(merr(
                "MCP_CONFIG_PARSE",
                format!("{} is not a TOML table", path.display()),
            ));
        }
        let root = doc.as_table_mut().expect("checked is_table");
        let mcp = root
            .entry("mcp")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let mcp_table = mcp.as_table_mut().ok_or_else(|| {
            merr(
                "MCP_CONFIG_PARSE",
                "config has a non-table [mcp] value".to_string(),
            )
        })?;
        let servers = mcp_table
            .entry("servers")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let servers_table = servers.as_table_mut().ok_or_else(|| {
            merr(
                "MCP_CONFIG_PARSE",
                "config has a non-table [mcp.servers] value".to_string(),
            )
        })?;
        let entry = servers_table
            .entry(name)
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        let table = entry.as_table_mut().ok_or_else(|| {
            merr(
                "MCP_CONFIG_PARSE",
                format!("[mcp.servers.{name}] is not a table"),
            )
        })?;
        // Materialize the canonical row on first enable so the launcher has
        // a complete shape to spawn: never leave a bare `enabled = true`
        // with no command behind. This is the one canonical writer - the
        // dashboard path uses it too, so both surfaces produce
        // byte-identical tables.
        if table.is_empty() {
            materialize_recipe_table(table, &server);
        }
        table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
        let text = toml::to_string_pretty(&doc)
            .map_err(|e| merr("MCP_CONFIG_WRITE", format!("serialize: {e}")))?;
        // Atomic: write tmp then rename, so a crash never leaves half a config.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                merr(
                    "MCP_CONFIG_WRITE",
                    format!("create {}: {e}", parent.display()),
                )
            })?;
        }
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)
            .and_then(|_| std::fs::rename(&tmp, &path))
            .map_err(|e| merr("MCP_CONFIG_WRITE", format!("write {}: {e}", path.display())))?;
        Ok(())
    }

    // Small deterministic invariant tests only. Filesystem behavior
    // (set_enabled round-trip, config preservation, catalog-name rejection)
    // is covered in `pantheon-eval` (`eval/tests/mcp_enablement.rs`).

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cloudflare_materializes_oauth_and_docs_materializes_none() {
            let cf = find("cloudflare").expect("cloudflare recipe");
            assert_eq!(cf.to_config_entry().auth.as_deref(), Some("oauth"));
            let docs = find("cloudflare-docs").expect("cloudflare-docs recipe");
            assert_eq!(docs.to_config_entry().auth, None);
        }
    }
}
