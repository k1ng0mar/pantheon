//! Tests for `ReasoningLevel`: parsing, spelling, and the off default.
use super::*;

#[test]
fn parses_levels_case_insensitively() {
    use ReasoningLevel::*;
    assert_eq!(ReasoningLevel::parse("high"), Some(High));
    assert_eq!(ReasoningLevel::parse("HIGH"), Some(High));
    assert_eq!(ReasoningLevel::parse("med"), Some(Medium));
    assert_eq!(ReasoningLevel::parse("off"), Some(Off));
    assert_eq!(ReasoningLevel::parse("none"), Some(Off));
    assert_eq!(ReasoningLevel::parse("ultra"), None);
    assert_eq!(ReasoningLevel::parse(""), None);
}

#[test]
fn default_is_off_and_round_trips() {
    assert_eq!(ReasoningLevel::default(), ReasoningLevel::Off);
    assert_eq!(ReasoningLevel::High.as_str(), "high");
    // Serde lowercase: configs read and write the same spellings.
    let v = serde_json::to_string(&ReasoningLevel::Medium).unwrap();
    assert_eq!(v, "\"medium\"");
    let back: ReasoningLevel = serde_json::from_str("\"low\"").unwrap();
    assert_eq!(back, ReasoningLevel::Low);
}

#[test]
fn parses_the_full_ladder() {
    use ReasoningLevel::*;
    assert_eq!(ReasoningLevel::parse("minimal"), Some(Minimal));
    assert_eq!(ReasoningLevel::parse("xhigh"), Some(Xhigh));
    assert_eq!(ReasoningLevel::parse("max"), Some(Max));
    assert_eq!(Xhigh.as_str(), "xhigh");
    assert_eq!(Max.as_str(), "max");
}
