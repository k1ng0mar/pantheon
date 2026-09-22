//! Supervisor: run lifecycle, quotas, recovery, checkpointing (D decision).
//! Runs persist every event; a killed run resumes as RunRecovered.
pub mod session;

use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::events::Event;
use pantheon_storage::Ledger;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

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
    data_dir: PathBuf,
}

impl Supervisor {
    pub fn open(data_dir: PathBuf) -> Result<Self, PantheonError> {
        std::fs::create_dir_all(&data_dir).map_err(|e| rerr("RT_MKDIR", e.to_string()))?;
        let ledger = Ledger::open(&data_dir.join("ledger.db"))?;
        Ok(Self {
            inner: Arc::new(SupervisorInner { ledger, data_dir }),
        })
    }
    fn ledger(&self) -> &Ledger {
        &self.inner.ledger
    }
    pub fn data_dir(&self) -> &PathBuf {
        &self.inner.data_dir
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
    pub fn explain(&self, run_id: &str) -> Result<String, PantheonError> {
        self.ledger().explain(run_id)
    }

    pub fn ledger_status(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().status(run_id)
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
}
