//! Tests for `pantheon_exec::memory_tools::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_memory::MemoryStore;

fn writer_policy() -> pantheon_api::capability::Policy {
    use pantheon_api::capability::Capability as C;
    // MemoryConfirm is allowed explicitly here so the success-path tests
    // exercise the promotion itself; the default-policy regression test
    // below covers the approval gate.
    pantheon_api::capability::Policy::coder()
        .allow(C::MemoryWrite)
        .allow(C::MemoryConfirm)
}

fn read_policy() -> pantheon_api::capability::Policy {
    pantheon_api::capability::Policy::coder()
}

fn fresh(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "pantheon-memtools-{}-{}-{}",
        name,
        std::process::id(),
        n
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn opts_with(policy: pantheon_api::capability::Policy) -> (MemoryToolOptions, VecMemorySink) {
    let dir = fresh("opt");
    let store = Arc::new(MemoryStore::open(&dir.join("memory.db")).unwrap());
    let sink = VecMemorySink::new();
    let opts = MemoryToolOptions {
        store,
        policy: Arc::new(policy),
        namespace: "nyx".into(),
        max_bytes: 4096,
        sink: Arc::new(sink.clone()),
        backend_label: "native".into(),
    };
    (opts, sink)
}

fn build_registry(opts: MemoryToolOptions) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_memory_tools(&mut reg, opts);
    reg
}

#[derive(Debug, Default)]
struct RecordingBackend {
    seen: std::sync::Mutex<Vec<Proposal>>,
}
impl pantheon_memory::MemoryBackend for RecordingBackend {
    fn recall(
        &self,
        _policy: &pantheon_api::capability::Policy,
        _namespaces: &[&str],
        _layers: &[LayerKind],
        _query: &str,
        _limit: usize,
    ) -> Result<Vec<pantheon_memory::Recalled>, PantheonError> {
        Ok(vec![])
    }
    fn write(
        &self,
        _policy: &pantheon_api::capability::Policy,
        proposal: Proposal,
        _max_bytes: usize,
    ) -> Result<pantheon_memory::MemoryRecord, PantheonError> {
        let rec = pantheon_memory::MemoryRecord {
            layer: proposal.layer,
            namespace: proposal.namespace.clone(),
            key: proposal.key.clone(),
            value: proposal.value.clone(),
            provenance: proposal.provenance.clone(),
        };
        self.seen.lock().unwrap().push(proposal);
        Ok(rec)
    }
    fn list_agent(&self, _namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
        Ok(vec![])
    }
}

fn opts_with_backend(
    policy: pantheon_api::capability::Policy,
    backend: Arc<dyn pantheon_memory::MemoryBackend>,
) -> MemoryToolOptions {
    MemoryToolOptions {
        store: backend,
        policy: Arc::new(policy),
        namespace: "nyx".into(),
        max_bytes: 4096,
        sink: Arc::new(VecMemorySink::new()),
        backend_label: "external".into(),
    }
}

/// The gate runs before the boundary: what a plugin backend receives
/// is already validated + trust-clamped (model origin => Untrusted).

#[test]
fn external_backend_receives_already_gated_proposals() {
    let backend = Arc::new(RecordingBackend::default());
    let opts = opts_with_backend(writer_policy(), backend.clone());
    let reg = build_registry(opts);
    reg.execute(
        "memory_propose",
        r#"{"key":"k","value":"v","layer":"agent"}"#,
    )
    .unwrap();
    let seen = backend.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].provenance.origin, "model");
    assert_eq!(
        seen[0].provenance.trust,
        pantheon_api::provenance::TrustTier::Untrusted
    );
}

/// recall_via gates on policy before the backend sees the query.

#[test]
fn external_backend_recall_is_denied_without_capability() {
    let backend = Arc::new(RecordingBackend::default());
    // researcher_readonly grants MemoryRead; use a raw coder policy
    // built WITHOUT memory grants to prove the via-gate blocks.
    let policy = pantheon_api::capability::Policy::coder()
        .deny(pantheon_api::capability::Capability::MemoryRead);
    let opts = opts_with_backend(policy, backend);
    let reg = build_registry(opts);
    let err = reg
        .execute("memory_recall", r#"{"query":"anything"}"#)
        .unwrap_err();
    assert_eq!(err.code, "MEM_NO_READ_CAPABILITY");
}

/// Trust-tier promotion is native-only; external backends surface a
/// structured unsupported error instead of silently pretending.

#[test]
fn external_backend_confirm_is_structured_unsupported() {
    let backend = Arc::new(RecordingBackend::default());
    let opts = opts_with_backend(writer_policy(), backend);
    let reg = build_registry(opts);
    let err = reg
        .execute("memory_confirm", r#"{"key":"anything"}"#)
        .unwrap_err();
    assert_eq!(err.code, "MEM_BACKEND_UNSUPPORTED");
}
