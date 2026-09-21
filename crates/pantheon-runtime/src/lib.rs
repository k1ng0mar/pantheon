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
    pub fn explain(&self, run_id: &str) -> Result<String, PantheonError> {
        self.ledger().explain(run_id)
    }

    pub fn ledger_status(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.ledger().status(run_id)
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
            tool: "shell".into(),
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
}
