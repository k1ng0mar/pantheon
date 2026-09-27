//! Tests for `pantheon_memory::markdown::tests` — sibling file so sources stay test-free.
use super::*;
use crate::LayerKind;
use pantheon_api::provenance::TrustTier;

#[test]
fn parse_round_trips_simple_records() {
    let md = "# Agent memory\n\n# city\n\nKano\n\n# tz\n\nAfrica/Lagos\n";
    let parsed = parse_md(md);
    assert_eq!(
        parsed,
        vec![
            ("city".into(), "Kano".into()),
            ("tz".into(), "Africa/Lagos".into()),
        ]
    );
}

#[test]
fn parse_handles_missing_top_heading() {
    let md = "# city\n\nKano\n";
    let parsed = parse_md(md);
    assert_eq!(parsed, vec![("city".into(), "Kano".into())]);
}

#[test]
fn parse_handles_multiline_body() {
    let md = "# notes\n\nline one\nline two\n\n# end\n\n";
    let parsed = parse_md(md);
    assert_eq!(parsed[0].0, "notes");
    assert!(parsed[0].1.contains("line one"));
    assert!(parsed[0].1.contains("line two"));
}

#[test]
fn render_then_parse_round_trip() {
    let rows = vec![
        ("city".to_string(), "Kano".to_string(), TrustTier::User),
        (
            "tz".to_string(),
            "Africa/Lagos".to_string(),
            TrustTier::User,
        ),
    ];
    let md = render_agent(&rows);
    let parsed = parse_md(&md);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].0, "city");
    assert_eq!(parsed[1].1, "Africa/Lagos");
}

#[test]
fn heading_inside_value_survives_round_trip() {
    let rows = vec![(
        "steps".to_string(),
        "step 1\n# step 2\n#\nstep 3".to_string(),
        TrustTier::User,
    )];
    let md = render_agent(&rows);
    let parsed = parse_md(&md);
    assert_eq!(
        parsed.len(),
        1,
        "heading-looking value lines must not split the record"
    );
    assert_eq!(parsed[0].1, "step 1\n# step 2\n#\nstep 3");
}

#[test]
fn key_named_agent_memory_round_trips_in_v1_file() {
    let rows = vec![
        (
            "Agent memory".to_string(),
            "meta".to_string(),
            TrustTier::User,
        ),
        ("other".to_string(), "v".to_string(), TrustTier::User),
    ];
    let md = render_agent(&rows);
    let parsed = parse_md(&md);
    assert_eq!(parsed.len(), 2);
    assert!(parsed
        .iter()
        .any(|(k, v)| k == "Agent memory" && v == "meta"));
}
