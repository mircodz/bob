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
    pub content: String,
    pub status: TodoStatus,
    /// In-progress label; falls back to `content`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_form: Option<String>,
}

impl TodoItem {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn todo_active_label_is_status_dependent_and_legacy_items_fall_back() {
        let mut item: TodoItem =
            serde_json::from_value(json!({"content": "Build", "status": "in_progress"})).unwrap();
        assert_eq!(render_todos(&[item.clone()]), "[~] Build");
        item.active_form = Some("Building".into());
        for (status, expected) in [
            (TodoStatus::Pending, "[ ] Build"),
            (TodoStatus::InProgress, "[~] Building"),
            (TodoStatus::Completed, "[x] Build"),
        ] {
            item.status = status;
            assert_eq!(render_todos(&[item.clone()]), expected);
        }
    }

    #[test]
    fn todo_json_round_trips_statuses_and_omits_absent_active_form() {
        for (status, encoded) in [
            (TodoStatus::Pending, "pending"),
            (TodoStatus::InProgress, "in_progress"),
            (TodoStatus::Completed, "completed"),
        ] {
            let value = json!({"content":"Task", "status":encoded});
            let item: TodoItem = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(item.status, status);
            assert_eq!(item.active_form, None);
            assert_eq!(serde_json::to_value(&item).unwrap(), value);
        }
        assert!(
            serde_json::from_value::<TodoItem>(json!({"content":"Task", "status":"unknown"}))
                .is_err()
        );
        assert!(serde_json::from_value::<TodoItem>(json!({"status":"pending"})).is_err());
    }
}
