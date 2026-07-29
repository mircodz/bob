//! The todo_write tool: replaces the current todo list.

use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext};
use crate::tools::todo::{render_todos, TodoItem, TodoStatus};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct TodoWriteTool;

#[async_trait]
impl Tool for TodoWriteTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "todo_write".to_string(),
            description: "Replace the current todo list to plan and track a multi-step task. Use \
                it for anything with 3+ non-trivial steps; skip it for trivial one-step work. Keep \
                EXACTLY one item `in_progress` at a time, and mark items `completed` the moment \
                you finish them (don't batch). Each item has `content` (imperative, e.g. \"Add \
                the auth middleware\"), an optional `active_form` (present-continuous shown while \
                running, e.g. \"Adding the auth middleware\"), and `status`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "Imperative task description." },
                                "active_form": { "type": "string", "description": "Present-continuous form shown while in progress." },
                                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] }
                            },
                            "required": ["content", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> String {
        let items: Vec<TodoItem> = input["todos"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| {
                        let content = t["content"].as_str()?.to_string();
                        let status = match t["status"].as_str()? {
                            "in_progress" => TodoStatus::InProgress,
                            "completed" => TodoStatus::Completed,
                            _ => TodoStatus::Pending,
                        };
                        let active_form = t["active_form"].as_str().map(|s| s.to_string());
                        Some(TodoItem {
                            content,
                            status,
                            active_form,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let (mut done, mut prog, mut pend) = (0, 0, 0);
        for t in &items {
            match t.status {
                TodoStatus::Completed => done += 1,
                TodoStatus::InProgress => prog += 1,
                TodoStatus::Pending => pend += 1,
            }
        }
        let rendered = render_todos(&items);
        ctx.todos.set(items);
        format!(
            "todo list updated ({} done, {} in progress, {} pending)\n{}",
            done, prog, pend, rendered
        )
    }
}
