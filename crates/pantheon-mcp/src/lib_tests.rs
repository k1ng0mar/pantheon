//! Tests for `pantheon_mcp::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn tokens_map_and_unknowns_stay_gated() {
    assert_eq!(
        capability_from_token("shell.execute"),
        Capability::ShellExecute
    );
    match capability_from_token("weird.thing") {
        Capability::Other(n) => assert_eq!(n, "weird.thing"),
        other => panic!("expected Other, got {other:?}"),
    }
}
#[test]
fn listing_shows_denied_tools_as_denied() {
    let tools = vec![
        McpTool {
            name: "read".into(),
            description: String::new(),
            requires: "filesystem.read".into(),
        },
        McpTool {
            name: "browse".into(),
            description: String::new(),
            requires: "browser".into(),
        },
    ];
    let projected = project("demo", &tools, &Policy::coder());
    assert!(projected[0].allowed);
    assert!(!projected[1].allowed);
}
