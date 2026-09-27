//! Tests for the canonical-event -> hook bridge. Every wired observer hook
//! must be reachable from a real `Event`, and the never-fire cases must stay
//! unreachable.
use super::*;
use pantheon_api::provenance::Provenance;

fn hook_of(ev: &Event) -> Option<Hook> {
    dispatch(ev).map(|f| f.hook)
}

#[test]
fn session_lifecycle_maps_to_start_and_end() {
    assert_eq!(
        hook_of(&Event::RunStarted {
            run_id: "r1".into()
        }),
        Some(Hook::OnSessionStart)
    );
    // Every terminal shape must reach on_session_end, or GalaxyMem-style
    // consolidation silently never runs on failures.
    for ev in [
        Event::RunCompleted {
            run_id: "r1".into(),
        },
        Event::RunFailed {
            run_id: "r1".into(),
            code: "X".into(),
        },
        Event::RunCanceled {
            run_id: "r1".into(),
            reason: "user".into(),
        },
    ] {
        assert_eq!(
            hook_of(&ev),
            Some(Hook::OnSessionEnd),
            "terminal event {ev:?}"
        );
    }
}

#[test]
fn model_events_map_to_api_and_stream_hooks() {
    let req = Event::ModelRequested {
        run_id: "r1".into(),
        model: "m".into(),
    };
    assert_eq!(hook_of(&req), Some(Hook::PreApiRequest));
    assert_eq!(
        dispatch_stream_edges(&req).map(|f| f.hook),
        Some(Hook::OnStreamStart)
    );

    let done = Event::ModelCompleted {
        run_id: "r1".into(),
    };
    assert_eq!(hook_of(&done), Some(Hook::PostApiRequest));
    assert_eq!(
        dispatch_stream_edges(&done).map(|f| f.hook),
        Some(Hook::OnStreamEnd)
    );
}

#[test]
fn subagent_events_map_to_swarm_hooks() {
    let s = Event::AgentSpawned {
        run_id: "r1".into(),
        agent: "coder".into(),
    };
    let c = Event::AgentCompleted {
        run_id: "r1".into(),
        agent: "coder".into(),
    };
    assert_eq!(hook_of(&s), Some(Hook::SubagentStart));
    assert_eq!(hook_of(&c), Some(Hook::SubagentStop));
    // The agent name must reach the plugin, or a swarm observer cannot tell
    // which sub-agent it is looking at.
    let f = dispatch(&s).unwrap();
    assert_eq!(f.extra.get("agent").map(String::as_str), Some("coder"));
}

#[test]
fn tool_completed_maps_to_post_tool_call() {
    let ev = Event::ToolCompleted {
        run_id: "r1".into(),
        call_id: "c1".into(),
        tool: "read".into(),
        provenance: Provenance::untrusted("read"),
    };
    assert_eq!(hook_of(&ev), Some(Hook::PostToolCall));
    let f = dispatch(&ev).unwrap();
    assert_eq!(f.extra.get("call_id").map(String::as_str), Some("c1"));
}

#[test]
fn tool_started_is_not_observer_mapped() {
    // pre_tool_call is a gate and must fire inline, not as an observer.
    let ev = Event::ToolStarted {
        run_id: "r1".into(),
        call_id: "c1".into(),
        tool: "bash".into(),
        args: "{}".into(),
        provenance: Provenance::untrusted("bash"),
    };
    assert_eq!(hook_of(&ev), None);
}

#[test]
fn bookkeeping_events_map_to_nothing() {
    for ev in [
        Event::TurnStarted {
            run_id: "r".into(),
            turn_id: "t".into(),
        },
        Event::MemoryProposed { run_id: "r".into() },
        Event::ApprovalRequested {
            run_id: "r".into(),
            scope: "bash".into(),
        },
        Event::RunProgress {
            run_id: "r".into(),
            detail: "d".into(),
        },
    ] {
        assert_eq!(hook_of(&ev), None, "unexpected hook for {ev:?}");
    }
}

#[test]
fn every_mapped_hook_is_an_observer() {
    // The bridge must never carry a gate or transform: those change control
    // flow and an observer's return value is discarded.
    let events = vec![
        Event::RunStarted { run_id: "r".into() },
        Event::RunCompleted { run_id: "r".into() },
        Event::ModelRequested {
            run_id: "r".into(),
            model: "m".into(),
        },
        Event::ModelCompleted { run_id: "r".into() },
        Event::AgentSpawned {
            run_id: "r".into(),
            agent: "a".into(),
        },
        Event::AgentCompleted {
            run_id: "r".into(),
            agent: "a".into(),
        },
        Event::ToolCompleted {
            run_id: "r".into(),
            call_id: "c".into(),
            tool: "t".into(),
            provenance: Provenance::untrusted("t"),
        },
    ];
    for ev in &events {
        for f in [dispatch(ev), dispatch_stream_edges(ev)]
            .into_iter()
            .flatten()
        {
            assert_eq!(
                f.hook.class(),
                crate::hooks::HookClass::Observer,
                "{} is not an observer",
                f.hook.name()
            );
            assert!(f.hook.is_wired(), "{} is unwired", f.hook.name());
        }
    }
}

#[test]
fn compaction_events_map_to_on_compaction() {
    // OMP's start/end pair collapses onto one post-facto observer; both
    // durable compaction events must reach it or the hook is unwired.
    for ev in [
        Event::ContextTrimmed {
            run_id: "r1".into(),
            estimated: 100,
            window: 128,
            dropped_rows: 4,
            compacted_rows: 2,
        },
        Event::ContextCompressed {
            run_id: "r1".into(),
            model: "m".into(),
            exchanges: 3,
            rows: 9,
            chars_before: 900,
            chars_after: 100,
        },
    ] {
        assert_eq!(hook_of(&ev), Some(Hook::OnCompaction), "compaction {ev:?}");
        let f = dispatch(&ev).unwrap();
        assert_eq!(f.extra.get("run_id").map(String::as_str), Some("r1"));
    }
}
