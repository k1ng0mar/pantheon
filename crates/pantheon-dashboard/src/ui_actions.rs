//! Dashboard UI-action submission: `POST /api/runs/:id/ui-action`.
//!
//! This is the dashboard's interactive client for the agent-native UI
//! protocol (`pantheon_api::ui`, see `docs/ui-protocol.md`). Forms,
//! buttons, and approval cards rendered by `assets/app.js` submit
//! through this route.
//!
//! Security posture (deliberate):
//! - The route lives under `/api/*`, so the gateway's `check_auth`
//!   enforces the dashboard token before this handler runs. There is
//!   no unauthenticated path.
//! - The run must exist in THIS dashboard's ledger (`known_run`): a
//!   caller cannot reach another user's session by guessing a run id.
//! - The handler does NO validation of its own beyond JSON shape. Every
//!   security decision — action registry, revision pin, nonce replay,
//!   capability gate, secret redaction — lives in
//!   `Session::submit_ui_action`, which this handler delegates to
//!   verbatim. The dashboard never constructs tool calls.
//! - Client params are never echoed back in responses (secret safety).
//! - Approval decisions resume the parked turn exactly like
//!   `runs::answer_input` does: under the run's send guard, spawning a
//!   turn child with `--resume`.

use crate::session_factory::open_maintenance_session;
use crate::{bad_json, conflict, err_json, json_ok, spawn_turn_child, App};
use pantheon_api::ui::UiActionSubmission;
use pantheon_gateway::http::{Request, Response};
use pantheon_runtime::session::UiActionOutcome;

/// `POST /api/runs/:id/ui-action`: submit a UI action from a dashboard-
/// rendered document. Body: `{ui_id, revision, node_id, action_id,
/// params, nonce}`.
pub fn submit_ui_action(app: &App, run_id: &str, req: &Request) -> Response {
    let body = match crate::body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let get_str = |k: &str| {
        body.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("")
            .to_string()
    };
    let ui_id = get_str("ui_id");
    let node_id = get_str("node_id");
    let action_id = get_str("action_id");
    let nonce = get_str("nonce");
    let revision = body.get("revision").and_then(|v| v.as_u64());
    let params = body
        .get("params")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    if ui_id.is_empty() || node_id.is_empty() || action_id.is_empty() || nonce.is_empty() {
        return bad_json(
            "fields \"ui_id\", \"node_id\", \"action_id\" and \"nonce\" are required and must be non-empty",
        );
    }
    let Some(revision) = revision else {
        return bad_json("field \"revision\" is required and must be a non-negative integer");
    };
    if let Err(r) = crate::runs::known_run(app, run_id) {
        return r;
    }
    let session = match open_maintenance_session(&app.data_dir) {
        Ok(s) => s,
        Err(e) => return err_json(500, "SESSION", &format!("open session: {e}")),
    };
    let sub = UiActionSubmission {
        ui_id,
        revision,
        node_id,
        action_id,
        params,
        nonce,
    };
    // Serialize with other dashboard submissions for this run. The
    // runtime's per-Session ui_action_lock does not span the fresh
    // Session opened per request above; without this, two concurrent
    // POSTs could both pass the nonce replay check and both execute.
    let ui_lock = app.ui_action_guard(run_id);
    let _ui_guard = ui_lock.lock().unwrap_or_else(|e| e.into_inner());
    match session.submit_ui_action(run_id, &sub) {
        Ok(UiActionOutcome::Executed { result }) => json_ok(serde_json::json!({
            "ok": true,
            "outcome": "executed",
            "result": result,
        })),
        Ok(UiActionOutcome::Decided { approved }) => {
            // The parked turn resumes exactly like an answered input:
            // serialize with other turn-starting requests, then spawn
            // the resume child. The decision itself is already settled
            // in the ledger by submit_ui_action.
            let send_lock = app.send_guard(run_id);
            let _send_guard = send_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mode = session
                .supervisor
                .ledger_run_mode(run_id)
                .unwrap_or_else(|_| crate::runs::DEFAULT_RUN_MODE.to_string());
            drop(session);
            if let Ok(pid) = spawn_turn_child(&[
                "run",
                "--taskID",
                run_id,
                "--resume",
                "--deliver",
                "session",
                "--mode",
                mode.as_str(),
            ]) {
                app.register_turn_child(run_id, pid);
            }
            json_ok(serde_json::json!({
                "ok": true,
                "outcome": "decided",
                "approved": approved,
            }))
        }
        Err(e) => match e.code.as_str() {
            // Client-timing problems: the document moved under the
            // submission. The frontend should re-fetch and re-render.
            "UI_STALE" | "UI_REPLAY" => conflict(e.code.as_str(), &format!("{e}")),
            // Unknown document/action or malformed submission.
            "UI_UNKNOWN" | "UI_BAD_ACTION" => conflict(e.code.as_str(), &format!("{e}")),
            // The action was valid but the tool refused: surfaced so the
            // UI can show the actionable denial (e.g. "use an approval
            // card"). Not a server error.
            "UI_TOOL_DENIED" => err_json(422, e.code.as_str(), &format!("{e}")),
            _ => err_json(500, "RUNTIME", &format!("submit UI action: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_api::events::Event;
    use pantheon_api::ui::{
        ActionKind, ActionRef, ApprovalCard, ApprovalDecision, Component, FieldType, Form,
        FormField, UiDocument, UiNode, UI_PROTOCOL_VERSION,
    };
    use pantheon_gateway::http::Request;
    use pantheon_runtime::Supervisor;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_app() -> (App, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dash-ui-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp data dir");
        let app = App {
            data_dir: dir.clone(),
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            ui_action_locks: Default::default(),
            turn_children: Default::default(),
            swarm: crate::swarm::orchestrator_for(&dir),
        };
        (app, dir)
    }

    fn post_req(body: serde_json::Value) -> Request {
        Request {
            method: "POST".to_string(),
            path: "/api/runs/r1/ui-action".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: serde_json::to_vec(&body).unwrap(),
        }
    }

    fn resp_json(resp: &Response) -> serde_json::Value {
        match resp {
            Response::Buffered { body, .. } => {
                serde_json::from_slice(body).expect("response is json")
            }
            _ => panic!("expected buffered response"),
        }
    }

    fn resp_status(resp: &Response) -> u16 {
        match resp {
            Response::Buffered { status, .. } => *status,
            _ => panic!("expected buffered response"),
        }
    }

    fn form_doc() -> UiDocument {
        UiDocument {
            version: UI_PROTOCOL_VERSION,
            id: "turn1-ui_0".to_string(),
            root: UiNode {
                id: "root".to_string(),
                component: Component::Form(Form {
                    title: Some("Deploy".to_string()),
                    description: None,
                    fields: vec![FormField {
                        name: "target".to_string(),
                        label: "Target".to_string(),
                        field_type: FieldType::Select {
                            options: vec!["staging".to_string(), "prod".to_string()],
                        },
                        required: true,
                        default: None,
                        placeholder: None,
                    }],
                    submit: ActionRef {
                        action_id: "deploy".to_string(),
                        label: "Deploy".to_string(),
                        kind: ActionKind::Tool {
                            tool: "list_dir".to_string(),
                            fixed_params: serde_json::json!({"path": "."})
                                .as_object()
                                .unwrap()
                                .clone(),
                        },
                    },
                }),
            },
        }
    }

    fn approval_doc() -> UiDocument {
        UiDocument {
            version: UI_PROTOCOL_VERSION,
            id: "turn1-ui_0".to_string(),
            root: UiNode {
                id: "root".to_string(),
                component: Component::Approval(ApprovalCard {
                    title: "Delete?".to_string(),
                    detail: "Delete 3 files.".to_string(),
                    approve: ActionRef {
                        action_id: "yes".to_string(),
                        label: "Delete".to_string(),
                        kind: ActionKind::Approval {
                            decision: ApprovalDecision::Grant,
                        },
                    },
                    deny: ActionRef {
                        action_id: "no".to_string(),
                        label: "Keep".to_string(),
                        kind: ActionKind::Approval {
                            decision: ApprovalDecision::Deny,
                        },
                    },
                    scope_hint: "delete 3 files".to_string(),
                }),
            },
        }
    }

    /// Seed a run with a published UI document at revision 0.
    fn seed_run(app: &App, run_id: &str, doc: UiDocument) {
        let sup = Supervisor::open(app.data_dir.clone()).expect("supervisor");
        sup.emit(Event::RunStarted {
            run_id: run_id.to_string(),
        })
        .expect("run started");
        sup.emit(Event::UiContent {
            run_id: run_id.to_string(),
            turn_id: "turn1".to_string(),
            ui_id: doc.id.clone(),
            document: doc,
        })
        .expect("ui content");
    }

    fn valid_body(ui_id: &str) -> serde_json::Value {
        serde_json::json!({
            "ui_id": ui_id,
            "revision": 0,
            "node_id": "root",
            "action_id": "deploy",
            "params": {"target": "staging"},
            "nonce": "dash-n1",
        })
    }

    #[test]
    fn valid_form_action_executes() {
        let (app, _dir) = test_app();
        seed_run(&app, "r1", form_doc());
        let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
        assert_eq!(resp_status(&resp), 200, "{:?}", resp_json(&resp));
        let j = resp_json(&resp);
        assert_eq!(j["ok"], true);
        assert_eq!(j["outcome"], "executed");
        assert!(!j["result"].as_str().unwrap_or("").is_empty());
    }

    #[test]
    fn unknown_run_is_404() {
        let (app, _dir) = test_app();
        // No run seeded: session isolation — a guessed run id 404s.
        let resp = submit_ui_action(&app, "nope", &post_req(valid_body("turn1-ui_0")));
        assert_eq!(resp_status(&resp), 404);
    }

    #[test]
    fn unknown_action_is_rejected() {
        let (app, _dir) = test_app();
        seed_run(&app, "r1", form_doc());
        let mut body = valid_body("turn1-ui_0");
        body["action_id"] = serde_json::json!("bogus");
        let resp = submit_ui_action(&app, "r1", &post_req(body));
        assert_eq!(resp_status(&resp), 409);
        assert_eq!(resp_json(&resp)["error"]["code"], "UI_BAD_ACTION");
    }

    #[test]
    fn invalid_params_are_rejected() {
        let (app, _dir) = test_app();
        seed_run(&app, "r1", form_doc());
        let mut body = valid_body("turn1-ui_0");
        body["params"] = serde_json::json!({"target": "mars"});
        let resp = submit_ui_action(&app, "r1", &post_req(body));
        assert_eq!(resp_status(&resp), 409);
        assert_eq!(resp_json(&resp)["error"]["code"], "UI_BAD_ACTION");
    }

    #[test]
    fn stale_revision_fails_closed() {
        let (app, _dir) = test_app();
        seed_run(&app, "r1", form_doc());
        // Publish revision 1.
        let sup = Supervisor::open(app.data_dir.clone()).expect("sup");
        let mut doc2 = form_doc();
        if let Component::Form(f) = &mut doc2.root.component {
            f.title = Some("Deploy v2".to_string());
        }
        sup.emit(Event::UiUpdated {
            run_id: "r1".to_string(),
            ui_id: "turn1-ui_0".to_string(),
            revision: 1,
            document: doc2,
        })
        .expect("revise");
        // Submit against rev 0: must fail closed, not execute.
        let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
        assert_eq!(resp_status(&resp), 409);
        assert_eq!(resp_json(&resp)["error"]["code"], "UI_STALE");
    }

    #[test]
    fn replay_is_rejected() {
        let (app, _dir) = test_app();
        seed_run(&app, "r1", form_doc());
        let body = valid_body("turn1-ui_0");
        let r1 = submit_ui_action(&app, "r1", &post_req(body.clone()));
        assert_eq!(resp_status(&r1), 200);
        let r2 = submit_ui_action(&app, "r1", &post_req(body));
        assert_eq!(resp_status(&r2), 409);
        assert_eq!(resp_json(&r2)["error"]["code"], "UI_REPLAY");
    }

    #[test]
    fn concurrent_duplicate_submissions_execute_once() {
        use std::sync::Arc;
        let (app, _dir) = test_app();
        seed_run(&app, "r1", form_doc());
        let app = Arc::new(app);
        let mut handles = vec![];
        for _ in 0..8 {
            let app = Arc::clone(&app);
            handles.push(std::thread::spawn(move || {
                let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
                resp_status(&resp)
            }));
        }
        let mut ok = 0;
        let mut conflicted = 0;
        for h in handles {
            match h.join().expect("thread") {
                200 => ok += 1,
                409 => conflicted += 1,
                s => panic!("unexpected status {s}"),
            }
        }
        assert_eq!(ok, 1, "exactly one executes");
        assert_eq!(conflicted, 7, "the rest are replays");
    }

    #[test]
    fn approval_decision_parks_and_resumes() {
        let (app, _dir) = test_app();
        seed_run(&app, "r1", approval_doc());
        // Park the turn like publish_ui_batch would.
        let sup = Supervisor::open(app.data_dir.clone()).expect("sup");
        sup.emit(Event::UiActionRequested {
            run_id: "r1".to_string(),
            ui_id: "turn1-ui_0".to_string(),
            call_id: "call-dash-1".to_string(),
        })
        .expect("request");
        let body = serde_json::json!({
            "ui_id": "turn1-ui_0",
            "revision": 0,
            "node_id": "root",
            "action_id": "yes",
            "params": {},
            "nonce": "dash-n2",
        });
        let resp = submit_ui_action(&app, "r1", &post_req(body));
        assert_eq!(resp_status(&resp), 200, "{:?}", resp_json(&resp));
        let j = resp_json(&resp);
        assert_eq!(j["outcome"], "decided");
        assert_eq!(j["approved"], true);
        // The parked call is settled exactly once.
        let entries = sup.replay("r1").expect("replay");
        let completions = entries
            .iter()
            .filter(|e| {
                matches!(&e.event, Event::ToolCompleted { call_id, .. } if call_id == "call-dash-1")
            })
            .count();
        assert_eq!(completions, 1);
    }

    #[test]
    fn denied_tool_is_surfaced_not_executed() {
        let (app, _dir) = test_app();
        // Bind to a tool absent from the registry.
        let mut doc = form_doc();
        if let Component::Form(f) = &mut doc.root.component {
            f.submit.kind = ActionKind::Tool {
                tool: "no_such_tool_xyz".to_string(),
                fixed_params: Default::default(),
            };
        }
        seed_run(&app, "r1", doc);
        let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
        assert_eq!(resp_status(&resp), 422);
        let j = resp_json(&resp);
        assert_eq!(j["error"]["code"], "UI_TOOL_DENIED");
    }

    #[test]
    fn malformed_body_is_rejected() {
        let (app, _dir) = test_app();
        seed_run(&app, "r1", form_doc());
        // Missing nonce.
        let body = serde_json::json!({
            "ui_id": "turn1-ui_0",
            "revision": 0,
            "node_id": "root",
            "action_id": "deploy",
            "params": {},
        });
        let resp = submit_ui_action(&app, "r1", &post_req(body));
        assert_eq!(resp_status(&resp), 400);
        // Not JSON at all.
        let bad = Request {
            method: "POST".to_string(),
            path: "/api/runs/r1/ui-action".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: b"not json".to_vec(),
        };
        let resp = submit_ui_action(&app, "r1", &bad);
        assert_eq!(resp_status(&resp), 400);
    }

    #[test]
    fn secret_params_are_not_echoed() {
        let (app, _dir) = test_app();
        let mut doc = form_doc();
        if let Component::Form(f) = &mut doc.root.component {
            f.fields = vec![FormField {
                name: "token".to_string(),
                label: "Token".to_string(),
                field_type: FieldType::Secret,
                required: true,
                default: None,
                placeholder: None,
            }];
        }
        seed_run(&app, "r1", doc);
        let body = serde_json::json!({
            "ui_id": "turn1-ui_0",
            "revision": 0,
            "node_id": "root",
            "action_id": "deploy",
            "params": {"token": "super-secret-value"},
            "nonce": "dash-n3",
        });
        let resp = submit_ui_action(&app, "r1", &post_req(body));
        let raw = match &resp {
            Response::Buffered { body, .. } => String::from_utf8(body.clone()).unwrap(),
            _ => panic!("buffered"),
        };
        assert!(
            !raw.contains("super-secret-value"),
            "secret must not appear in the response: {raw}"
        );
    }
}
