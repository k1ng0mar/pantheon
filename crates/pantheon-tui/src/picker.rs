//! Pickers built on the widgets, reading real runtime state.
//!
//! Placeholder for L0. Every picker here is a function that turns catalog or
//! ledger data into `Item` rows; the widget does the interaction.

use pantheon_core::catalog;

use crate::widget::{Item, SearchList, Select};

/// Provider rows for the picker. `dev: true` entries are filtered out: the
/// local router is a development target, not a product provider.
pub fn provider_items() -> Vec<Item> {
    catalog::all_providers()
        .into_iter()
        .filter(|p| !is_dev_provider(p.id.as_str()))
        .map(|p| {
            let tag = if p.models.is_empty() {
                "no curated models".to_string()
            } else {
                format!("{} models", p.models.len())
            };
            Item::new(p.label.clone(), p.id.clone())
                .desc(p.tag.clone())
                .tag(tag)
        })
        .collect()
}

/// True for providers that exist to make the development loop work and must
/// not be offered as a product choice.
pub fn is_dev_provider(id: &str) -> bool {
    matches!(id, "router")
}

/// Model rows for one provider, from the catalog.
pub fn model_items(provider: &str) -> Vec<Item> {
    let Some(p) = catalog::provider(provider) else {
        return Vec::new();
    };
    p.models
        .iter()
        .map(|m| {
            let mut caps = Vec::new();
            if m.reasoning {
                caps.push("reasoning");
            }
            if m.vision {
                caps.push("vision");
            }
            if !m.tools {
                caps.push("no tools");
            }
            let desc = match m.context_limit {
                Some(c) => format!("{}k context", c / 1000),
                None => "context unknown".to_string(),
            };
            let item = Item::new(m.model.clone(), m.model.clone()).desc(desc);
            if caps.is_empty() {
                item
            } else {
                item.tag(caps.join(" · "))
            }
        })
        .collect()
}

/// The provider browser screen.
pub fn provider_screen() -> Select {
    Select::new("Choose a model provider", provider_items())
        .hint("up/down navigate · type to filter · enter select · esc back")
        .empty_reason("no providers in the catalog")
}

/// The model browser for a provider, restricted to that provider's models.
pub fn model_screen(provider: &str) -> Select {
    let items = model_items(provider);
    if items.is_empty() {
        // A provider with no curated rows is a real state, and saying it is
        // better than an empty list that looks like a bug.
        return Select::new(format!("Model on {provider}"), items)
            .empty_reason("this provider has no curated models; use /settings to add one by name");
    }
    Select::new(format!("Model on {provider}"), items)
}

/// Session rows from the ledger, newest first.
pub fn session_screen(runs: &[(String, String, i64, Option<String>)]) -> SearchList {
    let items: Vec<Item> = runs
        .iter()
        .map(|(id, status, ts, title)| {
            let label = title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| id.chars().skip(4).take(8).collect::<String>());
            Item::new(label, id.clone()).desc(format!("{} · {}", status, age(*ts)))
        })
        .collect();
    SearchList::new("Conversations", items, "no runs yet")
}

fn age(ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let s = (now - ms).max(0) / 1000;
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

#[cfg(test)]
#[path = "picker_tests.rs"]
mod tests;
