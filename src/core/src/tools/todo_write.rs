use crate::core::types::ToolSpec;
use crate::tools::registry::{Tool, ToolContext, ToolResult};
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
                                "content": { "type": "string", "description": "Short imperative task label, ideally 3-7 words (e.g. \"Add auth middleware\", \"Fix fmt failures\"). NOT a paragraph — keep it scannable; details go in your reply, not the todo." },
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

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
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
        Ok(format!(
            "todo list updated ({} done, {} in progress, {} pending)\n{}",
            done, prog, pend, rendered
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{files::tool_context, TestDir};

    #[tokio::test]
    async fn todo_write_replaces_instead_of_appending_and_can_clear() {
        let dir = TestDir::new();
        let ctx = tool_context(dir.path());
        TodoWriteTool
            .execute(
                json!({"todos": [
                    {"content": "Old task", "status": "pending"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        let output = TodoWriteTool.execute(json!({"todos": [
            {"content": "Implement", "active_form": "Implementing", "status": "in_progress"},
            {"content": "Verify", "status": "pending"},
            {"content": "Inspect", "active_form": "Inspecting", "status": "completed"}
        ]}), &ctx).await.unwrap();
        assert_eq!(output.as_text().unwrap(), "todo list updated (1 done, 1 in progress, 1 pending)\n[~] Implementing\n[ ] Verify\n[x] Inspect");
        let snapshot = ctx.todos.items();
        assert_eq!(
            snapshot
                .iter()
                .map(|item| item.content.as_str())
                .collect::<Vec<_>>(),
            ["Implement", "Verify", "Inspect"]
        );
        assert_eq!(
            TodoWriteTool
                .execute(json!({"todos": []}), &ctx)
                .await
                .unwrap()
                .as_text()
                .unwrap(),
            "todo list updated (0 done, 0 in progress, 0 pending)\n(todo list empty)"
        );
        assert!(ctx.todos.items().is_empty());
        assert_eq!(snapshot.len(), 3);
    }

    #[tokio::test]
    async fn registered_invalid_write_preserves_the_entire_previous_list() {
        use crate::tools::registry::{ToolErrorKind, ToolRegistry};
        use std::sync::Arc;

        let dir = TestDir::new();
        let ctx = tool_context(dir.path());
        let mut registry = ToolRegistry::new(None);
        registry.add(Arc::new(TodoWriteTool));
        registry
            .execute(
                "todo_write",
                json!({"todos":[
                    {"content":"Keep", "status":"in_progress", "active_form":"Keeping"}
                ]}),
                &ctx,
            )
            .await
            .unwrap();
        let before = serde_json::to_value(ctx.todos.items()).unwrap();
        for input in [
            json!({}),
            json!({"todos":null}),
            json!({"todos":{}}),
            json!({"todos":[{"content":"Missing status"}]}),
            json!({"todos":[{"content":"Bad", "status":"unknown"}]}),
            json!({"todos":[{"content":12, "status":"pending"}]}),
            json!({"todos":[{"content":"Bad", "status":"pending", "active_form":false}]}),
            json!({"todos":[{"content":"Valid prefix", "status":"completed"}, {}]}),
        ] {
            let error = registry
                .execute("todo_write", input.clone(), &ctx)
                .await
                .unwrap_err();
            assert_eq!(error.kind, ToolErrorKind::InvalidInput, "{input}");
            assert_eq!(
                serde_json::to_value(ctx.todos.items()).unwrap(),
                before,
                "{input}"
            );
        }
    }
}
