//! Tests for `pantheon_exec::tests` — sibling file so sources stay test-free.
use super::*;
#[test]
fn small_output_untouched() {
    let c = compact_output("a\nb\n", &CompactionPolicy::default());
    assert!(!c.truncated);
    assert_eq!(c.text, "a\nb\n");
}
#[test]
fn wall_compacted_with_marker() {
    let raw: String = (0..1000)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let c = compact_output(&raw, &CompactionPolicy::default());
    assert!(c.truncated);
    assert!(c.text.contains("line 0"));
    assert!(c.text.contains("line 999"));
    assert!(c.text.contains("compacted: dropped"));
    assert_eq!(c.kept_lines + c.dropped_lines, 1000);
}
