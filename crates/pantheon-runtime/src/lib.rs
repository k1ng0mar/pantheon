//! Supervisor: run lifecycle, quotas, recovery, checkpointing (D decision).
//! Runs persist every event; a killed run resumes as RunRecovered.
pub mod operation;
pub mod pipeline;
pub mod pipeline_runner;
pub mod session;
pub mod watchdog;

pub use operation::{
    run_tool_operation, DurableOperationRunner, JsonToolAdapter, ToolOperationAdapter,
};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::events::Event;
pub use pantheon_storage::LostLeaseError;
use pantheon_storage::{
    Ledger, Operation, OperationStatus, OperationStore, RunLease, RunLeaseStore,
};
pub use pipeline::{run_model_stage, StageEvaluator, StageExecutor};
pub use pipeline_runner::{PipelineOutcome, PipelineRunner};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A subscriber to the run's event stream. Cheap to clone, shared across threads.
pub type EventObserver = std::sync::Arc<dyn Fn(&Event) + Send + Sync>;
pub use watchdog::{TurnWatchdog, WatchdogAction};

fn rerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Runtime,
        false,
        cause,
        "check runtime state and ledger",
        "",
    )
}

/// Supervisor handle. Cheap to clone, safe to share.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<SupervisorInner>,
}

/// Guard returned by `register_observer`. Removes the observer on drop
/// by pointer identity, so multiple observers coexist safely.
pub struct ObserverGuard {
    supervisor: Supervisor,
    cb: std::sync::Arc<dyn Fn(&Event) + Send + Sync>,
}

impl Drop for ObserverGuard {
    fn drop(&mut self) {
        if let Ok(mut observers) = self.supervisor.inner.observers.lock() {
            if let Some(pos) = observers.iter().position(|o| Arc::ptr_eq(o, &self.cb)) {
                observers.remove(pos);
            }
        }
    }
}

struct SupervisorInner {
    ledger: Ledger,
    operations: OperationStore,
    leases: RunLeaseStore,
    /// Session search index (FTS5 sidecar in the ledger file). Every
    /// emitted message/tool/title event is chunked and indexed here so
    /// the `session_search` tool can find past work without a model call.
    search: std::sync::Arc<pantheon_storage::search::SessionSearch>,
    /// Embeddings client for the vector recall layer. `None` until
    /// `set_embedder` attaches the policy-resolved client (Session::new);
    /// before that indexing falls back to the local hashing embedder.
    embedder: std::sync::Mutex<Option<std::sync::Arc<pantheon_providers::embeddings::EmbedClient>>>,
    data_dir: PathBuf,
    lease_id: String,
    /// Live observers (TUI, gateway, tests). Called after each successful
    /// ledger append. Registration is via `register_observer` on the
    /// handle; the list itself lives behind a mutex because registration
    /// happens while runs are already in flight.
    observers: std::sync::Mutex<Vec<EventObserver>>,
}

/// Holds a run lease until the work scope exits.  Dropping the guard is the
/// normal release path; a hard crash intentionally leaves the row behind so
/// another supervisor can take it over only after expiry.
pub struct RunLeaseGuard {
    supervisor: Supervisor,
    run_id: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    lost: Arc<std::sync::atomic::AtomicBool>,
    heartbeat: Option<std::thread::JoinHandle<()>>,
}

impl RunLeaseGuard {
    /// Acquire a run lease or fail. There is deliberately no panicking
    /// constructor: a busy lease is a normal operational condition, not a
    /// programmer error, and callers must handle it.
    pub fn try_new(
        supervisor: Supervisor,
        run_id: impl Into<String>,
    ) -> Result<Self, PantheonError> {
        let run_id = run_id.into();
        supervisor.assert_lease(&run_id).map_err(|e| {
            PantheonError::new(
                "LOST_LEASE",
                Layer::Runtime,
                true,
                e.to_string(),
                "acquire the run lease before starting work",
                "",
            )
        })?;
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interval =
            Duration::from_millis((supervisor.lease_ttl_ms() / 3).clamp(50, 1_000) as u64);
        let hb_supervisor = supervisor.clone();
        let hb_run_id = run_id.clone();
        let hb_stop = Arc::clone(&stop);
        let hb_lost = Arc::clone(&lost);
        let heartbeat = std::thread::spawn(move || {
            while !hb_stop.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(interval);
                if hb_stop.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                if hb_supervisor.renew_lease(&hb_run_id).is_err() {
                    hb_lost.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                // The ledger is the cancellation mailbox. A request from a
                // different Supervisor can safely ask the lease holder to stop
                // its process groups without that requester ever signaling a
                // potentially reused PGID.
                if matches!(
                    hb_supervisor
                        .ledger_status(&hb_run_id)
                        .ok()
                        .flatten()
                        .as_deref(),
                    Some("canceled")
                ) {
                    let _ = hb_supervisor.terminate_owned_process_groups(&hb_run_id);
                    let _ = hb_supervisor.settle_run_operation_cancellations(
                        &hb_run_id,
                        "run cancellation observed by lease holder",
                    );
                }
            }
        });
        Ok(Self {
            supervisor,
            run_id,
            stop,
            lost,
            heartbeat: Some(heartbeat),
        })
    }
    pub fn run_id(&self) -> &str {
        &self.run_id
    }
    pub fn is_healthy(&self) -> bool {
        !self.lost.load(std::sync::atomic::Ordering::Acquire)
            && self.supervisor.assert_lease(&self.run_id).is_ok()
    }
    pub fn health_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.lost)
    }
}

impl Drop for RunLeaseGuard {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(handle) = self.heartbeat.take() {
            let _ = handle.join();
        }
        // Do not signal a persisted PGID after losing the lease: the number may
        // have been reused by a replacement supervisor.
        if self.supervisor.assert_lease(&self.run_id).is_ok() {
            let _ = self.supervisor.terminate_owned_process_groups(&self.run_id);
        }
        let _ = self.supervisor.release_lease(&self.run_id);
    }
}

impl Supervisor {
    pub fn open(data_dir: PathBuf) -> Result<Self, PantheonError> {
        std::fs::create_dir_all(&data_dir).map_err(|e| rerr("RT_MKDIR", e.to_string()))?;
        let ledger = Ledger::open(&data_dir.join("ledger.db"))?;
        // Keep operations and leases in the same SQLite file as the event
        // ledger. Separate connections are safe because each store owns its
        // mutex and SQLite provides the cross-connection CAS semantics.
        let operations = OperationStore::open(&data_dir.join("ledger.db"))?;
        let leases = RunLeaseStore::open(&data_dir.join("ledger.db"))?;
        let search = std::sync::Arc::new(pantheon_storage::search::SessionSearch::open(
            &data_dir.join("ledger.db"),
        )?);
        let lease_id = format!("lease_{}_{}", std::process::id(), new_run_id());
        Ok(Self {
            inner: Arc::new(SupervisorInner {
                ledger,
                operations,
                leases,
                data_dir,
                lease_id,
                search,
                embedder: std::sync::Mutex::new(None),
                observers: std::sync::Mutex::new(Vec::new()),
            }),
        })
    }
    /// Shared handle to the session search index (for tool wiring).
    pub fn shared_search(&self) -> std::sync::Arc<pantheon_storage::search::SessionSearch> {
        std::sync::Arc::clone(&self.inner.search)
    }

    /// The embeddings client used to index chunks, or None when no
    /// embeddings auxiliary is configured. The search tool needs the same
    /// client to embed a query: indexing and querying with different
    /// embedders would make the vector scores meaningless.
    pub fn shared_embedder(
        &self,
    ) -> Option<std::sync::Arc<pantheon_providers::embeddings::EmbedClient>> {
        self.inner.embedder.lock().ok().and_then(|g| g.clone())
    }

    /// Attach the embeddings client resolved from the session's model
    /// policy. Called by `Session::new`; before that, indexing falls back
    /// to the local hashing embedder.
    pub fn set_embedder(&self, client: pantheon_providers::embeddings::EmbedClient) {
        if let Ok(mut slot) = self.inner.embedder.lock() {
            *slot = Some(std::sync::Arc::new(client));
        }
    }
    fn ledger(&self) -> &Ledger {
        &self.inner.ledger
    }
    pub fn data_dir(&self) -> &PathBuf {
        &self.inner.data_dir
    }

    /// The stable lease identity owned by this supervisor handle.
    pub fn lease_id(&self) -> &str {
        &self.inner.lease_id
    }

    /// Default lease lifetime.  It is intentionally short enough for a
    /// crashed process to be recovered by the next session.
    pub fn lease_ttl_ms(&self) -> i64 {
        std::env::var("PANTHEON_RUN_LEASE_TTL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30_000)
    }

    pub fn operations(&self) -> &OperationStore {
        &self.inner.operations
    }

    pub fn acquire_lease(&self, run_id: &str) -> Result<RunLease, PantheonError> {
        self.inner
            .leases
            .acquire(run_id, &self.inner.lease_id, self.lease_ttl_ms())?
            .ok_or_else(|| {
                rerr(
                    "RT_LEASE_BUSY",
                    format!("run {run_id} is owned by another supervisor"),
                )
            })
    }

    pub fn renew_lease(&self, run_id: &str) -> Result<RunLease, PantheonError> {
        self.inner
            .leases
            .renew(run_id, &self.inner.lease_id, self.lease_ttl_ms())
    }

    pub fn release_lease(&self, run_id: &str) -> Result<bool, PantheonError> {
        self.inner.leases.release(run_id, &self.inner.lease_id)
    }

    pub fn assert_lease(&self, run_id: &str) -> Result<RunLease, LostLeaseError> {
        self.inner.leases.assert_owned(run_id, &self.inner.lease_id)
    }

    /// Persist cancellation intent, terminate every process group owned by
    /// the run lease, then settle the operation as canceled. This is the
    /// operation-level form of `cancel_run`.
    pub fn cancel_operation_with_groups(
        &self,
        operation_id: &str,
        run_id: &str,
        reason: impl Into<String>,
    ) -> Result<Operation, PantheonError> {
        let reason = reason.into();
        let current = self.inner.operations.get(operation_id)?.ok_or_else(|| {
            rerr(
                "OPERATION_NOT_FOUND",
                format!("operation {operation_id} not found"),
            )
        })?;
        let linked_run = current
            .state
            .get("request")
            .and_then(|request| request.get("run_id"))
            .and_then(|run_id| run_id.as_str())
            == Some(run_id)
            || operation_id.starts_with(&format!("{run_id}:"));
        if !linked_run {
            return Err(rerr(
                "OPERATION_RUN_MISMATCH",
                format!("operation {operation_id} is not owned by run {run_id}"),
            ));
        }
        let canceling = self.cancel_operation(operation_id, reason.clone())?;
        if canceling.status.is_terminal() {
            return Ok(canceling);
        }
        self.terminate_owned_process_groups(run_id)?;
        let mut state = canceling.state.as_object().cloned().unwrap_or_default();
        state.insert("phase".into(), serde_json::Value::String("canceled".into()));
        state.insert("reason".into(), serde_json::Value::String(reason));
        self.inner.operations.cancel(
            operation_id,
            canceling.version,
            serde_json::Value::Object(state),
        )
    }

    /// Start a run only after winning its lease.  Callers doing actual work
    /// should use this instead of `start_run`.
    pub fn start_run_with_lease(&self, run_id: &str) -> Result<(bool, RunLease), PantheonError> {
        let lease = self.acquire_lease(run_id)?;
        match self.start_run(run_id) {
            Ok(recovered) => Ok((recovered, lease)),
            Err(error) => {
                let _ = self.release_lease(run_id);
                Err(error)
            }
        }
    }

    pub fn create_operation(
        &self,
        id: impl Into<String>,
        operation_type: impl Into<String>,
        state: serde_json::Value,
    ) -> Result<Operation, PantheonError> {
        self.inner.operations.create(id, operation_type, state)
    }

    pub fn operation(&self, id: &str) -> Result<Option<Operation>, PantheonError> {
        self.inner.operations.get(id)
    }

    /// Persist cancellation intent before a caller performs process-group
    /// termination. A second call settles the already-canceling operation.
    pub fn cancel_operation(
        &self,
        id: &str,
        reason: impl Into<String>,
    ) -> Result<Operation, PantheonError> {
        let op = self
            .inner
            .operations
            .get(id)?
            .ok_or_else(|| rerr("OPERATION_NOT_FOUND", format!("operation {id} not found")))?;
        if op.status.is_terminal() {
            return Ok(op);
        }
        let mut state = op.state.as_object().cloned().unwrap_or_default();
        state.insert(
            "cancel_reason".into(),
            serde_json::Value::String(reason.into()),
        );
        let state = serde_json::Value::Object(state);
        if op.status == OperationStatus::Canceling {
            Ok(op)
        } else {
            self.inner.operations.request_cancel(id, op.version, state)
        }
    }
    /// Start a run. If a previous ledger shows it unfinished, emit RunRecovered.
    /// A run parked on approval is NOT recoverable into a fresh chat: the
    /// caller must resume() or grant() first, never start over it.
    ///
    /// Goes through `emit`, not a bare ledger append, so live observers (TUI,
    /// the hook bridge) see the run boundary. Appending directly would make
    /// `RunStarted` invisible to every subscriber while still durable — the
    /// exact "durable but unobserved" split the observer contract forbids.
    pub fn start_run(&self, run_id: &str) -> Result<bool, PantheonError> {
        let status = self.ledger().status(run_id)?;
        let recovered = matches!(status.as_deref(), Some("running"));
        self.emit(Event::RunStarted {
            run_id: run_id.into(),
        })?;
        if recovered {
            self.emit(Event::RunRecovered {
                run_id: run_id.into(),
            })?;
        }
        Ok(recovered)
    }
    pub fn emit(&self, ev: Event) -> Result<(), PantheonError> {
        self.ledger().append(&ev)?;
        // Index for session search (best-effort: a failed index write must
        // never break the run, same contract as observers).
        if let Err(e) = self.index_for_search(&ev) {
            eprintln!("session search index: {e}");
        }
        // Fan out to live observers after the durable write. Observer
        // failures must never break the run: the ledger is the contract,
        // observers are best-effort views.
        if let Ok(observers) = self.inner.observers.lock() {
            for cb in observers.iter() {
                cb(&ev);
            }
        }
        Ok(())
    }
    /// Chunk-and-index one event into the session search sidecar. Only
    /// content-bearing events are indexed; bookkeeping events (started,
    /// completed, approvals) carry no searchable text.
    fn index_for_search(&self, ev: &Event) -> Result<(), PantheonError> {
        use pantheon_core::events::Event as E;
        use pantheon_providers::embeddings::EmbedderClient;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let seq = self.ledger().max_seq()?;
        let (run_id, kind, text) = match ev {
            E::AssistantMessage { run_id, message } => (run_id, "message", message.content.clone()),
            E::ToolMessage { run_id, message } => (run_id, "tool", message.content.clone()),
            E::SessionTitled { run_id, title, .. } => (run_id, "title", title.clone()),
            _ => return Ok(()),
        };
        let chunk = pantheon_storage::search::SessionChunk {
            chunk_id: format!("{run_id}:{seq}:{kind}"),
            run_id: run_id.clone(),
            seq,
            kind: kind.into(),
            text: text.clone(),
            ts_ms: ts,
        };
        // Embed via the attached client (policy-resolved auxiliary), or
        // the local hashing embedder when none is set. Either way the
        // vector layer gets real vectors; a failed embed degrades this
        // chunk to lexical-only — indexing never breaks the run.
        let attached = self
            .inner
            .embedder
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(std::sync::Arc::clone));
        let embedding = match attached {
            Some(client) => client
                .embed(std::slice::from_ref(&text))
                .ok()
                .and_then(|mut v| v.drain(..).next())
                .map(|e| e.vec),
            None => pantheon_providers::embeddings::EmbedClient::local()
                .embed(std::slice::from_ref(&text))
                .ok()
                .and_then(|mut v| v.drain(..).next())
                .map(|e| e.vec),
        };
        self.inner
            .search
            .index_with_embedding(&chunk, embedding.as_deref())
    }

    /// Register a live event observer (TUI, gateway). Returns a handle
    /// that removes the observer when dropped.
    ///
    /// # Locking contract — read before writing an observer
    ///
    /// Observers are invoked **while the supervisor's observer lock is
    /// held** (see `emit`). An observer callback therefore MUST:
    ///
    /// * be fast and non-blocking (the only current observer is a
    ///   non-blocking `mpsc::Sender::send` into the TUI event loop),
    /// * never block on a mutex another thread holds,
    /// * never call back into the `Supervisor` (`emit`, `register_observer`,
    ///   dropping its own `ObserverGuard`, ...) — `std::sync::Mutex` is not
    ///   reentrant, so this self-deadlocks,
    /// * never panic across the FFI-free boundary in a way that poisons the
    ///   lock for every later emit.
    ///
    /// A blocking observer would freeze the emitting thread, which is the
    /// agent loop worker, not the UI. Do not turn this into a synchronous
    /// callback path; if you need work done off the emit path, queue it and
    /// process it elsewhere.
    pub fn register_observer(
        &self,
        cb: std::sync::Arc<dyn Fn(&Event) + Send + Sync>,
    ) -> ObserverGuard {
        if let Ok(mut observers) = self.inner.observers.lock() {
            observers.push(cb.clone());
        }
        ObserverGuard {
            supervisor: self.clone(),
            cb,
        }
    }
    /// Terminal run boundary. Routed through `emit` so `on_session_end` (and
    /// the TUI) observe completion; a bare append would make the run finish
    /// durably while every live subscriber still believed it was running.
    pub fn complete(&self, run_id: &str) -> Result<(), PantheonError> {
        self.emit(Event::RunCompleted {
            run_id: run_id.into(),
        })
    }
    pub fn fail(&self, run_id: &str, code: &str) -> Result<(), PantheonError> {
        self.emit(Event::RunFailed {
            run_id: run_id.into(),
            code: code.into(),
        })
    }

    /// Record an approval grant for a parked run. Emits ApprovalGranted and
    /// flips the run back to running so resume() can continue it.
    pub fn grant(&self, run_id: &str, scope: &str) -> Result<(), PantheonError> {
        match self.ledger().status(run_id)?.as_deref() {
            Some("awaiting_approval") => {}
            Some(other) => {
                return Err(rerr(
                    "RT_NOT_PARKED",
                    format!("run {run_id} is {other}, not parked on approval"),
                ));
            }
            None => {
                return Err(rerr("RT_NO_RUN", format!("no run {run_id} in ledger")));
            }
        }
        let entries = self.ledger().replay(run_id)?;
        let mut requested = false;
        for e in &entries {
            if let Event::ApprovalRequested { scope: s, .. } = &e.event {
                if s == scope {
                    requested = true;
                    break;
                }
            }
        }
        if !requested {
            return Err(rerr(
                "RT_APPROVAL_UNKNOWN",
                format!("run {run_id} has no pending approval for scope {scope}"),
            ));
        }
        if entries.iter().any(|entry| {
            matches!(&entry.event, Event::ApprovalGranted { scope: s, .. } | Event::ApprovalDenied { scope: s, .. } if s == scope)
        }) {
            return Err(rerr(
                "RT_APPROVAL_RESOLVED",
                format!("approval scope {scope} was already resolved"),
            ));
        }
        self.ledger().append(&Event::ApprovalGranted {
            run_id: run_id.into(),
            scope: scope.into(),
        })?;
        self.ledger().append(&Event::RunProgress {
            run_id: run_id.into(),
            detail: format!("approval granted for {scope}, run resumable"),
        })?;
        Ok(())
    }
    /// Deny one approval scope and leave the run alive.  This is intentionally
    /// different from `fail`: a scoped denial may be followed by another tool
    /// call, a model explanation, or a normal completion.
    pub fn deny(&self, run_id: &str, scope: &str) -> Result<(), PantheonError> {
        if scope.is_empty() {
            return Err(rerr(
                "RT_APPROVAL_SCOPE",
                "approval scope is required".into(),
            ));
        }
        match self.ledger().status(run_id)?.as_deref() {
            Some("awaiting_approval") => {}
            Some(other) => {
                return Err(rerr(
                    "RT_NOT_PARKED",
                    format!("run {run_id} is {other}, not parked on approval"),
                ))
            }
            None => return Err(rerr("RT_NO_RUN", format!("no run {run_id} in ledger"))),
        }
        let entries = self.ledger().replay(run_id)?;
        if !entries
            .iter()
            .any(|e| matches!(&e.event, Event::ApprovalRequested { scope: s, .. } if s == scope))
        {
            return Err(rerr(
                "RT_APPROVAL_UNKNOWN",
                format!("run {run_id} has no pending approval for scope {scope}"),
            ));
        }
        if entries.iter().any(|entry| {
            matches!(&entry.event, Event::ApprovalGranted { scope: s, .. } | Event::ApprovalDenied { scope: s, .. } if s == scope)
        }) {
            return Err(rerr(
                "RT_APPROVAL_RESOLVED",
                format!("approval scope {scope} was already resolved"),
            ));
        }
        self.ledger().append(&Event::ApprovalDenied {
            run_id: run_id.into(),
            scope: scope.into(),
        })?;
        self.ledger().append(&Event::RunProgress {
            run_id: run_id.into(),
            detail: format!("approval denied for {scope}; run remains active"),
        })?;
        Ok(())
    }

    pub fn register_process_group(&self, run_id: &str, pgid: i32) -> Result<(), PantheonError> {
        self.assert_lease_owned(run_id)?;
        self.ledger()
            .register_process_group(run_id, pgid, &self.inner.lease_id)
    }

    pub fn process_groups(&self, run_id: &str) -> Result<Vec<i32>, PantheonError> {
        self.ledger().process_groups(run_id, &self.inner.lease_id)
    }

    fn operation_belongs_to_run(&self, operation: &Operation, run_id: &str) -> bool {
        operation
            .state
            .get("request")
            .and_then(|request| request.get("run_id"))
            .and_then(|value| value.as_str())
            == Some(run_id)
            || operation.id.starts_with(&format!("{run_id}:"))
    }

    fn request_run_operation_cancellations(
        &self,
        run_id: &str,
        reason: &str,
    ) -> Result<(), PantheonError> {
        for operation in self.inner.operations.list(None)? {
            if !self.operation_belongs_to_run(&operation, run_id)
                || operation.status.is_terminal()
                || operation.status == OperationStatus::Canceling
            {
                continue;
            }
            let mut state = operation.state.as_object().cloned().unwrap_or_default();
            state.insert(
                "cancel_reason".into(),
                serde_json::Value::String(reason.to_string()),
            );
            if let Err(error) = self.inner.operations.request_cancel(
                &operation.id,
                operation.version,
                serde_json::Value::Object(state),
            ) {
                if error.code != "OPERATION_CONFLICT" {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn settle_run_operation_cancellations(
        &self,
        run_id: &str,
        reason: &str,
    ) -> Result<(), PantheonError> {
        for operation in self.inner.operations.list(None)? {
            if !self.operation_belongs_to_run(&operation, run_id)
                || operation.status != OperationStatus::Canceling
            {
                continue;
            }
            let mut state = operation.state.as_object().cloned().unwrap_or_default();
            state.insert("phase".into(), serde_json::Value::String("canceled".into()));
            state.insert(
                "reason".into(),
                serde_json::Value::String(reason.to_string()),
            );
            if let Err(error) = self.inner.operations.cancel(
                &operation.id,
                operation.version,
                serde_json::Value::Object(state),
            ) {
                if error.code != "OPERATION_CONFLICT" {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Terminate only groups registered by this supervisor's currently live
    /// lease. The lease is checked again before every signal to prevent PID
    /// reuse after a handoff.
    pub fn terminate_owned_process_groups(&self, run_id: &str) -> Result<(), PantheonError> {
        self.assert_lease_owned(run_id)?;
        let groups = self.process_groups(run_id)?;
        for pgid in groups {
            self.assert_lease_owned(run_id)?;
            if let Some(group) = pantheon_exec::process::ProcessGroup::new(pgid) {
                group.terminate(std::time::Duration::from_secs(5));
            }
            self.ledger()
                .unregister_process_group_owned(run_id, pgid, &self.inner.lease_id)?;
        }
        Ok(())
    }

    fn assert_lease_owned(&self, run_id: &str) -> Result<(), PantheonError> {
        self.assert_lease(run_id).map(|_| ()).map_err(|e| {
            PantheonError::new(
                "LOST_LEASE",
                Layer::Runtime,
                true,
                e.to_string(),
                "stop work and reacquire the run lease",
                "",
            )
        })
    }

    /// Cancel a run and all process groups registered by its current lease.
    /// TERM/KILL escalation is performed only after the cancellation intent is
    /// recorded in the ledger.
    /// Phase 1 of cancellation: record intent (RunCanceled event + operation
    /// cancel requests) without touching any process. Safe to call from a
    /// request handler thread; it only writes to the ledger.
    pub fn cancel_run_intent(&self, run_id: &str, reason: &str) -> Result<(), PantheonError> {
        match self.ledger().status(run_id)?.as_deref() {
            Some("canceled") => {
                self.settle_run_operation_cancellations(run_id, reason)?;
                return Ok(());
            }
            Some("completed") | Some("failed") => {
                return Err(rerr(
                    "RT_TERMINAL",
                    format!("run {run_id} is already terminal"),
                ));
            }
            Some(_) => {}
            None => return Err(rerr("RT_NO_RUN", format!("no run {run_id} in ledger"))),
        }
        self.emit(Event::RunCanceled {
            run_id: run_id.into(),
            reason: reason.to_string(),
        })?;
        self.request_run_operation_cancellations(run_id, reason)?;
        Ok(())
    }

    /// Phase 2 of cancellation: terminate process groups owned by this
    /// supervisor's lease and settle the canceled operations. Blocking (up
    /// to the TERM grace per group); call from a worker thread.
    pub fn finish_cancel(&self, run_id: &str, reason: &str) -> Result<(), PantheonError> {
        if self.assert_lease_owned(run_id).is_ok() {
            self.terminate_owned_process_groups(run_id)?;
            self.settle_run_operation_cancellations(run_id, reason)?;
        }
        Ok(())
    }

    /// Cancel a run and all process groups registered by its current lease.
    /// TERM/KILL escalation is performed only after the cancellation intent is
    /// recorded in the ledger. Blocking form; prefer the two-phase
    /// `cancel_run_intent` + `finish_cancel` from request handlers.
    pub fn cancel_run(&self, run_id: &str, reason: &str) -> Result<(), PantheonError> {
        self.cancel_run_intent(run_id, reason)?;
        self.finish_cancel(run_id, "process groups terminated")
    }

    pub fn explain(&self, run_id: &str) -> Result<String, PantheonError> {
        self.ledger().explain(run_id)
    }

    pub fn ledger_status(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().status(run_id)
    }
    pub fn ledger_list_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<pantheon_storage::RunListing>, PantheonError> {
        self.ledger().list_runs(limit)
    }
    /// Current display title of a run (`None` = never titled).
    pub fn ledger_title(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().run_title(run_id)
    }
    pub fn ledger_reopen_run(&self, run_id: &str) -> Result<bool, PantheonError> {
        self.ledger().reopen_run(run_id)
    }
    pub fn replay(
        &self,
        run_id: &str,
    ) -> Result<Vec<pantheon_storage::LedgerEntry>, PantheonError> {
        self.ledger().replay(run_id)
    }
    pub fn max_seq(&self) -> Result<i64, PantheonError> {
        self.ledger().max_seq()
    }
    pub fn put_artifact(
        &self,
        task_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<pantheon_storage::Artifact, PantheonError> {
        self.ledger().put_artifact(task_id, mime, bytes)
    }
    pub fn artifact(
        &self,
        task_id: &str,
    ) -> Result<Option<pantheon_storage::Artifact>, PantheonError> {
        self.ledger().artifact(task_id)
    }
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn new_scoped_id(prefix: &str) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let sequence = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let n = (sequence as u32).wrapping_add(std::process::id()) % 10_000;
    format!("{prefix}_{ms}_{n:04}")
}

/// Run IDs: run_<epochms>_<process-local sequence>.
pub fn new_run_id() -> String {
    new_scoped_id("run")
}

/// Turn IDs are host-assigned and stable across streaming/parking/recovery.
pub fn new_turn_id() -> String {
    new_scoped_id("turn")
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
