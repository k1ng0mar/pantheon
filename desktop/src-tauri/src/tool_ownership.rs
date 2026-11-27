//! Architecture regression tests for Pantheon Desktop tool ownership.
//!
//! These tests fail when the desktop starts growing a parallel agent tool
//! ecosystem or an alternative harness.

#[cfg(test)]
mod tests {
    #[test]
    fn only_pantheon_can_be_selected_or_launched() {
        assert_eq!(crate::providers::PROVIDERS.len(), 1);
        assert_eq!(crate::providers::PROVIDERS[0].0, "pantheon");
        assert_eq!(crate::providers::HARNESSES, &["pantheon"]);
        assert_eq!(crate::cloud_agents::setup::HARNESSES, ["pantheon"]);
    }

    #[test]
    fn every_live_catalog_tool_is_in_the_pantheon_contract() {
        let mut tools = Vec::new();
        tools.extend(crate::agent_tools::catalog());
        tools.extend(crate::agent_tools::preview_catalog());
        tools.extend(crate::agent_tools::optional_jev_catalog(true, true));
        tools.extend(crate::architect::catalog());
        tools.extend(crate::architect::coordination::tools(false));
        tools.extend(crate::bots::tools::catalog());
        tools.extend(crate::cloud::deploy::catalog());
        tools.extend(crate::design::catalog());
        tools.extend(crate::simulator::tools::catalog());
        tools.extend(crate::vm::tools::catalog());
        pantheon_api::desktop_tool_contract::validate_catalog(&tools)
            .expect("live desktop catalogs must use the Pantheon contract");
        for name in [crate::session_titles::TOOL, crate::turn_changes::host::TOOL] {
            assert!(
                pantheon_api::desktop_tool_contract::is_desktop_tool(name),
                "adapter tool is outside the contract: {name}"
            );
        }
    }

    #[test]
    fn unknown_and_foreign_tools_cannot_be_registered() {
        for name in ["shell", "legacy_review", "pantheon_not_a_tool"] {
            let tools = vec![serde_json::json!({
                "type": "function",
                "name": name,
                "description": "foreign",
                "inputSchema": {"type": "object"},
            })];
            assert!(
                pantheon_api::desktop_tool_contract::validate_catalog(&tools).is_err(),
                "foreign tool was accepted: {name}"
            );
        }
    }
}
