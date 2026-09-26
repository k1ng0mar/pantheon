//! Shared CLI argument helpers. One parser for every verb: `--flag value`
//! and `--flag=value` both work, positional args are those that do not
//! start with `--`.
use std::collections::HashMap;

/// Parsed arguments: positional values in order plus a flag map (last
/// occurrence wins).
pub struct Args {
    pub positionals: Vec<String>,
    flags: HashMap<String, String>,
}

impl Args {
    pub fn parse(raw: &[String]) -> Self {
        let mut positionals = Vec::new();
        let mut flags = HashMap::new();
        let mut i = 0;
        while i < raw.len() {
            let a = &raw[i];
            if let Some(stripped) = a.strip_prefix("--") {
                if let Some((k, v)) = stripped.split_once('=') {
                    flags.insert(k.to_string(), v.to_string());
                } else if let Some(v) = raw.get(i + 1) {
                    if v.starts_with("--") {
                        // A bare flag followed by another flag is a boolean
                        // switch. Without this the following flag is eaten as
                        // its value and silently disappears: `--yes
                        // --provider openai` lost the provider entirely.
                        flags.insert(stripped.to_string(), String::new());
                    } else {
                        flags.insert(stripped.to_string(), v.clone());
                        i += 1;
                    }
                } else {
                    // Boolean flag at end of argv.
                    flags.insert(stripped.to_string(), String::new());
                }
            } else {
                positionals.push(a.clone());
            }
            i += 1;
        }
        Self { positionals, flags }
    }
    pub fn flag(&self, name: &str) -> Option<String> {
        self.flags.get(name).filter(|v| !v.is_empty()).cloned()
    }
    /// True when the flag appeared, even without a value (boolean flags).
    pub fn has(&self, name: &str) -> bool {
        self.flags.contains_key(name)
    }
    pub fn positional(&self, index: usize) -> Option<String> {
        self.positionals.get(index).cloned()
    }
}

#[cfg(test)]
#[path = "cli_args_tests.rs"]
mod tests;
