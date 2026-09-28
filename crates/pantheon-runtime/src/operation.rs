//! Durable tool operations.
//!
//! A tool is not one opaque blocking call here.  It is represented by an
//! operation whose state records each phase:
//!
//! `translate -> execute -> translate_result`.
//!
//! The phase is persisted before the next side effect.  A replacement
//! worker can resume an operation and will not execute a phase whose result
//! is already in SQLite.

use crate::Supervisor;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_storage::{Operation, OperationStatus, OperationStore};
use serde_json::Value;
use std::sync::Arc;

pub fn operation_err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Runtime,
        false,
        cause,
        "retry the operation or inspect its durable state",
        "",
    )
}

/// Adapter boundary for a real tool.  Implementations may be HTTP, a
/// process, or an in-process capability.  The runtime only owns the durable
/// protocol around them.
pub trait ToolOperationAdapter: Send + Sync {
    fn translate(&self, request: &Value) -> Result<Value, PantheonError>;
    /// Execute a translated request. The legacy form remains the required
    /// method; adapters that need crash-safe side effects should additionally
    /// override [`ToolOperationAdapter::execute_keyed`].
    fn execute(&self, translated: &Value) -> Result<Value, PantheonError>;
    /// Execute with the durable operation id as the idempotency key. An adapter
    /// whose underlying side effect is not naturally idempotent must persist
    /// that key and return the prior result when the phase is retried.
    fn execute_keyed(
        &self,
        _operation_id: &str,
        translated: &Value,
    ) -> Result<Value, PantheonError> {
        self.execute(translated)
    }
    fn translate_result(&self, result: &Value) -> Result<Value, PantheonError>;
}

fn validate_existing(
    op: &Operation,
    operation_type: &str,
    request: &Value,
) -> Result<(), PantheonError> {
    if op.operation_type != operation_type || op.state.get("request") != Some(request) {
        return Err(operation_err(
            "OPERATION_IDENTITY",
            format!("operation {} belongs to a different request", op.id),
        ));
    }
    Ok(())
}

fn failed_state(op: &Operation, phase: &str, error: &PantheonError) -> Value {
    let mut state = op.state.as_object().cloned().unwrap_or_default();
    state.insert("phase".into(), Value::String(phase.into()));
    state.insert("error".into(), Value::String(error.code.clone()));
    Value::Object(state)
}

/// Execute or resume one operation. The durable operation id is the
/// idempotency key supplied to adapters, so a crash after `execute` can be
/// recovered without duplicating a side effect when the adapter honors it.
pub fn run_tool_operation(
    store: &OperationStore,
    operation_id: &str,
    operation_type: &str,
    request: Value,
    adapter: &dyn ToolOperationAdapter,
) -> Result<Operation, PantheonError> {
    let mut op = match store.get(operation_id)? {
        Some(existing) => existing,
        None => match store.create(
            operation_id,
            operation_type,
            serde_json::json!({"phase": "translate", "request": request}),
        ) {
            Ok(created) => created,
            Err(create_error) => store.get(operation_id)?.ok_or(create_error)?,
        },
    };
    loop {
        validate_existing(&op, operation_type, &request)?;
        match op.status {
            status if status.is_terminal() => return Ok(op),
            OperationStatus::Canceling => return Ok(op),
            OperationStatus::Ready => {
                op = store.transition(
                    operation_id,
                    op.version,
                    OperationStatus::Awaiting,
                    serde_json::json!({"phase": "translate", "request": request}),
                )?;
            }
            OperationStatus::Awaiting => {}
            OperationStatus::Completed | OperationStatus::Failed | OperationStatus::Canceled => {
                unreachable!()
            }
        }
        let phase = op
            .state
            .get("phase")
            .and_then(Value::as_str)
            .unwrap_or("translate");
        match phase {
            "translate" => {
                let translated = match adapter.translate(&request) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ =
                            store.fail(operation_id, op.version, failed_state(&op, phase, &error));
                        return Err(error);
                    }
                };
                op = store.transition(
                    operation_id,
                    op.version,
                    OperationStatus::Awaiting,
                    serde_json::json!({
                        "phase": "execute",
                        "request": request,
                        "translated": translated,
                    }),
                )?;
            }
            "execute" => {
                let translated = op.state.get("translated").cloned().ok_or_else(|| {
                    operation_err(
                        "OPERATION_STATE",
                        "execute phase has no translated request".into(),
                    )
                })?;
                let result = match adapter.execute_keyed(operation_id, &translated) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ =
                            store.fail(operation_id, op.version, failed_state(&op, phase, &error));
                        return Err(error);
                    }
                };
                op = store.transition(
                    operation_id,
                    op.version,
                    OperationStatus::Awaiting,
                    serde_json::json!({
                        "phase": "translate_result",
                        "request": request,
                        "translated": translated,
                        "raw_result": result,
                    }),
                )?;
            }
            "translate_result" => {
                let raw = op.state.get("raw_result").cloned().ok_or_else(|| {
                    operation_err(
                        "OPERATION_STATE",
                        "translate_result phase has no raw result".into(),
                    )
                })?;
                let result = match adapter.translate_result(&raw) {
                    Ok(value) => value,
                    Err(error) => {
                        let _ =
                            store.fail(operation_id, op.version, failed_state(&op, phase, &error));
                        return Err(error);
                    }
                };
                return store.complete(
                    operation_id,
                    op.version,
                    serde_json::json!({
                        "phase": "completed",
                        "request": request,
                        "result": result,
                    }),
                );
            }
            other => {
                return Err(operation_err(
                    "OPERATION_STATE",
                    format!("unknown operation phase {other}"),
                ));
            }
        }
    }
}

/// Convenience adapter for the common case where translation is identity and
/// the result is a JSON-safe copy.  This is useful for builtins and tests;
/// network/process adapters can implement the full trait.
pub struct JsonToolAdapter<F>
where
    F: Fn(&Value) -> Result<Value, PantheonError> + Send + Sync,
{
    pub execute_fn: F,
}

impl<F> ToolOperationAdapter for JsonToolAdapter<F>
where
    F: Fn(&Value) -> Result<Value, PantheonError> + Send + Sync,
{
    fn translate(&self, request: &Value) -> Result<Value, PantheonError> {
        Ok(request.clone())
    }
    fn execute(&self, translated: &Value) -> Result<Value, PantheonError> {
        (self.execute_fn)(translated)
    }
    fn translate_result(&self, result: &Value) -> Result<Value, PantheonError> {
        Ok(result.clone())
    }
}

/// A small runner facade used by integrations that own a `Supervisor` rather
/// than a separate operation store.
pub struct DurableOperationRunner {
    supervisor: Arc<Supervisor>,
}

impl DurableOperationRunner {
    pub fn new(supervisor: Supervisor) -> Self {
        Self {
            supervisor: Arc::new(supervisor),
        }
    }
    pub fn run(
        &self,
        id: &str,
        kind: &str,
        request: Value,
        adapter: &dyn ToolOperationAdapter,
    ) -> Result<Operation, PantheonError> {
        run_tool_operation(self.supervisor.operations(), id, kind, request, adapter)
    }
}
