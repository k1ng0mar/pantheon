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
use std::sync::{Arc, Mutex};
use std::time::Duration;
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

struct SupervisorInner {
    ledger: Ledger,
    operations: OperationStore,
    leases: RunLeaseStore,
    data_dir: PathBuf,
    lease_id: String,
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
        let lease_id = format!("lease_{}_{}", std::process::id(), new_run_id());
        Ok(Self {
            inner: Arc::new(SupervisorInner {
                ledger,
                operations,
                leases,
                data_dir,
                lease_id,
            }),
        })
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

    pub fn transition_operation(
        &self,
        id: &str,
        expected_version: u64,
        status: OperationStatus,
        state: serde_json::Value,
    ) -> Result<Operation, PantheonError> {
        self.inner
            .operations
            .transition(id, expected_version, status, state)
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
    pub fn start_run(&self, run_id: &str) -> Result<bool, PantheonError> {
        let status = self.ledger().status(run_id)?;
        let recovered = matches!(status.as_deref(), Some("running"));
        self.ledger().append(&Event::RunStarted {
            run_id: run_id.into(),
        })?;
        if recovered {
            self.ledger().append(&Event::RunRecovered {
                run_id: run_id.into(),
            })?;
        }
        Ok(recovered)
    }
    pub fn emit(&self, ev: Event) -> Result<(), PantheonError> {
        self.ledger().append(&ev)?;
        Ok(())
    }
    pub fn complete(&self, run_id: &str) -> Result<(), PantheonError> {
        self.ledger().append(&Event::RunCompleted {
            run_id: run_id.into(),
        })?;
        Ok(())
    }
    pub fn fail(&self, run_id: &str, code: &str) -> Result<(), PantheonError> {
        self.ledger().append(&Event::RunFailed {
            run_id: run_id.into(),
            code: code.into(),
        })?;
        Ok(())
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

    pub fn unregister_process_group(&self, run_id: &str, pgid: i32) -> Result<(), PantheonError> {
        self.assert_lease_owned(run_id)?;
        self.ledger()
            .unregister_process_group_owned(run_id, pgid, &self.inner.lease_id)
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
        self.ledger().append(&Event::RunCanceled {
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
    ) -> Result<Vec<(String, String, i64)>, PantheonError> {
        self.ledger().list_runs(limit)
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

/// Run IDs: run_<epochms>_<rand4>. No external deps.
pub fn new_run_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let r = Mutex::new(0u32);
    let n = {
        let mut g = r.lock().unwrap();
        *g = ((*g).wrapping_mul(1664525).wrapping_add(1013904223)) % 10000;
        *g
    };
    let n = (ms as u32).wrapping_add(n + std::process::id()) % 10000;
    format!("run_{ms}_{n:04}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn start_complete_explain() {
        let dir = std::env::temp_dir().join(format!("pantheon-rt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        let id = "run_test_1";
        assert!(!sup.start_run(id).unwrap());
        sup.emit(Event::ToolStarted {
            run_id: id.into(),
            call_id: "t".into(),
            tool: "shell".into(),
            args: String::new(),
        })
        .unwrap();
        sup.complete(id).unwrap();
        assert!(sup.explain(id).unwrap().contains("completed"));
    }
    #[test]
    fn crash_recovery_flag() {
        let dir = std::env::temp_dir().join(format!("pantheon-rt2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir.clone()).unwrap();
        sup.start_run("run_crash").unwrap();
        drop(sup);
        let sup2 = Supervisor::open(dir).unwrap();
        assert!(sup2.start_run("run_crash").unwrap());
        assert!(sup2.explain("run_crash").unwrap().contains("recovered"));
    }

    #[test]
    fn grant_flips_parked_run_back_to_running() {
        let dir = std::env::temp_dir().join(format!("pantheon-rt3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        sup.start_run("run_park").unwrap();
        sup.emit(Event::ApprovalRequested {
            run_id: "run_park".into(),
            scope: "call_0_0".into(),
        })
        .unwrap();
        assert_eq!(
            sup.ledger_status("run_park").unwrap().as_deref(),
            Some("awaiting_approval")
        );
        sup.grant("run_park", "call_0_0").unwrap();
        assert_eq!(
            sup.ledger_status("run_park").unwrap().as_deref(),
            Some("running")
        );
    }

    #[test]
    fn replay_rebuilds_transcript_and_unfinished_calls() {
        use pantheon_core::message::Message;
        let dir = std::env::temp_dir().join(format!("pantheon-rt4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        sup.start_run("run_replay").unwrap();
        sup.emit(Event::AssistantMessage {
            run_id: "run_replay".into(),
            message: Message::user("do the thing"),
        })
        .unwrap();
        sup.emit(Event::ToolStarted {
            run_id: "run_replay".into(),
            call_id: "call_0_0".into(),
            tool: "shell".into(),
            args: "{\"cmd\":\"ls\"}".into(),
        })
        .unwrap();
        let entries = sup.replay("run_replay").unwrap();
        let msgs = crate::session::rebuild_messages(entries.clone());
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].content, "do the thing");
        assert_eq!(
            crate::session::unfinished_calls(&entries),
            vec!["call_0_0".to_string()]
        );
    }

    #[test]
    fn grant_rejects_unknown_scope() {
        let dir = std::env::temp_dir().join(format!("pantheon-rt5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        sup.start_run("run_unknown").unwrap();
        // Park the run on approval so grant() reaches the scope check.
        sup.emit(Event::ApprovalRequested {
            run_id: "run_unknown".into(),
            scope: "real_scope".into(),
        })
        .unwrap();
        assert_eq!(
            sup.ledger_status("run_unknown").unwrap().as_deref(),
            Some("awaiting_approval")
        );
        // Grant a different scope: must refuse as unknown.
        let err = sup.grant("run_unknown", "other_scope").unwrap_err();
        assert_eq!(err.code, "RT_APPROVAL_UNKNOWN");
    }

    #[test]
    fn lease_guard_blocks_a_second_supervisor_until_release() {
        let dir = std::env::temp_dir().join(format!("pantheon-lease-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = Supervisor::open(dir.clone()).unwrap();
        let second = Supervisor::open(dir).unwrap();
        first.start_run_with_lease("run-guarded").unwrap();
        let guard = RunLeaseGuard::try_new(first.clone(), "run-guarded").unwrap();
        assert!(guard.is_healthy());
        assert!(second.acquire_lease("run-guarded").is_err());
        drop(guard);
        assert!(second.acquire_lease("run-guarded").is_ok());
    }

    #[test]
    fn canceling_a_run_moves_linked_operations_to_canceled() {
        let dir =
            std::env::temp_dir().join(format!("pantheon-op-cancel-run-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        sup.start_run_with_lease("run-op").unwrap();
        sup.create_operation(
            "run-op:call_0_0",
            "tool.execute",
            serde_json::json!({
                "phase": "execute",
                "request": {"run_id": "run-op", "name": "shell", "args": ""},
                "translated": {"name": "shell", "args": ""}
            }),
        )
        .unwrap();
        sup.cancel_run("run-op", "user stop").unwrap();
        assert_eq!(
            sup.operations()
                .get("run-op:call_0_0")
                .unwrap()
                .unwrap()
                .status,
            OperationStatus::Canceled
        );
    }

    #[test]
    fn operation_cannot_be_canceled_for_another_run() {
        let dir = std::env::temp_dir().join(format!("pantheon-op-mismatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        sup.create_operation(
            "op",
            "tool.execute",
            serde_json::json!({
                "phase": "execute", "request": {"run_id": "run-a"}
            }),
        )
        .unwrap();
        let error = sup
            .cancel_operation_with_groups("op", "run-b", "stop")
            .unwrap_err();
        assert_eq!(error.code, "OPERATION_RUN_MISMATCH");
        assert_eq!(
            sup.operations().get("op").unwrap().unwrap().status,
            OperationStatus::Ready
        );
    }

    #[test]
    fn grant_rejects_duplicate_scope() {
        let dir = std::env::temp_dir().join(format!("pantheon-rt6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        sup.start_run("run_dup").unwrap();
        sup.emit(Event::ApprovalRequested {
            run_id: "run_dup".into(),
            scope: "call_0_0".into(),
        })
        .unwrap();
        sup.grant("run_dup", "call_0_0").unwrap();
        // First grant unparked the run. Second grant must refuse because
        // the run is no longer parked — the duplicate is caught by the
        // status gate, not the scope check.
        let err = sup.grant("run_dup", "call_0_0").unwrap_err();
        assert_eq!(err.code, "RT_NOT_PARKED");
    }

    #[test]
    fn chat_on_parked_run_is_refused() {
        use std::path::PathBuf;
        let dir = std::env::temp_dir().join(format!("pantheon-rt7-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir.clone()).unwrap();
        // Park the run synthetically.
        sup.start_run("run_park").unwrap();
        sup.emit(Event::ApprovalRequested {
            run_id: "run_park".into(),
            scope: "call_0_0".into(),
        })
        .unwrap();
        assert_eq!(
            sup.ledger_status("run_park").unwrap().as_deref(),
            Some("awaiting_approval")
        );
        // Build a session whose chat() must refuse before talking to any model.
        // Use an unreachable endpoint to prove the refusal happens pre-flight.
        let session = crate::session::Session::new(
            PathBuf::from(dir),
            pantheon_core::capability::Policy::coder(),
            pantheon_core::model::ModelPolicy {
                default: pantheon_core::model::DefaultModel {
                    provider: "unreachable.test".into(),
                    model: "x".into(),
                },
                fallbacks: pantheon_core::model::FallbackChain::default(),
                auxiliaries: vec![],
            },
            String::new(),
        )
        .unwrap();
        let err = session.chat("run_park", "again").unwrap_err();
        assert_eq!(err.code, "RUN_PARKED");
    }
}
