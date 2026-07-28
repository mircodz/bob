//! Simple todo list the agent maintains to plan and track multi-step work.
//! The full list is replaced on each write.

use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TodoItem {
    /// Imperative description of the task ("Add auth middleware").
    pub content: String,
    pub status: TodoStatus,
    /// Present-continuous form shown while in progress ("Adding auth middleware").
    /// Falls back to `content` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

impl TodoItem {
    /// The label to show for this item given its status.
    pub fn label(&self) -> &str {
        if self.status == TodoStatus::InProgress {
            self.active_form.as_deref().unwrap_or(&self.content)
        } else {
            &self.content
        }
    }
}

#[derive(Default)]
pub struct TodoStore {
    items: Mutex<Vec<TodoItem>>,
}

impl TodoStore {
    pub fn new() -> Self {
        TodoStore::default()
    }

    pub fn set(&self, items: Vec<TodoItem>) {
        *self.items.lock().unwrap() = items;
    }

    pub fn items(&self) -> Vec<TodoItem> {
        self.items.lock().unwrap().clone()
    }
}

pub fn render_todos(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "(todo list empty)".to_string();
    }
    items
        .iter()
        .map(|t| {
            let mark = match t.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Completed => "[x]",
            };
            format!("{} {}", mark, t.label())
        })
        .collect::<Vec<_>>()
        .join("\n")
}
