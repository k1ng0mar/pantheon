//! Repair and coerce tool-call arguments before a tool sees them.
//!
//! Models get JSON nearly right: a fenced block, a trailing comma, a bare
//! newline inside a string, `True` instead of `true`, a number as a string
//! because the schema said "number". OpenClaw ships a dedicated repair package
//! and Hermes coerces arguments; this is the same idea, kept conservative:
//! every repair is a rewrite that a strict JSON parser then accepts, and the
//! original bytes are preserved in a log line when a repair fired.

use serde_json::{Map, Value};

/// What changed, for the log line. Empty = untouched.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RepairNote {
    pub repairs: Vec<&'static str>,
    pub coercions: Vec<String>,
}

impl RepairNote {
    pub fn fired(&self) -> bool {
        !self.repairs.is_empty() || !self.coercions.is_empty()
    }
    pub fn summary(&self) -> String {
        let mut parts = self.repairs.to_vec();
        parts.extend(self.coercions.iter().map(String::as_str));
        parts.join(", ")
    }
}

/// Parse `args`, repairing the text if needed, then coerce values against the
/// tool's parameter schema. Returns the JSON string to hand the tool (empty
/// input stays empty, which tools read as `{}`).
pub fn repair_args(args: &str, schema: &Value) -> (String, RepairNote) {
    let mut note = RepairNote::default();
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return (args.to_string(), note);
    }
    let value = match serde_json::from_str::<Value>(trimmed) {
        Ok(v) => v,
        Err(_) => match repair_text(trimmed, &mut note) {
            Some(v) => v,
            // Unrepairable: hand the original through so the tool's own
            // parse_args produces the error the model needs to see.
            None => return (args.to_string(), note),
        },
    };
    let value = coerce_value(value, schema, "", &mut note);
    // Nothing to fix: hand the original bytes through, so key order and
    // number formatting for clean calls never change.
    if !note.fired() {
        return (args.to_string(), note);
    }
    (value.to_string(), note)
}

/// Rewrite the common malformations into strict JSON, then parse.
fn repair_text(text: &str, note: &mut RepairNote) -> Option<Value> {
    let mut t = text.trim().to_string();

    // Markdown fence around the payload.
    if let Some(start) = t.find("```") {
        let after = &t[start + 3..];
        let after = after.strip_prefix("json").unwrap_or(after);
        let after = after.strip_prefix('\n').unwrap_or(after);
        if let Some(end) = after.find("```") {
            t = after[..end].trim().to_string();
            note.repairs.push("code fence");
        }
    }

    // Prose around the object: keep the first `{` … last `}`.
    if t.starts_with('{') && t.ends_with('}') {
        // already shaped
    } else if let (Some(a), Some(b)) = (t.find('{'), t.rfind('}')) {
        if b > a {
            t = t[a..=b].to_string();
            note.repairs.push("surrounding prose");
        }
    }

    // A double-encoded object (`"{\"a\":1}"`).
    if t.starts_with('"') {
        if let Ok(Value::String(inner)) = serde_json::from_str::<Value>(&t) {
            if serde_json::from_str::<Value>(&inner).is_ok() {
                note.repairs.push("double-encoded");
                t = inner;
            }
        }
    }

    // Python literals outside strings.
    let py = replace_outside_strings(&t, &[("True", "true"), ("False", "false"), ("None", "null")]);
    if py != t {
        note.repairs.push("python literals");
        t = py;
    }

    // Trailing commas before } or ].
    let mut no_trailing = String::with_capacity(t.len());
    let mut chars = t.chars().peekable();
    let mut changed_trailing = false;
    while let Some(c) = chars.next() {
        if c == ',' {
            let mut look = chars.clone();
            while matches!(look.peek(), Some(c) if c.is_whitespace()) {
                look.next();
            }
            if matches!(look.peek(), Some('}') | Some(']')) {
                changed_trailing = true;
                continue;
            }
        }
        no_trailing.push(c);
    }
    if changed_trailing {
        note.repairs.push("trailing comma");
        t = no_trailing;
    }

    // Raw control characters inside strings (pretty-printed answers pasted in).
    if let Some(fixed) = escape_control_in_strings(&t) {
        note.repairs.push("unescaped newline");
        t = fixed;
    }

    // Single-quoted payload with no double quotes at all.
    if !t.contains('"') && t.contains('\'') {
        let swapped = t.replace('\'', "\"");
        if serde_json::from_str::<Value>(&swapped).is_ok() {
            note.repairs.push("single quotes");
            t = swapped;
        }
    }

    serde_json::from_str::<Value>(&t).ok()
}

/// Replace tokens only when they are not inside a double-quoted string.
fn replace_outside_strings(text: &str, pairs: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    'outer: while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        for (from, to) in pairs {
            let f: Vec<char> = from.chars().collect();
            if bytes[i..].starts_with(&f) {
                out.push_str(to);
                i += f.len();
                continue 'outer;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Escape raw newlines/tabs/CRs that appear inside a string literal. Returns
/// `None` when nothing needed fixing.
fn escape_control_in_strings(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + 8);
    let mut in_string = false;
    let mut escaped = false;
    let mut changed = false;
    for c in text.chars() {
        if in_string {
            match c {
                '\n' if !escaped => {
                    out.push_str("\\n");
                    changed = true;
                }
                '\r' if !escaped => {
                    out.push_str("\\r");
                    changed = true;
                }
                '\t' if !escaped => {
                    out.push_str("\\t");
                    changed = true;
                }
                _ => out.push(c),
            }
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        if c == '"' {
            in_string = true;
        }
        out.push(c);
    }
    changed.then_some(out)
}

/// Coerce values to what the schema declares. Conservative: only shapes that
/// are unambiguous (`"3"` for a number, `"true"` for a boolean, a lone value
/// for an array, a JSON string for an object) and never inside a tool that
/// declared no schema for the property.
fn coerce_value(value: Value, schema: &Value, path: &str, note: &mut RepairNote) -> Value {
    let ty = schema.get("type").and_then(Value::as_str).unwrap_or("");
    match value {
        Value::String(s) => match ty {
            "number" | "integer" => {
                if let Ok(n) = s.trim().parse::<f64>() {
                    if ty == "integer" && n.fract() != 0.0 {
                        return Value::String(s);
                    }
                    note.coercions.push(format!("{path}: string→number"));
                    if ty == "integer" {
                        Value::from(n as i64)
                    } else {
                        Value::from(n)
                    }
                } else {
                    Value::String(s)
                }
            }
            "boolean" => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" => {
                    note.coercions.push(format!("{path}: string→boolean"));
                    Value::Bool(true)
                }
                "false" | "no" => {
                    note.coercions.push(format!("{path}: string→boolean"));
                    Value::Bool(false)
                }
                _ => Value::String(s),
            },
            "array" => match serde_json::from_str::<Value>(&s) {
                Ok(v @ Value::Array(_)) => {
                    note.coercions.push(format!("{path}: string→array"));
                    coerce_value(v, schema, path, note)
                }
                _ => {
                    note.coercions.push(format!("{path}: value→array"));
                    Value::Array(vec![Value::String(s)])
                }
            },
            "object" => match serde_json::from_str::<Value>(&s) {
                Ok(v @ Value::Object(_)) => {
                    note.coercions.push(format!("{path}: string→object"));
                    coerce_value(v, schema, path, note)
                }
                _ => Value::String(s),
            },
            _ => Value::String(s),
        },
        Value::Array(items) => {
            let item_schema = schema.get("items").cloned().unwrap_or(Value::Null);
            Value::Array(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| coerce_value(v, &item_schema, &format!("{path}[{i}]"), note))
                    .collect(),
            )
        }
        Value::Object(map) => {
            let props = schema.get("properties").and_then(Value::as_object);
            let mut out = Map::new();
            for (k, v) in map {
                let child = props
                    .and_then(|p| p.get(&k))
                    .cloned()
                    .unwrap_or(Value::Null);
                let child_path = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                out.insert(k, coerce_value(v, &child, &child_path, note));
            }
            Value::Object(out)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "force": {"type": "boolean"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "opts": {"type": "object", "properties": {"depth": {"type": "integer"}}},
            }
        })
    }

    #[test]
    fn clean_args_pass_through_untouched() {
        let (out, note) = repair_args(r#"{"path":"a.rs","limit":3}"#, &schema());
        assert_eq!(out, r#"{"path":"a.rs","limit":3}"#);
        assert!(!note.fired());
    }

    #[test]
    fn fences_and_prose_are_stripped() {
        let (out, note) = repair_args("```json\n{\"path\": \"a\"}\n```", &schema());
        assert_eq!(out, r#"{"path":"a"}"#);
        assert!(note.repairs.contains(&"code fence"));
        let (out, _) = repair_args("Here you go: {\"path\":\"b\"} enjoy", &schema());
        assert_eq!(out, r#"{"path":"b"}"#);
    }

    #[test]
    fn trailing_comma_and_python_literals_repair() {
        let (out, note) = repair_args(r#"{"path":"a","force":True,}"#, &schema());
        assert_eq!(out, r#"{"force":true,"path":"a"}"#);
        assert!(note.fired());
    }

    #[test]
    fn raw_newline_inside_a_string_is_escaped() {
        let (out, note) = repair_args("{\"path\":\"line1\nline2\"}", &schema());
        assert_eq!(out, r#"{"path":"line1\nline2"}"#);
        assert!(note.repairs.contains(&"unescaped newline"));
    }

    #[test]
    fn double_encoded_object_decodes_once() {
        let inner = r#"{"path":"c"}"#;
        let outer = serde_json::to_string(&inner).unwrap();
        let (out, _) = repair_args(&outer, &schema());
        assert_eq!(out, r#"{"path":"c"}"#);
    }

    #[test]
    fn schema_coercion_handles_numbers_booleans_arrays_and_nested() {
        let (out, note) = repair_args(
            r#"{"limit":"5","ratio":"0.5","force":"yes","tags":"a","opts":"{\"depth\":\"2\"}"}"#,
            &schema(),
        );
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["limit"], json!(5));
        assert_eq!(v["ratio"], json!(0.5));
        assert_eq!(v["force"], json!(true));
        assert_eq!(v["tags"], json!(["a"]));
        assert_eq!(v["opts"]["depth"], json!(2));
        assert!(note.coercions.len() >= 5, "{:?}", note.coercions);
    }

    #[test]
    fn a_fraction_is_not_truncated_into_an_integer_field() {
        let (out, _) = repair_args(r#"{"limit":"2.5"}"#, &schema());
        assert_eq!(
            serde_json::from_str::<Value>(&out).unwrap()["limit"],
            json!("2.5"),
            "a lossy coercion is worse than the tool's own error"
        );
    }

    #[test]
    fn unrepairable_text_is_handed_through() {
        let (out, note) = repair_args("not json at all", &schema());
        assert_eq!(out, "not json at all");
        assert!(!note.fired());
    }
}
