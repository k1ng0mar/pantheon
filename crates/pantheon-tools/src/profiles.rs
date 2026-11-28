//! Surface profiles: which tool classes exist at all for a given session.
//!
//! Capability policy decides whether a call *runs*; a profile decides whether
//! the tool is even registered. That matters for untrusted origins: a webhook
//! body, a shared channel message or a subagent fed by third-party content
//! should not be able to talk the model into a shell it was never offered.
//!
//! Two profiles today:
//! * `full` (default) - every group the config enabled.
//! * `safe` - read-only surface: file reads, search, memory reads, skills,
//!   `ask_user`, `load_tools`, and `schedule` *listing*. No shell, no writes,
//!   no browser, no delegation, no plugins.

use crate::tools::ToolRegistry;
use pantheon_api::capability::Capability;

/// Resolved profile for one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Full,
    Safe,
}

impl Profile {
    /// Parse a config/env spelling. `None` for an unknown value so callers can
    /// warn instead of silently choosing `full`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "full" | "standard" => Some(Self::Full),
            "safe" | "readonly" | "read-only" => Some(Self::Safe),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Safe => "safe",
        }
    }
}

/// Whether `capability` may exist at all under `profile`.
pub fn allows(profile: Profile, capability: &Capability) -> bool {
    match profile {
        Profile::Full => true,
        // Fail closed: an unknown `Other(..)` capability is not assumed safe.
        // Only the two host-mediated read surfaces are allowed by name.
        Profile::Safe => match capability {
            Capability::FilesystemRead
            | Capability::MemoryRead
            | Capability::GitRead
            | Capability::NetworkOutbound => true,
            Capability::Other(name) => name == "load_tools" || name == "schedule",
            _ => false,
        },
    }
}

/// Apply a profile to a built registry.
pub fn apply(reg: &mut ToolRegistry, profile: Profile) {
    if profile == Profile::Full {
        return;
    }
    // A tool whose capability depends on its arguments can escalate past its
    // base capability (`skill_exec` is FilesystemRead plus ShellExecute per
    // call). The base capability alone is not enough to call it safe, so the
    // safe profile keeps only the two host-mediated dynamic tools.
    const SAFE_DYNAMIC: [&str; 2] = ["schedule", "load_tools"];
    reg.retain(|name, cap, dynamic| {
        if !allows(profile, cap) {
            return false;
        }
        !dynamic || SAFE_DYNAMIC.contains(&name)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "read_file".into(),
                description: "read".into(),
                parameters: serde_json::json!({}),
            },
            Capability::FilesystemRead,
            |_| Ok("ok".into()),
        );
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "shell".into(),
                description: "run".into(),
                parameters: serde_json::json!({}),
            },
            Capability::ShellExecute,
            |_| Ok("ok".into()),
        );
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "schedule".into(),
                description: "jobs".into(),
                parameters: serde_json::json!({}),
            },
            Capability::Other("schedule".into()),
            |_| Ok("ok".into()),
        );
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "schedule_write".into(),
                description: "jobs".into(),
                parameters: serde_json::json!({}),
            },
            Capability::Other("schedule.write".into()),
            |_| Ok("ok".into()),
        );
        reg
    }

    #[test]
    fn safe_drops_tools_that_can_escalate_per_call() {
        // `skill_exec` is FilesystemRead base + ShellExecute per call: the base
        // capability must not make it safe.
        let mut reg = ToolRegistry::new();
        reg.register_with(
            pantheon_api::message::ToolSchema {
                name: "skill_exec".into(),
                description: "exec".into(),
                parameters: serde_json::json!({}),
            },
            Capability::FilesystemRead,
            |_| Ok("ok".into()),
            Some(Box::new(|_| vec![Capability::ShellExecute])),
        );
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "schedule".into(),
                description: "jobs".into(),
                parameters: serde_json::json!({}),
            },
            Capability::Other("schedule".into()),
            |_| Ok("ok".into()),
        );
        apply(&mut reg, Profile::Safe);
        assert!(reg.get("skill_exec").is_none(), "dynamic capability dropped");
        assert!(reg.get("schedule").is_some(), "allowlisted dynamic tool kept");
    }

    #[test]
    fn safe_keeps_reads_and_drops_mutations() {
        let mut r = reg();
        apply(&mut r, Profile::Safe);
        assert!(r.get("read_file").is_some());
        assert!(r.get("schedule").is_some());
        assert!(r.get("shell").is_none(), "no shell on the safe surface");
        assert!(r.get("schedule_write").is_none(), "no mutating schedule");
        // an unknown Other(..) capability is not assumed safe
        let mut r2 = reg();
        r2.register(
            pantheon_api::message::ToolSchema {
                name: "skill_exec".into(),
                description: "exec".into(),
                parameters: serde_json::json!({}),
            },
            Capability::Other("skill_exec".into()),
            |_| Ok("ok".into()),
        );
        apply(&mut r2, Profile::Safe);
        assert!(r2.get("skill_exec").is_none(), "unknown Other is denied");
    }

    #[test]
    fn full_is_a_no_op_and_unknown_profiles_are_rejected() {
        let mut r = reg();
        let before = r.names().len();
        apply(&mut r, Profile::Full);
        assert_eq!(r.names().len(), before);
        assert_eq!(Profile::parse("safe"), Some(Profile::Safe));
        assert_eq!(Profile::parse("read-only"), Some(Profile::Safe));
        assert_eq!(Profile::parse(""), Some(Profile::Full));
        assert_eq!(Profile::parse("wat"), None);
    }
}
