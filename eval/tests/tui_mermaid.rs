//! Mermaid fenced-block rendering → Unicode box diagrams.
//!
//! Kept in `/eval` per the test-location policy: these are behavioral
//! rendering tests, not small deterministic invariants.

use pantheon_tui::richtext::render_mermaid;

#[test]
fn td_graph_renders_boxes_and_down_arrows() {
    let src = "graph TD\nA[Start]-->B[End]";
    let out = render_mermaid(src).expect("should parse");
    let text = out.join("\n");
    assert!(text.contains("Start"), "label present:\n{text}");
    assert!(text.contains("End"), "label present:\n{text}");
    assert!(
        text.contains('╭') && text.contains('╮'),
        "box corners:\n{text}"
    );
    assert!(text.contains('▼'), "down connector:\n{text}");
}

#[test]
fn lr_graph_renders_sideways_arrows() {
    let src = "graph LR\nA[Left]-->B[Right]";
    let out = render_mermaid(src).expect("should parse");
    let text = out.join("\n");
    assert!(
        text.contains("Left") && text.contains("Right"),
        "labels:\n{text}"
    );
    assert!(text.contains('▶'), "right arrow:\n{text}");
}

#[test]
fn node_shapes_and_edge_labels() {
    let src = "graph TD\nA[text]-->|yes|B{decision}\nB-->C((done))";
    let out = render_mermaid(src).expect("should parse");
    let text = out.join("\n");
    for label in ["text", "decision", "done"] {
        assert!(text.contains(label), "missing {label}:\n{text}");
    }
}

#[test]
fn unknown_syntax_falls_back_to_none() {
    // Not a flowchart: caller must show the raw fence.
    assert!(render_mermaid("sequenceDiagram\nAlice->>Bob: hi").is_none());
    assert!(render_mermaid("just some text").is_none());
    // Gibberish edge lines are rejected, not guessed.
    assert!(render_mermaid("graph TD\nA !!! B").is_none());
}

#[test]
fn flowchart_alias_and_comments() {
    let src = "flowchart TD\n%% a comment\nA-->B";
    assert!(render_mermaid(src).is_some());
}
