//! Supervisor: run lifecycle, quotas, recovery, checkpointing (D decision).
//! Runs persist every event; a killed run resumes as RunRecovered.
//!
//! Owns the **Runtime API** (ARCHITECTURE §18): the JSON-RPC command
//! surface (`rpc`, `agui_serve`) and the AG-UI streaming path
//! (`agui`) moved here from `pantheon-api`, which is now the bottom
//! protocol-leaf crate (commands, events, types).

/// Best-effort stderr diagnostic. Rust's `eprintln!` panics when the
/// write fails (closed pipe, full disk); on a long-lived server that
/// panic lands in a request thread and kills the connection. This
/// never panics: the message is dropped when stderr is unwritable.
/// Defined before the modules so every child module sees it.
macro_rules! log_warn {
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($arg)*);
    }};
}

pub mod agent_runtime;
pub mod agui;
pub mod agui_serve;
pub mod computer;
pub mod delegate_budget;
pub mod judge;
pub mod nightly_tools;
pub mod operation;
pub mod pipeline;
pub mod pipeline_runner;
pub mod rpc;
pub mod session;
pub mod skill_exec_tool;
pub mod swarm;
pub mod swarm_exec;
pub mod temporal;
pub mod tool_config;
pub mod watchdog;

pub use agent_runtime::{profile_err, AgentRuntime, DEFAULT_PROFILE};
pub use agui::{
    dispatcher_for, dispatcher_for_with_hint, dispatcher_for_with_hint_and_base,
    dispatcher_for_with_hint_and_host,
};
pub use agui_serve::{remember_thread, snapshot_frames};
pub use operation::{
    run_tool_operation, DurableOperationRunner, JsonToolAdapter, ToolOperationAdapter,
};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::events::Event;
pub use pantheon_storage::LostLeaseError;
use pantheon_storage::{
    Ledger, Operation, OperationStatus, OperationStore, RunLease, RunLeaseStore,
};
pub use pipeline::{run_model_stage, StageEvaluator, StageExecutor};
pub use pipeline_runner::{PipelineOutcome, PipelineRunner};
pub use rpc::{Dispatcher, Id, MethodHandler, Request, Response, RpcError};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
pub use tool_config::{
    browser_backend_config, build_browser_backend, resolve_browser_section, BrowserToolConfig,
    CamofoxToolConfig, WebsearchToolConfig,
};

/// A subscriber to the run's event stream. Cheap to clone, shared across threads.
pub type EventObserver = std::sync::Arc<dyn Fn(&Event) + Send + Sync>;
pub use watchdog::{TurnWatchdog, WatchdogAction};

/// Default approval time-to-live: a parked approval request expires 24h
/// after it was raised. Stale decisions are dangerous - the run's context
/// (files, world state, the operator's intent) has moved on. Override
/// with `PANTHEON_APPROVAL_TTL_MS`.
pub const APPROVAL_TTL_MS: i64 = 24 * 60 * 60 * 1000;

/// Pure expiry predicate, kept separate so it is unit-testable without a
/// ledger. `requested_ts_ms` is the ledger ts_ms of the scope's
/// `ApprovalRequested`; `now_ms` is the current unix millis.
pub fn approval_request_expired(requested_ts_ms: i64, now_ms: i64, ttl_ms: i64) -> bool {
    now_ms.saturating_sub(requested_ts_ms) > ttl_ms
}

/// The configured approval TTL: `PANTHEON_APPROVAL_TTL_MS` when set to a
/// positive integer, else [`APPROVAL_TTL_MS`].
pub fn approval_ttl_ms() -> i64 {
    std::env::var("PANTHEON_APPROVAL_TTL_MS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(APPROVAL_TTL_MS)
}

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

/// Confirm `pid` really is the turn child for `run_id`: on Linux its
/// `/proc` cmdline must contain `--taskID <run_id>`. PID numbers alone
/// are never treated as ownership - a recycled PID must not be
/// signaled. The dashboard spawns turn children as
/// `pantheon run --taskID <run_id> --say ...`, so the flag is always
/// present on a genuine turn child.
#[cfg(target_os = "linux")]
fn verify_turn_child(run_id: &str, pid: u32) -> Result<(), PantheonError> {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).map_err(|_| {
        rerr(
            "RT_NO_TURN",
            format!("turn process {pid} for run {run_id} is gone"),
        )
    })?;
    let mut args = cmdline.split(|b| *b == 0);
    let mut owned = false;
    while let Some(arg) = args.next() {
        if arg == b"--taskID" {
            owned = args.next().is_some_and(|v| v == run_id.as_bytes());
            break;
        }
    }
    if owned {
        Ok(())
    } else {
        Err(rerr(
            "RT_NO_TURN",
            format!("pid {pid} is not run {run_id}'s turn child"),
        ))
    }
}

/// Non-Linux: no `/proc` cmdline to check against. The caller recorded
/// the PID at spawn and the active-lease check in `kill_run_turn`
/// already gates the kill.
#[cfg(not(target_os = "linux"))]
fn verify_turn_child(_run_id: &str, _pid: u32) -> Result<(), PantheonError> {
    Ok(())
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

/// A pre-state hash recorded when a file-writing tool call parked for
/// approval. Used to detect stale grants: if the file's contents changed
/// between park and resume, the grant is refused.
#[derive(Debug, Clone)]
pub struct PreStateRecord {
    pub scope: String,
    pub path: String,
    pub sha256: String,
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
        let supervisor = Self {
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
        };
        supervisor.startup_recovery();
        Ok(supervisor)
    }

    /// Data dirs this process has already run startup recovery for.
    /// `Supervisor::open` happens per RPC in the AG-UI path and all over
    /// the TUI; the crash-orphan sweep is a once-per-process-per-dir
    /// affair, and the sweep itself is idempotent (only `running` runs
    /// with no live lease are settled), so a re-run would just find
    /// nothing.
    fn startup_recovered_dirs(
    ) -> &'static std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>> {
        static DIRS: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
        > = std::sync::OnceLock::new();
        DIRS.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
    }

    /// Settle crash-orphaned runs once per process per data dir: runs
    /// whose status is still `running` but whose lease has expired
    /// (the driver died mid-turn) are marked failed with a REPAIRED
    /// note so they stop showing as live in the dashboard. Live-lease
    /// runs and `awaiting_approval` parks are never touched (see
    /// [`Ledger::settle_expired_runs`]).
    ///
    /// Best-effort: a failed sweep must never prevent the daemon from
    /// starting, so errors are logged and dropped. This is the storage
    /// half of startup recovery; the gateway daemon startup also calls
    /// [`Supervisor::settle_expired_runs`] directly for an explicit
    /// once-at-boot sweep.
    fn startup_recovery(&self) {
        let already = Self::startup_recovered_dirs()
            .lock()
            .map(|mut set| !set.insert(self.inner.data_dir.clone()))
            .unwrap_or(true);
        if already {
            return;
        }
        match self.settle_expired_runs() {
            Ok(settled) if !settled.is_empty() => {
                eprintln!(
                    "startup recovery: settled {} crash-orphaned run(s): {}",
                    settled.len(),
                    settled.join(", ")
                );
            }
            Err(e) => {
                eprintln!("startup recovery: settle_expired_runs failed: {e}");
            }
            _ => {}
        }
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
    /// `RunStarted` invisible to every subscriber while still durable - the
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
        let entry = self.ledger().append(&ev)?;
        // Index for session search (best-effort: a failed index write must
        // never break the run, same contract as observers). The chunk is
        // indexed under the seq append() returned for this event - never
        // recomputed via max_seq(), which would race with concurrent
        // writers and index the text under another chunk's id.
        if let Err(e) = self.index_for_search(&ev, entry.seq) {
            log_warn!("session search index: {e}");
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
    ///
    /// `seq` must be the seq `Ledger::append` returned for this event. It
    /// is a parameter (not recomputed via `max_seq()`) because recomputing
    /// races with concurrent writers: another chunk can land between the
    /// append and the index write, silently indexing the text under the
    /// wrong chunk id.
    fn index_for_search(&self, ev: &Event, seq: i64) -> Result<(), PantheonError> {
        use pantheon_api::events::Event as E;
        use pantheon_providers::embeddings::EmbedderClient;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
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
        // chunk to lexical-only - indexing never breaks the run.
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
    /// # Locking contract - read before writing an observer
    ///
    /// Observers are invoked **while the supervisor's observer lock is
    /// held** (see `emit`). An observer callback therefore MUST:
    ///
    /// * be fast and non-blocking (the only current observer is a
    ///   non-blocking `mpsc::Sender::send` into the TUI event loop),
    /// * never block on a mutex another thread holds,
    /// * never call back into the `Supervisor` (`emit`, `register_observer`,
    ///   dropping its own `ObserverGuard`, ...) - `std::sync::Mutex` is not
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

    /// Atomically read and clear the run's queued message slot: one drain,
    /// one consumer. Used by the queue auto-drain (CLI `run` loops) and the
    /// dashboard's idle-path send.
    pub fn take_queued_message(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().take_queued_message(run_id)
    }

    /// Peek the run's queued-message list (FIFO, oldest first) without
    /// removing anything. The AG-UI turn worker peeks the head into the
    /// turn input and pops it via [`Self::take_queued_message`] once the
    /// turn has started, so a turn that fails to start never eats it.
    pub fn queued_messages(&self, run_id: &str) -> Result<Vec<String>, PantheonError> {
        self.ledger().queued_messages(run_id)
    }

    /// One replay-scan for the approval surface. Returns `(requested,
    /// resolved)`: every `ApprovalRequested` scope in request order, and
    /// the set of scopes that have since been granted or denied. A scope
    /// is pending when requested and never resolved.
    ///
    /// Shared by `grant`, `deny`, and `pending_approvals`, which had
    /// drifted into three copy-pasted scans of the same events.
    fn approval_scan(
        entries: &[pantheon_storage::LedgerEntry],
    ) -> (Vec<&str>, std::collections::HashSet<&str>) {
        let mut requested: Vec<&str> = Vec::new();
        let mut resolved: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for e in entries {
            match &e.event {
                Event::ApprovalRequested { scope, .. } => requested.push(scope),
                Event::ApprovalGranted { scope, .. } | Event::ApprovalDenied { scope, .. } => {
                    resolved.insert(scope);
                }
                _ => {}
            }
        }
        (requested, resolved)
    }

    /// Every pre-state hash recorded for this run, in emission order.
    fn prestate_scan(entries: &[pantheon_storage::LedgerEntry]) -> Vec<PreStateRecord> {
        entries
            .iter()
            .filter_map(|e| match &e.event {
                Event::PreStateRecorded {
                    scope,
                    path,
                    sha256,
                    ..
                } => Some(PreStateRecord {
                    scope: scope.clone(),
                    path: path.clone(),
                    sha256: sha256.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// The run must currently be parked on approval. Shared by `grant` and
    /// `deny`; `pending_approvals` is read-only and intentionally skips it.
    fn require_parked(&self, run_id: &str) -> Result<(), PantheonError> {
        match self.ledger().status(run_id)?.as_deref() {
            Some("awaiting_approval") => Ok(()),
            Some(other) => Err(rerr(
                "RT_NOT_PARKED",
                format!("run {run_id} is {other}, not parked on approval"),
            )),
            None => Err(rerr("RT_NO_RUN", format!("no run {run_id} in ledger"))),
        }
    }

    /// A parked approval expires `PANTHEON_APPROVAL_TTL_MS` (default
    /// [`APPROVAL_TTL_MS`]) after it was requested. Deciding on a stale
    /// scope is refused: the run's context has moved on, so the operator
    /// must send a new message and let the agent re-request. Called after
    /// the unknown/resolved checks so the error is specifically "expired".
    fn require_approval_fresh(
        run_id: &str,
        scope: &str,
        entries: &[pantheon_storage::LedgerEntry],
    ) -> Result<(), PantheonError> {
        let requested_ts = entries.iter().rev().find_map(|e| match &e.event {
            Event::ApprovalRequested { scope: s, .. } if s == scope => Some(e.ts_ms),
            _ => None,
        });
        let now = pantheon_api::logging::now_ms();
        match requested_ts {
            Some(ts) if approval_request_expired(ts, now, approval_ttl_ms()) => Err(rerr(
                "RT_APPROVAL_EXPIRED",
                format!(
                    "approval scope {scope} on run {run_id} expired; send a new message so the agent can request approval again"
                ),
            )),
            _ => Ok(()),
        }
    }

    /// Record an approval grant for a parked run. Emits ApprovalGranted and
    /// flips the run back to running so resume() can continue it.
    pub fn grant(&self, run_id: &str, scope: &str) -> Result<(), PantheonError> {
        self.require_parked(run_id)?;
        let entries = self.ledger().replay(run_id)?;
        let (requested, resolved) = Self::approval_scan(&entries);
        if !requested.contains(&scope) {
            return Err(rerr(
                "RT_APPROVAL_UNKNOWN",
                format!("run {run_id} has no pending approval for scope {scope}"),
            ));
        }
        if resolved.contains(scope) {
            return Err(rerr(
                "RT_APPROVAL_RESOLVED",
                format!("approval scope {scope} was already resolved"),
            ));
        }
        Self::require_approval_fresh(run_id, scope, &entries)?;
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
    /// Every approval scope still awaiting an operator decision, in the
    /// order it was requested.
    ///
    /// This exists so a park message can name the exact scope instead of
    /// sending the user to a second command to find it. The scope is
    /// `call_id:tool:args` and the args are long JSON with embedded quotes,
    /// so "run the other command and copy it" was a real papercut on the
    /// approval path, not a hypothetical one.
    ///
    /// Scopes already granted or denied are filtered out, so the result is
    /// exactly what still needs a human.
    /// Look up the pre-state hash recorded for an approval scope. Returns
    /// None when no PreStateRecorded event exists for that scope (e.g. a
    /// read-only tool, or a run from before this feature).
    pub fn prestate_for_scope(
        &self,
        run_id: &str,
        scope: &str,
    ) -> Result<Option<PreStateRecord>, PantheonError> {
        let entries = self.ledger().replay(run_id)?;
        Ok(Self::prestate_scan(&entries)
            .into_iter()
            .rev()
            .find(|p| p.scope == scope))
    }

    pub fn pending_approvals(&self, run_id: &str) -> Result<Vec<String>, PantheonError> {
        let entries = self.ledger().replay(run_id)?;
        let (requested, resolved) = Self::approval_scan(&entries);
        Ok(requested
            .into_iter()
            .filter(|s| !resolved.contains(s))
            .map(str::to_string)
            .collect())
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
        self.require_parked(run_id)?;
        let entries = self.ledger().replay(run_id)?;
        let (requested, resolved) = Self::approval_scan(&entries);
        if !requested.contains(&scope) {
            return Err(rerr(
                "RT_APPROVAL_UNKNOWN",
                format!("run {run_id} has no pending approval for scope {scope}"),
            ));
        }
        if resolved.contains(scope) {
            return Err(rerr(
                "RT_APPROVAL_RESOLVED",
                format!("approval scope {scope} was already resolved"),
            ));
        }
        Self::require_approval_fresh(run_id, scope, &entries)?;
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

    /// Answer a parked `ask_user` question. Records the answer durably,
    /// appends it as the `ask_user` tool result (so the resumed turn sees
    /// it in the ledger like any completed tool call), and leaves the run
    /// resumable. Mirrors [`Supervisor::grant`].
    pub fn answer_input(
        &self,
        run_id: &str,
        call_id: &str,
        answer: &str,
    ) -> Result<(), PantheonError> {
        let entries = self.ledger().replay(run_id)?;
        let requested = entries.iter().any(
            |e| matches!(&e.event, Event::UserInputRequested { call_id: c, .. } if c == call_id),
        );
        if !requested {
            return Err(rerr(
                "RT_INPUT_UNKNOWN",
                format!("run {run_id} has no pending question for call {call_id}"),
            ));
        }
        if entries.iter().any(
            |e| matches!(&e.event, Event::UserInputProvided { call_id: c, .. } if c == call_id),
        ) {
            return Err(rerr(
                "RT_INPUT_RESOLVED",
                format!("question {call_id} was already answered"),
            ));
        }
        self.ledger().append(&Event::UserInputProvided {
            run_id: run_id.into(),
            call_id: call_id.into(),
            answer: answer.into(),
        })?;
        // The answer rides a ToolMessage so `rebuild_messages` feeds it to
        // the model as the ask_user result on resume.
        self.ledger().append(&Event::ToolMessage {
            run_id: run_id.into(),
            message: pantheon_api::message::Message::tool(call_id, answer),
        })?;
        self.ledger().append(&Event::RunProgress {
            run_id: run_id.into(),
            detail: format!("operator answered {call_id}; run resumable"),
        })?;
        Ok(())
    }

    /// Questions still awaiting an operator answer, in request order.
    /// Exists so a re-entered session re-renders the clarify card
    /// the approval equivalent is [`Supervisor::pending_approvals`].
    pub fn pending_input(
        &self,
        run_id: &str,
    ) -> Result<Vec<(String, String, Vec<String>)>, PantheonError> {
        let entries = self.ledger().replay(run_id)?;
        let mut answered: Vec<&str> = Vec::new();
        let mut out: Vec<(String, String, Vec<String>)> = Vec::new();
        for e in &entries {
            match &e.event {
                Event::UserInputRequested {
                    call_id,
                    question,
                    options,
                    ..
                } => out.push((call_id.clone(), question.clone(), options.clone())),
                Event::UserInputProvided { call_id, .. } => answered.push(call_id),
                _ => {}
            }
        }
        out.retain(|(c, _, _)| !answered.contains(&c.as_str()));
        Ok(out)
    }

    pub fn register_process_group(&self, run_id: &str, pgid: i32) -> Result<(), PantheonError> {
        self.assert_lease_owned(run_id)?;
        self.ledger()
            .register_process_group(run_id, pgid, &self.inner.lease_id)
    }

    pub fn process_groups(&self, run_id: &str) -> Result<Vec<i32>, PantheonError> {
        self.ledger().process_groups(run_id, &self.inner.lease_id)
    }

    /// Remove one process-group row the current lease registered (e.g.
    /// the `shell` tool's Exited hook). Scoped to this supervisor's
    /// lease like [`Self::register_process_group`]: the ledger's
    /// ownership check is not optional, and there is no unowned twin.
    /// Best-effort callers ignore the error - a stale row is reaped by
    /// the next cancel or overwritten by re-registration.
    pub fn unregister_process_group(&self, run_id: &str, pgid: i32) -> Result<(), PantheonError> {
        self.assert_lease_owned(run_id)?;
        self.ledger()
            .unregister_process_group_owned(run_id, pgid, &self.inner.lease_id)
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
        // Cross-thread/process channel for worker-thread turns: the AG-UI
        // server builds a fresh `Session` per RPC, so its Cancel handler
        // cannot reach the in-process cancel token on the session driving
        // the turn. The drive loop polls this flag at every turn boundary
        // and winds down cooperatively. `reopen_run` clears it when the
        // run is continued.
        self.ledger().set_cancel_intent(run_id, true)?;
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

    /// Hard-kill the run's in-flight turn child. Unlike `cancel_run`
    /// (cooperative - the loop winds down at its next boundary), this
    /// force-terminates the turn process itself: TERM, a short grace,
    /// then KILL of its whole process group.
    ///
    /// `turn_pid` is the PID recorded when the turn child was spawned
    /// (a process-group leader; see the dashboard's turn spawner). This
    /// runs in a different process than the turn, so the lease-ownership
    /// gate in `finish_cancel` cannot be used; instead the PID is
    /// identity-checked first ([`verify_turn_child`]) so a recycled PID
    /// is never signaled blindly.
    ///
    /// Blocking up to the TERM grace; call from a worker thread.
    pub fn kill_run_turn(&self, run_id: &str, turn_pid: u32) -> Result<(), PantheonError> {
        if !self.has_active_lease(run_id).unwrap_or(false) {
            return Err(rerr(
                "RT_NO_TURN",
                format!("run {run_id} has no turn in flight"),
            ));
        }
        verify_turn_child(run_id, turn_pid)?;
        // Durable intent first, so the ledger is honest even if the
        // process dies mid-write below. A still-dying canceled turn
        // passes through: the `canceled` branch of `cancel_run_intent`
        // settles operations and returns Ok.
        self.cancel_run_intent(run_id, "killed from dashboard")?;
        if let Some(group) = pantheon_exec::process::ProcessGroup::new(turn_pid as i32) {
            group.terminate(std::time::Duration::from_secs(5));
        }
        self.settle_run_operation_cancellations(run_id, "turn killed")?;
        Ok(())
    }

    pub fn render_run_log(&self, run_id: &str) -> Result<String, PantheonError> {
        self.ledger().render_run_log(run_id)
    }

    pub fn ledger_status(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().status(run_id)
    }

    /// Runs left in a non-terminal state by a crash. See
    /// [`Ledger::stuck_runs`](pantheon_storage::Ledger::stuck_runs).
    pub fn stuck_runs(&self) -> Result<Vec<pantheon_storage::RunListing>, PantheonError> {
        self.ledger().stuck_runs()
    }

    /// SQLite integrity check, verbatim. See
    /// [`Ledger::integrity_check`](pantheon_storage::Ledger::integrity_check).
    pub fn integrity_check(&self) -> Result<Vec<String>, PantheonError> {
        self.ledger().integrity_check()
    }

    /// Whether a live run lease exists for this run. A stuck run holding one
    /// is a session still working, not a corpse.
    pub fn has_active_lease(&self, run_id: &str) -> Result<bool, PantheonError> {
        self.ledger().has_active_lease(run_id)
    }

    /// Named alias of [`has_active_lease`] for the dashboard compress
    /// guard: compress must 409 while the run holds a live lease, and the
    /// dashboard leaf asked for this exact symbol. Same predicate, same
    /// fail-closed semantics.
    pub fn run_has_active_lease(&self, run_id: &str) -> Result<bool, PantheonError> {
        self.ledger().run_has_active_lease(run_id)
    }

    /// Cooperative-cancel intent for the run row, set by
    /// [`cancel_run_intent`](Self::cancel_run_intent). Worker-thread turns
    /// (AG-UI) poll this at every drive-loop turn boundary and wind down
    /// without needing a PID. Contract for the dashboard kill path: issue
    /// cancel through `cancel_run_intent` (or `cancel_run`); the flag is
    /// set there, and the turn observes it on its next boundary. Cleared
    /// by `reopen_run` when the run is continued.
    pub fn cancel_intent(&self, run_id: &str) -> Result<bool, PantheonError> {
        self.ledger().cancel_intent(run_id)
    }

    /// Force a stuck run terminal by appending real events. See
    /// [`Ledger::settle_stuck_run`](pantheon_storage::Ledger::settle_stuck_run).
    pub fn settle_stuck_run(&self, run_id: &str, reason: &str) -> Result<(), PantheonError> {
        self.ledger().settle_stuck_run(run_id, reason)
    }

    /// Settle every crash-orphaned run (status `running`, no live lease).
    /// See [`Ledger::settle_expired_runs`](pantheon_storage::Ledger::settle_expired_runs).
    /// The gateway daemon calls this once at startup so a crash mid-turn
    /// stops showing the run as live in the dashboard. Returns the settled
    /// run ids, oldest first.
    pub fn settle_expired_runs(&self) -> Result<Vec<String>, PantheonError> {
        self.ledger().settle_expired_runs()
    }
    pub fn ledger_list_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<pantheon_storage::RunListing>, PantheonError> {
        self.ledger().list_runs(limit)
    }
    /// Auto-create the permanent home session if it has no run row yet
    /// (idempotent). See
    /// [`Ledger::ensure_home_session`](pantheon_storage::Ledger::ensure_home_session).
    pub fn ensure_home_session(&self) -> Result<(), PantheonError> {
        self.ledger().ensure_home_session()
    }
    /// Current display title of a run (`None` = never titled).
    pub fn ledger_title(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().run_title(run_id)
    }
    /// The run's stored agent mode (`"plan"`/`"build"`, default `"build"`).
    /// See [`Ledger::run_mode`](pantheon_storage::Ledger::run_mode).
    pub fn ledger_run_mode(&self, run_id: &str) -> Result<String, PantheonError> {
        self.ledger().run_mode(run_id)
    }
    /// The agent profile bound to a run, if any.
    ///
    /// `None` is a real answer, not a fallback: a run created before agent
    /// profiles existed has no owner, and reporting it as "default" would
    /// attribute old history to an agent that never ran it.
    pub fn ledger_run_agent(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().run_agent(run_id)
    }
    /// The run's named project (`None` = never assigned). See
    /// [`Ledger::run_project`](pantheon_storage::Ledger::run_project).
    pub fn ledger_run_project(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().run_project(run_id)
    }
    /// Assign a run to a named project (`None` unassigns). See
    /// [`Ledger::set_run_project`](pantheon_storage::Ledger::set_run_project).
    pub fn ledger_set_run_project(
        &self,
        run_id: &str,
        project: Option<&str>,
    ) -> Result<(), PantheonError> {
        self.ledger().set_run_project(run_id, project)
    }
    /// Every named project in use, most-recently-active first. See
    /// [`Ledger::list_projects`](pantheon_storage::Ledger::list_projects).
    pub fn ledger_list_projects(&self) -> Result<Vec<String>, PantheonError> {
        self.ledger().list_projects()
    }
    /// Delete a run, its events, and its session-search rows. The home
    /// session is protected (hard
    /// error). See [`Ledger::delete_run`](pantheon_storage::Ledger::delete_run).
    pub fn ledger_delete_run(&self, run_id: &str) -> Result<(usize, usize), PantheonError> {
        self.ledger().delete_run(run_id)
    }
    pub fn ledger_reopen_run(&self, run_id: &str) -> Result<bool, PantheonError> {
        self.ledger().reopen_run(run_id)
    }
    /// Replace the run's todo snapshot in the ledger's `todos` table. See
    /// [`Ledger::set_todos`](pantheon_storage::Ledger::set_todos).
    pub fn save_todos(
        &self,
        run_id: &str,
        items: &[pantheon_api::todo::TodoItem],
    ) -> Result<(), PantheonError> {
        self.ledger().set_todos(run_id, items)
    }
    /// The run's persisted todo snapshot (empty when never set). See
    /// [`Ledger::todos`](pantheon_storage::Ledger::todos).
    pub fn load_todos(
        &self,
        run_id: &str,
    ) -> Result<Vec<pantheon_api::todo::TodoItem>, PantheonError> {
        self.ledger().todos(run_id)
    }
    /// The direct parent run this run was forked from, or `None` when the
    /// run was not created by a fork. Repeatedly following the chain yields
    /// the full lineage to the root run.
    pub fn ledger_forked_from(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().forked_from(run_id)
    }
    pub fn replay(
        &self,
        run_id: &str,
    ) -> Result<Vec<pantheon_storage::LedgerEntry>, PantheonError> {
        self.ledger().replay(run_id)
    }
    /// Fork run `src` at user turn `turn` into a brand-new run, returning
    /// the new run id and the number of turns it contains.
    ///
    /// `turn` is 1-based; `None` forks at the latest turn. The fork copies
    /// the durable event prefix - everything through the end of that turn
    /// verbatim into a fresh run, re-addressed via
    /// [`Event::with_run_id`](pantheon_api::events::Event::with_run_id).
    /// The source run is untouched. Replay already honors `TurnRewound`,
    /// so forking a rewound session branches from the rewound history,
    /// not from the hidden turns. The fork keeps the source title with a
    /// ` (fork)` suffix and records its provenance in a `RunProgress`
    /// marker, so the audit trail shows where the branch came from.
    pub fn fork_run(
        &self,
        src: &str,
        turn: Option<usize>,
    ) -> Result<(String, usize), PantheonError> {
        let entries = self.replay(src)?;
        let mut user_at: Vec<usize> = Vec::new();
        for (i, e) in entries.iter().enumerate() {
            if let Event::AssistantMessage { message, .. } = &e.event {
                if message.role == pantheon_api::message::Role::User {
                    user_at.push(i);
                }
            }
        }
        if user_at.is_empty() {
            return Err(rerr(
                "RT_FORK_EMPTY",
                format!("run {src} has no turns to fork"),
            ));
        }
        let n = match turn {
            None => user_at.len(),
            Some(0) => {
                return Err(rerr(
                    "RT_FORK_RANGE",
                    format!("turn 0 out of range (1..={})", user_at.len()),
                ))
            }
            Some(t) if t > user_at.len() => {
                return Err(rerr(
                    "RT_FORK_RANGE",
                    format!("turn {t} out of range (1..={})", user_at.len()),
                ))
            }
            Some(t) => t,
        };
        // The turn's events run from its user message up to (excluding)
        // the next turn's user message; the last turn owns the tail.
        let cut = user_at.get(n).copied().unwrap_or(entries.len());
        let new_id = new_run_id();
        self.start_run(&new_id)?;
        // Record lineage: this run was forked from `src`. Written once,
        // before any events are replayed into it, so the parent pointer
        // exists even if the fork dies mid-replay.
        self.ledger().set_run_forked_from(&new_id, src)?;
        for e in &entries[..cut] {
            // Skip the source's run boundary: start_run already emitted
            // the fork's own RunStarted, and a boundary is per-run.
            if matches!(e.event, Event::RunStarted { .. }) {
                continue;
            }
            self.emit(e.event.with_run_id(&new_id))?;
        }
        if let Some(title) = self
            .ledger_title(&new_id)
            .ok()
            .flatten()
            .filter(|t| !t.is_empty())
        {
            self.emit(Event::SessionTitled {
                run_id: new_id.clone(),
                title: pantheon_api::model::bound_title(
                    &format!("{title} (fork)"),
                    pantheon_api::model::TITLE_MAX_CHARS,
                ),
                model: String::new(),
                source: "fork".into(),
            })?;
        }
        self.emit(Event::RunProgress {
            run_id: new_id.clone(),
            detail: format!("forked from {src} at turn {n}"),
        })?;
        Ok((new_id, n))
    }
    /// Per-run counters folded from the event log.
    pub fn run_metrics(&self, run_id: &str) -> Result<pantheon_storage::RunMetrics, PantheonError> {
        self.ledger().metrics(run_id)
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
mod tests {
    use super::*;

    fn temp_supervisor() -> (Supervisor, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-fts-race-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sup = Supervisor::open(dir.clone()).expect("test supervisor opens");
        (sup, dir)
    }

    /// Regression test for the FTS `chunk_id` TOCTOU race: the chunk for an
    /// event must be indexed under the seq returned by `append` for that
    /// event, not under whatever `max_seq()` returns when indexing runs.
    /// This test deterministically simulates the interleaving the old code
    /// was vulnerable to: chunk B lands between A's ledger append and A's
    /// FTS insert, and the old `max_seq()` recompute indexed A's text under
    /// B's seq (silent FTS corruption).
    #[test]
    fn fts_chunk_indexed_under_append_seq_not_max_seq() {
        let (sup, dir) = temp_supervisor();
        let run_id = new_run_id();
        // A's ledger row; seq captured as append() returned it.
        let ev_a = Event::SessionTitled {
            run_id: run_id.clone(),
            title: "alpha zzzunique chunk".into(),
            model: "test".into(),
            source: "test".into(),
        };
        let entry_a = sup.ledger().append(&ev_a).expect("append A");
        // The racing writer: lands after A's append, before A's indexing.
        let ev_b = Event::SessionTitled {
            run_id: run_id.clone(),
            title: "beta zzzother chunk".into(),
            model: "test".into(),
            source: "test".into(),
        };
        let entry_b = sup.ledger().append(&ev_b).expect("append B");
        assert!(entry_b.seq > entry_a.seq, "test setup: B must land after A");

        // Index A's chunk using the seq append() returned for A. The buggy
        // code recomputed seq via max_seq() here and picked up B's seq
        // instead of A's.
        sup.index_for_search(&ev_a, entry_a.seq).expect("index A");

        let hits = sup.shared_search().search("zzzunique", 10).expect("search");
        assert_eq!(hits.len(), 1, "expected exactly the alpha chunk");
        assert_eq!(
            hits[0].chunk.seq, entry_a.seq,
            "FTS chunk indexed under wrong seq (TOCTOU race)"
        );
        assert_eq!(
            hits[0].chunk.chunk_id,
            format!("{run_id}:{}:title", entry_a.seq)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 8: `Supervisor::open` runs startup recovery once per data
    /// dir - a crash-orphaned `running` run (no live lease) is settled
    /// failed with a REPAIRED note, while a live-lease run and an
    /// `awaiting_approval` park are left alone. The orphan is seeded
    /// through a raw `Ledger` so it predates the supervisor's open (the
    /// once-per-dir guard only skips a *second* open of the same dir).
    #[test]
    fn supervisor_open_settles_crash_orphaned_running_run() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-startup-recovery-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("test dir");
        let db = dir.join("ledger.db");
        {
            let ledger = pantheon_storage::ledger::Ledger::open(&db).expect("raw ledger");
            ledger
                .append(&Event::RunStarted {
                    run_id: "orphan".to_string(),
                })
                .expect("orphan start");
            ledger
                .append(&Event::RunStarted {
                    run_id: "live".to_string(),
                })
                .expect("live start");
            // A live lease (heartbeat-fresh) must survive the sweep.
            pantheon_storage::leases::RunLeaseStore::open(&db)
                .expect("lease store")
                .acquire("live", "lease-holder", 60_000)
                .expect("acquire lease");
        }
        let sup = Supervisor::open(dir.clone()).expect("supervisor opens");
        assert_eq!(
            sup.ledger_status("orphan").expect("status").as_deref(),
            Some("failed"),
            "crash-orphaned run settled on startup"
        );
        assert_eq!(
            sup.ledger_status("live").expect("status").as_deref(),
            Some("running"),
            "live-lease run untouched"
        );
        let entries = sup.ledger().replay("orphan").expect("replay orphan");
        assert!(
            entries
                .iter()
                .any(|e| format!("{:?}", e.event).contains("REPAIRED")),
            "settlement carries the REPAIRED note"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
