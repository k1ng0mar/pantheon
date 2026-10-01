//! Canonical todo types: the agent's per-session task list.
//!
//! The `todo` tool (pantheon-tools) replaces the whole list in one call,
//! the event ledger (pantheon-storage) persists it per run, and the TUI
//! renders it as a card. This module is the single definition they all
//! share. Status strings are `pending` / `in_progress` / `completed` on
//! the wire (opencode/Claude-Code style); `activeForm` is the optional
//! present-tense phrasing shown while an item is in progress.

use serde::{Deserialize, Serialize};

/// Tool name, capability token, and ledger key prefix for the todo list.
pub const TODO_TOOL_NAME: &str = "todo";

/// One todo's lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    /// The wire string: `pending` / `in_progress` / `completed`.
    pub fn as_str(self) -> &'static str {
        match self {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in_progress",
            TodoStatus::Completed => "completed",
        }
    }

    /// Parse a model-supplied status string. Anything else is `None` so
    /// the tool layer rejects it with `TOOL_BAD_ARGS` instead of guessing.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" => Some(TodoStatus::InProgress),
            "completed" => Some(TodoStatus::Completed),
            _ => None,
        }
    }
}

/// One entry on the agent's todo list.
///
/// Wire format is camelCase (`activeForm`), matching the opencode /
/// Claude-Code tool schema the model sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TodoItem {
    /// The task, imperative phrasing ("Read the auth module").
    pub content: String,
    pub status: TodoStatus,
    /// Present-tense phrasing shown while the item is in progress
    /// ("Reading the auth module"). Optional; only meaningful on
    /// `in_progress` items.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

/// Why a replacement list was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TodoValidationError {
    /// An item's content was empty or whitespace-only (index into the
    /// proposed list).
    EmptyContent(usize),
    /// More than one item claimed `in_progress`.
    MultipleInProgress,
}

impl std::fmt::Display for TodoValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TodoValidationError::EmptyContent(i) => {
                write!(f, "todos[{i}]: content must not be empty")
            }
            TodoValidationError::MultipleInProgress => write!(
                f,
                "at most one todo may be in_progress; finish or pause the current one first"
            ),
        }
    }
}

/// The whole per-session list. Replace-only: there is no per-item update
/// call; the tool hands over the full new list and [`TodoList::replace`]
/// validates it as a unit. Replacing is the update mechanism.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

impl TodoList {
    /// Replace the list, enforcing the invariants: every item has
    /// non-empty content and at most one item is `in_progress`.
    /// On error the list is left untouched.
    pub fn replace(&mut self, items: Vec<TodoItem>) -> Result<(), TodoValidationError> {
        let mut in_progress = 0usize;
        for (i, item) in items.iter().enumerate() {
            if item.content.trim().is_empty() {
                return Err(TodoValidationError::EmptyContent(i));
            }
            if item.status == TodoStatus::InProgress {
                in_progress += 1;
            }
        }
        if in_progress > 1 {
            return Err(TodoValidationError::MultipleInProgress);
        }
        self.items = items;
        Ok(())
    }

    /// The single item currently being worked, if any.
    pub fn in_progress(&self) -> Option<&TodoItem> {
        self.items
            .iter()
            .find(|i| i.status == TodoStatus::InProgress)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}
