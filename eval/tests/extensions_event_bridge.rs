//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::events::Event;
use pantheon_extensions::hooks::Hook;

fn hook_of(ev: &pantheon_api::events::Event) -> Option<pantheon_extensions::hooks::Hook> {
    pantheon_extensions::event_bridge::dispatch(ev).map(|f| f.hook)
}

#[test]
fn stream_delta_is_opt_in() {
    let d = Event::ModelDelta {
        run_id: "r1".into(),
        delta: "tok".into(),
    };
    // Off by default: one subprocess per token would stall the loop.
    assert_eq!(hook_of(&d), None);
    std::env::set_var("PANTHEON_HOOK_STREAM_DELTA", "1");
    assert_eq!(hook_of(&d), Some(Hook::OnStreamDelta));
    std::env::remove_var("PANTHEON_HOOK_STREAM_DELTA");
    assert_eq!(hook_of(&d), None);
}
