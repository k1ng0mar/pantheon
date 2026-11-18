use serde_json::Value;

#[test]
fn harness_eval_cases_are_registered() {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/cases.json")).unwrap();
    let root: Value = serde_json::from_str(&text).unwrap();
    let cases = root["cases"].as_array().expect("cases must be an array");
    for id in ["harness-single-tool-edit", "harness-reset-handoff"] {
        let case = cases
            .iter()
            .find(|case| case["id"] == id)
            .unwrap_or_else(|| panic!("missing harness eval case {id}"));
        assert!(
            case["commands"].as_array().is_some_and(|c| !c.is_empty()),
            "{id} has no commands"
        );
        assert!(case["expect"].is_object(), "{id} has no expectations");
    }
}
